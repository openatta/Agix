//! 会话能调用什么：工具注册表、技能、命令、MCP 服务器，以及包在每次工具调用外面的中间件。

use std::collections::HashSet;
use std::sync::Arc;

/// What the model can call, and what wraps a call.
pub(crate) struct Tooling {
    tools: Arc<dyn base::tool::ToolRegistry>,
    skills: Arc<dyn base::interface::skill_catalog::SkillCatalog>,
    commands: Arc<crate::commands::CommandRegistry>,
    mcp: Box<dyn base::interface::mcp_servers::McpServers>,
    delegator: Arc<dyn base::interface::delegate::Delegator>,
    tool_middleware: Arc<Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>>,
    result_transformers: Arc<Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>>,
    /// Built-in/MCP name collisions already logged, so a persistent one does
    /// not warn on every request.
    mcp_shadow_warned: HashSet<String>,
    tool_policy: Arc<crate::tool_policy::ToolPolicy>,
}

impl Tooling {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tools: Arc<dyn base::tool::ToolRegistry>,
        skills: Arc<dyn base::interface::skill_catalog::SkillCatalog>,
        commands: Arc<crate::commands::CommandRegistry>,
        mcp: Box<dyn base::interface::mcp_servers::McpServers>,
        delegator: Arc<dyn base::interface::delegate::Delegator>,
        tool_middleware: Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>,
        result_transformers: Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>,
        tool_policy: Arc<crate::tool_policy::ToolPolicy>,
    ) -> Self {
        Self {
            tools,
            skills,
            commands,
            mcp,
            delegator,
            tool_middleware: Arc::new(tool_middleware),
            result_transformers: Arc::new(result_transformers),
            mcp_shadow_warned: HashSet::new(),
            tool_policy,
        }
    }

    pub(crate) fn tools(&self) -> &Arc<dyn base::tool::ToolRegistry> {
        &self.tools
    }

    pub(crate) fn skills(&self) -> &Arc<dyn base::interface::skill_catalog::SkillCatalog> {
        &self.skills
    }

    pub(crate) fn commands(&self) -> &Arc<crate::commands::CommandRegistry> {
        &self.commands
    }

    pub(crate) fn mcp(&self) -> &dyn base::interface::mcp_servers::McpServers {
        self.mcp.as_ref()
    }

    pub(crate) fn delegator(&self) -> &Arc<dyn base::interface::delegate::Delegator> {
        &self.delegator
    }

    pub(crate) fn tool_middleware(
        &self,
    ) -> &Arc<Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>> {
        &self.tool_middleware
    }

    pub(crate) fn result_transformers(
        &self,
    ) -> &Arc<Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>> {
        &self.result_transformers
    }

    pub(crate) fn tool_policy(&self) -> &crate::tool_policy::ToolPolicy {
        &self.tool_policy
    }

    /// Whether this is the first time `name` was found shadowing an MCP tool.
    pub(crate) fn first_shadowing(&mut self, name: &str) -> bool {
        self.mcp_shadow_warned.insert(name.to_string())
    }

    /// Relist every MCP server's tools and register what they offer now.
    pub(crate) async fn refresh_mcp(&mut self) {
        self.mcp.refresh_tools().await;
        self.sync_mcp();
    }

    /// Register what the MCP servers offer now, relisting only those that
    /// said their tools changed.
    pub(crate) async fn refresh_mcp_if_changed(&mut self) {
        self.mcp.refresh_if_changed().await;
        self.sync_mcp();
    }

    fn sync_mcp(&self) {
        sync_mcp_adapters(self.tools.as_ref(), self.mcp.as_ref(), |t| {
            self.tool_policy.permits(t)
        });
    }

    #[cfg(test)]
    pub(crate) fn set_skills(
        &mut self,
        skills: Arc<dyn base::interface::skill_catalog::SkillCatalog>,
    ) {
        self.skills = skills;
    }

    #[cfg(test)]
    pub(crate) fn set_commands(&mut self, commands: Arc<crate::commands::CommandRegistry>) {
        self.commands = commands;
    }

    #[cfg(test)]
    pub(crate) fn set_mcp(&mut self, mcp: Box<dyn base::interface::mcp_servers::McpServers>) {
        self.mcp = mcp;
    }

    #[cfg(test)]
    pub(crate) fn shadow_warned(&self) -> &HashSet<String> {
        &self.mcp_shadow_warned
    }
}

/// Make the dispatch registry's MCP entries match `mcp`'s current adapter
/// list: new tools are added, re-listed ones take the fresh instance, and ones
/// a server stopped listing are withdrawn so they can no longer be called.
/// Adapters the session's tool policy does not permit are never registered
/// — see `tool_policy`.
pub(crate) fn sync_mcp_adapters(
    tools: &dyn base::tool::ToolRegistry,
    mcp: &dyn base::interface::mcp_servers::McpServers,
    permits: impl Fn(&dyn base::tool::Tool) -> bool,
) {
    let is_mcp = |t: &Arc<dyn base::tool::Tool>| t.spec().origin.is_mcp();
    let current: Vec<_> = mcp
        .tools()
        .into_iter()
        .filter(|a| permits(a.as_ref()))
        .collect();
    let live: HashSet<&str> = current.iter().map(|a| a.name()).collect();
    let registered = tools.list();
    let builtin_names: HashSet<&str> = registered
        .iter()
        .filter(|t| !is_mcp(t))
        .map(|t| t.name())
        .collect();
    for stale in registered
        .iter()
        .filter(|t| is_mcp(t) && !live.contains(t.name()))
    {
        // `remove` withdraws every registration under the name, a builtin's too.
        if !builtin_names.contains(stale.name()) {
            tools.remove(stale.name());
        }
    }
    for adapter in &current {
        if builtin_names.contains(adapter.name()) {
            continue;
        }
        match tools.get(adapter.name()) {
            Some(existing) if Arc::ptr_eq(&existing, adapter) => {}
            _ => {
                tools.replace(adapter.clone());
            }
        }
    }
}
