//! `model.request` — the knobs on a request, just before it is sent.

use std::sync::Arc;

use crate::interface::model_interceptor::{ModelInterceptor, ModelRequestView};
use crate::interface::script::{ScriptCarrier, ScriptOutcome};
use crate::settings::ThinkingMode;

/// A script bound to the model-request point.
///
/// # What the script is given
///
/// ```json
/// {
///   "model": "claude-opus-5",
///   "maxTokens": 8192,
///   "thinkingMode": "auto",
///   "fallbackModel": null,
///   "tools": ["Read", "Write", "Bash"],
///   "promptBlocks": ["scene.skeleton", "rules"],
///   "messageCount": 24
/// }
/// ```
///
/// **Not the conversation.** `ModelRequestView` also carries every message and
/// every tool's JSON schema, and that is tens of kilobytes on an ordinary
/// turn — serialized, parsed by the interpreter and thrown away again on every
/// model call. What a script can act on here is the small part: the sampling
/// knobs and which tools are offered. Message content has two points of its
/// own — `prompt.assemble` on the way out, `model.message` on the way back —
/// and both are cheaper places to read it than this one.
///
/// # What it may return
///
/// An object with any subset of these keys; anything absent is left alone.
///
/// ```json
/// { "maxTokens": 2048, "model": "claude-haiku-5", "thinkingMode": "off", "tools": ["Read"] }
/// ```
///
/// - **model**, **maxTokens**, **thinkingMode**, **fallbackModel** — replace
///   the request's. `fallbackModel: null` clears it. `maxTokens: 0` and an
///   empty `model` are refused, because a request built from them cannot be
///   sent and "the script had a bug" would surface as a provider error.
/// - **tools** — keeps the tools it names and drops the rest, in the order
///   they already had. A name the request does not offer is ignored: a script
///   cannot conjure a tool definition, so this narrows the tool set and never
///   widens it. `[]` is a legitimate answer and means this call needs none.
///
/// Anything else — a value that is not an object, a field of the wrong type, a
/// script that threw or ran out of time — leaves the request exactly as it
/// was, whole. There is no half-applied return: the whole object is read
/// before any of it is applied.
///
/// # What it costs
///
/// One script call per model call — a handful per turn, against a model call
/// that already costs hundreds of milliseconds. A script's default budget is
/// 100ms, so the worst case this point can add is a low single-digit
/// percentage of a request that was going to be slow anyway. That ratio is
/// what makes the point affordable, and it is why the bulky fields above are
/// summarized rather than passed: the cost that would matter here is
/// serializing the conversation, not running the script.
pub struct ModelRequestScript {
    carrier: Arc<ScriptCarrier>,
    entry: String,
}

