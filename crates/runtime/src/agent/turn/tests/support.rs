//! 回合测试之间共用的夹具。

use base::interface::model::ModelMessage;
use base::settings::ThinkingMode;

pub(super) fn recall_test_settings(root: &std::path::Path) -> base::settings::LoadedSettings {
    let mut values = base::settings::Settings::defaults_for("test-model");
    values.model.base_url = "https://api.example.com".into();
    values.model.auth_token = "test".into();
    values.model.max_tokens = 4096;
    values.model.thinking_mode = ThinkingMode::Off;
    values.model.effort = None;
    values.telemetry_enabled = false;
    let mut paths = base::settings::PathSettings::default();
    paths.user_data_dir = root.join("user-data");
    paths.global_data_dir = root.join("global-data");
    paths.workspace = base::settings::Workspace::project_of(&root.join("local-data"));
    paths.local_data_dir = root.join("local-data");
    base::settings::LoadedSettings::new(values, paths)
}

/// Answers directly and reports a caller-controlled `Usage`, so budget
/// tests can drive `total_tokens_used` without needing a real provider.
pub(super) struct FixedUsageModel {
    pub(super) usage: base::interface::model::Usage,
}

#[async_trait::async_trait]
impl base::interface::model::Model for FixedUsageModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let events = vec![
            Ok(base::interface::model::ModelEvent::TextDelta { text: "ok".into() }),
            Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: self.usage,
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}
