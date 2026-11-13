//! Delegation: one request type for every way a session starts a sub-agent —
//! the model's `Agent` call, a team member, an `"agent"` hook, a forked skill,
//! a host's `session.startAgent` — and the one trait that serves it. The mode
//! says how the delegate runs; everything else about it is in the request.
//!
//! The implementation lives in `agents`; this sits below `team`, `skills` and
//! `hooks`, which consume it.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::permission::Grant;
use super::tool::Tool;
use super::tool_selector::ToolSelector;
use crate::permission::PermissionRule;
use crate::settings::PermissionMode;

#[non_exhaustive]
pub struct DelegateRequest {
    pub prompt: String,
    pub description: Option<String>,
    pub agent_type: Option<String>,
    /// Over the agent type's model, or the parent's.
    pub model: Option<String>,
    /// Narrows what the delegate would otherwise have; never widens it.
    pub tools: ToolSelector,
    /// Only their `deny` takes effect: a delegate can be refused more than
    /// its parent, never permitted more.
    pub deny_rules: Vec<PermissionRule>,
    /// Turn grants that hold in the delegate's own session and are not
    /// handed back to the parent.
    pub grants: Vec<Grant>,
    pub output_schema: Option<Value>,
    pub workdir: Workdir,
    pub stops_with: StopsWith,
    pub called_from: Option<CallSite>,
    pub asks: AskRoute,
    /// The caller's own label, carried on the delegate's task untouched.
    pub tag: Option<String>,
    pub mode: DelegateMode,
}

impl DelegateRequest {
    /// `prompt` run in `mode` until `stops_with` says stop, with nothing else
    /// narrowed: the parent's tools and working directory, questions asked
    /// of the host.
    pub fn new(prompt: impl Into<String>, mode: DelegateMode, stops_with: StopsWith) -> Self {
        Self {
            prompt: prompt.into(),
            description: None,
            agent_type: None,
            model: None,
            tools: ToolSelector::all(),
            deny_rules: Vec::new(),
            grants: Vec::new(),
            output_schema: None,
            workdir: Workdir::Inherit,
            stops_with,
            called_from: None,
            asks: AskRoute::Host,
            tag: None,
            mode,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workdir {
    Inherit,
    /// A git worktree of the parent's project, by name.
    Worktree(String),
    At(PathBuf),
}

#[derive(Debug, Clone)]
pub enum StopsWith {
    /// The caller's token: the delegate stops on a child of it.
    Call(CancellationToken),
    /// The parent session's end.
    Session,
}

/// The parent's tool call that started the delegate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub turn: u32,
    pub tool_use_id: String,
}

/// Where a question the delegate's permission leaves open goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskRoute {
    Host,
    /// Refused instead: a program's delegate has nobody to ask.
    Refuse,
}

/// Matched exhaustively by the one dispatch in `agents`, across crates, so a
/// new mode cannot compile without its rules.
pub enum DelegateMode {
    /// Run it and wait for its answer.
    Wait,
    /// A task of the parent session, started at once.
    Background { notify_parent: bool },
    TeamMember {
        seat: TeamSeat,
        /// Held to the parent's mode: it can make the member stricter, never
        /// freer. `None` keeps the parent's.
        permission_mode: Option<PermissionMode>,
        lifetime: MemberLifetime,
    },
    /// What a host asked for, as a task of the parent session.
    Host { notify_parent: bool },
    /// The judge for an `"agent"` hook on an event of `judged_session`.
    Hook { judged_session: String },
}

pub enum MemberLifetime {
    /// One stage of a team, by name; `extra_tools` go to this member's
    /// registry only.
    Stage {
        stage: String,
        extra_tools: Vec<Arc<dyn Tool>>,
    },
    /// Kept across messages until stopped.
    Persistent,
}

pub enum Delegated {
    Finished(DelegateOutcome),
    Started(StartedAgent),
}

/// Which team a member works in, and its name there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamSeat {
    pub team: String,
    pub member: String,
}

#[derive(Debug, Clone)]
pub struct StartedAgent {
    pub task_id: String,
    /// The sidechain session the agent runs in.
    pub agent_session_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DelegateOutcome {
    pub text: String,
    pub end: DelegateEnd,
    pub structured: Option<Value>,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateEnd {
    Completed,
    Cancelled,
    Failed(String),
    /// Ended by a ceiling before it finished — `max_turns`, say.
    CutShort(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DelegateError {
    /// As many delegates are running as the session allows.
    #[error("{0}")]
    Limit(String),
    /// As deep as delegation goes.
    #[error("{0}")]
    Depth(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Build(String),
    #[error("{0}")]
    Invalid(String),
}

#[async_trait]
pub trait Delegator: Send + Sync {
    async fn delegate(&self, req: DelegateRequest) -> Result<Delegated, DelegateError>;

    /// Stop a persistent member; nothing when there is no such member.
    async fn stop_member(&self, seat: &TeamSeat);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl Delegator for Echo {
        async fn delegate(&self, req: DelegateRequest) -> Result<Delegated, DelegateError> {
            match req.mode {
                DelegateMode::Wait | DelegateMode::Hook { .. } => {
                    Ok(Delegated::Finished(DelegateOutcome {
                        text: req.prompt,
                        end: DelegateEnd::Completed,
                        structured: None,
                        session_id: "s".into(),
                    }))
                }
                DelegateMode::Background { .. }
                | DelegateMode::Host { .. }
                | DelegateMode::TeamMember { .. } => {
                    Err(DelegateError::Unsupported("echo only waits".into()))
                }
            }
        }

        async fn stop_member(&self, _: &TeamSeat) {}
    }

    #[test]
    fn a_new_request_narrows_nothing() {
        let req = DelegateRequest::new("go", DelegateMode::Wait, StopsWith::Session);
        assert_eq!(req.tools, ToolSelector::all());
        assert!(req.deny_rules.is_empty() && req.grants.is_empty());
        assert_eq!(req.workdir, Workdir::Inherit);
        assert_eq!(req.asks, AskRoute::Host);
        assert!(req.called_from.is_none() && req.tag.is_none());
    }

    #[tokio::test]
    async fn a_delegator_answers_by_mode() {
        let waited = Echo
            .delegate(DelegateRequest::new(
                "hello",
                DelegateMode::Wait,
                StopsWith::Call(CancellationToken::new()),
            ))
            .await;
        match waited {
            Ok(Delegated::Finished(outcome)) => {
                assert_eq!(outcome.text, "hello");
                assert_eq!(outcome.end, DelegateEnd::Completed);
            }
            _ => panic!("a waited delegate finishes"),
        }
        let refused = Echo
            .delegate(DelegateRequest::new(
                "x",
                DelegateMode::Background {
                    notify_parent: true,
                },
                StopsWith::Session,
            ))
            .await;
        assert!(matches!(refused, Err(DelegateError::Unsupported(m)) if m == "echo only waits"));
    }
}
