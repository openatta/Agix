//! 会话事件的出口：池把每个会话产生的事件当场同步交给订阅它的 `SessionWatcher`，
//! 事件怎么上线（投影成什么帧、写进哪条连接）由订阅者决定。

use base::event::AgentEvent;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WatcherId(pub u64);

/// Something watching sessions — a client connection, an embedder's sink.
pub trait SessionWatcher: Send + Sync {
    fn id(&self) -> WatcherId;
    /// Called with the session's lock held: must not block. `false` drops
    /// this watcher's subscription to the session.
    fn deliver(&self, frame: &SessionFrame) -> bool;
}

#[derive(Debug, Clone)]
pub struct SessionFrame {
    pub session_id: Arc<str>,
    /// The turn running when the event happened; `None` between turns.
    pub turn_id: Option<Arc<str>>,
    pub event: SessionEvent,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SessionEvent {
    /// What the engine said, unfiltered.
    Agent(AgentEvent),
    /// A task of the session started, changed, printed or finished
    /// (`SessionState::subscribe_tasks`).
    Task(base::context::TaskEvent),
    /// The session's goal changed; `None` once cleared.
    GoalChanged(Option<base::context::goal::GoalView>),
    /// A host wrote an extension entry into the session's log.
    ExtensionEntry(serde_json::Value),
    /// The engine went away before the running turn completed.
    EngineStopped,
    /// `session.close` / `session.delete` cut the running turn off.
    Closed,
}

/// Whether the engine sends `event` whether or not a turn is running — a
/// question, a tool call's, a sub-agent's or a team's progress, changed
/// skills, a notification. Between turns only these go to the watchers.
///
/// A question is not terminal: the tool call that asked waits on
/// `session.respondToPrompt`. A sub-agent's activity arrives under its own
/// `kind`, so a client that does not know it ignores it and one that does
/// can show the sub-agent's tool calls under its `agent_label` instead of a
/// stalled `Agent` call.
pub fn emitted_outside_turns(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::PermissionPrompt { .. }
            | AgentEvent::QuestionPrompt { .. }
            | AgentEvent::SubagentProgress { .. }
            | AgentEvent::ToolProgress { .. }
            | AgentEvent::SkillsChanged { .. }
            | AgentEvent::Notification { .. }
            | AgentEvent::TeamProgress { .. }
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AskKind {
    Permission,
    Question,
}

impl AskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
        }
    }
}

/// A permission prompt or question raised and not yet answered.
#[derive(Debug, Clone)]
pub struct PendingAsk {
    pub prompt_id: String,
    /// The background task whose sub-agent asks, when one does.
    pub task_id: Option<String>,
    pub kind: AskKind,
    pub frame: Arc<SessionFrame>,
}

/// What a new watcher of a session needs to catch up: the transcript
/// watermark it was subscribed at, and the asks still open.
#[derive(Debug, Clone)]
pub struct Subscribed {
    pub session_id: String,
    pub last_seq: usize,
    pub pending: Vec<PendingAsk>,
}
