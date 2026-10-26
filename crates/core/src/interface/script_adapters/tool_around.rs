//! `tool.around` — one decision, taken before a tool call is dispatched.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::interface::script::{ScriptCarrier, ScriptOutcome};
use crate::interface::tool_middleware::{
    NextDispatch, ToolAnswer, ToolCall, ToolExec, ToolMiddleware, ToolOutcome,
};

/// A script bound to the tool-middleware point.
///
/// # What the script is given
///
/// ```json
/// { "tool": "Read", "input": { "file_path": "/etc/hosts" } }
/// ```
///
/// # What it may return
///
/// One object, or anything else to mean "carry on":
///
/// ```json
/// { "action": "deny", "reason": "reads outside the project are not allowed" }
/// { "action": "respond", "text": "(cached)" }
/// { "action": "proceed", "timeoutMs": 2000 }
/// ```
///
/// - **deny** — the call is not dispatched and the tool reports `reason` as
///   its error. Without a `reason` there is nothing to tell the model, so the
///   decision is discarded and the call proceeds.
/// - **respond** — the call is not dispatched and `text` is the result the
///   model sees, which is how a cache hit is expressed. Without a `text`
///   string it proceeds, because a wrapper that answered an absent field with
///   an empty result would be handing the model a tool that silently produced
///   nothing.
/// - **proceed** — dispatch normally. `timeoutMs`, on this or on any decision
///   that dispatches, stops the call after that long.
///
/// Anything else — a number, `null`, an unknown `action`, a script that threw
/// or ran out of time — dispatches the call exactly as it would have been
/// dispatched with no script bound.
///
/// # What it cannot return
///
/// **New arguments.** `Read(a.txt)` cannot become `Read(~/.ssh/id_rsa)` here.
/// Rewriting an approved call into a different one is a different act with
/// different trust rules, and the point itself does not offer it — the call
/// arrives behind a shared reference and dispatch takes no arguments. A
/// script that wants different arguments has to say so where arguments are
/// decided, not in the ring around them.
///
/// **A longer deadline.** `timeoutMs` narrows and never widens: the signal it
/// installs is a child of the turn's, so it can fire earlier than the session
/// gave up on the call but never later. A script asking for an hour on a
/// cancelled turn gets the cancellation.
///
/// **A different result after the fact.** The script runs once, before
/// dispatch, and does not see the outcome. That keeps the cost at one call per
/// tool call, and what the result looks like already has a point of its own in
/// `tool.result`, which runs later and therefore wins anyway.
///
/// # What a denial is and is not
///
/// This ring sits outside the permission gate, so a denial here refuses a call
/// before anything is asked of the user, and a `respond` answers without the
/// gate being consulted at all. Neither exercises a capability — nothing is
/// executed on either path — but both let a script decide what the model is
/// told a tool did, which is the same authority `tool.result` already carries.
pub struct ToolAroundScript {
    carrier: Arc<ScriptCarrier>,
    entry: String,
}

