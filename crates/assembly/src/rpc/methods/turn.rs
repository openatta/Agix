//! 跑一轮与上下文方法（`session.run_turn` / `compact` / `clear` /
//! `respondToPrompt` / `inspectContext` / `dropMessages` / `editMessage`）的参数
//! 与线上回执。

use super::lenient_vec;
use crate::rpc::RawOptions;
use crate::session_pool::api::{AskKind, TurnOutcome};
use crate::session_pool::OnBusy;
use runtime::agent::{Attachment, PermissionDecision, QuestionAnswer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RunTurnParams {
    /// Omitted: a new session.
    #[serde(default)]
    pub session_id: Option<String>,
    pub message: String,
    /// Omitted: the daemon makes one up.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// An entry that does not read is dropped, not the turn.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub options: Option<RawOptions>,
    #[serde(default)]
    pub on_busy: Option<OnBusy>,
}

/// How a turn went, as `session.run_turn`, `session.compact` and
/// `session.clear` answer it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum TurnReply {
    Turn {
        session_id: String,
        turn_id: String,
        delivery: Delivery,
        /// The session's name after the turn.
        name: Option<String>,
        api_calls: u32,
    },
    Steer {
        session_id: String,
        turn_id: String,
        delivery: Delivery,
        into_turn_id: Option<String>,
    },
    Skipped {
        session_id: String,
        turn_id: String,
        skipped: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// The message ran a turn of its own.
    Turn,
    /// The message went to the turn already running.
    Steer,
}

impl From<TurnOutcome> for TurnReply {
    fn from(outcome: TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Completed {
                session_id,
                turn_id,
                name,
                api_calls,
            } => Self::Turn {
                session_id,
                turn_id,
                delivery: Delivery::Turn,
                name,
                api_calls,
            },
            TurnOutcome::Steered {
                session_id,
                turn_id,
                into_turn_id,
            } => Self::Steer {
                session_id,
                turn_id,
                delivery: Delivery::Steer,
                into_turn_id,
            },
            TurnOutcome::Skipped {
                session_id,
                turn_id,
            } => Self::Skipped {
                session_id,
                turn_id,
                skipped: true,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RespondToPromptParams {
    pub session_id: String,
    pub prompt_id: String,
    /// The `prompt_type` of the frame answered; omitted: `permission`.
    #[serde(default)]
    pub prompt_type: Option<AskKind>,
    /// The answer to a `permission` prompt.
    #[serde(default)]
    pub decision: Option<PermissionDecision>,
    /// The answer to a `question`.
    #[serde(default)]
    pub answer: Option<QuestionAnswer>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PromptAnswered {
    pub prompt_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DropMessagesParams {
    pub session_id: String,
    /// Positions, in `session.history`'s coordinates.
    pub at: Vec<usize>,
    /// How many messages the caller saw; refused when the context has since
    /// changed.
    #[serde(default)]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Dropped {
    pub dropped: Vec<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditMessageParams {
    pub session_id: String,
    pub at: usize,
    pub text: String,
    #[serde(default)]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Edited {
    pub edited: usize,
}
