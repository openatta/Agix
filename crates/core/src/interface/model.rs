//! `Model` trait — protocol-agnostic LLM backend interface.

use crate::prompt::PromptBlock;
use crate::provider::ApiType;
use crate::settings::ThinkingMode;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Protocol-agnostic LLM client trait.
///
/// Implementations translate `PromptBlock` and `ToolDef` into
/// the API-specific wire format (Anthropic Messages, OpenAI Chat Completions).
#[async_trait]
pub trait Model: Send + Sync {
    /// Which API protocol this model client uses.
    fn api_type(&self) -> ApiType;

    /// The provider id this client was built for, stamped on the reasoning it
    /// produces so a later request to a different provider does not echo it
    /// back. `None` for a client built outside a provider route; reasoning
    /// from such a client carries no provenance and is sent wherever it goes.
    fn provenance(&self) -> Option<&str> {
        None
    }

    /// Stream a response from the LLM.
    ///
    /// Returns a stream of `ModelEvent`s (text deltas, tool use blocks).
    /// The cancel token allows the caller to abort mid-stream.
    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError>;
}

/// Parameters for a streaming LLM call.
#[derive(Debug, Clone)]
pub struct StreamParams {
    pub model: String,
    pub max_tokens: u32,
    pub thinking_mode: ThinkingMode,
    /// Fallback model to switch to on persistent Overloaded/529 errors (e.g. Opus → Sonnet).
    pub fallback_model: Option<String>,
    /// Cache edits: tool_use_ids whose tool result content should be deleted from the
    /// server-side cached prefix. Wired to the Anthropic `cache_edits` content block.
    pub cache_edits: Vec<String>,
    /// Where this call sits in the conversation. Carried for observers only —
    /// no adapter reads it, and it never reaches the wire.
    pub origin: Option<CallOrigin>,
    /// What this turn's user message is made of. Observers only, like `origin`.
    pub input_map: Option<InputMap>,
}

/// The composition of the user message a turn was started by.
///
/// A user message goes out as one message whose first block holds the prompt
/// text, and it has to stay that way: splitting the sources into separate
/// content blocks would change the bytes the model receives. So composition is
/// expressed as coordinates over the message rather than in its structure —
/// the same reason [`crate::prompt::PromptBlock::name`] is a
/// field and not a delimiter inside the content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputMap {
    /// Index into the request's messages. Resolved when the request is
    /// assembled, not when the message was pushed: compaction rewrites the
    /// message list in between, and an index captured earlier would point at
    /// whatever moved into that slot.
    pub user_message: usize,
    pub spans: Vec<InputSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSpan {
    /// One of the constants in [`input_source`].
    pub source: String,
    /// Which content block of the message.
    pub block: usize,
    /// Byte range within that block's text. Absent for a non-text block, which
    /// has no interior to point into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<(usize, usize)>,
}

/// Values for [`InputSpan::source`].
pub mod input_source {
    /// What the user actually typed.
    pub const USER_PROMPT: &str = "user_prompt";
    /// An `@server:scheme://path` reference resolved and inlined into the
    /// prompt text.
    pub const MCP_RESOURCE: &str = "mcp_resource";
    /// A file or image the user attached, carried as its own content block.
    pub const ATTACHMENT: &str = "attachment";
}

/// Which session issued a call, and what the call is doing there.
///
/// Every model call has one. It used to be optional, with `None` standing for
/// "not turn work" — which also erased the session id, so those calls could
/// only be filed under a shared `unattributed` name that concurrent sessions
/// then overwrote. Naming the *kind* instead keeps the session known while
/// still refusing to give an auxiliary call turn coordinates it does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOrigin {
    pub session_id: String,
    /// The session that spawned this one — a sub-agent's parent. `None` for a
    /// top-level session. This is what lets a set of recordings be read as one
    /// tree rather than as unrelated files.
    pub parent_session_id: Option<String>,
    /// Which kind of agent this session runs, for a sub-agent that has one.
    pub agent_type: Option<String>,
    pub kind: CallKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallKind {
    /// A call the turn loop made.
    Turn {
        turn: u32,
        /// Index of this model call within its turn — a turn that calls tools
        /// runs several.
        step: u32,
    },
    /// A call made outside any turn: compaction, memory extraction, permission
    /// classification, session titling, a hook.
    ///
    /// `purpose` is a free string on purpose. The set grows with the engine,
    /// and an enum would mean editing this crate for every addition; the
    /// conventional values are the constants in [`call_purpose`]. Readers
    /// treat it as opaque, and it takes no part in replay divergence checks —
    /// renaming a classification must not turn a passing replay red.
    Auxiliary { purpose: String },
}

