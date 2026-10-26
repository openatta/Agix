//! `model.message` — the text of a finished message, before it is recorded.

use std::sync::Arc;

use crate::interface::model::{ModelContentBlock, ModelMessage};
use crate::interface::model_interceptor::ModelInterceptor;
use crate::interface::script::{ScriptCarrier, ScriptOutcome};

/// A script bound to the model-message point.
///
/// # When it runs
///
/// On whole messages, never on stream deltas. The point itself has no
/// per-chunk equivalent — a hook called thousands of times a turn costs
/// whatever its author wrote without their being able to see it — so this
/// fires when the stream that carried a message has finished and the message
/// is coherent: once for an assistant turn's thinking and text, once for its
/// batch of tool calls. A few times per model call, not thousands.
///
/// # What the script is given
///
/// ```json
/// { "role": "assistant", "text": ["Here is what I found."], "toolUses": ["Read"] }
/// ```
///
/// `text` holds the message's text blocks in order. `toolUses` names the tools
/// the message asked for, as context; their arguments are not included,
/// because a single `Write` call carries a whole file.
///
/// A message with no text at all — a batch of tool calls, for instance — does
/// not call the script, since text is the only thing it could change and a
/// call that can change nothing is pure cost on a point that fires this often.
///
/// # What it may return
///
/// An array of strings, exactly as long as the `text` it was given. Each entry
/// replaces the text block at the same position; `""` empties one.
///
/// ```json
/// ["Here is what I found. [reviewed]"]
/// ```
///
/// A different length is not a rewrite the adapter can act on — nothing says
/// which block the extra or missing entry belongs to — so it is discarded
/// along with every other shape: an object, a number, a script that threw or
/// ran out of time. In all of those the message is recorded exactly as the
/// model produced it.
///
/// # What it cannot reach
///
/// **Thinking blocks and their signatures.** Extended-thinking blocks must be
/// echoed back to the provider verbatim on the next request or the call is
/// rejected, so a script that could edit or drop one could break the turn
/// after this one and nothing here would look wrong. They are not in the
/// input and cannot be produced by the output.
///
/// **Images.** Megabytes of base64, on a point that fires several times a
/// model call.
///
/// **Tool-use blocks.** Their arguments are what the engine is about to
/// dispatch, and rewriting a dispatch after the model chose it belongs to
/// `tool.around` — where the rule is that it cannot be done at all.
pub struct ModelMessageScript {
    carrier: Arc<ScriptCarrier>,
    entry: String,
}

impl ModelMessageScript {
    pub fn new(carrier: Arc<ScriptCarrier>, entry: impl Into<String>) -> Self {
        Self {
            carrier,
            entry: entry.into(),
        }
    }
}

