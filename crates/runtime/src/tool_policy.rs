//! `ToolPolicy` — which tools a session may have, decided once when the
//! session is built and read by every place that filters tools: the registry
//! itself, each request's tool list, and the MCP adapter sync.
//!
//! A tool is permitted when the delegation selector admits it and either the
//! scene's tool surface admits it (`SceneSpec::tools`), or a plugin
//! registered that tool, admitted it with `Registrar::admit_tools`, and the
//! scene's deny list does not name it. Every tool answers to this one
//! judgment — the engine's and modules', the scene's and plugins', MCP
//! adapters connected at build or later — so no path can skip a list another
//! path honors.

use std::collections::HashSet;

use base::interface::scene::SceneSpec;
use base::interface::tool::{Tool, ToolOrigin};
use base::interface::tool_selector::ToolSelector;

/// What a session cannot work without once it has them: the way back to a
/// deferred schema, and the way to hand back the output a host asked for.
const STRUCTURAL: &[&str] = &["ToolSearch", "StructuredOutput"];

pub(crate) struct ToolPolicy {
    scene: ToolSelector,
    scene_deny: ToolSelector,
    delegation: ToolSelector,
    admitted: HashSet<String>,
}

impl ToolPolicy {
    /// `delegation` is what the session's delegator narrowed it to —
    /// [`ToolSelector::all`] for a session nobody delegated.
    /// `admitted_plugin_tools` must be tools a plugin registered into this
    /// session and admitted itself: `plugin::register_all` matches each
    /// plugin's patterns only against its own tools.
    pub(crate) fn new(
        scene: &SceneSpec,
        delegation: ToolSelector,
        admitted_plugin_tools: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            scene: scene.tools.selector(),
            scene_deny: ToolSelector::layer(None, scene.tools.deny.clone()),
            delegation,
            admitted: admitted_plugin_tools.into_iter().collect(),
        }
    }

    pub(crate) fn permits(&self, tool: &dyn Tool) -> bool {
        self.admits(tool.name(), &tool.spec().origin)
    }

    fn admits(&self, name: &str, origin: &ToolOrigin) -> bool {
        let delegated = STRUCTURAL.contains(&name) || self.delegation.admits(name, origin);
        delegated
            && (self.scene.admits(name, origin)
                || (self.admitted.contains(name) && self.scene_deny.admits(name, origin)))
    }

    /// For a built-in the builder adds after plugins have registered: a
    /// plugin that took the same name must not carry it in.
    #[cfg(any(feature = "ptc", test))]
    pub(crate) fn scene_permits(&self, name: &str) -> bool {
        let builtin = ToolOrigin::Builtin;
        self.scene.admits(name, &builtin) && self.delegation.admits(name, &builtin)
    }
}

/// A plugin's admit entry: an exact name, or a prefix ending in `*`.
pub(crate) fn covers(entry: &str, name: &str) -> bool {
    match entry.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => entry == name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::scene::{FixedPrompt, ToolSurface};
    use std::sync::Arc;

    fn scene(tools: ToolSurface) -> SceneSpec {
        let mut s = SceneSpec::new("whitelisted", "whitelisted", Arc::new(FixedPrompt(vec![])));
        s.tools = tools;
        s
    }

    #[test]
    fn an_admitted_plugin_tool_passes_a_whitelist_but_not_the_scenes_disallowed_list() {
        let scene = scene(ToolSurface::from_lists(&["Read"], &["demo__blocked*"]));
        let policy = ToolPolicy::new(
            &scene,
            ToolSelector::all(),
            ["demo__x", "demo__blocked_one"].map(String::from),
        );
        let plugin = ToolOrigin::plugin("demo");
        assert!(policy.admits("Read", &ToolOrigin::Builtin));
        assert!(policy.admits("demo__x", &plugin));
        assert!(!policy.admits("demo__blocked_one", &plugin));
        assert!(!policy.admits("Bash", &ToolOrigin::Builtin));
        assert!(!policy.scene_permits("demo__x"));
    }

    #[test]
    fn with_nothing_admitted_the_policy_is_the_scenes() {
        let scene = scene(ToolSurface::allowing(&["Read", "mcp__*"]));
        let policy = ToolPolicy::new(&scene, ToolSelector::all(), []);
        assert!(policy.admits("Read", &ToolOrigin::Builtin));
        assert!(policy.admits("mcp__srv__do", &ToolOrigin::mcp("srv")));
        assert!(!policy.admits("demo__x", &ToolOrigin::plugin("demo")));
    }

    #[test]
    fn a_scene_deny_holds_under_its_allow_list() {
        let scene = scene(ToolSurface::from_lists(&["Read", "Bash"], &["Bash"]));
        let policy = ToolPolicy::new(&scene, ToolSelector::all(), []);
        assert!(policy.admits("Read", &ToolOrigin::Builtin));
        assert!(!policy.admits("Bash", &ToolOrigin::Builtin));
    }
    #[test]
    fn the_delegation_selector_holds_every_tool_but_the_structural_ones() {
        let scene = scene(ToolSurface::default());
        let policy = ToolPolicy::new(
            &scene,
            ToolSelector::from_lists(&["Read".to_string()], &["mcp__my.docs".to_string()]),
            ["demo__x".to_string()],
        );
        assert!(policy.admits("Read", &ToolOrigin::Builtin));
        assert!(!policy.admits("CronCreate", &ToolOrigin::Builtin));
        assert!(!policy.admits("demo__x", &ToolOrigin::Builtin));
        assert!(!policy.admits("mcp__my_docs__x", &ToolOrigin::mcp("my.docs")));
        assert!(policy.admits("ToolSearch", &ToolOrigin::Builtin));
        assert!(policy.admits("StructuredOutput", &ToolOrigin::Builtin));
        assert!(!policy.scene_permits("Bash"));
    }

    #[test]
    fn a_structural_tool_still_answers_to_the_scene() {
        let scene = scene(ToolSurface::allowing(&["Read"]));
        let policy = ToolPolicy::new(&scene, ToolSelector::all(), []);
        assert!(!policy.admits("ToolSearch", &ToolOrigin::Builtin));
    }
}