impl CallOrigin {
    /// A turn-loop call in `session_id`.
    pub fn turn(session_id: impl Into<String>, turn: u32, step: u32) -> Self {
        Self {
            session_id: session_id.into(),
            parent_session_id: None,
            agent_type: None,
            kind: CallKind::Turn { turn, step },
        }
    }

    /// A call outside any turn, still belonging to `session_id`.
    pub fn auxiliary(session_id: impl Into<String>, purpose: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            parent_session_id: None,
            agent_type: None,
            kind: CallKind::Auxiliary {
                purpose: purpose.into(),
            },
        }
    }

    pub fn with_lineage(
        mut self,
        parent_session_id: Option<String>,
        agent_type: Option<String>,
    ) -> Self {
        self.parent_session_id = parent_session_id;
        self.agent_type = agent_type;
        self
    }

    /// Turn coordinates, or `(0, 0)` for a call that has none.
    pub fn turn_step(&self) -> (u32, u32) {
        match &self.kind {
            CallKind::Turn { turn, step } => (*turn, *step),
            CallKind::Auxiliary { .. } => (0, 0),
        }
    }

    pub fn purpose(&self) -> Option<&str> {
        match &self.kind {
            CallKind::Turn { .. } => None,
            CallKind::Auxiliary { purpose } => Some(purpose),
        }
    }
}

/// Conventional values for [`CallKind::Auxiliary::purpose`].
pub mod call_purpose {
    pub const COMPACT: &str = "compact";
    pub const MEMORY: &str = "memory";
    pub const CLASSIFY: &str = "classify";
    pub const TITLE: &str = "title";
    pub const HOOK: &str = "hook";
    /// Team aggregation picking or merging its agents' results.
    pub const TEAM_JUDGE: &str = "team_judge";
}

/// Tool definition sent to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Where this tool came from — `builtin`, `mcp:<server>`, `plugin:<id>`.
    ///
    /// Annotation only, like [`crate::prompt::PromptBlock::source`]:
    /// both adapters translate a `ToolDef` field by field, so this one is
    /// simply not copied and never reaches the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A message in the model's conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: Vec<ModelContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

/// Token cost to charge a [`ModelContentBlock::Image`] in context-budget
/// estimates.
///
/// Anthropic bills images at roughly `width * height / 750` tokens, capped near
/// 1600 for an image at the ~1.15 MP resize ceiling. We do not have pixel
/// dimensions at this layer, so we charge the ceiling — deliberately
/// pessimistic, because under-counting context is what causes a request to be
/// rejected for length.
///
/// The trap this constant exists to avoid: estimating an image the same way as
/// text (`data.len() / 4`) reads a 1 MB screenshot as ~250_000 tokens, which
/// would trip compaction immediately on the first image the user pastes. The
/// base64 payload length says almost nothing about the token cost.
pub const IMAGE_TOKEN_ESTIMATE: usize = 1600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContentBlock {
    Text {
        text: String,
    },
    /// A base64-encoded image. Carries the same two fields the Anthropic API's
    /// `source` object needs, kept flat here because every provider that
    /// supports images needs exactly these and nothing more.
    ///
    /// `data` is **not** logged or Debug-printed usefully — it is megabytes of
    /// base64. Anything that renders a message for humans or for a token
    /// estimate must special-case this variant rather than falling through to
    /// a generic text path.
    Image {
        /// e.g. `image/png`, `image/jpeg`.
        media_type: String,
        /// Base64-encoded bytes, no data-URI prefix.
        data: String,
    },
    /// An extended-thinking block, with the signature that closed it if the
    /// provider sent one.
    ///
    /// When thinking is enabled and the assistant's turn contained tool use,
    /// the Anthropic API requires the previous assistant turn's thinking
    /// blocks to be echoed back *with their signatures* on the following
    /// request. Blocks without a signature (reasoning from an
    /// OpenAI-compatible endpoint, or a relay that never sends one) are kept
    /// for the transcript and are never sent to the Anthropic API.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// [`Model::provenance`] of the client that produced it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Thinking the API chose to encrypt. Opaque to us; must still be echoed
    /// back verbatim for the same reason as [`ModelContentBlock::Thinking`].
    RedactedThinking {
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: Option<bool>,
    },
}

