//! `AgentTool` helpers that resolve what a delegate gets: tools, prompt, settings,
//! permission handler.

use super::*;

impl AgentTool {
    /// Returns a permission handler for sub-agent creation.
    pub(crate) fn sub_permission(&self) -> PermissionSet {
        self.permission_handler()
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

    /// The tools a delegate of `subagent_type` gets: what it inherits
    /// (`Inner::inherited_tools`), narrowed by the type's `allowed_tools` /
    /// `disallowed_tools` and then by the caller's `allowed` / `disallowed`
    /// (empty narrows nothing). Every spawn path resolves its tools here.
    ///
    /// MCP tools are not among them: a type's `mcp_servers` grants the
    /// delegate those servers (`Inner::mcp_granted_to`), apart from its tools.
    pub(crate) fn delegate_tools(
        &self,
        subagent_type: Option<&str>,
        allowed: &[String],
        disallowed: &[String],
    ) -> Arc<InMemoryToolRegistry> {
        let def = subagent_type.and_then(|name| self.inner.agent_type_def(name));
        let registry = InMemoryToolRegistry::new();
        for tool in self.inner.pool.inherited() {
            if type_admits_tool(def.as_ref(), tool.name())
                && lists_admit_tool(allowed, disallowed, tool.name())
            {
                registry.register(tool);
            }
        }

        Arc::new(registry)
    }

    #[cfg(test)]
    pub(super) fn sub_settings(&self, model_name: Option<&str>) -> LoadedSettings {
        self.inner.sub_settings(model_name)
    }

    /// The parent session's own policy, so a session's rules cannot be
    /// bypassed by spawning a sub-agent.
    pub(crate) fn permission_handler(&self) -> PermissionSet {
        self.inner.permission_handler()
    }
}
