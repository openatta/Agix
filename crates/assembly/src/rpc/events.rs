//! `session.event` 的线上形状：协议对池的 `SessionEvent` 做的投影。
//!
//! `AgentEvent` 是内核的事件流，`SessionEventPayload` 是协议 §7 的白名单。投影
//! 用不带 `_` 的穷尽 match、按字段穷尽解构：内核加了变体或字段，这里编译不过，
//! 逼着人决定它上不上线。

use crate::session_pool::api::{SessionEvent, SessionFrame};
use base::context::goal::GoalView;
use base::context::{TaskEvent, TaskInfo};
use base::event::{
    AgentEvent, ContextUsage, FailureKind, PromptResolution, QueuedUserMessage, TeamStageStatus,
    TurnFailure,
};
use base::interface::elicitation::ElicitOption;
use base::interface::model::Usage;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEventPayload {
    TurnStart {},
    StepStart {
        step: u32,
    },
    ModelRetry {
        step: u32,
        attempt: u32,
        delay_ms: u64,
        reason: String,
    },
    StepEnd {
        step: u32,
        stop_reason: String,
        tool_calls: u32,
        seq: u64,
        usage: Usage,
    },
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolArgsDelta {
        id: String,
        partial_json: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolProgress {
        tool_use_id: String,
        name: String,
        message: String,
        #[serde(
            skip_serializing_if = "Option::is_none",
            serialize_with = "progress_count"
        )]
        progress: Option<f64>,
        #[serde(
            skip_serializing_if = "Option::is_none",
            serialize_with = "progress_count"
        )]
        total: Option<f64>,
        dropped: u64,
    },
    ToolResult {
        id: String,
        name: String,
        content: String,
        is_error: Option<bool>,
    },
    Prompt(PromptPayload),
    PromptResolved {
        prompt_id: String,
        resolution: PromptResolution,
    },
    SubagentProgress {
        agent_label: String,
        agent_session_id: String,
        agent_type: Option<String>,
        parent_turn: u32,
        parent_tool_use_id: Option<String>,
        task_id: Option<String>,
        event: Box<SessionEventPayload>,
    },
    TeamProgress {
        team: String,
        team_id: String,
        stage: String,
        stage_index: usize,
        stage_count: usize,
        status: TeamStageStatus,
        members: Vec<String>,
        failed: Vec<String>,
    },
    SkillsChanged {
        added: Vec<String>,
        removed: Vec<String>,
    },
    Notification {
        message: String,
        status: Option<String>,
    },
    Compact {
        strategy: String,
        messages_before: usize,
        messages_after: usize,
        tokens_before: usize,
        tokens_after: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        dropped_rounds: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        dropped_messages: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        estimated_tokens_saved: Option<usize>,
    },
    UserMessageDelivered {
        message_id: String,
        content: String,
    },
    UserMessageRefused {
        message_id: String,
        content: String,
        message: Option<String>,
    },
    UserMessagesReturned {
        messages: Vec<QueuedUserMessage>,
    },
    TurnComplete {
        stop_reason: String,
        api_calls: u32,
        tool_calls: u32,
        seq: u64,
        last_call_usage: Usage,
        usage: Usage,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<ContextUsage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        structured_output: Option<Value>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        persisted: bool,
    },
    Error {
        code: FailureKind,
        message: String,
        retryable: bool,
        retry_after_ms: Option<u64>,
    },
    TaskStarted {
        task: TaskInfo,
    },
    TaskUpdated {
        task: TaskInfo,
    },
    TaskProgress {
        task_id: String,
        appended: String,
        skipped_bytes: u64,
        from_byte: u64,
        next_byte: u64,
    },
    TaskFinished {
        task: TaskInfo,
    },
    GoalChanged {
        goal: Option<GoalView>,
    },
    ExtensionEntry {
        entry: Value,
    },
    SubscriptionDropped {
        reason: DropReason,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "prompt_type", rename_all = "snake_case")]
