//! What a plugin will do, stated before it is trusted to do it.
//!
//! Sandboxes handle what a plugin *executes*. They do nothing about what it
//! *says*: a tool description, an agent description and a scene's system
//! prompt all reach the model verbatim, and text reaching the model is the
//! one attack the isolation model cannot address. The only defence is a
//! person reading it, so installation has to put it in front of them.

use crate::manifest::{Plugin, PluginError};

/// A single one-line description is a label, not a document. Anything longer
/// is either a mistake or an attempt to smuggle instructions into a field a
/// reviewer skims.
pub const MAX_DESCRIPTION_CHARS: usize = 500;

/// A system prompt is legitimately long, but not unbounded — a plugin whose
/// prompt runs to novel length is spending the user's context on something
/// they have no practical way to review.
pub const MAX_PROMPT_CHARS: usize = 40_000;

/// One piece of text that will reach the model, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleText {
    /// Human-readable provenance, e.g. `agent "reviewer" description`.
    pub origin: String,
    pub text: String,
}

/// Everything an installer should show before a plugin is enabled.
#[derive(Debug, Clone, Default)]
pub struct Disclosure {
    pub plugin: String,
    pub version: String,
    /// Text the model will see, verbatim.
    pub model_visible: Vec<VisibleText>,
    /// Capability grants, one human-readable line each.
    pub capabilities: Vec<String>,
    /// Scene id this plugin registers, if any.
    pub scene: Option<String>,
    /// MCP servers it will start, by declared name and kind.
    pub mcp_servers: Vec<String>,
}

impl Disclosure {
    /// Collect everything the manifest and the plugin's files declare.
    ///
    /// Fails when a piece of model-visible text is over its limit. That is a
    /// refusal to install, not a warning: a limit that only warns is one
    /// every automated installer will step straight past.
    pub fn from_plugin(plugin: &Plugin) -> Result<Self, PluginError> {
        let m = &plugin.manifest;
        let mut d = Disclosure {
            plugin: m.plugin.name.clone(),
            version: m.plugin.version.clone(),
            scene: plugin.scene_id(),
            ..Default::default()
        };

        if !m.plugin.description.is_empty() {
            d.push_text(
                "plugin description",
                &m.plugin.description,
                MAX_DESCRIPTION_CHARS,
            )?;
        }

        // A script the host runs in its own process, at a point that can
        // rewrite a tool result or what goes to the model. Nothing isolates
        // it from the host, so it is disclosed with the capability
        // grants rather than left for the reader to infer from the package
        // contents.
        for script in &m.script {
            d.capabilities.push(format!(
                "run its own JavaScript at the `{}` extension point",
                script.point
            ));
        }

        for server in &m.mcp {
            let kind = match server.kind {
                crate::manifest::McpKind::Native => "native",
                crate::manifest::McpKind::Dsh => "dsh",
            };
            d.mcp_servers.push(format!("{} ({kind})", server.name));
        }

        for agent in &m.agent {
            d.push_text(
                &format!("agent `{}` description", agent.name),
                &agent.description,
                MAX_DESCRIPTION_CHARS,
            )?;
            let path = plugin.path(&agent.prompt);
            if let Ok(body) = std::fs::read_to_string(&path) {
                d.push_text(
                    &format!("agent `{}` system prompt", agent.name),
                    &body,
                    MAX_PROMPT_CHARS,
                )?;
            }
        }

        if let Some(loaded) = crate::scenes::declared_scene(plugin) {
            let scene = loaded.map_err(|e| PluginError::Schema(e.to_string()))?;
            let description = base::interface::scene::Scene::description(&scene);
            if !description.is_empty() {
                d.push_text("scene description", description, MAX_DESCRIPTION_CHARS)?;
            }
            d.push_text("scene system prompt", scene.prompt(), MAX_PROMPT_CHARS)?;
        }

        Ok(d)
    }

    /// Does this plugin reach anything outside itself, or shape what the
    /// model is told? A plugin that does neither needs no scrutiny beyond
    /// its name.
    pub fn is_inert(&self) -> bool {
        self.model_visible.is_empty()
            && self.capabilities.is_empty()
            && self.scene.is_none()
            && self.mcp_servers.is_empty()
    }

