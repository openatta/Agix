//! Agent spawner trait — breaks the circular dependency between team and runtime crates.
//!
//! The `team` crate needs to spawn sub-agents during orchestration but `runtime` (which
//! contains the Agent/AgentTool logic) already depends on `team`. This trait lives in `base`
//! (below both) so `runtime` can implement it and `team` can consume it without creating
//! a cycle.
//!
//! The tool pool is assembled by the caller and passed in as a parameter,
//! avoiding circular imports.

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// How sub-agents are spawned and their text collected. The implementation
/// lives in the `agents` crate (`RuntimeAgentSpawner`).
#[async_trait]
pub trait AgentSpawner: Send + Sync {
    /// Spawn one sub-agent. With [`SpawnMode::Wait`] or
    /// [`SpawnMode::TeamMember`] this returns its final text; with
    /// [`SpawnMode::Background`], the id of the task it runs as, at once.
    async fn spawn(
        &self,
        request: SpawnRequest,
    ) -> Result<String, Box<dyn std::error::Error + Send>>;

    /// Stop a persistent team member (the `Agent` tool's `team_name`+`name`
    /// mode — a member that survives across many messages, not a one-shot
    /// spawn) and drop its handle. `team::coordinator`'s
    /// `TeamDelete` cleanup calls this once per member it has on record.
    ///
    /// No-op if the named member doesn't exist — matches `TaskStop`'s
    /// tolerance for stale ids; "does this member exist" is `TeamList`'s
    /// job, not this one's. Default implementation is a no-op — spawners
    /// that don't support persistent members (Skill fork, hooks, test
    /// fakes) have nothing to stop. Only `RuntimeAgentSpawner` overrides it
    /// for real.
    async fn stop_team_member(&self, team_name: &str, member_name: &str) {
        let _ = (team_name, member_name);
    }

    /// Start a sub-agent a host asked for, in the background, as a task of
    /// `session` — the way the model's `Agent` call with `background: true`
    /// starts one, but with nobody's turn behind it. It stops when `cancel`
    /// does, which the host ties to the session's own lifetime.
    async fn start_agent(
        &self,
        request: AgentRequest,
        session: Arc<crate::context::SessionState>,
        cancel: CancellationToken,
    ) -> Result<StartedAgent, StartAgentError> {
        let _ = (request, session, cancel);
        Err(StartAgentError::Unsupported(
            "this session cannot start sub-agents".into(),
        ))
    }
}

/// One sub-agent to spawn through [`AgentSpawner::spawn`].
#[derive(Clone)]
#[non_exhaustive]
pub struct SpawnRequest {
    pub prompt: String,
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    /// A type in the runtime's agent catalog to spawn as; `None` runs on the
    /// parent's model with no per-type overrides.
    pub agent_type: Option<String>,
    /// Over the agent type's model, or the parent's.
    pub model: Option<String>,
    /// Narrows the tools the sub-agent inherits; empty keeps them all.
    pub allowed_tools: Vec<String>,
    pub mode: SpawnMode,
}

impl SpawnRequest {
    pub fn new(prompt: impl Into<String>, cwd: PathBuf, cancel: CancellationToken) -> Self {
        Self {
            prompt: prompt.into(),
            cwd,
            cancel,
            agent_type: None,
            model: None,
            allowed_tools: Vec::new(),
            mode: SpawnMode::Wait,
        }
    }

    pub fn agent_type(mut self, agent_type: Option<String>) -> Self {
        self.agent_type = agent_type;
        self
    }

    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    pub fn allowed_tools(mut self, allowed_tools: Vec<String>) -> Self {
        self.allowed_tools = allowed_tools;
        self
    }

    pub fn mode(mut self, mode: SpawnMode) -> Self {
        self.mode = mode;
        self
    }
}

#[derive(Clone, Default)]
#[non_exhaustive]
pub enum SpawnMode {
    /// Run it and wait for its final text.
    #[default]
    Wait,
    /// Run it as a task of `session` — the *parent* session, where
    /// `TaskOutput` / `TaskStop` find it — and return the task id at once.
    Background {
        session: Arc<crate::context::SessionState>,
    },
    /// Run it as a team member and wait for its final text.
    TeamMember {
        /// Registered into this member's own tool registry only — how a
        /// stage's members get mailbox tools scoped to their siblings. `Tool`
        /// objects rather than a `team` type, since this crate sits below
        /// `team`.
        extra_tools: Vec<Arc<dyn crate::tool::Tool>>,
        /// Chosen once for the whole team; held to the spawning session's
        /// mode, so it can make the member stricter, never freer. `None`
        /// keeps the spawning session's.
        permission_mode: Option<crate::settings::PermissionMode>,
        /// Names the member for its `TaskCompleted` / `TeammateIdle` hooks.
        seat: Option<TeamSeat>,
    },
}

/// Which team a member works in, and its name there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamSeat {
    pub team: String,
    pub member: String,
}

/// A sub-agent a host starts under a session (`session.startAgent`).
#[derive(Debug, Clone, Default)]
pub struct AgentRequest {
    pub prompt: String,
    pub description: Option<String>,
    pub subagent_type: Option<String>,
    /// Over the agent type's model.
    pub model: Option<String>,
    /// Narrow what the agent type already allows; they never widen it.
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// Only their `deny` rules take effect: a host-started agent is held to
    /// its parent's permissions and can only be refused more.
    pub permission_rules: Vec<crate::permission::PermissionRule>,
    pub output_schema: Option<serde_json::Value>,
    /// Whether the parent hears of it ending as a `<task-notification>`.
    pub notify_parent: bool,
    /// The host's own label, carried on the task untouched.
    pub tag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StartedAgent {
    pub task_id: String,
    /// The sidechain session the agent runs in.
    pub agent_session_id: String,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum StartAgentError {
    /// The session is running as many sub-agents as it may, or is as deep
    /// as delegation goes.
    #[error("{0}")]
    Limit(String),
    #[error("{0}")]
    Unsupported(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl AgentSpawner for Echo {
        async fn spawn(
            &self,
            request: SpawnRequest,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            let mode = match request.mode {
                SpawnMode::Wait => "wait",
                SpawnMode::Background { .. } => "background",
                SpawnMode::TeamMember { .. } => "member",
            };
            Ok(format!(
                "{mode} {} {}",
                request.agent_type.unwrap_or_default(),
                request.model.unwrap_or_default()
            ))
        }
    }

    #[tokio::test]
    async fn spawn_carries_the_request_to_the_implementation() {
        let request = SpawnRequest::new("go", PathBuf::from("/tmp"), CancellationToken::new())
            .agent_type(Some("t".into()))
            .model(Some("m".into()));
        assert_eq!(
            Echo.spawn(request).await.map_err(|e| e.to_string()),
            Ok("wait t m".into())
        );
    }
}