pub enum PromptPayload {
    Permission {
        prompt_id: String,
        tool_name: String,
        message: String,
        paths: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
    },
    Question {
        prompt_id: String,
        tool_use_id: Option<String>,
        question: String,
        header: Option<String>,
        options: Vec<ElicitOption>,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    Overflow,
}

fn progress_count<S: serde::Serializer>(value: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => base::event::progress_count_json(*v).serialize(s),
        None => s.serialize_none(),
    }
}

impl SessionEventPayload {
    /// Every `kind` a `session.event` frame can carry.
    pub const KINDS: &'static [&'static str] = &[
        "turn_start",
        "step_start",
        "model_retry",
        "step_end",
        "text_delta",
        "thinking_delta",
        "tool_use_start",
        "tool_args_delta",
        "tool_use",
        "tool_progress",
        "tool_result",
        "prompt",
        "prompt_resolved",
        "subagent_progress",
        "team_progress",
        "skills_changed",
        "notification",
        "compact",
        "user_message_delivered",
        "user_message_refused",
        "user_messages_returned",
        "turn_complete",
        "error",
        "task_started",
        "task_updated",
        "task_progress",
        "task_finished",
        "goal_changed",
        "extension_entry",
        "subscription_dropped",
    ];

    fn error(failure: &TurnFailure) -> Self {
        let TurnFailure {
            kind,
            retryable,
            retry_after_ms,
            side_effects: _,
            message,
            ..
        } = failure;
        Self::Error {
            code: *kind,
            message: message.clone(),
            retryable: *retryable,
            retry_after_ms: *retry_after_ms,
        }
    }

    /// What goes on the wire for `event`; `None` for one that does not.
    pub fn project(event: &SessionEvent) -> Option<Self> {
        match event {
            SessionEvent::Agent(event) => Self::from_agent(event),
            SessionEvent::Task(event) => Self::from_task(event),
            SessionEvent::GoalChanged(goal) => Some(Self::GoalChanged { goal: goal.clone() }),
            SessionEvent::ExtensionEntry(entry) => Some(Self::ExtensionEntry {
                entry: entry.clone(),
            }),
            SessionEvent::EngineStopped => Some(Self::error(&TurnFailure::new(
                FailureKind::EngineStopped,
                "engine stopped before the turn completed",
            ))),
            SessionEvent::Closed => Some(Self::error(&TurnFailure::new(
                FailureKind::SessionClosed,
                "the session was closed before the turn completed",
            ))),
        }
    }

    fn from_task(event: &TaskEvent) -> Option<Self> {
        Some(match event.clone() {
            TaskEvent::Started { task } => Self::TaskStarted { task },
            TaskEvent::Updated { task } => Self::TaskUpdated { task },
            TaskEvent::Finished { task } => Self::TaskFinished { task },
            TaskEvent::Progress {
                task_id,
                skipped_bytes,
                appended,
                from_byte,
                next_byte,
            } => Self::TaskProgress {
                task_id,
                appended,
                skipped_bytes,
                from_byte,
                next_byte,
            },
        })
    }

    fn from_agent(event: &AgentEvent) -> Option<Self> {
        Some(match event {
            AgentEvent::TextDelta { text, turn_id: _ } => Self::TextDelta { text: text.clone() },
            AgentEvent::ThinkingDelta { text, turn_id: _ } => {
                Self::ThinkingDelta { text: text.clone() }
            }
            AgentEvent::ToolUseStart {
                id,
                name,
                turn_id: _,
            } => Self::ToolUseStart {
                id: id.clone(),
                name: name.clone(),
            },
            AgentEvent::ToolArgsDelta {
                id,
                partial_json,
                turn_id: _,
            } => Self::ToolArgsDelta {
                id: id.clone(),
                partial_json: partial_json.clone(),
            },
            AgentEvent::ToolUse {
                id,
                name,
                input,
                turn_id: _,
            } => Self::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            AgentEvent::ToolProgress {
                tool_use_id,
                name,
                message,
                progress,
                total,
                dropped,
                turn_id: _,
            } => Self::ToolProgress {
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
                message: message.clone(),
                progress: *progress,
                total: *total,
                dropped: *dropped,
            },
            AgentEvent::ToolResult {
                id,
                name,
                content,
                is_error,
                turn_id: _,
            } => Self::ToolResult {
                id: id.clone(),
                name: name.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
            AgentEvent::PermissionPrompt {
                prompt_id,
                tool_name,
                message,
                paths,
                turn_id: _,
                tool_use_id,
                task_id: _,
            } => Self::Prompt(PromptPayload::Permission {
                prompt_id: prompt_id.clone(),
                tool_name: tool_name.clone(),
                message: message.clone(),
                paths: paths.iter().map(|p| p.display().to_string()).collect(),
                tool_use_id: tool_use_id.clone(),
            }),
            AgentEvent::QuestionPrompt {
                prompt_id,
                question,
                header,
                options,
                turn_id: _,
                tool_use_id,
                task_id: _,
            } => Self::Prompt(PromptPayload::Question {
                prompt_id: prompt_id.clone(),
                tool_use_id: tool_use_id.clone(),
                question: question.clone(),
                header: header.clone(),
                options: options.clone(),
            }),
            AgentEvent::PromptResolved {
                prompt_id,
                resolution,
            } => Self::PromptResolved {
                prompt_id: prompt_id.clone(),
                resolution: *resolution,
            },
            AgentEvent::TurnStart { turn_id: _ } => Self::TurnStart {},
            AgentEvent::StepStart { step, turn_id: _ } => Self::StepStart { step: *step },
            AgentEvent::ModelRetry {
                step,
                attempt,
                delay_ms,
                reason,
                turn_id: _,
            } => Self::ModelRetry {
                step: *step,
                attempt: *attempt,
                delay_ms: *delay_ms,
                reason: reason.clone(),
            },
            AgentEvent::StepEnd {
                step,
                stop_reason,
                usage,
                tool_calls,
                turn_id: _,
                seq,
            } => Self::StepEnd {
                step: *step,
                stop_reason: stop_reason.clone(),
                tool_calls: *tool_calls,
                seq: *seq,
                usage: *usage,
            },
            AgentEvent::TurnComplete {
                stop_reason,
                api_calls,
                tool_calls,
                usage,
                turn_id: _,
                message,
                last_call_usage,
                context,
                structured_output,
                seq,
                truncated,
                persisted,
            } => Self::TurnComplete {
                stop_reason: stop_reason.clone(),
                api_calls: *api_calls,
                tool_calls: *tool_calls,
                seq: *seq,
                last_call_usage: *last_call_usage,
                usage: *usage,
                message: message.clone(),
                context: *context,
                structured_output: structured_output.clone(),
                truncated: *truncated,
                persisted: *persisted,
            },
            AgentEvent::UserMessageDelivered {
                message_id,
                content,
                turn_id: _,
            } => Self::UserMessageDelivered {
                message_id: message_id.clone(),
                content: content.clone(),
            },
            AgentEvent::UserMessageRefused {
                message_id,
                content,
                message,
                turn_id: _,
            } => Self::UserMessageRefused {
                message_id: message_id.clone(),
                content: content.clone(),
                message: message.clone(),
            },
            AgentEvent::UserMessagesReturned {
                messages,
                turn_id: _,
            } => Self::UserMessagesReturned {
                messages: messages.clone(),
            },
            AgentEvent::SystemInit {
                scene: _,
                tools: _,
                mcp_servers: _,
            }
            | AgentEvent::System { message: _ }
            | AgentEvent::SessionChanged { session_id: _ }
            | AgentEvent::SessionPersisted { session_id: _ }
            | AgentEvent::AgentSpawned {
                agent_id: _,
                parent_turn: _,
                turn_id: _,
            }
            | AgentEvent::AgentCompleted {
                agent_id: _,
                outcome: _,
                turn_id: _,
            } => return None,
            AgentEvent::CompactAction {
                strategy,
                messages_before,
                messages_after,
                turn_id: _,
                dropped_rounds,
                dropped_messages,
                estimated_tokens_saved,
                tokens_before,
                tokens_after,
            } => Self::Compact {
                strategy: strategy.clone(),
                messages_before: *messages_before,
                messages_after: *messages_after,
                tokens_before: *tokens_before,
                tokens_after: *tokens_after,
                dropped_rounds: *dropped_rounds,
                dropped_messages: *dropped_messages,
                estimated_tokens_saved: *estimated_tokens_saved,
            },
            AgentEvent::SkillsChanged {
                added,
                removed,
                turn_id: _,
            } => Self::SkillsChanged {
                added: added.clone(),
                removed: removed.clone(),
            },
            AgentEvent::Notification {
                message,
                status,
                turn_id: _,
            } => Self::Notification {
                message: message.clone(),
                status: status.clone(),
            },
            // The nested event goes through this same projection, so a
            // sub-agent's frames have the shape the parent's do; one with no
            // projection is not sent at all.
            AgentEvent::SubagentProgress {
                agent_label,
                agent_session_id,
                agent_type,
                parent_session_id: _,
                parent_turn,
                parent_tool_use_id,
                task_id,
                event,
            } => Self::SubagentProgress {
                agent_label: agent_label.clone(),
                agent_session_id: agent_session_id.clone(),
                agent_type: agent_type.clone(),
                parent_turn: *parent_turn,
                parent_tool_use_id: parent_tool_use_id.clone(),
                task_id: task_id.clone(),
                event: Box::new(Self::from_agent(event)?),
            },
            AgentEvent::TeamProgress {
                team,
                team_id,
                stage,
                stage_index,
                stage_count,
                status,
                members,
                failed,
            } => Self::TeamProgress {
                team: team.clone(),
                team_id: team_id.clone(),
                stage: stage.clone(),
                stage_index: *stage_index,
                stage_count: *stage_count,
                status: *status,
                members: members.clone(),
                failed: failed.clone(),
            },
            AgentEvent::Error {
                failure,
                turn_id: _,
                persisted: _,
            } => Self::error(failure),
        })
    }
}

