//! Real `PromptHookExecutor`/`AgentHookExecutor` implementations.
//!
//! `crates/hooks` deliberately has no way to run these itself (it doesn't
//! depend on `model`/`runtime` to avoid a circular dependency — see the
//! `PromptHookExecutor` doc comment in `hooks::runner`); without an executor
//! a `"type": "prompt"` or `"type": "agent"` hook is `HookOutcome::Skipped`
//! with "no executor configured". `Builder::build()` wires these in using
//! the same `model`/agent-spawning capability it already has on hand.

use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelEvent, ModelMessage, StreamParams,
};
use base::interface::prompt::PromptBlock;
use base::interface::settings::ThinkingMode;
use hooks::payload::HookInput;
use hooks::runner::{AgentHookExecutor, PromptHookExecutor};
use std::sync::Arc;

/// Runs a `"type": "prompt"` hook as a single one-shot, tool-less model call:
/// the hook's `prompt` becomes the system prompt, the serialized `HookInput`
/// payload becomes the user message, and the concatenated text deltas become
/// the response `HookRunner` parses as a `HookResponse`.
pub struct ModelPromptHookExecutor {
    model: Arc<dyn Model>,
    default_model_name: String,
    max_tokens: u32,
    /// The session whose hooks these are, so the call is filed under it.
    session_id: Option<String>,
}

impl ModelPromptHookExecutor {
    pub fn new(
        model: Arc<dyn Model>,
        default_model_name: String,
        max_tokens: u32,
        session_id: Option<String>,
    ) -> Self {
        Self {
            model,
            default_model_name,
            max_tokens,
            session_id,
        }
    }
}

#[async_trait::async_trait]
impl PromptHookExecutor for ModelPromptHookExecutor {
    async fn execute(
        &self,
        prompt: &str,
        model: Option<&str>,
        payload: &HookInput,
    ) -> Result<String, String> {
        let payload_json = serde_json::to_string(payload).unwrap_or_default();
        let params = StreamParams {
            model: model
                .map(str::to_string)
                .unwrap_or_else(|| self.default_model_name.clone()),
            max_tokens: self.max_tokens,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            fallback_model: None,
            cache_edits: vec![],
            origin: self.session_id.as_ref().map(|id| {
                base::interface::model::CallOrigin::auxiliary(
                    id,
                    base::interface::model::call_purpose::HOOK,
                )
            }),
            input_map: None,
            provider_search: None,
        };
        let messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: payload_json }],
        }];
        let prompt_blocks = vec![PromptBlock::system(prompt.to_string())];
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stream = self
            .model
            .stream(prompt_blocks, vec![], messages, params, cancel)
            .await
            .map_err(|e| format!("model call failed: {e}"))?;

        use futures::StreamExt;
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ModelEvent::TextDelta { text: t }) => text.push_str(&t),
                Ok(ModelEvent::EndTurn { .. }) => break,
                Ok(_) => {}
                Err(e) => return Err(format!("model stream error: {e}")),
            }
        }
        Ok(text)
    }
}

/// Runs a `"type": "agent"` hook by delegating to the same `AgentSpawner`
/// used for sub-agent tool calls (see `agents::spawner::RuntimeAgentSpawner`).
/// The `HookInput` payload is folded into the prompt text (`AgentSpawner`
/// has no separate "context" parameter); a hook's `model` runs the agent on
/// that model, routed the way the session routes a model name.
pub struct AgentSpawnerHookExecutor {
    spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner>,
    cwd: std::path::PathBuf,
}

impl AgentSpawnerHookExecutor {
    pub fn new(
        spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner>,
        cwd: std::path::PathBuf,
    ) -> Self {
        Self { spawner, cwd }
    }
}

#[async_trait::async_trait]
impl AgentHookExecutor for AgentSpawnerHookExecutor {
    async fn execute(
        &self,
        prompt: &str,
        model: Option<&str>,
        payload: &HookInput,
    ) -> Result<String, String> {
        let payload_json = serde_json::to_string(payload).unwrap_or_default();
        let full_prompt = format!("{prompt}\n\nContext:\n{payload_json}");
        let request = base::interface::agent_spawner::SpawnRequest::new(
            full_prompt,
            self.cwd.clone(),
            tokio_util::sync::CancellationToken::new(),
        )
        .model(model.map(str::to_string))
        .mode(base::interface::agent_spawner::SpawnMode::Hook {
            judged_session: payload.session_id.clone(),
        });
        self.spawner.spawn(request).await.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{ModelError, ModelEvent, ModelStream, ToolDef};

    fn test_input() -> HookInput {
        HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "test".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "ls"})),
            tool_use_id: Some("toolu_1".into()),
            tool_result: None,
            is_error: None,
            user_prompt: None,
            ..Default::default()
        }
    }

    /// Returns a fixed text response, ignoring the actual prompt/messages —
    /// enough to prove `ModelPromptHookExecutor` really calls `Model::stream`
    /// and collects its `TextDelta`s, without needing a real API call.
    struct StubModel {
        response: &'static str,
    }
    #[async_trait::async_trait]
    impl Model for StubModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<PromptBlock>,
            _tools: Vec<ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            let text = self.response.to_string();
            let events = vec![
                Ok(ModelEvent::TextDelta { text }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn model_prompt_hook_executor_returns_the_models_text() {
        let model: Arc<dyn Model> = Arc::new(StubModel {
            response: r#"{"decision":"approve"}"#,
        });
        let executor = ModelPromptHookExecutor::new(model, "test-model".into(), 100, None);
        let out = executor
            .execute("is this safe?", None, &test_input())
            .await
            .unwrap();
        assert_eq!(out, r#"{"decision":"approve"}"#);
    }

    struct StubSpawner {
        response: &'static str,
    }
    #[async_trait::async_trait]
    impl base::interface::agent_spawner::AgentSpawner for StubSpawner {
        async fn spawn(
            &self,
            request: base::interface::agent_spawner::SpawnRequest,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            Ok(match request.model {
                Some(model) => format!("ran on {model}"),
                None => self.response.to_string(),
            })
        }
    }

    #[tokio::test]
    async fn agent_spawner_hook_executor_returns_the_spawned_agents_text() {
        let spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> =
            Arc::new(StubSpawner {
                response: r#"{"ok":true}"#,
            });
        let executor = AgentSpawnerHookExecutor::new(spawner, std::path::PathBuf::from("/tmp"));
        let out = executor
            .execute("review this", None, &test_input())
            .await
            .unwrap();
        assert_eq!(out, r#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn an_agent_hooks_model_is_the_model_its_agent_runs_on() {
        let spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> =
            Arc::new(StubSpawner {
                response: r#"{"ok":true}"#,
            });
        let executor = AgentSpawnerHookExecutor::new(spawner, std::path::PathBuf::from("/tmp"));
        let out = executor
            .execute("review this", Some("claude-haiku-4-5"), &test_input())
            .await
            .unwrap();
        assert_eq!(out, "ran on claude-haiku-4-5");
    }
}
