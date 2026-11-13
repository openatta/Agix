//! `AgentEvent` — streaming events emitted by the Engine.

use crate::interface::model::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// How much of the context window a session is using, after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextUsage {
    /// What the last model call was sent, as the provider counted it:
    /// input plus both cache figures.
    pub tokens: u64,
    /// Where automatic compaction starts.
    pub compact_threshold: u64,
    /// The model's context window.
    pub window: u64,
}

/// A user message that arrived while a turn was running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedUserMessage {
    pub message_id: String,
    pub content: String,
}

/// Events emitted by the Engine during a turn.
///
/// The upper layer (a CLI, a TUI, the daemon, a desktop host) consumes these
/// via `EventReceiver` and renders/dispatches as appropriate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Model text delta (high frequency).
    TextDelta { text: String, turn_id: String },

    /// Extended-thinking delta (high frequency). Separate from
    /// [`AgentEvent::TextDelta`] so hosts can render reasoning distinctly —
    /// collapsed, dimmed, or hidden — instead of splicing it into the answer.
    /// A host that does not care can ignore this variant; nothing else
    /// depends on it being consumed.
    ThinkingDelta { text: String, turn_id: String },

    /// The model has started writing a tool call. `ToolUse` with the same
    /// `id` follows once its arguments are complete.
    ToolUseStart {
        id: String,
        name: String,
        turn_id: String,
    },

    /// A raw fragment of a started tool call's arguments (high frequency).
    /// The fragments concatenate to the JSON that `ToolUse` carries parsed.
    ToolArgsDelta {
        id: String,
        partial_json: String,
        turn_id: String,
    },

    /// Model requested a tool call.
    ToolUse {
        id: String,
        name: String,
        input: Value,
        turn_id: String,
    },

    /// What a running tool call reported through its `ProgressSender`. At
    /// most one a second per call; `dropped` counts the reports merged away
    /// since the previous one. Never written to the transcript.
    ToolProgress {
        tool_use_id: String,
        name: String,
        message: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            serialize_with = "serialize_progress_count"
        )]
        progress: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            serialize_with = "serialize_progress_count"
        )]
        total: Option<f64>,
        #[serde(default)]
        dropped: u64,
        turn_id: String,
    },

    /// Tool execution completed.
    ToolResult {
        id: String,
        name: String,
        content: String,
        is_error: Option<bool>,
        turn_id: String,
    },

    /// Permission check requires upper-layer decision.
    PermissionPrompt {
        prompt_id: String,
        tool_name: String,
        message: String,
        paths: Vec<PathBuf>,
        turn_id: String,
        /// The `tool_use` block this prompt is about, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        /// The background task whose sub-agent asks, when one does. Its
        /// question outlives the turn it happened to arrive during.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },

    /// The model asked the user something (`AskUserQuestion`) and waits for
    /// the host's answer.
    QuestionPrompt {
        prompt_id: String,
        question: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<String>,
        /// Offered answers; empty means free-form.
        #[serde(default)]
        options: Vec<crate::interface::elicitation::ElicitOption>,
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },

    /// A turn has started. Every `TurnStart` is followed by exactly one
    /// `TurnComplete` or `Error` with the same `turn_id`.
    TurnStart { turn_id: String },

    /// The turn is about to call the model. `step` counts the turn's model
    /// calls from 0.
    StepStart { step: u32, turn_id: String },

    /// Model call `step` failed after its stream had started, and is sent
    /// again as it was once `delay_ms` has passed; a `StepStart` of the same
    /// `step` follows. `attempt` counts these retries of the call from 1;
    /// `reason` is the failure.
    ModelRetry {
        step: u32,
        attempt: u32,
        delay_ms: u64,
        reason: String,
        turn_id: String,
    },

    /// The model call `step` has finished and its reply — plus the results of
    /// the tools it asked for — is in the transcript.
    StepEnd {
        step: u32,
        stop_reason: String,
        /// What this one call cost.
        usage: Usage,
        tool_calls: u32,
        turn_id: String,
        /// How many messages the transcript holds now — the same position
        /// `session.history` pages by. Everything this step produced is on
        /// disk below it.
        #[serde(default)]
        seq: u64,
    },

    /// A turn has completed.
    TurnComplete {
        stop_reason: String,
        api_calls: u32,
        tool_calls: u32,
        usage: Usage,
        turn_id: String,
        /// Why the turn was stopped, when whatever stopped it said so — a
        /// hook's `message`, for instance.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// What the turn's last model call reported. `usage` is the sum.
        #[serde(default)]
        last_call_usage: Usage,
        /// How full the context is. `None` when the turn made no model call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<ContextUsage>,
        /// What the model last handed to `StructuredOutput` in this turn —
        /// validated against the host's schema when the session has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured_output: Option<Value>,
        /// The transcript position the turn ended at; see `StepEnd::seq`.
        #[serde(default)]
        seq: u64,
        /// A model call of this turn ended without the provider marking the
        /// end of its reply ([`crate::interface::model::ModelEvent::Truncated`]),
        /// so the reply may be cut short.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },

    /// A message the user sent while this turn was running is now part of
    /// the conversation; the model reads it on its next call.
    UserMessageDelivered {
        message_id: String,
        content: String,
        turn_id: String,
    },

    /// A message the user sent while this turn was running was refused on its
    /// way in (`UserPromptSubmit` blocked it). It is not in the transcript and
    /// the turn goes on; sending it again would be refused again.
    UserMessageRefused {
        message_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        turn_id: String,
    },

    /// Messages the user sent while this turn was running that the model
    /// never got to read — the turn ended (interrupted, stopped by a policy
    /// or an error) before the next point they could be added. They are not
    /// in the transcript; whether to send them again is the host's call.
    UserMessagesReturned {
        messages: Vec<QueuedUserMessage>,
        turn_id: String,
    },

    /// System initialization completed.
    SystemInit {
        scene: String,
        tools: Vec<ToolInfo>,
        mcp_servers: Vec<String>,
    },

    /// System notification.
    System { message: String },

    /// Context compaction occurred.
    CompactAction {
        strategy: String,
        messages_before: usize,
        messages_after: usize,
        turn_id: String,
        /// Number of rounds dropped by the Snip strategy (if applicable).
        dropped_rounds: Option<usize>,
        /// Number of messages in dropped rounds (if applicable).
        dropped_messages: Option<usize>,
        /// Estimated tokens saved by the compaction (if applicable).
        estimated_tokens_saved: Option<usize>,
        /// The compactor's estimate of the history's size before and after.
        #[serde(default)]
        tokens_before: usize,
        #[serde(default)]
        tokens_after: usize,
    },

    /// Session was changed (via set_session_id).
    SessionChanged { session_id: String },

    /// Session was persisted to disk.
    SessionPersisted { session_id: String },

    /// The skill catalog changed on disk — emitted at the top of the turn
    /// that observed it, right after the reload.
    ///
    /// Skill-derived slash commands resolve through the live catalog, so a
    /// host that caches a command list (a completion popup, a command
    /// palette) has no other way to learn its copy went stale short of
    /// polling. Names are skill names, i.e. slash commands without the
    /// leading `/`.
    ///
    /// Content edits to an existing skill are not reported: the reload is
    /// detected by set difference over the catalog, which sees a rename as
    /// one removal plus one addition and an in-place body edit as nothing at
    /// all. That is the honest scope — a consumer that cares about bodies
    /// should re-read on any change rather than trust this to fire.
    SkillsChanged {
        added: Vec<String>,
        removed: Vec<String>,
        turn_id: String,
    },

    /// The model asked for the user's attention with `PushNotification`.
    Notification {
        message: String,
        /// `"proactive"` when the model sent it unprompted; `None` otherwise.
        status: Option<String>,
        turn_id: String,
    },

    /// A sub-agent was spawned.
    AgentSpawned {
        agent_id: String,
        parent_turn: u32,
        turn_id: String,
    },

    /// A sub-agent completed.
    AgentCompleted {
        agent_id: String,
        outcome: String,
        turn_id: String,
    },

    /// An event emitted *inside* a spawned sub-agent, re-emitted on the
    /// parent session's channel so the host (IDE plugin / CLI) can render
    /// sub-agent activity live instead of staring at a long pause.
    ///
    /// This is purely additive: the sub-agent's text is still collected and
    /// returned as the `Agent` tool's result for the model to read. Hosts
    /// that don't understand this variant can ignore it and behave exactly
    /// as before.
    ///
    /// Attribution: `agent_label` is stable for the whole lifetime of one
    /// sub-agent run, so a host can group every forwarded event under one
    /// collapsible node. `parent_turn` is the parent's turn *number*
    /// (`ToolContext::turn_no`); the parent's turn *id* is deliberately not
    /// carried here because `runtime::turn` does not thread it into
    /// `ToolContext` — transports that need it already know it from the
    /// enclosing turn (see the daemon's `run_turn` stream frames).
    SubagentProgress {
        /// Stable per-run label, e.g. `"explore#3f2a1b7c"`.
        agent_label: String,
        /// The sub-agent's own session id.
        agent_session_id: String,
        /// Named agent type, when the spawn selected one.
        agent_type: Option<String>,
        /// Parent session id, when the spawn site knew it (`""` otherwise —
        /// e.g. the generic `AgentSpawner` bridge, which has no
        /// `ToolContext`).
        parent_session_id: String,
        /// Parent turn number, `0` when unknown (same caveat as above).
        parent_turn: u32,
        /// The parent's `Agent` tool call this sub-agent is running for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
        /// The parent session's task this sub-agent runs as.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        /// The forwarded sub-agent event. Boxed so `AgentEvent` stays small.
        event: Box<AgentEvent>,
    },

    /// Team orchestration progress, emitted as each stage of a `TeamCreate`
    /// run starts and finishes — so a large team streams incremental
    /// progress instead of going silent until the whole scratchpad is
    /// returned as one tool result.
    TeamProgress {
        /// Team name as given to `TeamCreate`.
        team: String,
        /// Generated team id (also the `.agix/teams/<id>/` directory name).
        team_id: String,
        /// Stage name.
        stage: String,
        /// 0-based stage index.
        stage_index: usize,
        /// Total number of stages in this team run.
        stage_count: usize,
        /// Where this stage is in its lifecycle.
        status: TeamStageStatus,
        /// Labels of the members in this stage.
        members: Vec<String>,
        /// Labels of members whose result was an error (only meaningful for
        /// `Completed`).
        failed: Vec<String>,
    },

    /// The turn ended in failure.
    Error {
        failure: TurnFailure,
        turn_id: String,
    },
}

/// How a turn failed, in the terms a host decides on: whether to send it
/// again, when, and whether doing so repeats something.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    RateLimited,
    Overloaded,
    /// The provider could not be reached or failed on its side; the request
    /// itself was fine.
    ModelUnavailable,
    ContextTooLong,
    /// The provider refused the request as malformed, or for a model or a
    /// feature it does not serve.
    ModelRejected,
    Auth,
    Billing,
    EgressDenied,
    /// The provider failed in a way sending again will not fix.
    ModelError,
    SessionClosed,
    EngineStopped,
    /// The engine did not take or answer a request.
    EngineUnreachable,
    PersistFailed,
    Internal,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Overloaded => "overloaded",
            Self::ModelUnavailable => "model_unavailable",
            Self::ContextTooLong => "context_too_long",
            Self::ModelRejected => "model_rejected",
            Self::Auth => "auth",
            Self::Billing => "billing",
            Self::EgressDenied => "egress_denied",
            Self::ModelError => "model_error",
            Self::SessionClosed => "session_closed",
            Self::EngineStopped => "engine_stopped",
            Self::EngineUnreachable => "engine_unreachable",
            Self::PersistFailed => "persist_failed",
            Self::Internal => "internal",
        }
    }

    /// Whether the same request, sent again later, may well succeed.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited
                | Self::Overloaded
                | Self::ModelUnavailable
                | Self::EngineStopped
                | Self::EngineUnreachable
                | Self::PersistFailed
        )
    }
}

