//! Agent tests: shared fixtures here, cases grouped by area in the sibling files.

use super::builder::build_hook_runner;
use super::*;

pub(super) fn test_settings() -> base::settings::LoadedSettings {
    use base::settings::*;
    base::settings::LoadedSettings::new(
        Settings {
            model: ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: String::new(),
                auth_token: String::new(),
                model_name: "test".into(),
                max_tokens: 2000,
                thinking_mode: ThinkingMode::Auto,
                effort: None,
                fallback_model: None,
            },
            execution: ExecutionSettings::default(),
            compaction: CompactionConfig::default(),
            sandbox: SandboxConfig::default(),
            plugins: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            path_style: Default::default(),
            permission_mode: PermissionMode::default(),
            allow_client_permission_override: false,
            telemetry_enabled: false,
            hooks_config: None,
            project_trust: Default::default(),
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
            language: None,
            scripts: Vec::new(),
        },
        PathSettings {
            user_data_dir: "/tmp".into(),
            global_data_dir: "/tmp".into(),
            workspace: base::settings::Workspace::project_of(std::path::Path::new("/tmp")),
            local_data_dir: "/tmp".into(),
            scope: "code".into(),
        },
    )
}

mod modules_and_session;
use modules_and_session::*;
mod registry;
mod turn_loop;
use turn_loop::*;
mod events_and_steering;
mod policies;
use events_and_steering::*;
mod build_snapshot;
mod compacting;
mod exec_providers;
mod prompt_middleware;
mod web_tasks;
