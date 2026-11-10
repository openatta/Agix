//! Tool authorization, in two parts: [`PermissionPolicy`] decides whether a
//! call runs, [`PermissionGrants`] keeps the allows a person or a skill hands
//! out along the way. A session holds the two together as a [`PermissionSet`].

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::context::SessionState;
use crate::interface::tool::Tool;

/// Permission check outcome.
#[derive(Debug, Clone)]
pub enum PermissionOutcome {
    /// Allowed.
    Permit,
    /// Denied with a reason.
    Deny { reason: String },
    /// Needs upper-layer decision. Engine emits `AgentEvent::PermissionPrompt`
    /// and waits for `InputMessage::PermissionResponse`.
    Prompt {
        prompt_id: String,
        message: String,
        paths: Vec<PathBuf>,
        /// The deployment's managed layer asks this. A `PreToolUse` hook's
        /// `approve` answers any other question, but not this one: only the
        /// person at the session does.
        managed: bool,
    },
}

/// One tool call put to a [`PermissionPolicy`].
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct PermissionRequest<'a> {
    pub tool_name: &'a str,
    /// The tool the call dispatches to, looked up in the registry dispatch
    /// uses. `None` is a name nothing answers to.
    pub tool: Option<&'a Arc<dyn Tool>>,
    pub input: &'a serde_json::Value,
    pub cwd: &'a Path,
    pub session_id: &'a str,
    /// The live state of the session the policy belongs to: its permission
    /// mode as it is now, what it has read.
    pub session: &'a Arc<SessionState>,
    /// Set when the call is a delegate's — a sub-agent's, a team member's —
    /// relayed to this session's policy. The call is then judged against the
    /// delegate's state, in the mode `PermissionMode::for_delegate` gives it
    /// against `session`'s.
    pub delegate: Option<&'a Arc<SessionState>>,
}

impl<'a> PermissionRequest<'a> {
    pub fn new(
        tool_name: &'a str,
        tool: Option<&'a Arc<dyn Tool>>,
        input: &'a serde_json::Value,
        cwd: &'a Path,
        session_id: &'a str,
        session: &'a Arc<SessionState>,
    ) -> Self {
        Self {
            tool_name,
            tool,
            input,
            cwd,
            session_id,
            session,
            delegate: None,
        }
    }

    pub fn for_delegate(mut self, delegate: &'a Arc<SessionState>) -> Self {
        self.delegate = Some(delegate);
        self
    }

    /// The state the call is judged against: the delegate's when there is
    /// one, the session's otherwise.
    pub fn judged(&self) -> &'a Arc<SessionState> {
        self.delegate.unwrap_or(self.session)
    }
}

/// Whether a tool call runs. The engine's own is
/// `permissions::rule_set_permission::RuleSetPermission` (rules, modes, the
/// tools' own verdicts).
#[async_trait]
pub trait PermissionPolicy: Send + Sync {
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome;
}

/// How long a [`Grant`] lasts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum GrantScope {
    /// Until the next user turn starts: a skill's `allowed-tools`.
    Turn,
    /// For the rest of the session: "always allow" answered for this session.
    Session,
    /// For this project, kept in its `settings.local.json`; a session with no
    /// project keeps it for the session.
    Project,
}

/// An allow for calls to one tool.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Grant {
    pub tool_name: String,
    /// `None` allows every call to the tool. For [`GrantScope::Turn`] this is
    /// a rule pattern, matched as a `settings.json` rule's content is; for
    /// [`GrantScope::Session`] and [`GrantScope::Project`] it is the
    /// approved call's own match content, kept literally so the allow covers
    /// that call and not whatever its text happens to match as a pattern.
    ///
    /// A grant relayed from project-controlled data — a skill's frontmatter —
    /// must not be `None`: anyone who can write a file in the repository could
    /// then switch off confirmation for `Bash` wholesale.
    pub content: Option<String>,
    pub scope: GrantScope,
}

impl Grant {
    pub fn new(tool_name: impl Into<String>, content: Option<String>, scope: GrantScope) -> Self {
        Self {
            tool_name: tool_name.into(),
            content,
            scope,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GrantError {
    /// This permission keeps no allows of its own.
    #[error("this permission keeps no grants")]
    NotKept,
    /// Kept for the session, but not written where it would outlast it.
    #[error("kept for this session only, not written to {}: {reason}", .path.display())]
    NotPersisted { path: PathBuf, reason: String },
}

/// Where the allows handed out during a session are kept.
#[async_trait]
pub trait PermissionGrants: Send + Sync {
    async fn grant(&self, grant: Grant) -> Result<(), GrantError>;
    /// Drop every grant of `scope`.
    fn clear(&self, scope: GrantScope);
}

/// Keeps nothing: [`PermissionGrants::grant`] answers
/// [`GrantError::NotKept`], so a caller can tell an allow was not kept.
pub struct NoGrants;

#[async_trait]
impl PermissionGrants for NoGrants {
    async fn grant(&self, _: Grant) -> Result<(), GrantError> {
        Err(GrantError::NotKept)
    }
    fn clear(&self, _: GrantScope) {}
}

/// A session's permission: the policy its calls are judged by and where the
/// allows it is handed are kept.
#[derive(Clone)]
pub struct PermissionSet {
    pub policy: Arc<dyn PermissionPolicy>,
    pub grants: Arc<dyn PermissionGrants>,
}

impl PermissionSet {
    /// One value that both decides and keeps grants, so what it is granted
    /// changes what it decides.
    pub fn of<P: PermissionPolicy + PermissionGrants + 'static>(p: Arc<P>) -> Self {
        Self {
            policy: p.clone(),
            grants: p,
        }
    }

    pub fn without_grants(policy: Arc<dyn PermissionPolicy>) -> Self {
        Self {
            policy,
            grants: Arc::new(NoGrants),
        }
    }
}
