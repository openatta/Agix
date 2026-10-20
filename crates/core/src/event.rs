//! `AgentEvent` — streaming events emitted by the Engine.

use crate::interface::model::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// How much of the context window a session is using, after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
/// The upper layer (CLI/TUI/daemon/Jiandu desktop) consumes these
/// via `EventReceiver` and renders/dispatches as appropriate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    // ── Streaming ──
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

    /// Tool execution completed.
    ToolResult {
        id: String,
        name: String,
        content: String,
        is_error: Option<bool>,
        turn_id: String,
    },

    // ── Permission ──
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
    },

    // ── Turn lifecycle ──
    /// A turn has started. Every `TurnStart` is followed by exactly one
    /// `TurnComplete` or `Error` with the same `turn_id`.
    TurnStart { turn_id: String },

    /// The turn is about to call the model. `step` counts the turn's model
    /// calls from 0.
    StepStart { step: u32, turn_id: String },

    /// The model call `step` has finished and its reply — plus the results of
    /// the tools it asked for — is in the transcript.
    StepEnd {
        step: u32,
        stop_reason: String,
        /// What this one call cost.
        usage: Usage,
        tool_calls: u32,
        turn_id: String,
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
    },

    /// A message the user sent while this turn was running is now part of
    /// the conversation; the model reads it on its next call.
    UserMessageDelivered {
        message_id: String,
        content: String,
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

    // ── System ──
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

    // ── Sub-agent ──
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
        /// The forwarded sub-agent event. Boxed so `AgentEvent` stays small.
        event: Box<AgentEvent>,
    },

    // ── Team ──
    /// Team orchestration progress, emitted as each stage of a `TeamCreate`
    /// run starts and finishes — so a large team streams incremental
    /// progress instead of going silent until the whole scratchpad is
    /// returned as one tool result.
    TeamProgress {
        /// Team name as given to `TeamCreate`.
        team: String,
        /// Generated team id (also the `.atta/teams/<id>/` directory name).
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

    // ── Error ──
    /// An error occurred.
    Error {
        code: String,
        message: String,
        turn_id: String,
    },
}

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
