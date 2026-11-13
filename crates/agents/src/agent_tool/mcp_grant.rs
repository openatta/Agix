//! A delegate's MCP grant held to its caller's tool selector: `session.startAgent`'s
//! `allowed_tools` / `disallowed_tools` reach the granted servers' tools too.

use super::*;
use base::interface::mcp_servers::{McpPromptCommand, McpPromptInvocation, McpServers};

pub(super) struct ListedMcp {
    pub(super) servers: Box<dyn McpServers>,
    pub(super) selector: ToolSelector,
}

#[async_trait]
impl McpServers for ListedMcp {
    fn tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        self.servers
            .tools()
            .into_iter()
            .filter(|t| self.selector.admits(t.name(), &t.spec().origin))
            .collect()
    }
    async fn refresh_tools(&mut self) {
        self.servers.refresh_tools().await;
    }
    async fn refresh_if_changed(&mut self) -> bool {
        self.servers.refresh_if_changed().await
    }
    fn instructions(&self) -> Vec<base::context::McpServerInstruction> {
        self.servers.instructions()
    }
    async fn resolve_resource_refs(&self, text: &str) -> Option<String> {
        self.servers.resolve_resource_refs(text).await
    }
    fn prompt_commands(&self) -> Vec<McpPromptCommand> {
        self.servers.prompt_commands()
    }
    fn has_prompt_command(&self, command: &str) -> bool {
        self.servers.has_prompt_command(command)
    }
    async fn invoke_prompt_command(&self, command: &str, raw_args: &str) -> McpPromptInvocation {
        self.servers.invoke_prompt_command(command, raw_args).await
    }
    fn granted(&self, servers: Option<&[String]>) -> Box<dyn McpServers> {
        Box::new(ListedMcp {
            servers: self.servers.granted(servers),
            selector: self.selector.clone(),
        })
    }
}
