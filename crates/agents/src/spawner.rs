//! Runtime implementation of the AgentSpawner trait.
//!
//! Wraps AgentTool::run_sub from this crate to provide sub-agent spawning
//! to consumers (e.g., the team coordinator) without creating a circular dependency.

use async_trait::async_trait;
use base::interface::agent_spawner::AgentSpawner;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent_tool::AgentTool;

/// Spawns sub-agents by delegating to AgentTool::run_sub.
pub struct RuntimeAgentSpawner {
    agent_tool: Arc<AgentTool>,
}

impl RuntimeAgentSpawner {
    pub fn new(agent_tool: Arc<AgentTool>) -> Self {
        Self { agent_tool }
    }
}

#[async_trait]
impl AgentSpawner for RuntimeAgentSpawner {
    async fn spawn_agent(
        &self,
        prompt: String,
        _allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        // allowed_tools filtering is not implemented — the sub-agent gets
        // the same tool pool as the parent.
        let tools = self.agent_tool.sub_tools();
        let perm = self.agent_tool.sub_permission();

        self.agent_tool
            .run_sub(prompt, tools, cwd, cancel, perm, agent_type.as_deref())
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)
    }

    async fn spawn_agent_background(
        &self,
        prompt: String,
        _allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
        session: Arc<base::context::SessionState>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        let tools = self.agent_tool.sub_tools();
        self.agent_tool
            .spawn_background(prompt, tools, cwd, cancel, agent_type.as_deref(), session)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)
    }

    async fn spawn_agent_with_tools(
        &self,
        prompt: String,
        _allowed_tools: Vec<String>,
        extra_tools: Vec<Arc<dyn base::tool::Tool>>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
        permission_mode: Option<base::interface::settings::PermissionMode>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        // Same "fresh per-call registry" pattern as `Builder::build()` uses
        // for the per-session registry — cloning into a new
        // `InMemoryToolRegistry` instead of mutating the shared
        // `sub_tools()` Arc, so `extra_tools` (mailbox tools scoped to one
        // team member's label) don't leak into every other caller sharing
        // that same base registry.
        let base_tools = self.agent_tool.sub_tools();
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        for t in base_tools.list() {
            tools.register(t);
        }
        for t in extra_tools {
            tools.register(t);
        }
        let perm = match permission_mode {
            Some(mode) => self.agent_tool.team_member_permission(&tools, mode),
            None => self.agent_tool.sub_permission(),
        };

        self.agent_tool
            .run_team_member(
                prompt,
                tools,
                cwd,
                cancel,
                perm,
                agent_type.as_deref(),
                permission_mode,
            )
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)
    }

    async fn start_agent(
        &self,
        request: base::interface::agent_spawner::AgentRequest,
        session: Arc<base::context::SessionState>,
        cancel: CancellationToken,
    ) -> Result<
        base::interface::agent_spawner::StartedAgent,
        base::interface::agent_spawner::StartAgentError,
    > {
        self.agent_tool.start_for_host(request, session, cancel)
    }

    async fn stop_team_member(&self, team_name: &str, member_name: &str) {
        self.agent_tool
            .stop_team_member(team_name, member_name)
            .await;
    }
}
