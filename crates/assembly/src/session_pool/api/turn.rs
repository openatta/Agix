//! 跑一轮的请求与结局：谁发起、发给哪个会话、忙时怎么办，以及这一轮是跑完了、
//! 交给了正在跑的那一轮，还是被跳过。

use super::options::SessionOptions;
use crate::session_pool::OnBusy;

#[derive(Debug, Clone)]
pub struct TurnRequest {
    /// `None` starts a session of its own.
    pub session_id: Option<String>,
    pub message: String,
    pub attachments: Vec<runtime::agent::Attachment>,
    pub turn_id: String,
    /// Applied only when the request brings the session up.
    pub options: Option<SessionOptions>,
    pub on_busy: OnBusy,
    pub initiator: Initiator,
}

/// Who asked for a turn. A client's turn crosses no scene boundary; the
/// daemon's own turns go to sessions it already serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Initiator {
    Client,
    Schedule,
    Goal,
}

impl Initiator {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Schedule => "schedule",
            Self::Goal => "goal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnOutcome {
    /// The turn ran; `name` is the session's name after it.
    Completed {
        session_id: String,
        turn_id: String,
        name: Option<String>,
        api_calls: u32,
    },
    /// The message went to the turn already running, `into_turn_id`.
    Steered {
        session_id: String,
        turn_id: String,
        into_turn_id: Option<String>,
    },
    /// A goal round nobody wants any more.
    Skipped { session_id: String, turn_id: String },
}
