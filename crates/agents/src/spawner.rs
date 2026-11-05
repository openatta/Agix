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
        // Delegate to AgentTool::run_sub with all tools available.
        // allowed_tools filtering is not yet implemented — the sub-agent gets
        // the same tool pool as the parent (matching the TS reference where
        // workers run with the full tool set unless explicitly restricted).
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
        let task_id = self
            .agent_tool
            .spawn_background(prompt, tools, cwd, cancel, agent_type.as_deref(), session)
            .await;
        Ok(task_id)
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
        // `Some(mode)` — a real, rule-driven `Permission` built from the
        // caller's chosen grant (see the trait doc comment). `None` keeps
        // the prior behavior (`sub_permission()`, today `AlwaysPermit`).
        // No `permission_rules` are threaded in here (empty `RuleSet`) —
        // team members don't currently inherit the parent session's
        // configured allow/deny rules, only the coarse `PermissionMode`
        // itself; narrower per-tool rule inheritance is a possible future
        // refinement, not done here.
        let perm: Arc<dyn base::interface::permission::Permission> = match permission_mode {
            Some(mode) => Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
                Arc::new(permissions::gate::PermissionGate::new(
                    permissions::ruleset::RuleSet::new(vec![]),
                )),
                tools.clone(),
                mode.into(),
            )),
            None => self.agent_tool.sub_permission(),
        };

        self.agent_tool
            .run_sub(prompt, tools, cwd, cancel, perm, agent_type.as_deref())
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)
    }

    async fn stop_team_member(&self, team_name: &str, member_name: &str) {
        self.agent_tool
            .stop_team_member(team_name, member_name)
            .await;
    }
}
