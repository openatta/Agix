//! `McpServers` — the MCP servers a session is connected to, as the turn loop
//! uses them.
//!
//! Through this the kernel offers their tools to the model, carries their
//! instructions in the prompt, inlines `@server:uri` references, and runs
//! their prompts as `/mcp__<server>__<prompt>` commands. The mcp module owns
//! the connections; a session assembled without it has [`NoMcpServers`].

use std::sync::Arc;

use async_trait::async_trait;

use crate::context::McpServerInstruction;
use crate::tool::Tool;

#[async_trait]
pub trait McpServers: Send + Sync {
    /// The servers' tools, as tools a session can dispatch to.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
    /// Lists the servers' tools again.
    async fn refresh_tools(&mut self);
    /// Lists again what a server announced has changed since this last
    /// looked; whether anything had.
    async fn refresh_if_changed(&mut self) -> bool;
    fn instructions(&self) -> Vec<McpServerInstruction>;
    /// The resources `text` references, rendered for the model; `None` when
    /// it references none.
    async fn resolve_resource_refs(&self, text: &str) -> Option<String>;
    fn prompt_commands(&self) -> Vec<McpPromptCommand>;
    fn has_prompt_command(&self, command: &str) -> bool;
    /// Never fails: an unreachable server or a refused prompt comes back as
    /// text the model can read, flagged with `is_error`.
    async fn invoke_prompt_command(&self, command: &str, raw_args: &str) -> McpPromptInvocation;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptCommand {
    pub command: String,
    pub description: String,
    pub argument_hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptInvocation {
    pub text: String,
    pub is_error: bool,
}

pub struct NoMcpServers;

#[async_trait]
impl McpServers for NoMcpServers {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    async fn refresh_tools(&mut self) {}
    async fn refresh_if_changed(&mut self) -> bool {
        false
    }
    fn instructions(&self) -> Vec<McpServerInstruction> {
        Vec::new()
    }
    async fn resolve_resource_refs(&self, _: &str) -> Option<String> {
        None
    }
    fn prompt_commands(&self) -> Vec<McpPromptCommand> {
        Vec::new()
    }
    fn has_prompt_command(&self, _: &str) -> bool {
        false
    }
    async fn invoke_prompt_command(&self, command: &str, _: &str) -> McpPromptInvocation {
        McpPromptInvocation {
            text: format!("no MCP server provides `/{command}`"),
            is_error: true,
        }
    }
}
