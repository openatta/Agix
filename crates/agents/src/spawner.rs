//! Runtime implementation of the AgentSpawner trait.
//!
//! Wraps `AgentTool`'s delegate runs to provide sub-agent spawning to
//! consumers (the team coordinator, Skill fork, agent hooks) without creating
//! a circular dependency. It also serves `base::interface::delegate::Delegator`
//! by mapping each mode onto the same runs, until the one dispatch replaces
//! both.

use async_trait::async_trait;
use base::interface::agent_spawner::{
    AgentRequest, AgentSpawner, SpawnMode, SpawnRequest, StartAgentError,
};
use base::interface::delegate::{
    AskRoute, DelegateEnd, DelegateError, DelegateMode, DelegateOutcome, DelegateRequest,
    Delegated, Delegator, MemberLifetime, StartedAgent, StopsWith, TeamSeat, Workdir,
};
use base::interface::tool_selector::ToolSelector;
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
            ..
        } = request;
        let tools = self
            .agent_tool
            .delegate_tools(agent_type.as_deref(), &allowed_tools, &[]);
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
                        allowed_tools,
                    )
                    .await
            }
            SpawnMode::Hook { judged_session } => {
                self.agent_tool
                    .run_hook_agent(
                        &judged_session,
                        prompt,
                        tools,
                        cwd,
                        cancel,
                        agent_type.as_deref(),
                        model,
                        allowed_tools,
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
                        allowed_tools,
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
                self.agent_tool
                    .run_team_member(
                        prompt,
                        tools,
                        cwd,
                        cancel,
                        agent_type.as_deref(),
                        model,
                        permission_mode,
                        seat,
                    )
                    .await
            }
            _ => {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "this spawner does not know that spawn mode",
                )))
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

fn outcome(ran: Result<String, base::error::ToolError>) -> Delegated {
    let (text, end) = match ran {
        Ok(text) => (text, DelegateEnd::Completed),
        Err(e) => (String::new(), DelegateEnd::Failed(e.to_string())),
    };
    Delegated::Finished(DelegateOutcome {
        text,
        end,
        structured: None,
        session_id: String::new(),
    })
}

fn started(task_id: Result<String, base::error::ToolError>) -> Result<Delegated, DelegateError> {
    task_id
        .map(|task_id| {
            Delegated::Started(StartedAgent {
                task_id,
                agent_session_id: String::new(),
            })
        })
        .map_err(|e| DelegateError::Build(e.to_string()))
}

/// What every mode runs with, resolved from the request.
struct Run {
    prompt: String,
    agent_type: Option<String>,
    model: Option<String>,
    cwd: std::path::PathBuf,
    cancel: CancellationToken,
    tools: Arc<base::tool::InMemoryToolRegistry>,
    parent: Arc<base::context::SessionState>,
}

impl Run {
    fn parent(&self) -> Result<Arc<base::context::SessionState>, DelegateError> {
        Ok(self.parent.clone())
    }
}

impl RuntimeAgentSpawner {
    fn prepare(
        &self,
        prompt: String,
        agent_type: Option<String>,
        model: Option<String>,
        workdir: Workdir,
        stops_with: StopsWith,
    ) -> Result<Run, DelegateError> {
        let parent = self.agent_tool.parent_session_state();
        let cwd = match workdir {
            Workdir::At(dir) => dir,
            Workdir::Inherit => parent.cwd.clone(),
            Workdir::Worktree(_) => {
                return Err(DelegateError::Unsupported(
                    "a delegate in a worktree of its own is not available yet".into(),
                ))
            }
        };
        let cancel = match stops_with {
            StopsWith::Call(token) => token,
            StopsWith::Session => self.agent_tool.parent_session_ended(),
        };
        let tools = self
            .agent_tool
            .delegate_tools(agent_type.as_deref(), &[], &[]);
        Ok(Run {
            prompt,
            agent_type,
            model,
            cwd,
            cancel,
            tools,
            parent,
        })
    }

    async fn wait(&self, run: Run, asks: AskRoute) -> Delegated {
        let perm = match asks {
            AskRoute::Host => self.agent_tool.sub_permission(),
            AskRoute::Refuse => self.agent_tool.sub_permission_refusing_questions(),
        };
        let ran = self
            .agent_tool
            .run_sub(
                run.prompt,
                run.tools,
                run.cwd,
                run.cancel,
                perm,
                run.agent_type.as_deref(),
                run.model,
                Vec::new(),
            )
            .await;
        outcome(ran)
    }

