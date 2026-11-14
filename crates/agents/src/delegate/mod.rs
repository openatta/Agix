//! Delegation: what a session's delegates are built from, captured once when
//! the session is assembled, and the one dispatch every way of starting one
//! comes through — the model's `Agent` call, a team member, an `"agent"` hook,
//! a forked skill, a host's `session.startAgent`.
//!
//! `Delegator::delegate` matches the mode once and exhaustively. The mode
//! decides the slot, the task and what comes back; the request decides the
//! rest, the same way for every mode.

mod admission;
mod env;
mod members;
mod run;
mod tools;

use async_trait::async_trait;
use base::interface::delegate::{
    DelegateError, DelegateMode, DelegateRequest, Delegated, LiveMember, MemberLifetime, StopsWith,
    TeamSeat, Workdir,
};

pub(crate) use admission::{Admission, HookAgentRunning, HookAgents, Limits, SlotRule};
pub use admission::{SubagentSlots, DEFAULT_MAX_CONCURRENT_SUBAGENTS};
pub(crate) use env::*;
pub(crate) use members::*;
pub(crate) use tools::*;

use crate::agent_tool::{AgentCatalog, Spec};

pub(crate) struct Delegator {
    pub(crate) env: DelegateEnv,
    pub(crate) catalog: AgentCatalog,
    pub(crate) pool: ToolPool,
    pub(crate) limits: Limits,
    pub(crate) members: PersistentMembers,
    /// What a run that outlives the call — a background delegate, a
    /// persistent member — holds the delegator by.
    me: std::sync::Weak<Delegator>,
}

impl Delegator {
    pub(crate) fn new(
        env: DelegateEnv,
        catalog: AgentCatalog,
        pool: ToolPool,
        limits: Limits,
        members: PersistentMembers,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new_cyclic(|me| Self {
            env,
            catalog,
            pool,
            limits,
            members,
            me: me.clone(),
        })
    }

    fn me(&self) -> Result<std::sync::Arc<Self>, DelegateError> {
        self.me
            .upgrade()
            .ok_or_else(|| DelegateError::Unsupported("the session is closing".into()))
    }

    fn workdir(&self, workdir: Workdir) -> Result<std::path::PathBuf, DelegateError> {
        match workdir {
            Workdir::Inherit => Ok(self.env.parent.state.env().cwd.clone()),
            Workdir::At(dir) => Ok(dir),
            Workdir::Worktree(_) => Err(DelegateError::Unsupported(
                "a delegate's worktree is made by its caller, which removes it after".into(),
            )),
        }
    }
}

#[async_trait]
impl base::interface::delegate::Delegator for Delegator {
    async fn delegate(&self, req: DelegateRequest) -> Result<Delegated, DelegateError> {
        let DelegateRequest {
            prompt,
            description,
            agent_type,
            model,
            tools,
            deny_rules,
            grants,
            output_schema,
            workdir,
            stops_with,
            called_from,
            asks,
            tag,
            mode,
            ..
        } = req;
        let spec = Spec {
            session_id: base::id::Id::new().to_string(),
            agent_type,
            model,
            tools,
            output_schema,
            permission_mode: None,
            seat: None,
            cwd: self.workdir(workdir)?,
            called_from,
        };
        let stop = match stops_with {
            StopsWith::Call(token) => token.child_token(),
            StopsWith::Session => self.env.parent.ended.child_token(),
        };
        let rules = deny_rules
            .into_iter()
            .filter(|r| r.behavior == base::permission::RuleBehavior::Deny)
            .chain(grants.into_iter().map(crate::agent_tool::grant_rule))
            .collect();
        let ask = run::Ask { asks, rules };
        match mode {
            DelegateMode::Wait => self.wait(spec, prompt, description, stop, ask).await,
            DelegateMode::Background { notify_parent } => {
                let started = run::Started {
                    description,
                    tag: None,
                    notify_parent,
                };
                self.start(spec, prompt, stop, ask, started)
            }
            DelegateMode::Host { notify_parent } => {
                let started = run::Started {
                    description,
                    tag,
                    notify_parent,
                };
                self.start(spec, prompt, stop, ask, started)
            }
            DelegateMode::TeamMember {
                seat,
                permission_mode,
                lifetime: MemberLifetime::Stage { stage, extra_tools },
            } => {
                let spec = Spec {
                    permission_mode,
                    seat: Some(seat),
                    ..spec
                };
                self.stage_member(spec, prompt, stop, ask, stage, extra_tools)
                    .await
            }
            DelegateMode::TeamMember {
                seat,
                permission_mode,
                lifetime: MemberLifetime::Persistent,
            } => {
                let spec = Spec {
                    permission_mode,
                    ..spec
                };
                self.message_member(seat, spec, prompt).await
            }
            DelegateMode::Hook { judged_session } => {
                self.judge(&judged_session, spec, prompt, stop).await
            }
        }
    }

    async fn stop_member(&self, seat: &TeamSeat) {
        self.stop_persistent_member(seat);
    }

    fn live_members(&self) -> Vec<LiveMember> {
        self.live_persistent_members()
    }
}