impl From<crate::interface::model::ModelErrorKind> for FailureKind {
    fn from(kind: crate::interface::model::ModelErrorKind) -> Self {
        use crate::interface::model::ModelErrorKind as M;
        match kind {
            M::RateLimited => Self::RateLimited,
            M::Overloaded => Self::Overloaded,
            M::Unavailable | M::Transport | M::Timeout | M::StreamInterrupted => {
                Self::ModelUnavailable
            }
            M::ContextTooLong => Self::ContextTooLong,
            M::InvalidRequest | M::NotFound | M::Unsupported => Self::ModelRejected,
            M::Auth => Self::Auth,
            M::Billing => Self::Billing,
            M::Denied => Self::EgressDenied,
            M::ServerError | M::Protocol | M::Cancelled | M::NotRecorded => Self::ModelError,
        }
    }
}

impl std::fmt::Display for FailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A turn's failure, as the engine, the pool and the wire all carry it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TurnFailure {
    pub kind: FailureKind,
    pub retryable: bool,
    /// How long the provider asked to be left alone, when it said — also
    /// past what the client's backoff was willing to wait.
    pub retry_after_ms: Option<u64>,
    /// A tool already ran in this turn, so sending it again repeats that.
    pub side_effects: bool,
    pub message: String,
}

