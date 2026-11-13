//! What a permit hands on: the confinement the call runs under, what is owed
//! once it has run, and the two kept for the call as its approval.

use super::{Attendance, Basis, Runs};

/// How a permitted command is confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ExecProfile {
    #[default]
    Session,
    /// Let through on its own word: held tighter than one a person approved.
    Strict,
    /// Nobody attends the session.
    Unattended,
}

/// What the caller owes once a permitted call has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Obligations {
    pub scan_nested_repos: bool,
}

impl Obligations {
    pub fn scanning_nested_repos(yes: bool) -> Self {
        Self {
            scan_nested_repos: yes,
        }
    }

    /// A command confined any looser than [`ExecProfile::Strict`] can leave
    /// behind a repository whose configuration the engine's own git would
    /// act on.
    pub fn after(runs: Option<Runs>, profile: ExecProfile) -> Self {
        Self::scanning_nested_repos(runs.is_some() && profile != ExecProfile::Strict)
    }
}

impl ExecProfile {
    /// The confinement a permitted call runs under follows from why it was
    /// let through: on a command's own word it is held tighter than one a
    /// person (or a rule, a hook, bypass) let through, and nobody attending
    /// tightens it too.
    pub fn for_permit(basis: &Basis, runs: Option<Runs>, attendance: Option<Attendance>) -> Self {
        match (basis, runs) {
            (Basis::Contained | Basis::AcceptEdits, Some(_)) => Self::Strict,
            (Basis::UnattendedConfined, _) => Self::Unattended,
            _ if attendance == Some(Attendance::Unattended) => Self::Unattended,
            _ => Self::Session,
        }
    }
}

/// What the call being run was let through on: `ToolContext::approval`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Approval {
    pub basis: Basis,
    pub profile: ExecProfile,
}

impl Approval {
    pub fn new(basis: Basis, profile: ExecProfile) -> Self {
        Self { basis, profile }
    }
}
