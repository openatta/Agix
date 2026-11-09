//! `ToolMiddleware` — the ring around tool dispatch.
//!
//! Timeouts, retries, metrics, caching, idempotent de-duplication: none of
//! these belong to a tool's implementation and none of them are permission
//! decisions. They are the things you wrap a call in. With nowhere to put
//! them, they get hardcoded into the loop — which is where the engine's
//! existing timeout and retry logic lives today, and why adding a per-tool
//! deadline currently means editing the turn.
//!
//! # What a wrapper may do, and the one thing it may not
//!
//! It may **narrow the signal**: hand the call a stricter cancellation than
//! the turn's, which is how a timeout is expressed. It may **short-circuit**:
//! answer without calling through at all, which is how a cache hit is
//! expressed. It may **call through more than once**, which is how a retry is
//! expressed.
//!
//! It may not rewrite the call's input. That is a different act with a
//! different risk — rewriting arguments turns an approved call into a
//! different call, after approval — and it belongs to a hook point of its own
//! with its own trust rules. Here it is not a rule to remember, it is a thing
//! the types do not let you say: the call arrives behind a shared reference
//! and dispatch takes no arguments.
//!
//! # Order
//!
//! Wrappers nest in registration order: the first registered is outermost, so
//! it sees the others' retries as one call. That is the ordering a metrics
//! wrapper wants (register it first and it measures everything inside) and
//! the one a cache wants (register it first and a hit costs nothing further
//! in).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// What dispatch returns.
pub type ToolOutcome = Result<ToolAnswer, String>;

/// A tool that ran, and what it produced.
///
/// `reported_failure` is not the same thing as the `Err` this rides beside.
/// `Err` is the engine failing to run a tool at all: it cancels the rest of a
/// concurrent batch and fires `PostToolUseFailure`. This is the tool running
/// exactly as designed and saying the work did not go well — a shell command
/// with a non-zero exit, an HTTP status a fetch will not follow, an MCP
/// server answering `isError`, a plugin whose guest trapped. The model has to
/// be able to tell those from an answer, which is what `is_error` on the wire
/// is for, and carrying the flag here is the only way it gets there: a
/// wrapper sits between the tool and the caller, so a shape with no room for
/// it drops it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolAnswer {
    /// The text the model sees.
    pub text: String,
    /// Messages the tool asked to have injected.
    pub new_messages: Option<Vec<serde_json::Value>>,
    /// The tool ran and is calling its own result a failure.
    pub reported_failure: bool,
    /// The result's structured content, when the tool gave one.
    pub structured: Option<serde_json::Value>,
}

impl ToolAnswer {
    /// A tool that ran and answered.
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            new_messages: None,
            reported_failure: false,
            structured: None,
        }
    }

    /// A tool that ran and is calling the result a failure.
    pub fn failure(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            new_messages: None,
            reported_failure: true,
            structured: None,
        }
    }
}

/// The call being made, as a wrapper may see it. Read-only by design — see
/// the module docs.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub input: serde_json::Value,
}

/// The conditions the call runs under, as a wrapper may change them.
pub struct ToolExec {
    /// Cancelled when the call should stop. A wrapper replaces this to impose
    /// something stricter; it cannot loosen the turn's, because a child token
    /// fires when its parent does no matter what.
    cancel: CancellationToken,
}

impl ToolExec {
    pub fn new(cancel: CancellationToken) -> Self {
        Self { cancel }
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Stop the call after `after`, on top of whatever already stops it.
    ///
    /// The new signal is a *child* of the current one, which is what makes
    /// this one-way: it can fire earlier than the turn's cancellation, never
    /// later, so a wrapper cannot extend a call past the point the session
    /// gave up on it.
    pub fn with_timeout(&mut self, after: Duration) {
        let child = self.cancel.child_token();
        let fires = child.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(after) => fires.cancel(),
                // The parent already ended it; nothing left to do, and the
                // timer should not outlive the call it belongs to.
                _ = fires.cancelled() => {}
            }
        });
        self.cancel = child;
    }
}

type Terminal<'a> = dyn Fn(CancellationToken) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>
    + Send
    + Sync
    + 'a;

/// The rest of the chain, ending in the engine's own dispatch.
pub struct NextDispatch<'a> {
    remaining: &'a [Arc<dyn ToolMiddleware>],
    terminal: &'a Terminal<'a>,
}

impl<'a> NextDispatch<'a> {
    /// Run everything inside this wrapper.
    ///
    /// Takes `&self` so it can be called more than once — that is what makes
    /// a retry expressible rather than something a wrapper has to fake.
    pub async fn run(&self, call: &ToolCall, exec: &mut ToolExec) -> ToolOutcome {
        match self.remaining.split_first() {
            Some((next, rest)) => {
                let inner = NextDispatch {
                    remaining: rest,
                    terminal: self.terminal,
                };
                next.around(call, exec, inner).await
            }
            None => (self.terminal)(exec.cancel.clone()).await,
        }
    }
}

/// A ring around every tool call.
#[async_trait]
pub trait ToolMiddleware: Send + Sync {
    async fn around(
        &self,
        call: &ToolCall,
        exec: &mut ToolExec,
        next: NextDispatch<'_>,
    ) -> ToolOutcome;
}

