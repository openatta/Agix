//! MCP, as a module: a session's connected servers, their tools, and the
//! resource tools over them.
//!
//! [`McpModule`] hands one session the [`McpManager`] its host connected for
//! it, as the kernel's [`McpServers`]. The manager goes to the first session
//! assembled with the module: a delegate spawned from that session is granted
//! the servers its agent type names through the `Agent` tool instead of
//! connections of its own.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base::context::McpServerInstruction;
use base::interface::mcp_servers::{McpPromptCommand, McpPromptInvocation, McpServers};
use base::tool::Tool;
use runtime::module::{Module, ModuleError, SessionContext};

use crate::manager::McpManager;

pub const MODULE_ID: &str = "mcp";

#[async_trait]
impl McpServers for McpManager {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tool_adapters().to_vec()
    }
    async fn refresh_tools(&mut self) {
        McpManager::refresh_tools(self).await;
    }
    async fn refresh_if_changed(&mut self) -> bool {
        McpManager::refresh_if_changed(self).await
    }
    fn instructions(&self) -> Vec<McpServerInstruction> {
        self.server_instructions()
    }
    async fn resolve_resource_refs(&self, text: &str) -> Option<String> {
        McpManager::resolve_resource_refs(self, text).await
    }
    fn prompt_commands(&self) -> Vec<McpPromptCommand> {
        self.all_prompts()
            .iter()
            .map(|p| McpPromptCommand {
                command: p.command_name(),
                description: if p.description.is_empty() {
                    format!("MCP prompt `{}` from server `{}`", p.name, p.server)
                } else {
                    p.description.clone()
                },
                argument_hint: p.argument_hint(),
            })
            .collect()
    }
    fn has_prompt_command(&self, command: &str) -> bool {
        self.find_prompt_command(command).is_some()
    }
    async fn invoke_prompt_command(&self, command: &str, raw_args: &str) -> McpPromptInvocation {
        let invocation = McpManager::invoke_prompt_command(self, command, raw_args).await;
        McpPromptInvocation {
            text: invocation.text,
            is_error: invocation.is_error,
        }
    }
    fn granted(&self, servers: Option<&[String]>) -> Box<dyn McpServers> {
        Box::new(self.share_servers(servers))
    }
}

pub struct McpModule {
    manager: Mutex<Option<McpManager>>,
}

impl McpModule {
    pub fn new(manager: McpManager) -> Self {
        Self {
            manager: Mutex::new(Some(manager)),
        }
    }
}

impl Module for McpModule {
    fn id(&self) -> &'static str {
        MODULE_ID
    }

    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        let Some(mut manager) = self
            .manager
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return Ok(());
        };
        // An elicitation URL a server embeds in a tool result reaches the
        // session's hooks; the callback is sync, the hook is not.
        let hooks = cx.hooks.clone();
        let ambient = cx.hook_ambient.clone();
        manager.set_elicitation_callback(Arc::new(move |server_name, url| {
            let hooks = hooks.clone();
            let ambient = ambient.clone();
            tokio::spawn(async move {
                hooks.run_elicitation(&ambient, &server_name, &url).await;
            });
        }));
        if !manager.clients().is_empty() {
            let clients = manager.clients().to_vec();
            cx.tools
                .register(Arc::new(crate::tools::ListMcpResourcesTool::new(
                    clients.clone(),
                )));
            cx.tools
                .register(Arc::new(crate::tools::ReadMcpResourceTool::new(
                    clients.clone(),
                )));
            cx.tools
                .register(Arc::new(crate::tools::DispatchMcpTool::new(clients)));
            // Dispatch looks tools up in the session's registry only, so the
            // adapters live there too; the turn loop keeps them current.
            for adapter in manager.tool_adapters() {
                cx.tools.register(adapter.clone());
            }
        }
        cx.provide_mcp(Box::new(manager));
        Ok(())
    }
}