/// A `session.event` notification.
#[derive(Debug, Clone, Serialize)]
pub struct SessionEventNotification<'a> {
    jsonrpc: &'static str,
    method: &'static str,
    params: SessionEventParams<'a>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionEventParams<'a> {
    session_id: &'a str,
    turn_id: Option<&'a str>,
    event: SessionEventPayload,
}

impl<'a> SessionEventNotification<'a> {
    fn new(session_id: &'a str, turn_id: Option<&'a str>, event: SessionEventPayload) -> Self {
        Self {
            jsonrpc: "2.0",
            method: "session.event",
            params: SessionEventParams {
                session_id,
                turn_id,
                event,
            },
        }
    }

    /// The notification `frame` goes out as; `None` when it does not.
    pub fn of(frame: &'a SessionFrame) -> Option<Self> {
        let event = SessionEventPayload::project(&frame.event)?;
        Some(Self::new(
            &frame.session_id,
            frame.turn_id.as_deref(),
            event,
        ))
    }

    /// Told to a connection whose subscription to `session_id` was dropped
    /// because it fell too far behind. Not part of any turn.
    pub fn subscription_dropped(session_id: &'a str) -> Self {
        Self::new(
            session_id,
            None,
            SessionEventPayload::SubscriptionDropped {
                reason: DropReason::Overflow,
            },
        )
    }
}

#[cfg(test)]
mod tests;