/// Run `chain` around `dispatch`.
///
/// With an empty chain this is `dispatch` and nothing else — no clone, no
/// allocation, no future wrapped in another future. That matters: tool
/// dispatch is on the hot path, and a seam nobody uses should cost nothing.
pub async fn dispatch_through<'a, F, Fut>(
    chain: &'a [Arc<dyn ToolMiddleware>],
    call: ToolCall,
    cancel: CancellationToken,
    dispatch: F,
) -> ToolOutcome
where
    F: Fn(CancellationToken) -> Fut + Send + Sync + 'a,
    Fut: Future<Output = ToolOutcome> + Send + 'a,
{
    let terminal = move |cancel: CancellationToken| {
        Box::pin(dispatch(cancel)) as Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>
    };
    let next = NextDispatch {
        remaining: chain,
        terminal: &terminal,
    };
    let mut exec = ToolExec::new(cancel);
    next.run(&call, &mut exec).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn call() -> ToolCall {
        ToolCall {
            name: "Probe".into(),
            input: serde_json::json!({}),
        }
    }

    async fn run(
        chain: Vec<Arc<dyn ToolMiddleware>>,
        dispatch: impl Fn(CancellationToken) -> ToolOutcome + Send + Sync,
    ) -> ToolOutcome {
        dispatch_through(&chain, call(), CancellationToken::new(), |c| {
            let out = dispatch(c);
            async move { out }
        })
        .await
    }

    #[tokio::test]
    async fn an_empty_chain_is_the_call_itself() {
        let out = run(vec![], |_| Ok(ToolAnswer::text("ran"))).await;
        assert_eq!(out, Ok(ToolAnswer::text("ran")));
    }

    /// The acceptance case: a wrapper imposes a deadline the tool actually
    /// feels, without the turn knowing anything about it.
    #[tokio::test]
    async fn a_wrapper_can_impose_a_deadline_the_call_feels() {
        struct Deadline(Duration);
        #[async_trait]
        impl ToolMiddleware for Deadline {
            async fn around(
                &self,
                call: &ToolCall,
                exec: &mut ToolExec,
                next: NextDispatch<'_>,
            ) -> ToolOutcome {
                exec.with_timeout(self.0);
                next.run(call, exec).await
            }
        }

        let chain: Vec<Arc<dyn ToolMiddleware>> =
            vec![Arc::new(Deadline(Duration::from_millis(20)))];
        let outcome = dispatch_through(
            &chain,
            call(),
            CancellationToken::new(),
            |cancel| async move {
                // A tool that would run forever if nothing stopped it.
                cancel.cancelled().await;
                Err("cancelled".to_string())
            },
        )
        .await;
        assert_eq!(outcome, Err("cancelled".to_string()));
    }

    /// A wrapper narrows the signal; it cannot widen it. The turn's own
    /// cancellation still ends the call.
    #[tokio::test]
    async fn a_wrapper_cannot_outlive_the_turns_cancellation() {
        struct Generous;
        #[async_trait]
        impl ToolMiddleware for Generous {
            async fn around(
                &self,
                call: &ToolCall,
                exec: &mut ToolExec,
                next: NextDispatch<'_>,
            ) -> ToolOutcome {
                exec.with_timeout(Duration::from_secs(3600));
                next.run(call, exec).await
            }
        }

        let turn = CancellationToken::new();
        let chain: Vec<Arc<dyn ToolMiddleware>> = vec![Arc::new(Generous)];
        let fired = turn.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            fired.cancel();
        });
        let outcome = dispatch_through(&chain, call(), turn, |cancel| async move {
            cancel.cancelled().await;
            Err("cancelled".to_string())
        })
        .await;
        assert_eq!(outcome, Err("cancelled".to_string()));
    }

    #[tokio::test]
    async fn a_wrapper_can_answer_without_calling_through() {
        struct Cached;
        #[async_trait]
        impl ToolMiddleware for Cached {
            async fn around(
                &self,
                _call: &ToolCall,
                _exec: &mut ToolExec,
                _next: NextDispatch<'_>,
            ) -> ToolOutcome {
                Ok(ToolAnswer::text("from cache"))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let out = run(vec![Arc::new(Cached)], move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(ToolAnswer::text("ran"))
        })
        .await;
        assert_eq!(out, Ok(ToolAnswer::text("from cache")));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a short circuit must not dispatch"
        );
    }

    #[tokio::test]
    async fn a_wrapper_can_call_through_more_than_once() {
        struct RetryTwice;
        #[async_trait]
        impl ToolMiddleware for RetryTwice {
            async fn around(
                &self,
                call: &ToolCall,
                exec: &mut ToolExec,
                next: NextDispatch<'_>,
            ) -> ToolOutcome {
                match next.run(call, exec).await {
                    Ok(v) => Ok(v),
                    Err(_) => next.run(call, exec).await,
                }
            }
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let out = run(vec![Arc::new(RetryTwice)], move |_| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("flaked".into())
            } else {
                Ok(ToolAnswer::text("second time"))
            }
        })
        .await;
        assert_eq!(out, Ok(ToolAnswer::text("second time")));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    /// Registration order is nesting order, and the outermost wrapper sees an
    /// inner one's retries as a single call.
    #[tokio::test]
    async fn the_first_registered_wrapper_is_the_outermost() {
        struct Records(&'static str, Arc<std::sync::Mutex<Vec<String>>>);
        #[async_trait]
        impl ToolMiddleware for Records {
            async fn around(
                &self,
                call: &ToolCall,
                exec: &mut ToolExec,
                next: NextDispatch<'_>,
            ) -> ToolOutcome {
                self.1.lock().unwrap().push(format!("enter {}", self.0));
                let out = next.run(call, exec).await;
                self.1.lock().unwrap().push(format!("exit {}", self.0));
                out
            }
        }

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let chain: Vec<Arc<dyn ToolMiddleware>> = vec![
            Arc::new(Records("outer", log.clone())),
            Arc::new(Records("inner", log.clone())),
        ];
        let _ = run(chain, |_| Ok(ToolAnswer::text("ran"))).await;
        assert_eq!(
            log.lock().unwrap().as_slice(),
            ["enter outer", "enter inner", "exit inner", "exit outer"]
        );
    }
}