impl TurnFailure {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            retryable: kind.retryable(),
            retry_after_ms: None,
            side_effects: false,
            message: message.into(),
        }
    }

    pub fn model(error: &crate::interface::model::ModelError, side_effects: bool) -> Self {
        Self {
            retry_after_ms: error
                .retry_after
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            side_effects,
            ..Self::new(error.kind.into(), error.to_string())
        }
    }

    pub fn with_side_effects(mut self, side_effects: bool) -> Self {
        self.side_effects = side_effects;
        self
    }
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for TurnFailure {}

/// Lifecycle point a [`AgentEvent::TeamProgress`] event reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamStageStatus {
    /// Every member of the stage has been dispatched.
    Started,
    /// Every member of the stage has finished (successfully or not).
    Completed,
}

/// Summary info for a registered tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

/// A progress figure as it goes on the wire: whole numbers as JSON integers
/// (`42`, not `42.0`) so hosts can read line and byte counts as integers;
/// only a fractional figure keeps its fraction.
pub fn progress_count_json(value: f64) -> Value {
    const EXACT: f64 = 9_007_199_254_740_992.0;
    if value.fract() == 0.0 && value.abs() <= EXACT {
        if value >= 0.0 {
            return Value::from(value as u64);
        }
        return Value::from(value as i64);
    }
    Value::from(value)
}

