//! Turning a package's `[[agent]]` declarations into engine agent types.
//!
//! Every type produced here carries [`AgentTypeSource::Plugin`], which is what
//! clamps its `permission_mode` / `max_turns` overrides: a definition that
//! arrived over the network must not hand its sub-agent more than the session
//! already had.

use crate::manifest::{AgentDef, Plugin};
use base::agent_type::{AgentTypeDefinition, AgentTypeSource};

/// A definition whose prompt cannot be read is skipped with a warning, so one
/// bad file does not cost every other package its agent types.
pub fn agent_types(plugin: &Plugin) -> Vec<AgentTypeDefinition> {
    plugin
        .manifest
        .agent
        .iter()
        .filter_map(|def| agent_type(plugin, def))
        .collect()
}

fn agent_type(plugin: &Plugin, def: &AgentDef) -> Option<AgentTypeDefinition> {
    let prompt = plugin
        .path(&def.prompt)
        .and_then(|path| std::fs::read_to_string(path).map_err(Into::into));
    let system_prompt = match prompt {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(
                plugin = %plugin.name(),
                agent = %def.name,
                path = %def.prompt.display(),
                error = %e,
                "plugin agent type: failed to read system prompt, skipping"
            );
            return None;
        }
    };

    let permission_mode = def.permission_mode.as_deref().and_then(|raw| {
        let parsed = base::agent_type::parse_permission_mode(raw);
        if parsed.is_none() {
            tracing::warn!(
                plugin = %plugin.name(),
                agent = %def.name,
                permission_mode = %raw,
                "plugin agent type: unrecognized permission_mode, ignoring"
            );
        }
        parsed
    });

    Some(AgentTypeDefinition {
        name: def.name.clone(),
        source: AgentTypeSource::Plugin,
        description: def.description.clone(),
        allowed_tools: def.allowed_tools.clone(),
        disallowed_tools: def.disallowed_tools.clone(),
        model: def.model.clone(),
        permission_mode,
        effort: def.effort.clone(),
        max_turns: def.max_turns,
        scene: def.scene.clone(),
        system_prompt,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const HEAD: &str = "[plugin]\nname = \"p\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn load(root: &Path, body: &str) -> Plugin {
        std::fs::write(root.join("plugin.toml"), format!("{HEAD}{body}")).unwrap();
        Plugin::load(root, &root.join("plugin.toml")).unwrap()
    }

    fn agent_body(prompt_rel: &str) -> String {
        format!(
            r#"
[[agent]]
name = "helper"
description = "A plugin-declared agent"
prompt = "{prompt_rel}"
allowed_tools = ["Read"]
disallowed_tools = ["Bash"]
model = "claude-opus-5"
permission_mode = "plan"
effort = "high"
max_turns = 30
scene = "plugin:p"
"#
        )
    }

    #[test]
    fn every_declared_agent_field_reaches_the_engine_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prompt.md"), "You are a plugin agent.").unwrap();
        let types = agent_types(&load(dir.path(), &agent_body("prompt.md")));
        assert_eq!(types.len(), 1);
        let t = &types[0];

        assert_eq!(t.name, "helper");
        assert_eq!(t.system_prompt, "You are a plugin agent.");
        assert_eq!(t.allowed_tools, ["Read"]);
        assert_eq!(t.disallowed_tools, ["Bash"]);
        assert_eq!(t.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(
            t.permission_mode,
            Some(base::interface::settings::PermissionMode::Plan)
        );
        assert_eq!(t.effort.as_deref(), Some("high"));
        assert_eq!(t.max_turns, Some(30));
        assert_eq!(t.scene.as_deref(), Some("plugin:p"));
        assert_eq!(t.source, AgentTypeSource::Plugin);
    }

    #[test]
    fn an_unreadable_prompt_skips_that_definition_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(agent_types(&load(dir.path(), &agent_body("does-not-exist.md"))).is_empty());
    }

    /// Dropping the mode leaves the sub-agent on the session's inherited one,
    /// which is the safe direction.
    #[test]
    fn an_unrecognized_permission_mode_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prompt.md"), "body").unwrap();
        let p = load(
            dir.path(),
            "\n[[agent]]\nname = \"helper\"\ndescription = \"d\"\nprompt = \"prompt.md\"\npermission_mode = \"make-me-root\"\n",
        );
        let types = agent_types(&p);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].permission_mode, None);
    }
}
