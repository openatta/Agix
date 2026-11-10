//! 回合测试之间共用的夹具。

use base::interface::model::ModelMessage;
use base::settings::ThinkingMode;

#[allow(deprecated)]
pub(super) fn recall_test_settings(root: &std::path::Path) -> base::settings::Settings {
    base::settings::Settings {
        model: base::settings::ModelSettings {
            api_type: base::provider::ApiType::Anthropic,
            base_url: "https://api.example.com".into(),
            auth_token: "test".into(),
            model_name: "test-model".into(),
            max_tokens: 4096,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            fallback_model: None,
        },
        paths: base::settings::PathSettings {
            user_data_dir: root.join("user-data"),
            global_data_dir: root.join("global-data"),
            workspace: base::settings::Workspace::project_of(&root.join("local-data")),
            local_data_dir: root.join("local-data"),
            scope: "code".into(),
        },
        execution: Default::default(),
        compaction: Default::default(),
        sandbox: Default::default(),
        plugins: Default::default(),
        instruction_file: None,
        prompt_append: None,
        prompt_override: None,
        recorder: None,
        telemetry_url: None,
        memory_enabled: true,
        disable_skill_shell_execution: false,
        path_style: Default::default(),
        permission_mode: base::settings::PermissionMode::Default,
        permission_rules: Vec::new(),
        local_permission_rules: Vec::new(),
        allow_client_permission_override: false,
        telemetry_enabled: false,
        hooks_config: None,
        project_trust: Default::default(),
        project_layer: Default::default(),
        ignored_values: Vec::new(),
        managed: None,
        mcp_servers: Default::default(),
        mcp_server_dirs: Default::default(),
        providers: Default::default(),
        default_provider: None,
        task_models: Default::default(),
        language: None,
        session_dir: None,
        scripts: Vec::new(),
    }
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
