//! Settings a delegate starts from: agent-type overrides, inheritance from the parent
//! and the fallback when the parent has none.

use super::*;

pub(super) fn parse_effort(effort: &str) -> Option<Effort> {
    serde_json::from_value(serde_json::Value::String(effort.to_string())).ok()
}

/// What a delegate of type `def` may have, held to its caller's `selector`.
///
/// A type that lists `allowed_tools` is also allowed the servers its
/// `mcp_servers` grants and the `extra` tools its caller hands it: naming
/// those is how it asks for them. Its `disallowed_tools` hold for all of them.
pub(crate) fn delegate_selector(
    def: Option<&AgentTypeDefinition>,
    selector: &ToolSelector,
    extra: &[Arc<dyn base::tool::Tool>],
) -> ToolSelector {
    let Some(def) = def else {
        return selector.clone();
    };
    let parse = |list: &[String]| -> Vec<ToolPattern> {
        list.iter().map(|p| ToolPattern::parse(p)).collect()
    };
    let allow = (!def.allowed_tools.is_empty()).then(|| {
        let mut allow = parse(&def.allowed_tools);
        allow.extend(def.mcp_servers.iter().cloned().map(ToolPattern::McpServer));
        allow.extend(
            extra
                .iter()
                .map(|t| ToolPattern::Name(t.name().to_string())),
        );
        allow
    });
    let deny = parse(&def.disallowed_tools);
    let by_type = if allow.is_none() && deny.is_empty() {
        ToolSelector::all()
    } else {
        ToolSelector::layer(allow, deny)
    };
    by_type.and(selector.clone())
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
    let mut settings = Settings::defaults_for(&model_name);
    settings.model.max_tokens = max_tokens;
    settings.model.thinking_mode = ThinkingMode::Auto;
    settings.model.effort = None;
    settings.model.fallback_model = fallback_model;
    let mut paths = PathSettings::default();
    paths.user_data_dir = local_data_dir.join("scenes").join("code");
    paths.global_data_dir = local_data_dir.clone();
    paths.workspace = base::settings::Workspace::project_of(&local_data_dir);
    paths.local_data_dir = local_data_dir;
    paths.scope = "code".to_string();
    LoadedSettings::new(settings, paths)
}
