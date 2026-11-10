//! Wire types of the engine's input, output and permission channels: what hosts send in and what comes back.

use super::*;

#[derive(Debug)]
pub enum InputMessage {
    User {
        content: String,
        attachments: Vec<Attachment>,
        turn_id: String,
    },
    ToolResult {
        tool_use_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    PermissionResponse {
        prompt_id: String,
        decision: PermissionDecision,
    },
    /// The host's answer to an `AgentEvent::QuestionPrompt`.
    QuestionResponse {
        prompt_id: String,
        answer: QuestionAnswer,
    },
    System {
        kind: EngineCommand,
        content: String,
    },
    /// A user message meant for the turn that is running, read by the model
    /// at the turn's next step boundary. Only [`Agent::run`] routes these.
    ///
    /// With `ack`, the sender learns whether a turn took it, and keeps the
    /// message when none did. Without `ack`, a message that finds no turn
    /// running starts one, as an ordinary `User` message would.
    Steer {
        content: String,
        message_id: String,
        ack: Option<tokio::sync::oneshot::Sender<crate::steering::SteerAck>>,
    },
    /// A look at, or a change to, this session's context, handled between
    /// turns. The answer — or why not — goes back on `reply`.
    Context {
        request: ContextRequest,
        reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    },
}

/// What a host asks of a session's context (`InputMessage::Context`).
///
/// Positions are those of the session's messages as `session.history`
/// numbers them. `total`, when given, is how many messages the asker saw: a
/// change refuses to apply if the context has since grown or been compacted,
/// rather than land on a message the asker did not mean.
#[derive(Debug, Clone)]
pub enum ContextRequest {
    /// What the next model call would carry, without making it.
    Inspect,
    /// Blank out user messages or tool results (see `SessionManager::drop_message`).
    Drop {
        at: Vec<usize>,
        total: Option<usize>,
    },
    /// Rewrite a message the user typed.
    Edit {
        at: usize,
        text: String,
        total: Option<usize>,
    },
}

/// Something the user attached to their message — a pasted screenshot, a
/// dragged-in file, an editor selection.
///
/// The variants below are the three shapes hosts actually have, so that a host
/// can hand over whichever it already holds instead of being forced to pre-read
/// (or pre-decode) on the runtime's behalf.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attachment {
    /// A path the runtime should read. Images (by extension) become
    /// [`ModelContentBlock::Image`]; anything else is inlined as text.
    /// Use this when the host only has a path — e.g. a drag-and-drop.
    File { path: String },
    /// Text the host has already read. `path` is kept for the label shown to
    /// the model, and may be a synthetic name for non-file text such as an
    /// editor selection.
    Text { path: String, content: String },
    /// An image the host already decoded — the clipboard-paste case, where
    /// there is no file on disk at all. `data` is base64 without a data-URI
    /// prefix.
    Image { media_type: String, data: String },
}

/// Where a `PermitAlways` decision's effect should be retained.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistScope {
    /// In-memory only, this session.
    Session,
    /// In-memory for this session AND written to `.agix/settings.local.json`
    /// so future sessions in this project also allow it.
    Local,
}

/// Wire shape: `{"type":"permit"}` / `{"type":"deny","reason":"..."}` /
/// `{"type":"permit_always","scope":"session"}` /
/// `{"type":"permit_always","scope":"local"}` — what
/// `session.respondToPrompt` (daemon RPC) parses out of its `decision`
/// param and what `InputMessage::PermissionResponse` carries.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionDecision {
    Permit,
    Deny {
        reason: String,
    },
    /// Like `Permit`, but also persists an Allow rule beyond this single
    /// tool call — see `base::interface::permission::Permission::add_persistent_allow`.
    /// `scope: Session` keeps it in-memory only (rest of this session);
    /// `scope: Local` additionally writes it to `.agix/settings.local.json`
    /// so future sessions in this project inherit it too.
    PermitAlways {
        scope: PersistScope,
    },
}

/// Wire shape: `{"type":"answer","text":"..."}` / `{"type":"decline","reason":"..."}`
/// — what `session.respondToPrompt` parses out of its `answer` param for a
/// question and what `InputMessage::QuestionResponse` carries. `text` is one
/// of the question's option keys or free text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionAnswer {
    Answer { text: String },
    Decline { reason: String },
}

#[derive(Debug, Clone)]
pub enum EngineCommand {
    SetSessionId,
    CompactNow,
    RefreshMcp,
    UpdateModel,
    /// Interrupt the turn in flight and keep the session alive — what a host
    /// binds to Esc. Distinct from cancelling the token passed to
    /// [`Agent::run`], which ends the whole session.
    ///
    /// Handled in the input demultiplexer, never in the main loop: the loop
    /// awaits `process_turn` inline, so while a turn is running it is not
    /// reading the channel and could only act on this once the turn it was
    /// meant to interrupt had already finished. Same reason
    /// `PermissionResponse` is handled there.
    ///
    /// A no-op when no turn is running (the token it cancels belongs to the
    /// turn that just ended); it does not arm the next one.
    CancelTurn,
    Shutdown,
}

pub type InputSender = mpsc::UnboundedSender<InputMessage>;
pub type InputReceiver = mpsc::UnboundedReceiver<InputMessage>;
/// Where the engine emits. Every emission site in the runtime holds one of
/// these, which is why swapping the raw channel for [`EventBus`] reached all
/// of them at once — the bus keeps `send`'s signature deliberately.
///
/// [`EventBus`]: crate::event_bus::EventBus
pub type EventSender = crate::event_bus::EventBus;
pub type EventReceiver = mpsc::UnboundedReceiver<AgentEvent>;

/// Registry of permission requests awaiting a host response, keyed by
/// `prompt_id`. Shared between `Agent::pending_permissions` and
/// `agent_tool::ParentPermission` (a sub-agent registers into the *parent's*
/// copy of this same map — see `ParentPermission`'s doc comment).
/// What [`Builder::session_prompt`] puts over a session's settings. `None`
/// leaves the settings' `prompt_append` / `prompt_override` in force.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionPrompt {
    pub append: Option<String>,
    #[serde(rename = "override")]
    pub override_text: Option<String>,
}

impl SessionPrompt {
    pub fn is_empty(&self) -> bool {
        self.append.is_none() && self.override_text.is_none()
    }

    pub(super) fn applied_to(&self, settings: &Settings) -> Settings {
        let mut out = settings.clone();
        if let Some(append) = &self.append {
            out.prompt_append = Some(append.clone());
        }
        if let Some(text) = &self.override_text {
            out.prompt_override = Some(text.clone());
        }
        out
    }
}

pub type PendingPermissions = Arc<
    std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<PermissionDecision>>,
    >,
>;