impl ModelRequestScript {
    pub fn new(carrier: Arc<ScriptCarrier>, entry: impl Into<String>) -> Self {
        Self {
            carrier,
            entry: entry.into(),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Requested {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    thinking_mode: Option<ThinkingMode>,
    /// Absent and `null` are different answers here — "leave it" and "clear
    /// it" — and serde collapses both to `None` unless the value is read
    /// through something that never yields one.
    #[serde(default, deserialize_with = "present")]
    fallback_model: Option<serde_json::Value>,
    #[serde(default)]
    tools: Option<Vec<String>>,
}

fn present<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// The request as the script asked for it, with every field already checked.
struct Changes {
    model: Option<String>,
    max_tokens: Option<u32>,
    thinking_mode: Option<ThinkingMode>,
    fallback_model: Option<Option<String>>,
    tools: Option<Vec<String>>,
}

impl Changes {
    /// Whether the script asked for anything at all. An answer that names a
    /// field the request already holds still counts: what the ledger records
    /// is that the point took the script's answer, not that the bytes moved.
    fn names_a_field(&self) -> bool {
        self.model.is_some()
            || self.max_tokens.is_some()
            || self.thinking_mode.is_some()
            || self.fallback_model.is_some()
            || self.tools.is_some()
    }
}

fn read(returned: serde_json::Value) -> Option<Changes> {
    let requested: Requested = serde_json::from_value(returned).ok()?;

    let model = match requested.model {
        Some(m) if m.trim().is_empty() => return None,
        other => other,
    };
    let max_tokens = match requested.max_tokens {
        Some(0) => return None,
        other => other,
    };
    let fallback_model = match requested.fallback_model {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(m)) => Some(Some(m)),
        Some(_) => return None,
    };

    Some(Changes {
        model,
        max_tokens,
        thinking_mode: requested.thinking_mode,
        fallback_model,
        tools: requested.tools,
    })
}

impl ModelInterceptor for ModelRequestScript {
    fn on_request(&self, request: &mut ModelRequestView) {
        let input = serde_json::json!({
            "model": request.params.model,
            "maxTokens": request.params.max_tokens,
            "thinkingMode": request.params.thinking_mode,
            "fallbackModel": request.params.fallback_model,
            "tools": request.tool_defs.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "promptBlocks": request
                .prompt_blocks
                .iter()
                .filter_map(|b| b.name.as_deref())
                .collect::<Vec<_>>(),
            "messageCount": request.messages.len(),
        });

        let returned = match self.carrier.call_blocking(&self.entry, input) {
            Ok(v) => v,
            Err(error) => {
                tracing::warn!(
                    script = %self.carrier.script().id,
                    error = %error,
                    "model-request script did not run; the request is unchanged"
                );
                self.carrier
                    .record(&self.entry, ScriptOutcome::Failed { error });
                return;
            }
        };

        let Some(changes) = read(returned) else {
            self.carrier.record(
                &self.entry,
                ScriptOutcome::NoChange {
                    detail: Some("returned a shape a request cannot be built from".into()),
                },
            );
            return;
        };

        self.carrier.record(
            &self.entry,
            if changes.names_a_field() {
                ScriptOutcome::Applied
            } else {
                ScriptOutcome::NoChange { detail: None }
            },
        );

        if let Some(model) = changes.model {
            request.params.model = model;
        }
        if let Some(max_tokens) = changes.max_tokens {
            request.params.max_tokens = max_tokens;
        }
        if let Some(mode) = changes.thinking_mode {
            request.params.thinking_mode = mode;
        }
        if let Some(fallback) = changes.fallback_model {
            request.params.fallback_model = fallback;
        }
        if let Some(keep) = changes.tools {
            request.tool_defs.retain(|t| keep.contains(&t.name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::model::{StreamParams, ToolDef};
    use crate::interface::script::{ScriptEngine, ScriptError, ScriptLimits, ScriptSource};
    use crate::prompt::BlockOrigin;

    /// Stands in for the interpreter at a synchronous point.
    /// [`FnScriptEngine`](crate::interface::script::FnScriptEngine) answers
    /// only the asynchronous call, and this point never makes one.
    struct Blocking<F>(F);

    #[async_trait::async_trait]
    impl<F> ScriptEngine for Blocking<F>
    where
        F: Fn(serde_json::Value) -> Result<serde_json::Value, ScriptError> + Send + Sync,
    {
        async fn eval(
            &self,
            _s: &ScriptSource,
            _e: &str,
            input: serde_json::Value,
            _l: &ScriptLimits,
        ) -> Result<serde_json::Value, ScriptError> {
            (self.0)(input)
        }

        fn eval_blocking(
            &self,
            _s: &ScriptSource,
            _e: &str,
            input: serde_json::Value,
            _l: &ScriptLimits,
        ) -> Result<serde_json::Value, ScriptError> {
            (self.0)(input)
        }
    }

    fn engine_returning(value: serde_json::Value) -> Arc<dyn ScriptEngine> {
        Arc::new(Blocking(move |_| Ok(value.clone())))
    }

    fn interceptor(engine: Arc<dyn ScriptEngine>) -> ModelRequestScript {
        ModelRequestScript::new(
            Arc::new(ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.agix/scripts/model.js".into(),
                    origin: BlockOrigin::Script("./.agix/scripts/model.js".into()),
                    code: String::new(),
                },
                "model.request",
                ScriptLimits::default(),
            )),
            "onRequest",
        )
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            source: None,
        }
    }

    fn request() -> ModelRequestView {
        ModelRequestView {
            prompt_blocks: vec![crate::prompt::PromptBlock::system("be an agent").named("rules")],
            tool_defs: vec![tool("Read"), tool("Write"), tool("Bash")],
            messages: Vec::new(),
            params: StreamParams {
                model: "claude-opus-5".into(),
                max_tokens: 8192,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: Some("claude-sonnet-5".into()),
                cache_edits: Vec::new(),
                origin: None,
                input_map: None,
            },
        }
    }

    fn intercepted(returned: serde_json::Value) -> ModelRequestView {
        let mut req = request();
        interceptor(engine_returning(returned)).on_request(&mut req);
        req
    }

    fn names(req: &ModelRequestView) -> Vec<String> {
        req.tool_defs.iter().map(|t| t.name.clone()).collect()
    }

    /// The acceptance case: the request that leaves is not the request that
    /// was assembled.
    #[test]
    fn a_script_reshapes_the_knobs_it_names_and_nothing_else() {
        let out = intercepted(serde_json::json!({
            "maxTokens": 2048,
            "model": "claude-haiku-5",
            "thinkingMode": "off",
        }));
        assert_eq!(out.params.max_tokens, 2048);
        assert_eq!(out.params.model, "claude-haiku-5");
        assert_eq!(out.params.thinking_mode, ThinkingMode::Off);
        assert_eq!(
            out.params.fallback_model,
            Some("claude-sonnet-5".to_string()),
            "a field the script did not name is not a field it changed"
        );
        assert_eq!(names(&out), ["Read", "Write", "Bash"]);
    }

    #[test]
    fn a_script_narrows_the_tool_set_and_cannot_widen_it() {
        let out = intercepted(serde_json::json!({"tools": ["Read", "Grep"]}));
        assert_eq!(
            names(&out),
            ["Read"],
            "a name the request never offered cannot be conjured into one"
        );
    }

    #[test]
    fn a_script_can_say_this_call_needs_no_tools() {
        assert!(intercepted(serde_json::json!({"tools": []}))
            .tool_defs
            .is_empty());
    }

    #[test]
    fn a_script_can_clear_the_fallback_but_absence_is_not_clearing() {
        assert_eq!(
            intercepted(serde_json::json!({"fallbackModel": null}))
                .params
                .fallback_model,
            None
        );
        assert_eq!(
            intercepted(serde_json::json!({"maxTokens": 16}))
                .params
                .fallback_model,
            Some("claude-sonnet-5".to_string())
        );
    }

    /// A script handed the request back untouched changes nothing, which is
    /// the natural way to write "not this time" in JavaScript.
    #[test]
    fn echoing_the_input_back_is_not_a_change() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(Blocking(Ok));
        let mut out = request();
        interceptor(engine).on_request(&mut out);
        let before = request();
        assert_eq!(out.params.model, before.params.model);
        assert_eq!(out.params.max_tokens, before.params.max_tokens);
        assert_eq!(out.params.thinking_mode, before.params.thinking_mode);
        assert_eq!(out.params.fallback_model, before.params.fallback_model);
        assert_eq!(names(&out), names(&before));
    }

    /// One bad field is a bad return, not a partly applied one: a request with
    /// the script's `maxTokens` and the engine's everything-else is a request
    /// nobody wrote.
    #[test]
    fn a_field_of_the_wrong_type_discards_the_whole_return() {
        let out = intercepted(serde_json::json!({
            "maxTokens": 2048,
            "fallbackModel": {"name": "something"},
        }));
        assert_eq!(out.params.max_tokens, 8192);
        assert_eq!(
            out.params.fallback_model,
            Some("claude-sonnet-5".to_string())
        );
    }

    #[test]
    fn a_request_that_could_not_be_sent_is_refused() {
        for broken in [
            serde_json::json!({"maxTokens": 0}),
            serde_json::json!({"model": "  "}),
        ] {
            let out = intercepted(broken.clone());
            assert_eq!(out.params.max_tokens, 8192, "{broken}");
            assert_eq!(out.params.model, "claude-opus-5", "{broken}");
        }
    }

    #[test]
    fn a_script_that_returns_nonsense_leaves_the_request_alone() {
        for nonsense in [
            serde_json::json!("more tokens please"),
            serde_json::json!(4096),
            serde_json::json!(null),
            serde_json::json!([{"maxTokens": 16}]),
        ] {
            let out = intercepted(nonsense.clone());
            assert_eq!(out.params.max_tokens, 8192, "{nonsense}");
            assert_eq!(names(&out), ["Read", "Write", "Bash"], "{nonsense}");
        }
    }

    #[test]
    fn a_script_that_fails_leaves_the_request_alone() {
        let engine: Arc<dyn ScriptEngine> =
            Arc::new(Blocking(|_| Err(ScriptError::Failed("deliberate".into()))));
        let mut out = request();
        interceptor(engine).on_request(&mut out);
        assert_eq!(out.params.max_tokens, 8192);
        assert_eq!(names(&out), ["Read", "Write", "Bash"]);
    }

    /// The conversation is not what the script is handed — its size is the
    /// reason — but the script is told enough to know it is there.
    #[test]
    fn the_script_is_given_the_summary_rather_than_the_conversation() {
        let seen = Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let recorded = seen.clone();
        let engine: Arc<dyn ScriptEngine> = Arc::new(Blocking(move |input| {
            *recorded.lock().unwrap() = input;
            Ok(serde_json::Value::Null)
        }));
        let mut req = request();
        req.messages.push(crate::interface::model::ModelMessage {
            role: crate::interface::model::MessageRole::User,
            content: vec![crate::interface::model::ModelContentBlock::Text {
                text: "a very long conversation".into(),
            }],
        });
        interceptor(engine).on_request(&mut req);

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen["messageCount"], serde_json::json!(1));
        assert_eq!(seen["tools"], serde_json::json!(["Read", "Write", "Bash"]));
        assert_eq!(seen["promptBlocks"], serde_json::json!(["rules"]));
        assert_eq!(seen.get("messages"), None);
        assert!(
            !seen.to_string().contains("a very long conversation"),
            "message content must not be serialized on every model call: {seen}"
        );
    }
}
