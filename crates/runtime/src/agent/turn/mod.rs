//! Turn loop —— 用户消息 → 流式事件 → TurnOutcome。
//!
//! 在 agent 的协议无关类型之上的核心处理逻辑；按职责分成子模块，本文件留回合的公共类型与对外再导出。

mod attachments;
mod compact;
mod gate;
mod memory_extraction;
mod obligations;
mod process;
mod prompt;
mod recall;
mod reporting;
mod request;
mod round;
mod token_budget;
mod tool_env;
mod tool_exec;
mod tool_permission;
mod tool_progress;
mod tool_result;
mod user_turn;

#[cfg(test)]
mod tests;

pub(crate) use compact::Compaction;
pub(crate) use gate::{DemuxHandles, Gate};
pub(crate) use memory_extraction::MEMORY_EXTRACTION_GRACE;
pub(crate) use process::hook_permission_mode;
pub(crate) use prompt::PromptContext;
pub(crate) use recall::Memory;
pub(crate) use request::ModelRoute;
pub(crate) use tool_env::{collect_frozen, engine_exec_from, exec_providers_from};
pub(crate) use tool_exec::resolve_permission_response;

use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage, Usage};

/// What one user turn knows about itself; replaced as the turn begins.
#[derive(Default)]
pub(crate) struct TurnState {
    /// When the turn's user message was pushed. Compaction moves message
    /// indices; the time a message was pushed survives it.
    pub(crate) started_at: Option<std::time::Instant>,
    /// The turn's user message, identified by a fingerprint of its text
    /// rather than by index because compaction rewrites the message list
    /// before the request is assembled.
    pub(crate) pending_input: Option<(u64, Vec<base::interface::model::InputSpan>)>,
    pub(crate) last_call_usage: Option<Usage>,
    pub(crate) truncated: bool,
    pub(crate) scope: std::sync::Arc<base::tool::TurnScope>,
}

impl TurnState {
    pub(crate) fn opening(turn_id: &str) -> Self {
        Self {
            scope: std::sync::Arc::new(base::tool::TurnScope::new(
                base::context::goal::TurnOrigin::from_turn_id(turn_id),
            )),
            ..Self::default()
        }
    }
}

/// Why a compaction runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactTrigger {
    /// The context crossed the budget's threshold.
    Auto,
    /// Somebody asked: `/compact`, `EngineCommand::CompactNow`,
    /// [`Agent::compact_now`].
    Manual,
    /// The provider refused the request for its size.
    Recovery,
}

impl CompactTrigger {
    /// What `PreCompact` hooks are told. A size refusal is an automatic
    /// compaction as far as a hook is concerned; the two values are the ones
    /// hooks written for Claude Code match on.
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto | Self::Recovery => "auto",
            Self::Manual => "manual",
        }
    }
}

/// Why a compaction left the conversation as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotCompacted {
    /// A `PreCompact` hook declined it.
    Declined,
    /// Nothing could be dropped, or recovery would have cost what was.
    NothingToDrop,
    /// The compactor failed.
    Failed(String),
    /// The LLM summary failed and the compactor's cascade could not stand
    /// in for it.
    SummaryFailed(base::interface::model::ModelError),
    /// The turn was interrupted while the summary was being written.
    Cancelled,
}

impl std::fmt::Display for NotCompacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Declined => f.write_str("was declined by a PreCompact hook"),
            Self::NothingToDrop => f.write_str("saved nothing"),
            Self::Failed(e) => write!(f, "failed: {e}"),
            Self::SummaryFailed(e) => write!(f, "failed: summarization failed: {e}"),
            Self::Cancelled => f.write_str("was interrupted"),
        }
    }
}

/// What one compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactSummary {
    pub strategy: String,
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
}

/// Turn endings that leave a goal armed. Anything else — a cancel, a hook
/// veto, an exhausted budget, an error — stops its automatic rounds.
const ENDS_A_GOAL_ROUND_NORMALLY: &[&str] = &[
    "end_turn",
    "stop_sequence",
    "max_tokens",
    "command_executed",
    "tool_result_received",
];

/// Turn endings after which an armed goal starts its next round.
pub const CONTINUES_A_GOAL: &[&str] = &["end_turn", "stop_sequence", "max_tokens"];

pub(crate) const SESSION_CONTEXT_OPENING: &str =
    "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n";

/// Whether `messages` still hold the conversation's opening context message.
pub(crate) fn holds_session_context(messages: &[ModelMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == MessageRole::User
            && m.content.iter().any(|block| {
                matches!(block, ModelContentBlock::Text { text } if text.starts_with(SESSION_CONTEXT_OPENING))
            })
    })
}

#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub stop_reason: String,
    pub api_calls: u32,
    pub tool_calls: u32,
    /// What the **last** model call of this turn reported, and zero on the
    /// paths that end a turn without one.
    ///
    /// Kept as it is because hosts read it. It is not what its name suggests
    /// and never was — see [`Self::total_usage`], which is. The two will be
    /// reconciled at a version boundary rather than by changing what this one
    /// means underneath somebody.
    pub usage: Usage,
    /// What every model call in this turn reported, added up.
    ///
    /// The number to budget on: a turn is several calls, and one of them is a
    /// fraction of the answer. A host that capped spending on `usage` was
    /// capping on whatever the last round trip happened to cost.
    pub total_usage: Usage,
    /// Why the turn was stopped, when whatever stopped it said so.
    pub message: Option<String>,
}

impl TurnOutcome {
    fn stopped(stop_reason: impl Into<String>) -> Self {
        Self {
            stop_reason: stop_reason.into(),
            ..Self::default()
        }
    }

    fn ended(
        stop_reason: impl Into<String>,
        api_calls: u32,
        tool_calls: u32,
        usage: Usage,
        total_usage: Usage,
        message: Option<String>,
    ) -> Self {
        Self {
            stop_reason: stop_reason.into(),
            api_calls,
            tool_calls,
            usage,
            total_usage,
            message,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// A model call failed and was not recovered; the flag says a tool had
    /// already run in this turn.
    #[error("model error: {0}")]
    Model(base::interface::model::ModelError, bool),
    #[error("shutdown")]
    Shutdown,
    #[error("internal: {0}")]
    Internal(String),
}

impl TurnError {
    pub(crate) fn after_tools(self, tools_ran: bool) -> Self {
        match self {
            Self::Model(error, ran) => Self::Model(error, ran || tools_ran),
            other => other,
        }
    }

    /// The failure as the engine reports it.
    pub fn failure(&self) -> base::event::TurnFailure {
        use base::event::{FailureKind, TurnFailure};
        match self {
            Self::Model(error, tools_ran) => TurnFailure::model(error, *tools_ran),
            Self::Shutdown => TurnFailure::new(FailureKind::EngineStopped, "the engine shut down"),
            Self::Internal(message) => TurnFailure::new(FailureKind::Internal, message.clone()),
        }
    }
}
