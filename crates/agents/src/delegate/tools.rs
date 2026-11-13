//! `ToolPool`: the parent's tools a delegate may start from.

use std::sync::Arc;

use base::interface::tool::ToolOrigin;
use base::tool::InMemoryToolRegistry;

/// Never inherited: a delegate's own agents module registers its own `Agent`,
/// counted from the delegate's depth, or none at the limit.
pub(crate) const AGENT_TOOL_NAME: &str = "Agent";

pub(crate) struct ToolPool {
    /// What the parent session was built from.
    pub(crate) template: Arc<InMemoryToolRegistry>,
    /// The parent session's own registry, as it stands when a delegate spawns.
    pub(crate) parent_tools: Arc<InMemoryToolRegistry>,
}

impl ToolPool {
    pub(crate) fn new(
        template: Arc<InMemoryToolRegistry>,
        parent_tools: Arc<InMemoryToolRegistry>,
    ) -> Self {
        Self {
            template,
            parent_tools,
        }
    }

    /// The template's built-in tools the parent still has — so nothing its
    /// tool policy took away. What the parent's engine, modules, scene,
    /// plugins and MCP servers registered on top is not among them; the
    /// delegate's own build registers its own. Narrowing them to the
    /// delegate's type and caller is its tool policy's, not this.
    pub(crate) fn inherited(&self) -> Arc<InMemoryToolRegistry> {
        let out = InMemoryToolRegistry::new();
        for tool in self.template.list() {
            if tool.spec().origin == ToolOrigin::Builtin
                && tool.name() != AGENT_TOOL_NAME
                && self.parent_tools.get(tool.name()).is_some()
                && out.get(tool.name()).is_none()
            {
                out.register(tool);
            }
        }
        Arc::new(out)
    }
}