impl ModelInterceptor for ModelMessageScript {
    fn on_message(&self, message: &mut ModelMessage) {
        let mut text_at: Vec<usize> = Vec::new();
        let mut text: Vec<&str> = Vec::new();
        let mut tool_uses: Vec<&str> = Vec::new();
        for (i, block) in message.content.iter().enumerate() {
            match block {
                ModelContentBlock::Text { text: t } => {
                    text_at.push(i);
                    text.push(t);
                }
                ModelContentBlock::ToolUse { name, .. } => tool_uses.push(name),
                _ => {}
            }
        }
        if text.is_empty() {
            return;
        }

        let input = serde_json::json!({
            "role": message.role,
            "text": text,
            "toolUses": tool_uses,
        });

        let returned = match self.carrier.call_blocking(&self.entry, input) {
            Ok(v) => v,
            Err(error) => {
                tracing::warn!(
                    script = %self.carrier.script().id,
                    error = %error,
                    "model-message script did not run; the message is unchanged"
                );
                self.carrier
                    .record(&self.entry, ScriptOutcome::Failed { error });
                return;
            }
        };

        let Ok(rewritten) = serde_json::from_value::<Vec<String>>(returned) else {
            self.carrier.record(
                &self.entry,
                ScriptOutcome::NoChange {
                    detail: Some("returned something that is not a list of strings".into()),
                },
            );
            return;
        };
        if rewritten.len() != text_at.len() {
            self.carrier.record(
                &self.entry,
                ScriptOutcome::NoChange {
                    detail: Some(format!(
                        "returned {} block(s) for a message with {}",
                        rewritten.len(),
                        text_at.len()
                    )),
                },
            );
            return;
        }
        for (at, t) in text_at.into_iter().zip(rewritten) {
            message.content[at] = ModelContentBlock::Text { text: t };
        }
        self.carrier.record(&self.entry, ScriptOutcome::Applied);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::model::MessageRole;
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

    fn interceptor(engine: Arc<dyn ScriptEngine>) -> ModelMessageScript {
        ModelMessageScript::new(
            Arc::new(ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.agix/scripts/message.js".into(),
                    origin: BlockOrigin::Script("./.agix/scripts/message.js".into()),
                    code: String::new(),
                },
                "model.message",
                ScriptLimits::default(),
            )),
            "onMessage",
        )
    }

    fn engine_returning(value: serde_json::Value) -> Arc<dyn ScriptEngine> {
        Arc::new(Blocking(move |_| Ok(value.clone())))
    }

    fn message() -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                ModelContentBlock::Thinking {
                    text: "let me think".into(),
                    signature: Some("sig-abc".into()),
                    provider: None,
                },
                ModelContentBlock::Text {
                    text: "the key is hunter2".into(),
                },
                ModelContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "a.txt"}),
                },
                ModelContentBlock::Text {
                    text: "and that is all".into(),
                },
            ],
        }
    }

    fn intercepted(returned: serde_json::Value) -> ModelMessage {
        let mut m = message();
        interceptor(engine_returning(returned)).on_message(&mut m);
        m
    }

    fn texts(m: &ModelMessage) -> Vec<String> {
        m.content
            .iter()
            .filter_map(|b| match b {
                ModelContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The acceptance case: a script redacts what the model said, and what is
    /// recorded is the redacted text.
    #[test]
    fn a_script_rewrites_the_text_blocks_in_place() {
        let out = intercepted(serde_json::json!([
            "the key is [redacted]",
            "and that is all"
        ]));
        assert_eq!(texts(&out), ["the key is [redacted]", "and that is all"]);
        assert_eq!(
            out.content.len(),
            4,
            "the blocks around the text keep their places"
        );
        assert!(matches!(out.content[0], ModelContentBlock::Thinking { .. }));
        assert!(matches!(out.content[2], ModelContentBlock::ToolUse { .. }));
    }

    /// The signature is what the next request has to echo back. A script that
    /// rewrites the text beside it must not be able to disturb it.
    #[test]
    fn a_rewrite_leaves_thinking_and_its_signature_untouched() {
        let out = intercepted(serde_json::json!(["one", "two"]));
        match &out.content[0] {
            ModelContentBlock::Thinking {
                text, signature, ..
            } => {
                assert_eq!(text, "let me think");
                assert_eq!(signature.as_deref(), Some("sig-abc"));
            }
            other => panic!("the thinking block was replaced: {other:?}"),
        }
    }

    #[test]
    fn a_list_of_the_wrong_length_is_not_a_rewrite() {
        for wrong in [
            serde_json::json!(["only one"]),
            serde_json::json!(["one", "two", "three"]),
            serde_json::json!([]),
        ] {
            assert_eq!(
                texts(&intercepted(wrong.clone())),
                ["the key is hunter2", "and that is all"],
                "{wrong}"
            );
        }
    }

    #[test]
    fn a_script_that_returns_nonsense_leaves_the_message_alone() {
        for nonsense in [
            serde_json::json!("redacted"),
            serde_json::json!(null),
            serde_json::json!({"text": ["a", "b"]}),
            serde_json::json!([1, 2]),
        ] {
            assert_eq!(
                texts(&intercepted(nonsense.clone())),
                ["the key is hunter2", "and that is all"],
                "{nonsense}"
            );
        }
    }

    #[test]
    fn a_script_that_fails_leaves_the_message_alone() {
        let engine: Arc<dyn ScriptEngine> =
            Arc::new(Blocking(|_| Err(ScriptError::Failed("deliberate".into()))));
        let mut out = message();
        interceptor(engine).on_message(&mut out);
        assert_eq!(texts(&out), ["the key is hunter2", "and that is all"]);
    }

    #[test]
    fn the_script_is_given_the_text_and_the_tool_names_and_no_more() {
        let seen = Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let recorded = seen.clone();
        let engine: Arc<dyn ScriptEngine> = Arc::new(Blocking(move |input| {
            *recorded.lock().unwrap() = input;
            Ok(serde_json::Value::Null)
        }));
        let mut m = message();
        m.content.push(ModelContentBlock::Image {
            media_type: "image/png".into(),
            data: "AAAABBBBCCCC".into(),
        });
        interceptor(engine).on_message(&mut m);

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen["role"], serde_json::json!("assistant"));
        assert_eq!(
            seen["text"],
            serde_json::json!(["the key is hunter2", "and that is all"])
        );
        assert_eq!(seen["toolUses"], serde_json::json!(["Read"]));
        let rendered = seen.to_string();
        assert!(
            !rendered.contains("sig-abc"),
            "a signature is not a script's to see: {rendered}"
        );
        assert!(
            !rendered.contains("AAAABBBBCCCC"),
            "image bytes must stay out: {rendered}"
        );
        assert!(
            !rendered.contains("a.txt"),
            "tool arguments must stay out: {rendered}"
        );
    }

    /// A message the script could not change costs nothing.
    #[test]
    fn a_message_with_no_text_does_not_call_the_script() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let engine: Arc<dyn ScriptEngine> = Arc::new(Blocking(move |_| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!([]))
        }));
        let mut m = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: "t1".into(),
                name: "Read".into(),
                input: serde_json::json!({}),
            }],
        };
        interceptor(engine).on_message(&mut m);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(m.content.len(), 1);
    }
}
