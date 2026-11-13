//! `RuntimeAgentSpawner`: the session's `AgentSpawner`, for the consumers that
//! still ask through it (forked skills, agent hooks, a host's
//! `session.startAgent`). Every request becomes a `DelegateRequest` and
//! goes through the one dispatch, `Delegator::delegate`.

use async_trait::async_trait;
use base::interface::agent_spawner::{
    AgentRequest, AgentSpawner, SpawnMode, SpawnRequest, StartAgentError, StartedAgent,
};
use base::interface::delegate::{
    DelegateEnd, DelegateError, DelegateMode, DelegateRequest, Delegated, Delegator as _,
    StopsWith, TeamSeat, Workdir,
};
use base::interface::tool_selector::ToolSelector;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent_tool::tool_error;
use crate::delegate::Delegator;

pub struct RuntimeAgentSpawner {
    delegator: Arc<Delegator>,
}

impl RuntimeAgentSpawner {
    pub(crate) fn new(delegator: Arc<Delegator>) -> Self {
        Self { delegator }
    }
}

fn failure(message: String) -> Box<dyn std::error::Error + Send> {
    Box::new(std::io::Error::other(message))
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
        let mode = match mode {
            SpawnMode::Wait => DelegateMode::Wait,
            SpawnMode::Hook { judged_session } => DelegateMode::Hook { judged_session },
            SpawnMode::Background { .. } => DelegateMode::Background {
                notify_parent: true,
            },
            SpawnMode::TeamMember { .. } => {
                return Err(failure(
                    "a team member is delegated through `Delegator`, with its stage".into(),
                ))
            }
            _ => return Err(failure("this spawner does not know that spawn mode".into())),
        };
        let mut req = DelegateRequest::new(prompt, mode, StopsWith::Call(cancel));
        req.agent_type = agent_type;
        req.model = model;
        req.tools = ToolSelector::from_lists(&allowed_tools, &[]);
        req.workdir = Workdir::At(cwd);
        match self.delegator.delegate(req).await {
            Ok(Delegated::Finished(outcome)) => match outcome.end {
                DelegateEnd::Failed(e) => Err(failure(e)),
                DelegateEnd::Completed | DelegateEnd::Cancelled | DelegateEnd::CutShort(_) => {
                    Ok(outcome.text)
                }
            },
            Ok(Delegated::Started(started)) => Ok(started.task_id),
            Err(e) => Err(Box::new(tool_error(e))),
        }
    }

    /// Stops with `cancel`, which the host ties to the session's own life.
    async fn start_agent(
        &self,
        request: AgentRequest,
        _session: Arc<base::context::SessionState>,
        cancel: CancellationToken,
    ) -> Result<StartedAgent, StartAgentError> {
        let prompt = self
            .delegator
            .build_prompt(request.subagent_type.as_deref(), &request.prompt);
        let mode = DelegateMode::Host {
            notify_parent: request.notify_parent,
        };
        let mut req = DelegateRequest::new(prompt, mode, StopsWith::Call(cancel));
        req.description = request.description;
        req.agent_type = request.subagent_type;
        req.model = request.model;
        req.tools = ToolSelector::from_lists(&request.allowed_tools, &request.disallowed_tools);
        req.deny_rules = request.permission_rules;
        req.output_schema = request.output_schema;
        req.tag = request.tag;
        match self.delegator.delegate(req).await {
            Ok(Delegated::Started(started)) => Ok(started),
            Ok(Delegated::Finished(_)) => Err(StartAgentError::Unsupported(
                "the agent finished before it was started".into(),
            )),
            Err(DelegateError::Limit(m) | DelegateError::Depth(m)) => {
                Err(StartAgentError::Limit(m))
            }
            Err(e) => Err(StartAgentError::Unsupported(e.to_string())),
        }
    }

    async fn stop_team_member(&self, team_name: &str, member_name: &str) {
        self.delegator.stop_persistent_member(&TeamSeat {
            team: team_name.to_string(),
            member: member_name.to_string(),
        });
    }
}
