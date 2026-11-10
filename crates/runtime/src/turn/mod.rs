//! Turn loop —— 用户消息 → 流式事件 → TurnOutcome。
//!
//! 在 agent 的协议无关类型之上的核心处理逻辑；按职责分成子模块，本文件留回合的公共类型与对外再导出。

mod attachments;
mod compact;
mod memory_extraction;
mod process;
mod prompt;
mod reporting;
mod request;
mod round;
mod token_budget;
mod tool_env;
mod tool_exec;
mod tool_result;
mod user_turn;

#[cfg(test)]
mod tests;

pub(crate) use process::hook_permission_mode;
pub(crate) use tool_env::engine_git_from;
pub(crate) use tool_exec::resolve_permission_response;

use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage, Usage};

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
    /// The turn was interrupted while the summary was being written.
    Cancelled,
}

impl std::fmt::Display for NotCompacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Declined => f.write_str("was declined by a PreCompact hook"),
            Self::NothingToDrop => f.write_str("saved nothing"),
            Self::Failed(e) => write!(f, "failed: {e}"),
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

    fn ended_before_reply(
        stop_reason: impl Into<String>,
        api_calls: u32,
        tool_calls: u32,
        total_usage: Usage,
    ) -> Self {
        Self::ended(
            stop_reason,
            api_calls,
            tool_calls,
            Usage::default(),
            total_usage,
            None,
        )
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
    #[error("model error: {0}")]
    Model(String),
    #[error("shutdown")]
    Shutdown,
    #[error("internal: {0}")]
    Internal(String),
}