    async fn judge(&self, run: Run, judged_session: &str) -> Delegated {
        let ran = self
            .agent_tool
            .run_hook_agent(
                judged_session,
                run.prompt,
                run.tools,
                run.cwd,
                run.cancel,
                run.agent_type.as_deref(),
                run.model,
                Vec::new(),
            )
            .await;
        outcome(ran)
    }

    async fn background(&self, run: Run) -> Result<Delegated, DelegateError> {
        let session = run.parent()?;
        let ran = self
            .agent_tool
            .spawn_background(
                run.prompt,
                run.tools,
                run.cwd,
                run.cancel,
                run.agent_type.as_deref(),
                run.model,
                Vec::new(),
                session,
            )
            .await;
        started(ran)
    }

    fn host(&self, run: Run, request: AgentRequest) -> Result<Delegated, DelegateError> {
        self.agent_tool
            .start_for_host(request, run.parent()?, run.cancel)
            .map(Delegated::Started)
            .map_err(|e| match e {
                StartAgentError::Limit(m) => DelegateError::Limit(m),
                StartAgentError::Unsupported(m) => DelegateError::Unsupported(m),
            })
    }

    async fn member(
        &self,
        run: Run,
        seat: TeamSeat,
        permission_mode: Option<base::settings::PermissionMode>,
        lifetime: MemberLifetime,
    ) -> Result<Delegated, DelegateError> {
        match lifetime {
            MemberLifetime::Stage { extra_tools } => {
                for t in extra_tools {
                    run.tools.register(t);
                }
                let ran = self
                    .agent_tool
                    .run_team_member(
                        run.prompt,
                        run.tools,
                        run.cwd,
                        run.cancel,
                        run.agent_type.as_deref(),
                        run.model,
                        permission_mode,
                        Some(seat),
                    )
                    .await;
                Ok(outcome(ran))
            }
            MemberLifetime::Persistent => {
                let session = run.parent()?;
                let ran = self
                    .agent_tool
                    .spawn_or_message_team_member(
                        seat.team,
                        seat.member,
                        run.prompt,
                        run.agent_type,
                        run.cwd,
                        session,
                        permission_mode,
                    )
                    .await;
                started(ran)
            }
        }
    }
}

#[async_trait]
impl Delegator for RuntimeAgentSpawner {
    /// The runs it maps onto take tool lists, not a selector, and do not say
    /// how a run ended: a narrowed `tools` is refused rather than dropped, a
    /// run that fails is `Failed` with its error, every other end is
    /// `Completed`, and `grants` and `called_from` are not carried.
    async fn delegate(&self, req: DelegateRequest) -> Result<Delegated, DelegateError> {
        let DelegateRequest {
            prompt,
            description,
            agent_type,
            model,
            tools,
            deny_rules,
            output_schema,
            workdir,
            stops_with,
            asks,
            tag,
            mode,
            ..
        } = req;
        if tools != ToolSelector::all() {
            return Err(DelegateError::Unsupported(
                "narrowing a delegate's tools by selector is not available yet".into(),
            ));
        }
        let request_type = agent_type.clone();
        let request_model = model.clone();
        let run = self.prepare(prompt, agent_type, model, workdir, stops_with)?;
        match mode {
            DelegateMode::Wait => Ok(self.wait(run, asks).await),
            DelegateMode::Hook { judged_session } => Ok(self.judge(run, &judged_session).await),
            DelegateMode::Background { .. } => self.background(run).await,
            DelegateMode::Host { notify_parent } => {
                let request = AgentRequest {
                    prompt: run.prompt.clone(),
                    description,
                    subagent_type: request_type,
                    model: request_model,
                    allowed_tools: Vec::new(),
                    disallowed_tools: Vec::new(),
                    permission_rules: deny_rules,
                    output_schema,
                    notify_parent,
                    tag,
                };
                self.host(run, request)
            }
            DelegateMode::TeamMember {
                seat,
                permission_mode,
                lifetime,
            } => self.member(run, seat, permission_mode, lifetime).await,
            _ => Err(DelegateError::Unsupported(
                "this delegator does not know that mode".into(),
            )),
        }
    }

    async fn stop_member(&self, seat: &TeamSeat) {
        self.agent_tool
            .stop_team_member(&seat.team, &seat.member)
            .await;
    }
}
