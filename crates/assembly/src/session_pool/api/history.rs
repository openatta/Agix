//! 会话日志的领域结果类型：转录页、扩展条目、导出摘要。

use serde::{Deserialize, Serialize};

/// Where a history item came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryOrigin {
    /// Typed by the person.
    User,
    /// Sent by the person while a turn was running.
    Steer,
    /// The expansion of a slash command.
    Command,
    /// Put there by the engine or a tool: reminders, recalled memory, a
    /// skill's instructions.
    Injected,
    ToolResult,
    Assistant,
    /// A compaction's summary or kept history.
    Compact,
    Extension,
}

/// One projected message, plus where it sits and where it came from.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HistoryItem {
    #[serde(flatten)]
    pub message: base::message::Message,
    pub seq: usize,
    pub id: String,
    /// RFC 3339.
    pub ts: String,
    pub turn_id: Option<String>,
    /// `null` on lines written before origins were recorded.
    pub origin: Option<HistoryOrigin>,
    /// The model call's usage, on an assistant message.
    pub usage: Option<HistoryUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HistoryUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

impl From<&history::entry::UsageRecord> for HistoryUsage {
    fn from(usage: &history::entry::UsageRecord) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HistoryPage {
    pub session_id: String,
    pub messages: Vec<HistoryItem>,
    /// Projected messages in the whole transcript.
    pub total: usize,
    pub offset: usize,
    /// The limit actually applied.
    pub limit: usize,
    pub has_more: bool,
    pub entry_count: usize,
    /// Whether the session is in memory; a step in flight is not visible yet.
    pub active: bool,
}

/// An extension entry as `session.entries` lists it and `session.appendEntry`
/// answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EntryView {
    pub id: String,
    /// RFC 3339.
    pub ts: String,
    pub ns: String,
    pub event: String,
    /// The host's own content.
    pub payload: serde_json::Value,
    /// Messages before this entry, in `session.history`'s coordinates.
    pub at: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EntryList {
    pub entries: Vec<EntryView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionKindName {
    Primary,
    Sidechain,
}

impl From<history::entry::SessionKind> for SessionKindName {
    fn from(kind: history::entry::SessionKind) -> Self {
        match kind {
            history::entry::SessionKind::Primary => Self::Primary,
            history::entry::SessionKind::Sidechain => Self::Sidechain,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExportedSession {
    pub session_id: String,
    pub kind: SessionKindName,
    pub parent_session_id: Option<String>,
    pub entry_count: usize,
}

/// What `session.export` wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExportSummary {
    pub session_id: String,
    pub path: String,
    pub format: ArchiveFormat,
    /// The archive format's version, as in `manifest.json`.
    pub format_version: u32,
    /// Size on disk.
    pub bytes: u64,
    /// The moment the snapshot was cut, RFC 3339.
    pub cut_at: String,
    pub sessions: Vec<ExportedSession>,
    /// Files under `attachments/`.
    pub attachments: usize,
    pub recording: bool,
    pub warnings: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    #[default]
    Zip,
    Dir,
}