fn serialize_progress_count<S: serde::Serializer>(
    value: &Option<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => progress_count_json(*v).serialize(serializer),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::model::{ModelError, ModelErrorKind};

    #[test]
    fn a_model_failure_is_retryable_exactly_when_its_kind_is_transient() {
        use ModelErrorKind::*;
        for kind in [
            RateLimited,
            Overloaded,
            Unavailable,
            Transport,
            Timeout,
            StreamInterrupted,
            ContextTooLong,
            InvalidRequest,
            NotFound,
            Auth,
            Billing,
            Denied,
            ServerError,
            Protocol,
            Cancelled,
            Unsupported,
            NotRecorded,
        ] {
            let failure = TurnFailure::model(&ModelError::new(kind, "m"), false);
            assert_eq!(failure.retryable, kind.transient(), "{kind}");
        }
    }

    #[test]
    fn a_hint_past_what_the_client_waits_for_still_reaches_the_failure() {
        let error =
            ModelError::rate_limited().with_retry_after(Some(std::time::Duration::from_secs(120)));

        let failure = TurnFailure::model(&error, true);

        assert_eq!(failure.kind, FailureKind::RateLimited);
        assert_eq!(failure.retry_after_ms, Some(120_000));
        assert!(failure.side_effects);
        assert!(failure.to_string().starts_with("rate_limited: "));
    }
}
