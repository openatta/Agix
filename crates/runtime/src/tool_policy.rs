//! `ToolPolicy` — which tool names a session may have, decided once when the
//! session is built and read by every place that filters tools: the registry
//! itself, each request's tool list, and the MCP adapter sync.
//!
//! A name is permitted when the scene permits it (`Scene::permits_tool`), or
//! when a plugin registered that tool, admitted it with
//! `Registrar::admit_tools`, and the scene's `disallowed_tools` does not name
//! it. This is the only place the scene's own judgment is consulted, so the
//! plugin exception cannot be skipped by one path and honored by another.

use std::collections::HashSet;
use std::sync::Arc;

use base::interface::scene::Scene;

pub(crate) struct ToolPolicy {
    scene: Arc<dyn Scene>,
    admitted: HashSet<String>,
}

impl ToolPolicy {
    /// `admitted_plugin_tools` must be tools a plugin registered into this
    /// session and admitted itself: `plugin::register_all` matches each
    /// plugin's patterns only against its own tools, and the builder leaves
    /// out the ones a sub-agent's tool filter dropped.
    pub(crate) fn new(
        scene: Arc<dyn Scene>,
        admitted_plugin_tools: impl IntoIterator<Item = String>,
    ) -> Self {
        let disallowed = scene.disallowed_tools();
        let admitted = admitted_plugin_tools
            .into_iter()
            .filter(|name| !disallowed.iter().any(|entry| covers(entry, name)))
            .collect();
        Self { scene, admitted }
    }

    pub(crate) fn permits(&self, name: &str) -> bool {
        self.admitted.contains(name) || self.scene.permits_tool(name)
    }

    /// The scene alone, for a built-in the builder adds after plugins have
    /// registered: a plugin that took the same name must not carry it in.
    #[cfg(any(feature = "ptc", test))]
    pub(crate) fn scene_permits(&self, name: &str) -> bool {
        self.scene.permits_tool(name)
    }
}

/// A scene or admit entry: an exact name, or a prefix ending in `*`.
pub(crate) fn covers(entry: &str, name: &str) -> bool {
    match entry.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => entry == name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::scene::{ScenePromptContext, TokenBudget};
    use base::prompt::PromptBlock;

    struct Whitelisted {
        tools: Vec<&'static str>,
        disallowed: Vec<&'static str>,
    }

    impl Scene for Whitelisted {
        fn id(&self) -> &str {
            "whitelisted"
        }
        fn name(&self) -> &str {
            "whitelisted"
        }
        fn description(&self) -> &str {
            ""
        }
        fn build_system_prompt(&self, _: &ScenePromptContext) -> Vec<PromptBlock> {
            vec![]
        }
        fn tools(&self) -> Vec<String> {
            self.tools.iter().map(|s| s.to_string()).collect()
        }
        fn disallowed_tools(&self) -> Vec<String> {
            self.disallowed.iter().map(|s| s.to_string()).collect()
        }
        fn token_budget(&self) -> TokenBudget {
            scene::mini::MiniScene.token_budget()
        }
    }

    #[test]
    fn an_admitted_plugin_tool_passes_a_whitelist_but_not_the_scenes_disallowed_list() {
        let scene = Arc::new(Whitelisted {
            tools: vec!["Read"],
            disallowed: vec!["demo__blocked*"],
        });
        let policy = ToolPolicy::new(scene, ["demo__x", "demo__blocked_one"].map(String::from));
        assert!(policy.permits("Read"));
        assert!(policy.permits("demo__x"));
        assert!(!policy.permits("demo__blocked_one"));
        assert!(!policy.permits("Bash"));
        assert!(!policy.scene_permits("demo__x"));
    }

    #[test]
    fn with_nothing_admitted_the_policy_is_the_scenes() {
        let scene = Arc::new(Whitelisted {
            tools: vec!["Read", "mcp__*"],
            disallowed: vec![],
        });
        let policy = ToolPolicy::new(scene, []);
        assert!(policy.permits("Read"));
        assert!(policy.permits("mcp__srv__do"));
        assert!(!policy.permits("demo__x"));
    }
}
