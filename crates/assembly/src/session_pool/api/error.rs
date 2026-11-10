//! 池操作失败的唯一错误类型 `PoolError`。变体按领域含义划分，不带错误码；
//! 错误码和 `error.data` 的形状由 `server/errors.rs` 一处决定。`Display` 就是
//! 线上的 `message`。

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PoolError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{message}")]
    SessionNotFound { session_id: String, message: String },
    #[error("{}", .0.message)]
    SessionBusy(Busy),
    #[error("unknown scene: {scene}")]
    SceneNotFound { scene: String },
    #[error("scene is not active on this daemon: {scene}")]
    SceneNotActive { scene: String },
    #[error("scene `{scene}` was not added by scene.register")]
    SceneNotRegistered { scene: String },
    #[error("scene mismatch")]
    SceneMismatch {
        session_id: String,
        recorded: String,
        requested: String,
    },
    #[error("scene {scene} has active sessions: {}", .sessions.join(", "))]
    SceneHasActiveSessions {
        scene: String,
        sessions: Vec<String>,
    },
    #[error("project_root does not exist or is not a directory: {}", .path.display())]
    ProjectNotFound { path: PathBuf },
    #[error("scene `{scene}` requires a project; pass project_root")]
    ProjectRequired { scene: String },
    #[error("{0}")]
    ProjectUntrusted(String),
    #[error("sidechain session already ran to completion")]
    SidechainTerminal {
        session_id: String,
        parent_session_id: Option<String>,
        final_state: history::entry::SessionEndState,
    },
    #[error("session `{session_id}` has no task `{task_id}`")]
    TaskNotFound { session_id: String, task_id: String },
    #[error("from_byte is past the end of task `{task_id}`'s output; next_byte is {next_byte}")]
    OutputOutOfRange { task_id: String, next_byte: u64 },
    #[error("{0}")]
    TaskLimit(String),
    /// The session's spawner cannot start a sub-agent on a host's behalf.
    #[error("{0}")]
    SpawnUnsupported(String),
    #[error("{0}")]
    Engine(EngineFailure),
    #[error("engine stopped before the turn completed")]
    EngineStopped,
    /// The engine did not take or answer a request outside a turn.
    #[error("{0}")]
    EngineUnreachable(String),
    #[error("{0}")]
    McpServerDenied(String),
    #[error("{0}")]
    McpAuthFailed(String),
    #[error("plugin subsystem unavailable ({status})")]
    PluginsDisabled { status: &'static str },
    #[error("`{method}` belongs to the `{module}` module, which this build does not include")]
    ModuleNotCompiled {
        method: String,
        module: &'static str,
    },
    #[error("{0}")]
    Internal(String),
}

impl PoolError {
    pub(crate) fn session_not_found(session_id: &str) -> Self {
        Self::session_gone(session_id, format!("session not found: {session_id}"))
    }

    pub(crate) fn no_session(session_id: &str) -> Self {
        Self::session_gone(session_id, format!("no session `{session_id}`"))
    }

    pub(crate) fn session_gone(session_id: &str, message: impl Into<String>) -> Self {
        Self::SessionNotFound {
            session_id: session_id.to_string(),
            message: message.into(),
        }
    }
}

/// What the session is doing that keeps it from taking the request — the
/// session's state, not which method was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BusyReason {
    TurnRunning,
    Rebuilding,
    BackgroundTasks,
    PreviousEngine,
}

/// Why a session cannot take what was asked of it right now.
#[derive(Debug, Clone)]
pub struct Busy {
    pub session_id: String,
    pub reason: BusyReason,
    /// Set exactly when `reason` is `TurnRunning`.
    pub current_turn_id: Option<String>,
    pub running_task_ids: Vec<String>,
    pub message: String,
}

impl Busy {
    const MESSAGE: &'static str = "session is busy";

    fn new(session_id: &str, reason: BusyReason, message: String) -> Self {
        Self {
            session_id: session_id.to_string(),
            reason,
            current_turn_id: None,
            running_task_ids: Vec::new(),
            message,
        }
    }

    pub(crate) fn turn_running(session_id: &str, turn_id: &str) -> Self {
        Self {
            current_turn_id: Some(turn_id.to_string()),
            ..Self::new(session_id, BusyReason::TurnRunning, Self::MESSAGE.into())
        }
    }

    pub(crate) fn rebuilding(session_id: &str) -> Self {
        Self::new(session_id, BusyReason::Rebuilding, Self::MESSAGE.into())
    }

    pub(crate) fn previous_engine(session_id: &str, message: impl Into<String>) -> Self {
        Self::new(session_id, BusyReason::PreviousEngine, message.into())
    }

    pub(crate) fn background_tasks(session_id: &str, running_task_ids: Vec<String>) -> Self {
        Self {
            running_task_ids,
            ..Self::new(
                session_id,
                BusyReason::BackgroundTasks,
                Self::MESSAGE.into(),
            )
        }
    }
}

/// A turn the engine ended with an error of its own: its code and message.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code}: {message}")]
pub struct EngineFailure {
    pub code: String,
    pub message: String,
}
