//! `AgentTool` helpers that resolve what a delegate gets: tools, prompt, settings,
//! permission handler.

use super::*;

impl AgentTool {
    /// Returns the fallback tool registry for sub-agent creation.
    /// The parent's tools a delegate inherits, narrowed to `allowed`; empty
    /// allows them all.
    pub(crate) fn sub_tools(&self, allowed: &[String]) -> Arc<InMemoryToolRegistry> {
        let registry = InMemoryToolRegistry::new();
        for tool in self.inner.inherited_tools() {
            if lists_admit_tool(allowed, &[], tool.name()) {
                registry.register(tool);
            }
        }
        Arc::new(registry)
    }

    /// Returns a permission handler for sub-agent creation.
    pub(crate) fn sub_permission(&self) -> Arc<dyn Permission> {
        self.permission_handler()
    }

    /// A team member's permission: the parent's, judged in the member's own
    /// mode, as any sub-agent's is — the parent's deny rules and its host's
    /// answers reach the member. With no parent wired (an `AgentTool` built
    /// on its own) there are no rules to inherit, and the member is judged by
    /// its mode alone.
    pub(crate) fn team_member_permission(
        &self,
        tools: &Arc<InMemoryToolRegistry>,
        mode: base::interface::settings::PermissionMode,
    ) -> Arc<dyn Permission> {
        self.inner.team_member_permission(tools, mode)
    }

    /// System prompt injected into the sub-agent's context for the given
    /// `subagent_type`, looked up from the merged built-in + disk catalog.
    pub(super) fn type_prompt(&self, t: Option<&str>) -> Option<String> {
        t.and_then(|name| self.inner.agent_type_def(name))
            .map(|d| d.system_prompt)
    }

    pub(super) fn build_prompt(&self, subagent_type: Option<&str>, prompt: &str) -> String {
        if let Some(p) = self.type_prompt(subagent_type) {
            format!("{p}\n\nTask: {prompt}")
        } else {
            prompt.to_string()
        }
    }

    /// Does `tool_name` match an `allowed_tools`/`disallowed_tools` entry?
    /// Exact match, or an MCP server-level pattern: `mcp__<server>` or
    /// `mcp__<server>__*` matches every tool from that server (Agix's
    /// MCP tools are already named `mcp__<server>__<tool>`, so this is a
    /// plain prefix check, not a general glob).
    pub(super) fn tool_name_matches(tool_name: &str, pattern: &str) -> bool {
        if tool_name == pattern {
            return true;
        }
        if let Some(server) = pattern
            .strip_prefix("mcp__")
            .map(|s| s.trim_end_matches("__*"))
        {
            return tool_name
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split("__").next())
                == Some(server);
        }
        false
    }

    /// Resolve the tool set for a given subagent type.
    ///
    /// Returns a filtered [`InMemoryToolRegistry`] containing only the
    /// built-in tools the named agent type is allowed to use, plus (if
    /// `AgentTypeDefinition.mcp_servers` names any) the matching MCP tools
    /// from the parent's already-connected `McpManager`. Unknown types, and
    /// types whose `allowed_tools` is empty (the "full access" convention —
    /// see `general-purpose`/`claude`), fall back to the full built-in tool
    /// set **minus "Agent" itself** — see `AGENT_TOOL_NAME` doc comment for
    /// why. MCP is never part of that "full access" default: a subagent
    /// gets zero MCP tools unless `mcp_servers` explicitly grants them,
    /// regardless of how the built-in `allowed_tools` resolves — MCP access
    /// is opt-in per subagent, not inherited.
    pub(super) fn resolve_tools(&self, subagent_type: Option<&str>) -> Arc<InMemoryToolRegistry> {
        let def = subagent_type.and_then(|name| self.inner.agent_type_def(name));
        // `disallowed_tools` and `allowed_tools` together; a tool listed in
        // both ends up removed either way.
        let registry = InMemoryToolRegistry::new();
        for tool in self.inner.inherited_tools() {
            if tool.name() != AGENT_TOOL_NAME && type_admits_tool(def.as_ref(), tool.name()) {
                registry.register(tool);
            }
        }

        // MCP: additive, opt-in-only grant from `mcp_servers`, reusing the
        // parent's live connections rather than reconnecting. Applies on
        // top of either branch above — a restricted `allowed_tools` list
        // that doesn't mention any `mcp__...` name still lets a granted MCP
        // server's tools through, since `mcp_servers` is a separate axis
        // from the built-in tool allow/deny lists.
        if let Some(d) = &def {
            if !d.mcp_servers.is_empty() {
                let adapters = self.inner.mcp_tool_adapters();
                if adapters.is_empty() {
                    tracing::warn!(
                        agent_type = %d.name,
                        "agent mcp_servers: set but no MCP tools are attached to this AgentTool — no MCP tools granted"
                    );
                }
                for tool in &adapters {
                    if d.mcp_servers.iter().any(|server| {
                        Self::tool_name_matches(tool.name(), &format!("mcp__{server}"))
                    }) {
                        registry.register(tool.clone());
                    }
                }
            }
        }

        Arc::new(registry)
    }

    #[cfg(test)]
    pub(super) fn sub_settings(
        &self,
        model_name: Option<&str>,
        cwd: std::path::PathBuf,
    ) -> Settings {
        Inner::sub_settings(&self.inner, model_name, cwd)
    }

    /// Create a permission handler appropriate for this sub-agent's context.
    ///
    /// Resolution order:
    /// 1. the parent agent's own `Permission` impl, when wired via
    ///    [`AgentTool::set_parent_permission`] — so a session using
    ///    `RuleSetPermission` (or any other real handler) can't have its rules
    ///    bypassed by spawning a sub-agent;
    /// 2. [`AlwaysPermit`], for `AgentTool`s built
    ///    without any wiring (tests, embedders).
    pub(crate) fn permission_handler(&self) -> Arc<dyn Permission> {
        self.inner.permission_handler()
    }
}
