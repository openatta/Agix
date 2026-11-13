//! Settings a delegate starts from: agent-type overrides, inheritance from the parent
//! and the fallback when the parent has none.

use super::*;

pub(super) fn parse_effort(effort: &str) -> Option<Effort> {
    serde_json::from_value(serde_json::Value::String(effort.to_string())).ok()
}

/// Whether an agent type's `allowed_tools` / `disallowed_tools` let a tool
/// through. No type, or a type with neither list, admits everything.
pub(super) fn type_admits_tool(def: Option<&AgentTypeDefinition>, name: &str) -> bool {
    def.is_none_or(|def| lists_admit_tool(&def.allowed_tools, &def.disallowed_tools, name))
}

pub(super) fn lists_admit_tool(allowed: &[String], disallowed: &[String], name: &str) -> bool {
    let listed = |names: &[String]| names.iter().any(|n| AgentTool::tool_name_matches(name, n));
    !listed(disallowed) && (allowed.is_empty() || listed(allowed))
}

pub(super) fn contributed_tool_filter(
    def: Option<&AgentTypeDefinition>,
    run: &SubRun,
) -> runtime::agent::ToolFilter {
    let def = def.cloned();
    let (allowed, disallowed) = (run.allowed_tools.clone(), run.disallowed_tools.clone());
    Arc::new(move |name| {
        type_admits_tool(def.as_ref(), name) && lists_admit_tool(&allowed, &disallowed, name)
    })
}

/// Apply an `AgentTypeDefinition`'s `permission_mode`/`effort`/`max_turns`
/// overrides to an already-built subagent `Settings`, in place — the same
/// step whichever path built the base `Settings`.
///
/// A type's mode replaces the parent's, wider or narrower, as
/// `PermissionMode::for_delegate` allows. A plugin's types arrive over the
/// network and can only tighten; the API-call cap is clamped the same way for
/// them.
pub(super) fn apply_agent_type_overrides(settings: &mut Settings, def: &AgentTypeDefinition) {
    if let Some(mode) = def.permission_mode {
        let applied = if def.source.may_loosen() {
            settings.permission_mode.for_delegate(mode)
        } else {
            mode.min_permissive(settings.permission_mode)
        };
        if applied != mode {
            tracing::warn!(
                agent_type = %def.name,
                requested = ?mode,
                inherited = ?settings.permission_mode,
                applied = ?applied,
                "agent type's permission mode held by its parent's"
            );
        }
        settings.permission_mode = applied;
    }
    if let Some(effort) = &def.effort {
        if let Some(level) = parse_effort(effort) {
            settings.model.effort = Some(level);
        } else {
            tracing::warn!(effort = %effort, agent_type = %def.name, "unrecognized effort value, ignoring");
        }
    }
    if let Some(max_turns) = def.max_turns {
        settings.execution.max_api_calls_per_turn = if def.source.may_loosen() {
            max_turns
        } else {
            max_turns.min(settings.execution.max_api_calls_per_turn)
        };
    }
}

/// Build a sub-agent's `Settings` by cloning the parent's real settings and
/// overriding only the model-selection fields — the sub-agent inherits the
/// parent's actual scope/paths (skills/hooks/agents/memory all resolve
/// correctly), auth, sandbox policy, etc.
pub(super) fn settings_from_parent(
    parent: &LoadedSettings,
    model_name: String,
    max_tokens: u32,
    fallback_model: Option<String>,
) -> LoadedSettings {
    let mut settings = parent.clone();
    settings.values.model.model_name = model_name;
    settings.values.model.max_tokens = max_tokens;
    settings.values.model.fallback_model = fallback_model;
    settings
}

/// A parent's `Settings` for tests that do not care which: scope `code`, the
/// user-level roots under `local_data_dir` rather than `$HOME`.
#[cfg(test)]
pub(super) fn fallback_settings(
    model_name: String,
    max_tokens: u32,
    fallback_model: Option<String>,
    local_data_dir: std::path::PathBuf,
) -> LoadedSettings {
    LoadedSettings::new(
        Settings {
            model: ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: String::new(),
                auth_token: String::new(),
                model_name,
                max_tokens,
                thinking_mode: ThinkingMode::Auto,
                effort: None,
                fallback_model,
            },
            execution: ExecutionSettings::default(),
            compaction: Default::default(),
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
            telemetry_enabled: true,
            hooks_config: None,
            project_trust: Default::default(),
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
            language: None,
            // A sub-agent inherits no script bindings: a script bound in the
            // parent's configuration applies to the parent's prompt, and quietly
            // running it against every spawned agent's prompt too would be a
            // scope nobody asked for.
            scripts: Vec::new(),
        },
        PathSettings {
            user_data_dir: local_data_dir.join("scenes").join("code"),
            global_data_dir: local_data_dir.clone(),
            workspace: base::settings::Workspace::project_of(&local_data_dir),
            local_data_dir,
            scope: "code".to_string(),
        },
    )
}
