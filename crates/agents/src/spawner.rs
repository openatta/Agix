//! Runtime implementation of the AgentSpawner trait.
//!
//! Wraps `AgentTool`'s delegate runs to provide sub-agent spawning to
//! consumers (the team coordinator, Skill fork, agent hooks) without creating
//! a circular dependency.

use async_trait::async_trait;
use base::interface::agent_spawner::{AgentSpawner, SpawnMode, SpawnRequest};
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
    async fn spawn(
        &self,
        request: SpawnRequest,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        let SpawnRequest {
            prompt,
            cwd,
            cancel,
            agent_type,
            model,
            allowed_tools,
            mode,
        } = request;
        let tools = self.agent_tool.sub_tools(&allowed_tools);
        let ran = match mode {
            SpawnMode::Wait => {
                let perm = self.agent_tool.sub_permission();
                self.agent_tool
                    .run_sub(
                        prompt,
                        tools,
                        cwd,
                        cancel,
                        perm,
                        agent_type.as_deref(),
                        model,
                    )
                    .await
            }
            SpawnMode::Background { session } => {
                self.agent_tool
                    .spawn_background(
                        prompt,
                        tools,
                        cwd,
                        cancel,
                        agent_type.as_deref(),
                        model,
                        session,
                    )
                    .await
            }
            SpawnMode::TeamMember {
                extra_tools,
                permission_mode,
                seat,
            } => {
                // A fresh registry per call, so one member's mailbox tools reach
                // only that member.
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
                        model,
                        permission_mode,
                        seat,
                    )
                    .await
            }
        };
        ran.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)
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