    fn push_text(&mut self, origin: &str, text: &str, limit: usize) -> Result<(), PluginError> {
        let len = text.chars().count();
        if len > limit {
            return Err(PluginError::Schema(format!(
                "{origin} is {len} characters, over the {limit} allowed for text that \
                 reaches the model"
            )));
        }
        self.model_visible.push(VisibleText {
            origin: origin.to_string(),
            text: text.to_string(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const HEAD: &str = "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn load(root: &Path, body: &str) -> Plugin {
        std::fs::write(root.join("plugin.toml"), format!("{HEAD}{body}")).unwrap();
        Plugin::load(root, &root.join("plugin.toml")).unwrap()
    }

    #[test]
    fn a_plugin_that_declares_nothing_needs_no_scrutiny() {
        let dir = tempfile::tempdir().unwrap();
        let d = Disclosure::from_plugin(&load(dir.path(), "")).unwrap();
        assert!(d.is_inert());
        assert_eq!(d.plugin, "demo");
        assert_eq!(d.version, "1.0.0");
    }

    /// A script is a capability, and the least visible one: it runs in the
    /// host's own process, at a point that decides what the model reads, and
    /// no sandbox is between it and anything. A package that ships one is not
    /// inert.
    #[test]
    fn a_script_binding_is_disclosed_as_a_capability() {
        let dir = tempfile::tempdir().unwrap();
        let p = load(
            dir.path(),
            "\n[[script]]\npoint = \"tool.result\"\nentry = \"annotate.js:onResult\"\n",
        );
        let d = Disclosure::from_plugin(&p).unwrap();
        assert!(
            d.capabilities
                .iter()
                .any(|c| c.contains("`tool.result`") && c.contains("JavaScript")),
            "{:?}",
            d.capabilities
        );
        assert!(!d.is_inert(), "a package that runs code is not inert");
    }

    /// The prompt is the payload a sandbox cannot inspect, so the installer
    /// has to be able to show it verbatim with its provenance.
    #[test]
    fn model_visible_text_is_collected_with_where_it_came_from() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("scene")).unwrap();
        std::fs::write(
            dir.path().join("scene/scene.json"),
            r#"{"id": "plugin:demo", "name": "Demo", "description": "the demo scene"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("scene/prompt.md"), "Obey the demo plugin.").unwrap();
        std::fs::write(dir.path().join("agent.md"), "You review diffs.").unwrap();
        let p = load(
            dir.path(),
            r#"
description = "a demo"

[scene]
dir = "scene"

[[agent]]
name = "reviewer"
description = "reviews things"
prompt = "agent.md"
"#,
        );
        let d = Disclosure::from_plugin(&p).unwrap();

        let origins: Vec<&str> = d.model_visible.iter().map(|v| v.origin.as_str()).collect();
        assert!(origins.contains(&"plugin description"), "{origins:?}");
        assert!(origins.contains(&"scene system prompt"), "{origins:?}");
        assert!(
            origins.iter().any(|o| o.contains("reviewer")),
            "an agent's own prompt reaches the model too: {origins:?}"
        );
        assert!(d
            .model_visible
            .iter()
            .any(|v| v.text == "Obey the demo plugin."));
        assert_eq!(d.scene.as_deref(), Some("plugin:demo"));
    }

    /// A one-liner that is secretly a document is the shape of an attempt to
    /// get past a reviewer who is skimming a list of labels.
    #[test]
    fn an_overlong_description_refuses_the_install() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.md"), "body").unwrap();
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        let p = load(
            dir.path(),
            &format!(
                "\n[[agent]]\nname = \"a\"\ndescription = \"{long}\"\nprompt = \"agent.md\"\n"
            ),
        );
        let err = Disclosure::from_plugin(&p).unwrap_err().to_string();
        assert!(err.contains("over the"), "{err}");
        assert!(err.contains("reaches the model"), "{err}");
    }

    #[test]
    fn a_scene_directory_is_disclosed_and_a_broken_one_refuses_the_install() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("scene")).unwrap();
        std::fs::write(
            dir.path().join("scene/scene.json"),
            r#"{"id": "plugin:demo", "name": "Demo", "description": "the demo scene"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("scene/prompt.md"), "Obey {{cwd}}.").unwrap();
        let p = load(dir.path(), "\n[scene]\ndir = \"scene\"\n");
        let d = Disclosure::from_plugin(&p).unwrap();
        assert_eq!(d.scene.as_deref(), Some("plugin:demo"));
        assert!(d
            .model_visible
            .iter()
            .any(|v| v.origin == "scene system prompt" && v.text == "Obey {{cwd}}."));
        assert!(d
            .model_visible
            .iter()
            .any(|v| v.origin == "scene description" && v.text == "the demo scene"));

        std::fs::write(
            dir.path().join("scene/scene.json"),
            r#"{"id": "plugin:demo", "name": "Demo", "tool": ["Bash"]}"#,
        )
        .unwrap();
        let err = Disclosure::from_plugin(&p).unwrap_err().to_string();
        assert!(err.contains("unknown field `tool`"), "{err}");
    }

    #[test]
    fn an_overlong_system_prompt_refuses_the_install() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "x".repeat(MAX_PROMPT_CHARS + 1),
        )
        .unwrap();
        let p = load(
            dir.path(),
            "\n[[agent]]\nname = \"a\"\ndescription = \"d\"\nprompt = \"agent.md\"\n",
        );
        assert!(Disclosure::from_plugin(&p).is_err());
    }

    #[test]
    fn mcp_servers_are_listed_with_their_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.json"), "{}").unwrap();
        let p = load(
            dir.path(),
            r#"
[[mcp]]
name = "github"
kind = "native"
config = "s.json"

[[mcp]]
name = "helper"
kind = "dsh"
entry = "index.js"
"#,
        );
        let d = Disclosure::from_plugin(&p).unwrap();
        assert_eq!(d.mcp_servers, ["github (native)", "helper (dsh)"]);
    }
}
