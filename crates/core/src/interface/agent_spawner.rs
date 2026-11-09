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

/// A minimal interface for spawning sub-agents and collecting their text output.
/// The implementation in `runtime` wraps `AgentTool::run_sub` logic.
#[async_trait]
pub trait AgentSpawner: Send + Sync {
    /// Spawn a sub-agent with the given prompt and allowed tools, returning its
    /// text output (or an error).
    ///
    /// `agent_type`, when `Some` and it names a type in the runtime's agent
    /// catalog, selects which agent type to spawn as — the same catalog
    /// `AgentTool::run_sub`'s own `subagent_type` resolves against. `None`
    /// keeps the prior behavior (parent's model, no per-type overrides).
    async fn spawn_agent(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
    ) -> Result<String, Box<dyn std::error::Error + Send>>;

    /// Background variant of `spawn_agent` — used by a skill's `context:
    /// fork` + `background: true` frontmatter. Returns a task id
    /// immediately instead of awaiting the sub-agent's final text; the
    /// caller polls it via `TaskOutput`/`TaskStop`, the same mechanism the
    /// `Agent` tool's own `background: true` call argument already uses.
    /// `session` is the *parent* session the task gets registered on (and
    /// thus where `TaskOutput`/`TaskStop` will later look it up) — not the
    /// sub-agent's own, which doesn't exist yet when this is called.
    async fn spawn_agent_background(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
        session: Arc<crate::context::SessionState>,
    ) -> Result<String, Box<dyn std::error::Error + Send>>;

    /// Team-member variant of `spawn_agent`: identical execution, but
    /// `extra_tools` gets registered into the spawned sub-agent's own tool
    /// registry before it runs. `team::coordinator` uses this to give
    /// concurrently-running team members `SendMessage`/`ReadMail`/
    /// `ListPeers` tools scoped to a mailbox shared only by that stage's
    /// siblings — real-time coordination *among team members*, while
    /// they're actually running at the same time. It does not let the team
    /// *lead* participate: the lead is blocked awaiting this whole call, so
    /// it can't itself send or receive anything until the call returns (see
    /// `team::coordinator`'s module doc for why that's a deliberate limit
    /// of today's synchronous orchestration, not an oversight).
    ///
    /// `extra_tools` takes `Tool` trait objects (not, say, a `team`-crate
    /// mailbox type) specifically so this trait — which `team` depends on
    /// but which itself must not depend on `team` (see this module's own
    /// doc comment on why) — never needs to know what's inside them.
    ///
    /// Default implementation ignores `extra_tools` and delegates to
    /// `spawn_agent` — every existing spawner (Skill's `context: fork`,
    /// hook-triggered spawns, test fakes) keeps working unchanged without
    /// needing to know this method exists. Only `RuntimeAgentSpawner`
    /// overrides it for real.
    /// `permission_mode`, when `Some`, is the mode this spawn runs under
    /// (`base::interface::settings::PermissionMode`), chosen once by the
    /// caller at spawn time — see
    /// `team::coordinator::OrchestrateRequest::permission_mode`'s doc comment
    /// ("one team, one authorization"). It is held to the spawning session's
    /// mode: it can make the member stricter, never freer, and the session's
    /// rules apply to the member as to any sub-agent. `None` — like
    /// `extra_tools` being empty — delegates to `spawn_agent`'s behavior.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_agent_with_tools(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        extra_tools: Vec<Arc<dyn crate::tool::Tool>>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
        permission_mode: Option<crate::settings::PermissionMode>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        let _ = (extra_tools, permission_mode);
        self.spawn_agent(prompt, allowed_tools, cwd, cancel, agent_type)
            .await
    }

    /// Stop a persistent team member (the `Agent` tool's `team_name`+`name`
    /// mode — a member that survives across many messages, not a one-shot
    /// `spawn_agent` call) and drop its handle. `team::coordinator`'s
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
