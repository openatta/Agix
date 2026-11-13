//! `ToolPool`: the parent's tools a delegate may start from.

use std::collections::HashSet;
use std::sync::Arc;

use base::tool::{InMemoryToolRegistry, Tool};

/// Never inherited: a delegate's own agents module registers its own `Agent`,
/// counted from the delegate's depth, or none at the limit.
pub(crate) const AGENT_TOOL_NAME: &str = "Agent";

pub(crate) struct ToolPool {
    /// What the parent session was built from.
    pub(crate) template: Arc<InMemoryToolRegistry>,
    /// The parent session's own registry, as it stands when a delegate spawns.
    pub(crate) parent_tools: Arc<InMemoryToolRegistry>,
    /// The tools the parent's plugins registered in it. A delegate is not
    /// handed these copies: its own plugins register its own.
    pub(crate) plugin_tools: HashSet<String>,
}

impl ToolPool {
    pub(crate) fn new(
        template: Arc<InMemoryToolRegistry>,
        parent_tools: Arc<InMemoryToolRegistry>,
        plugin_tools: &[String],
    ) -> Self {
        Self {
            template,
            parent_tools,
            plugin_tools: plugin_tools.iter().cloned().collect(),
        }
    }

    /// The template's tools the parent still has — so nothing its scene took
    /// away. What the parent's engine, modules, scene and plugins registered
    /// on top is not among them; the delegate's own build registers its own.
    pub(crate) fn inherited(&self) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for tool in self.template.list() {
            if tool.name() != AGENT_TOOL_NAME
                && self.parent_tools.get(tool.name()).is_some()
                && !self.plugin_tools.contains(tool.name())
                && !out.iter().any(|t| t.name() == tool.name())
            {
                out.push(tool);
            }
        }
        out
    }
}