impl ToolAroundScript {
    pub fn new(carrier: Arc<ScriptCarrier>, entry: impl Into<String>) -> Self {
        Self {
            carrier,
            entry: entry.into(),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Decision {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// What the adapter decided to do, once the returned value has been read.
enum Directed {
    Dispatch { timeout: Option<Duration> },
    Deny(String),
    Respond(String),
}

fn read(returned: serde_json::Value) -> Directed {
    let Ok(decision) = serde_json::from_value::<Decision>(returned) else {
        return Directed::Dispatch { timeout: None };
    };
    match decision.action.as_deref() {
        Some("deny") => match decision.reason {
            Some(reason) => Directed::Deny(reason),
            None => Directed::Dispatch { timeout: None },
        },
        Some("respond") => match decision.text {
            Some(text) => Directed::Respond(text),
            None => Directed::Dispatch { timeout: None },
        },
        _ => Directed::Dispatch {
            timeout: decision.timeout_ms.map(Duration::from_millis),
        },
    }
}

/// A plain `proceed` is what the call would have done anyway, so only a
/// denial, an answer, or a narrowed deadline counts as this ring having done
/// something.
fn outcome_of(directed: &Directed) -> ScriptOutcome {
    match directed {
        Directed::Deny(_) | Directed::Respond(_) => ScriptOutcome::Applied,
        Directed::Dispatch { timeout: Some(_) } => ScriptOutcome::Applied,
        Directed::Dispatch { timeout: None } => ScriptOutcome::NoChange {
            detail: Some("dispatched as it would have been".into()),
        },
    }
}

#[async_trait]
impl ToolMiddleware for ToolAroundScript {
    async fn around(
        &self,
        call: &ToolCall,
        exec: &mut ToolExec,
        next: NextDispatch<'_>,
    ) -> ToolOutcome {
        let input = serde_json::json!({
            "tool": call.name,
            "input": call.input,
        });

        let directed = match self.carrier.call(&self.entry, input).await {
            Ok(returned) => {
                let directed = read(returned);
                self.carrier.record(&self.entry, outcome_of(&directed));
                directed
            }
            Err(error) => {
                tracing::warn!(
                    script = %self.carrier.script().id,
                    tool = %call.name,
                    error = %error,
                    "tool-around script did not run; the call is dispatched unchanged"
                );
                self.carrier
                    .record(&self.entry, ScriptOutcome::Failed { error });
                Directed::Dispatch { timeout: None }
            }
        };

        match directed {
            Directed::Deny(reason) => Err(reason),
            Directed::Respond(text) => Ok(ToolAnswer::text(text)),
            Directed::Dispatch { timeout } => {
                if let Some(after) = timeout {
                    exec.with_timeout(after);
                }
                next.run(call, exec).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::script::{
        FnScriptEngine, ScriptEngine, ScriptError, ScriptLimits, ScriptSource,
    };
    use crate::interface::tool_middleware::dispatch_through;
    use crate::prompt::BlockOrigin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    fn engine_returning(
        value: serde_json::Value,
    ) -> Arc<dyn crate::interface::script::ScriptEngine> {
        Arc::new(FnScriptEngine(
            move |_s: &ScriptSource, _e: &str, _i: serde_json::Value| {
                let value = value.clone();
                async move { Ok(value) }
            },
        ))
    }

    fn wrapper(engine: Arc<dyn ScriptEngine>) -> Arc<dyn ToolMiddleware> {
        Arc::new(ToolAroundScript::new(
            Arc::new(ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.agix/scripts/tool.js".into(),
                    origin: BlockOrigin::Script("./.agix/scripts/tool.js".into()),
                    code: String::new(),
                },
                "tool.around",
                ScriptLimits::default(),
            )),
            "onTool",
        ))
    }

    fn call() -> ToolCall {
        ToolCall {
            name: "Read".into(),
            input: serde_json::json!({"file_path": "a.txt"}),
        }
    }

    async fn run(engine: Arc<dyn ScriptEngine>, dispatched: Arc<AtomicUsize>) -> ToolOutcome {
        let chain = vec![wrapper(engine)];
        dispatch_through(&chain, call(), CancellationToken::new(), move |_| {
            let dispatched = dispatched.clone();
            async move {
                dispatched.fetch_add(1, Ordering::SeqCst);
                Ok(ToolAnswer::text("the file"))
            }
        })
        .await
    }

    /// The acceptance case: a script refuses a call, and the tool never runs.
    #[tokio::test]
    async fn a_script_can_refuse_a_call_before_it_is_dispatched() {
        let dispatched = Arc::new(AtomicUsize::new(0));
        let out = run(
            engine_returning(serde_json::json!({"action": "deny", "reason": "not this one"})),
            dispatched.clone(),
        )
        .await;
        assert_eq!(out, Err("not this one".to_string()));
        assert_eq!(
            dispatched.load(Ordering::SeqCst),
            0,
            "a denial must not dispatch"
        );
    }

    #[tokio::test]
    async fn a_script_can_answer_instead_of_dispatching() {
        let dispatched = Arc::new(AtomicUsize::new(0));
        let out = run(
            engine_returning(serde_json::json!({"action": "respond", "text": "(cached)"})),
            dispatched.clone(),
        )
        .await;
        assert_eq!(out, Ok(ToolAnswer::text("(cached)")));
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    }

    /// A denial with nothing to say is not a denial: there would be no message
    /// for the model, so the call proceeds rather than failing blankly.
    #[tokio::test]
    async fn a_decision_missing_the_field_it_needs_dispatches_anyway() {
        for undecided in [
            serde_json::json!({"action": "deny"}),
            serde_json::json!({"action": "respond"}),
        ] {
            let dispatched = Arc::new(AtomicUsize::new(0));
            let out = run(engine_returning(undecided.clone()), dispatched.clone()).await;
            assert_eq!(out, Ok(ToolAnswer::text("the file")), "{undecided}");
            assert_eq!(dispatched.load(Ordering::SeqCst), 1, "{undecided}");
        }
    }

    #[tokio::test]
    async fn a_script_that_returns_nonsense_leaves_the_call_alone() {
        for nonsense in [
            serde_json::json!("deny"),
            serde_json::json!(7),
            serde_json::json!(null),
            serde_json::json!({"action": "explode"}),
            serde_json::json!({"action": "deny", "reason": 12}),
        ] {
            let dispatched = Arc::new(AtomicUsize::new(0));
            let out = run(engine_returning(nonsense.clone()), dispatched.clone()).await;
            assert_eq!(out, Ok(ToolAnswer::text("the file")), "{nonsense}");
            assert_eq!(dispatched.load(Ordering::SeqCst), 1, "{nonsense}");
        }
    }

    #[tokio::test]
    async fn a_script_that_fails_leaves_the_call_alone() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(FnScriptEngine(
            |_s: &ScriptSource, _e: &str, _i: serde_json::Value| async {
                Err(ScriptError::Failed("deliberate".into()))
            },
        ));
        let dispatched = Arc::new(AtomicUsize::new(0));
        let out = run(engine, dispatched.clone()).await;
        assert_eq!(out, Ok(ToolAnswer::text("the file")));
        assert_eq!(dispatched.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_script_that_hangs_leaves_the_call_alone() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(FnScriptEngine(
            |_s: &ScriptSource, _e: &str, _i: serde_json::Value| async {
                std::future::pending::<()>().await;
                unreachable!()
            },
        ));
        let chain: Vec<Arc<dyn ToolMiddleware>> = vec![Arc::new(ToolAroundScript::new(
            Arc::new(ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.agix/scripts/slow.js".into(),
                    origin: BlockOrigin::Script("./.agix/scripts/slow.js".into()),
                    code: String::new(),
                },
                "tool.around",
                ScriptLimits {
                    timeout: Duration::from_millis(20),
                    ..Default::default()
                },
            )),
            "onTool",
        ))];
        let out = dispatch_through(&chain, call(), CancellationToken::new(), |_| async {
            Ok(ToolAnswer::text("the file"))
        })
        .await;
        assert_eq!(out, Ok(ToolAnswer::text("the file")));
    }

    /// The deadline a script asks for is felt by the tool.
    #[tokio::test]
    async fn a_script_can_impose_a_deadline_the_call_feels() {
        let chain = vec![wrapper(engine_returning(
            serde_json::json!({"action": "proceed", "timeoutMs": 20}),
        ))];
        let out = dispatch_through(
            &chain,
            call(),
            CancellationToken::new(),
            |cancel| async move {
                cancel.cancelled().await;
                Err("cancelled".to_string())
            },
        )
        .await;
        assert_eq!(out, Err("cancelled".to_string()));
    }

    /// The deadline is one-way. A script asking for an hour on a turn that has
    /// already been cancelled still gets the cancellation.
    #[tokio::test]
    async fn a_script_cannot_extend_a_call_past_the_turn() {
        let chain = vec![wrapper(engine_returning(
            serde_json::json!({"timeoutMs": 3_600_000}),
        ))];
        let turn = CancellationToken::new();
        let fires = turn.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            fires.cancel();
        });
        let out = dispatch_through(&chain, call(), turn, |cancel| async move {
            cancel.cancelled().await;
            Err("cancelled".to_string())
        })
        .await;
        assert_eq!(out, Err("cancelled".to_string()));
    }

    /// The script sees the call it is wrapping, not some other one.
    #[tokio::test]
    async fn the_script_is_given_the_call_it_wraps() {
        let seen = Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let recorded = seen.clone();
        let engine: Arc<dyn ScriptEngine> = Arc::new(FnScriptEngine(
            move |_s: &ScriptSource, _e: &str, input: serde_json::Value| {
                let recorded = recorded.clone();
                async move {
                    *recorded.lock().unwrap() = input;
                    Ok(serde_json::Value::Null)
                }
            },
        ));
        let _ = run(engine, Arc::new(AtomicUsize::new(0))).await;
        assert_eq!(
            *seen.lock().unwrap(),
            serde_json::json!({"tool": "Read", "input": {"file_path": "a.txt"}})
        );
    }
}