impl ModelContentBlock {
    /// Reasoning another provider produced. Only that provider can verify or
    /// use it, so it is never sent to `provenance`. Blocks without a recorded
    /// provider, and requests to a client without one, are not foreign.
    pub fn is_foreign_reasoning(&self, provenance: Option<&str>) -> bool {
        let produced_by = match self {
            ModelContentBlock::Thinking { provider, .. }
            | ModelContentBlock::RedactedThinking { provider, .. } => provider.as_deref(),
            _ => return false,
        };
        matches!((produced_by, provenance), (Some(a), Some(b)) if a != b)
    }
}

/// Stream of model events.
pub type ModelStream =
    Box<dyn futures::Stream<Item = Result<ModelEvent, ModelError>> + Send + Unpin>;

/// Events emitted by the model during streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    /// Incremental extended-thinking text. Emitted so hosts can render the
    /// model's reasoning as it streams; the accumulated text plus the
    /// signature from [`ModelEvent::ThinkingSignature`] become a
    /// [`ModelContentBlock::Thinking`] in the transcript.
    ThinkingDelta {
        text: String,
    },
    /// The signature that closes the current thinking block. The API sends it
    /// once, at the end of the block.
    ThinkingSignature {
        signature: String,
    },
    /// A thinking block the API encrypted; opaque, forwarded whole.
    RedactedThinking {
        data: String,
    },
    /// The model has started a tool call; its arguments are still streaming.
    /// The call itself is the [`ModelEvent::ToolUse`] that follows.
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// One raw fragment of a tool call's arguments, exactly as the model
    /// produced it. Observational only: the adapter still concatenates the
    /// fragments and emits the assembled [`ModelEvent::ToolUse`], which is what
    /// the engine acts on. Forwarded alongside it because a malformed or
    /// truncated argument stream is precisely the case worth investigating
    /// afterwards, and concatenation destroys the evidence.
    ToolArgsDelta {
        id: String,
        partial_json: String,
    },
    ContentBlockStart {
        index: usize,
        block: ModelContentBlock,
    },
    ContentBlockStop {
        index: usize,
    },
    EndTurn {
        stop_reason: String,
        usage: Usage,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens written into the prompt cache by this call, and tokens read
    /// back from it. Both are priced differently from ordinary input — a
    /// cache read costs a fraction of one — so a report that folded them
    /// into `input_tokens` would overcharge and one that dropped them, as
    /// this type used to force, undercharges by whatever the cache covered.
    ///
    /// `#[serde(default)]` because recordings written before these existed
    /// must still read back.
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

impl Usage {
    /// This and one more call's worth.
    ///
    /// A provider reports usage per call and a turn is several calls, so
    /// summing is what anyone asking "what did that cost" has to do. Saturating
    /// because a turn that overflowed a `u32` of tokens has a bigger problem
    /// than an inexact total, and wrapping to nearly zero would be the one
    /// answer worse than the true one.
    pub fn plus(self, other: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("auth error: {0}")]
    Auth(String),
    #[error("rate limited")]
    RateLimited,
    #[error("overloaded")]
    Overloaded,
    #[error("network error: {0}")]
    Network(String),
    #[error("cancelled")]
    Cancelled,
    #[error("internal: {0}")]
    Internal(String),
}
