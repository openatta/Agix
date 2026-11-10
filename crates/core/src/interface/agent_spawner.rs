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
///
/// Implement [`spawn`](Self::spawn). The deprecated `spawn_agent*` methods
/// default to it, and its own default serves an implementation written
/// against them; one that implements neither recurses.
#[async_trait]
pub trait AgentSpawner: Send + Sync {
    /// Spawn one sub-agent. With [`SpawnMode::Wait`] or
    /// [`SpawnMode::TeamMember`] this returns its final text; with
    /// [`SpawnMode::Background`], the id of the task it runs as, at once.
    ///
    /// The default reaches the two methods every implementation used to
    /// provide, `spawn_agent` and `spawn_agent_background`, and refuses what
    /// they cannot honor — a model, extra tools, a permission mode — rather
    /// than run the sub-agent without it.
    #[allow(deprecated)]
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
        if model.is_some() {
            return Err(unsupported(
                "this spawner cannot choose a sub-agent's model",
            ));
        }
        match mode {
            SpawnMode::Wait => {
                self.spawn_agent(prompt, allowed_tools, cwd, cancel, agent_type)
                    .await
            }
            SpawnMode::Background { session } => {
                self.spawn_agent_background(prompt, allowed_tools, cwd, cancel, agent_type, session)
                    .await
            }
            SpawnMode::TeamMember {
                extra_tools,
                permission_mode,
                seat: _,
            } => {
                if !extra_tools.is_empty() || permission_mode.is_some() {
                    return Err(unsupported(
                        "this spawner cannot give a sub-agent extra tools or its own permission mode",
                    ));
                }
                self.spawn_agent(prompt, allowed_tools, cwd, cancel, agent_type)
                    .await
            }
        }
    }

    #[deprecated(note = "use `spawn` with `SpawnMode::Wait`")]
    async fn spawn_agent(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        self.spawn(SpawnRequest {
            agent_type,
            allowed_tools,
            ..SpawnRequest::new(prompt, cwd, cancel)
        })
        .await
    }

    #[deprecated(note = "use `spawn` with `model` set")]
    async fn spawn_agent_on_model(
        &self,
        prompt: String,
        cwd: PathBuf,
        cancel: CancellationToken,
        model: String,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        self.spawn(SpawnRequest {
            model: Some(model),
            ..SpawnRequest::new(prompt, cwd, cancel)
        })
        .await
    }

    #[deprecated(note = "use `spawn` with `SpawnMode::Background`")]
    async fn spawn_agent_background(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
        session: Arc<crate::context::SessionState>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        self.spawn(SpawnRequest {
            agent_type,
            allowed_tools,
            mode: SpawnMode::Background { session },
            ..SpawnRequest::new(prompt, cwd, cancel)
        })
        .await
    }

    #[deprecated(note = "use `spawn` with `SpawnMode::TeamMember`")]
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
        seat: Option<TeamSeat>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        self.spawn(SpawnRequest {
            agent_type,
            allowed_tools,
            mode: SpawnMode::TeamMember {
                extra_tools,
                permission_mode,
                seat,
            },
            ..SpawnRequest::new(prompt, cwd, cancel)
        })
        .await
    }

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

fn unsupported(message: &str) -> Box<dyn std::error::Error + Send> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        message.to_string(),
    ))
}

/// One sub-agent to spawn through [`AgentSpawner::spawn`].
#[derive(Clone)]
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
}

#[derive(Clone, Default)]
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

    /// Written against the methods that used to be required.
    struct Plain;

    #[async_trait]
    impl AgentSpawner for Plain {
        async fn spawn_agent(
            &self,
            _: String,
            _: Vec<String>,
            _: PathBuf,
            _: CancellationToken,
            _: Option<String>,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            Ok("ran".into())
        }

        async fn spawn_agent_background(
            &self,
            _: String,
            _: Vec<String>,
            _: PathBuf,
            _: CancellationToken,
            _: Option<String>,
            _: Arc<crate::context::SessionState>,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            Ok("task".into())
        }
    }

    struct SpawnOnly;

    #[async_trait]
    impl AgentSpawner for SpawnOnly {
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

    fn session() -> Arc<crate::context::SessionState> {
        Arc::new(crate::context::SessionState::new(PathBuf::from("/tmp")))
    }

    fn request(mode: SpawnMode) -> SpawnRequest {
        SpawnRequest {
            mode,
            ..SpawnRequest::new("go", PathBuf::from("/tmp"), CancellationToken::new())
        }
    }

    fn member(mode: Option<crate::settings::PermissionMode>) -> SpawnMode {
        SpawnMode::TeamMember {
            extra_tools: vec![],
            permission_mode: mode,
            seat: None,
        }
    }

    async fn spawn(on: &dyn AgentSpawner, request: SpawnRequest) -> Result<String, String> {
        on.spawn(request).await.map_err(|e| e.to_string())
    }

    #[allow(deprecated)]
    async fn with(mode: Option<crate::settings::PermissionMode>) -> Result<String, String> {
        Plain
            .spawn_agent_with_tools(
                "go".into(),
                vec![],
                vec![],
                PathBuf::from("/tmp"),
                CancellationToken::new(),
                None,
                mode,
                None,
            )
            .await
            .map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn a_spawner_without_team_support_refuses_what_it_cannot_honor() {
        assert_eq!(with(None).await, Ok("ran".into()));
        assert!(with(Some(crate::settings::PermissionMode::Plan))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn spawn_reaches_a_spawner_written_against_the_old_methods() {
        assert_eq!(
            spawn(&Plain, request(SpawnMode::Wait)).await,
            Ok("ran".into())
        );
        assert_eq!(
            spawn(
                &Plain,
                request(SpawnMode::Background { session: session() })
            )
            .await,
            Ok("task".into())
        );
        assert_eq!(spawn(&Plain, request(member(None))).await, Ok("ran".into()));
        assert!(spawn(
            &Plain,
            request(member(Some(crate::settings::PermissionMode::Plan)))
        )
        .await
        .is_err());
        let on_model = SpawnRequest {
            model: Some("m".into()),
            ..request(SpawnMode::Wait)
        };
        assert!(spawn(&Plain, on_model).await.is_err());
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn the_old_methods_reach_a_spawner_that_implements_spawn() {
        let cwd = || PathBuf::from("/tmp");
        let cancel = CancellationToken::new;
        let text =
            |r: Result<String, Box<dyn std::error::Error + Send>>| r.map_err(|e| e.to_string());
        assert_eq!(
            text(
                SpawnOnly
                    .spawn_agent("go".into(), vec![], cwd(), cancel(), Some("t".into()))
                    .await
            ),
            Ok("wait t ".into())
        );
        assert_eq!(
            text(
                SpawnOnly
                    .spawn_agent_on_model("go".into(), cwd(), cancel(), "m".into())
                    .await
            ),
            Ok("wait  m".into())
        );
        assert_eq!(
            text(
                SpawnOnly
                    .spawn_agent_background("go".into(), vec![], cwd(), cancel(), None, session())
                    .await
            ),
            Ok("background  ".into())
        );
        assert_eq!(
            text(
                SpawnOnly
                    .spawn_agent_with_tools(
                        "go".into(),
                        vec![],
                        vec![],
                        cwd(),
                        cancel(),
                        None,
                        None,
                        None
                    )
                    .await
            ),
            Ok("member  ".into())
        );
    }
}
