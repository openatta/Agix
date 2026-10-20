//! jsonl 行级数据模型。
//!
//! `EnvelopedEntry` = 顶层字段（v / id / ts / session_id）+ flatten 进 `LogEntry`。

use crate::blob::BlobRef;
use base::id::Id;
use base::message::{ContentBlock, Message, StopReason, ToolResultContent};
use base::session::SessionId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 当前 schema 版本。旧历史文件无 `v` 字段 → 反序列化时按 0 解读
/// （由 default 兜底）。
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// 完整一行 = envelope + 内嵌的具体 entry。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopedEntry {
    /// schema 版本。default = 0（兼容旧格式）
    #[serde(default)]
    pub v: u32,

    /// 行级 id（dedup / resume 用）
    pub id: Id,

    /// 行写入时刻（UTC RFC3339）
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,

    /// 与 jsonl 文件名相同；冗余存供跨文件聚合
    pub session_id: SessionId,

    /// Optional transcript topology pointer. Old logs do not have this field.
    /// Writers can fill it when preserving a branch/sidechain conversation
    /// graph matters; the default linear replay path ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Id>,

    /// Marks entries written by subagents or side conversations. Kept optional
    /// for backward compatibility with older linear transcripts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_sidechain: bool,

    /// The turn this line was written in. Absent on lines written before
    /// turns were recorded, and on lines written outside any turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,

    /// Who a user-role line came from. Absent on older lines, where it is
    /// not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,

    #[serde(flatten)]
    pub entry: LogEntry,
}

/// Where a user-role message in the conversation came from. The model sees
/// them all as user turns; a person reading the transcript should not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    /// Typed by the person.
    User,
    /// Sent by the person while a turn was already running.
    Steer,
    /// The expansion of a slash command the person ran.
    Command,
    /// Put there by the engine or a tool: reminders, recalled memory, a
    /// skill's instructions.
    Injected,
    /// Tool results answering the model's previous reply.
    ToolResult,
}

/// What a writer knows about a line beyond its content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryTags {
    pub turn_id: Option<String>,
    pub origin: Option<MessageOrigin>,
}

impl EnvelopedEntry {
    /// Create a new enveloped entry with the current schema version, a fresh
    /// ID, and the current timestamp. No parent linkage or sidechain marker
    /// is set by default — use [`with_parent`] / [`as_sidechain`] for that.
    pub fn new(session_id: SessionId, entry: LogEntry) -> Self {
        Self::new_in(
            &base::interface::environment::SystemEnvironment,
            session_id,
            entry,
        )
    }

    /// The same, with the id and the timestamp coming from `env`.
    ///
    /// These two fields are the whole reason the contract exists: they are
    /// written once and kept forever, so a log replayed under a fixed
    /// environment is the same log rather than a similar one.
    pub fn new_in(
        env: &dyn base::interface::environment::Environment,
        session_id: SessionId,
        entry: LogEntry,
    ) -> Self {
        Self {
            v: CURRENT_SCHEMA_VERSION,
            id: env.new_id(),
            ts: env.now(),
            session_id,
            parent_id: None,
            is_sidechain: false,
            turn_id: None,
            origin: None,
            entry,
        }
    }

    pub fn with_tags(mut self, tags: EntryTags) -> Self {
        self.turn_id = tags.turn_id;
        self.origin = tags.origin;
        self
    }

    /// Link this entry to a parent entry, establishing a causal relationship
    /// in the history graph (e.g. a tool result that was spawned by a
    /// specific assistant turn).
    pub fn with_parent(mut self, parent_id: Id) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    /// Mark this entry as a sidechain entry — one that branches off the main
    /// conversation timeline (e.g. a sub-agent's background work) rather
    /// than belonging to the primary turn sequence.
    pub fn as_sidechain(mut self) -> Self {
        self.is_sidechain = true;
        self
    }
}

/// `Meta.schema_version` default for files written before this field existed.
fn default_meta_schema_version() -> u32 {
    1
}

/// Current `Meta` payload version. Only newly-created sessions write this
/// value; existing on-disk files keep whatever they were written with and
/// are never rewritten.
pub const CURRENT_META_SCHEMA_VERSION: u32 = 2;

/// Primary (user-facing) vs. sidechain (sub-agent / team member) session.
/// Both are stored and queried identically — this only changes default
/// visibility in `session.list` and resumability once terminal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    Primary,
    Sidechain,
}

/// 行的具体 kind。`#[serde(tag = "kind")]` 让 JSON 用 `"kind": "user"` 区分。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogEntry {
    /// 会话起始；每个 jsonl 文件首行都是这个。
    Meta {
        cwd: String,
        #[serde(with = "time::serde::rfc3339")]
        started_at: OffsetDateTime,
        model: String,
        permission_mode: String,
        engine_version: String,
        attacode_version: String,
        /// The session ID that spawned this session (parent-child relationship).
        /// Set when a sub-agent or forked conversation is created from an existing session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session_id: Option<String>,
        /// Scene id this session was created under. Missing on pre-v2 files —
        /// callers infer it from the resume/fork request instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scene: Option<String>,
        /// Absolute project root, or `None` for a no-project session.
        /// Missing (not merely `null`) on pre-v2 files.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_root: Option<String>,
        #[serde(default)]
        session_kind: SessionKind,
        #[serde(default = "default_meta_schema_version")]
        schema_version: u32,
    },

    /// 用户消息（含粘贴 / 图像；进 API）
    User { content: Vec<ContentBlock> },

    /// 模型消息（含 thinking / tool_use；进 API 和 transcript）
    Assistant {
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageRecord>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },

    /// 工具结果（送 API 时被引擎包到 user message 里）
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },

    /// UI-only 系统消息：本地命令输出 / 提醒 / 通知 —— **不**送 API
    System {
        subkind: SystemSubkind,
        text: String,
    },

    /// 一次压缩动作的标记
    Compact {
        before_tokens: u64,
        after_tokens: u64,
        /// 压缩生成的 summary 块所属 assistant 行的 envelope id
        summary_block_id: Option<Id>,
        /// 压缩后的完整替换历史。优先于 summary 用于重建投影视图。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement_history: Option<Vec<Message>>,
        /// 压缩产物的消息内容。通常是单条带 summary marker 的 user message。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<Vec<ContentBlock>>,
        /// Snip metadata: UUIDs of messages removed by snip compaction.
        /// Used during resume to filter out removed messages.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snip_removed_uuids: Option<Vec<String>>,
    },

    /// 周期性 cost 快照
    UsageSnapshot {
        total_input: u64,
        total_output: u64,
        total_cache_creation: u64,
        total_cache_read: u64,
        total_cost_usd: f64,
    },

    /// The pre-`Blob` externalization form: a paste id and nothing else.
    ///
    /// Still read, never written. Without a store name there is no way to ask
    /// whether the backend holding it is even mounted, which is why the shape
    /// changed; a load resolves it against whatever store is mounted and
    /// leaves it alone if that store does not have it.
    PasteRef { paste_id: String },

    /// Content kept outside the log, with a pointer to where.
    ///
    /// Written in place of a `User`, `Assistant` or `ToolResult` entry whose
    /// content is large or carries an image — things that cost a lot to keep
    /// inline and that nobody reads as text. A load with the naming store
    /// mounted puts the original entry back, and nothing above the store ever
    /// sees this variant.
    ///
    /// # An unresolvable reference is inert, never an error
    ///
    /// The blob may be in a store this process did not mount, or cleaned up,
    /// or left behind when a backend was uninstalled. In every one of those
    /// cases the session still loads, still forks and still resumes — with a
    /// gap where the content was. Refusing to open a conversation because
    /// part of it is unreachable trades a degraded session for no session,
    /// which is the trade [`Extension`](Self::Extension) already refuses.
    Blob { blob: BlobRef },

    /// Marks a sidechain session as having run its one-shot task to
    /// conclusion. Only written for sub-agent sessions that returned from
    /// their single turn on their own (see `run_sub_inner`/`run_sub_tagged`
    /// in `runtime::agent_tool`) — a session with no `SessionEnd` line is
    /// either primary, still running, or was cut off externally (process
    /// exit, `agent.stop`, parent crash), all of which stay resumable.
    /// `session.resume` rejects a sidechain that has one with
    /// `SIDECHAIN_TERMINAL` (docs/daemon_rpc_protocol.md §3.3/§9).
    SessionEnd { state: SessionEndState },

    /// State belonging to something outside the kernel — a plugin, a script,
    /// a host's own bookkeeping.
    ///
    /// The engine's rule is that state lives in the log. A closed enum makes
    /// that rule impossible to follow for anyone extending the engine: their
    /// state has to live somewhere else, out of order with everything it
    /// happened between, and gone on resume. This is the variant that lets
    /// them follow it.
    ///
    /// The kernel does not interpret `payload`. What it does promise is
    /// ordering against every other entry, persistence, and that the entry
    /// survives a load / fork / resume unchanged.
    ///
    /// # An unknown `ns` is inert, never an error
    ///
    /// A log outlives the extension that wrote it. Uninstall a plugin and its
    /// entries are still there, and the session must still load, fork and
    /// resume exactly as before — an unreadable line is not a reason to
    /// refuse a conversation. That works because nothing here parses
    /// `payload`: an entry whose `ns` nobody claims is carried along and
    /// otherwise ignored.
    ///
    /// Whether an extension entry can become something the model sees is a
    /// separate question, and today the answer is no — projection skips it.
    Extension {
        /// Who it belongs to. A plugin name or script path; the kernel treats
        /// it as an opaque key and only ever compares it.
        ns: String,
        /// What happened, in the extension's own vocabulary. Named `event`
        /// rather than the more natural `kind` because `kind` is this enum's
        /// own serde tag, and a variant field cannot share it.
        event: String,
        /// The extension's state. Opaque here.
        #[serde(default)]
        payload: serde_json::Value,
    },
}

impl LogEntry {
    /// The line's `kind` tag — the same string the JSON carries.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Meta { .. } => "meta",
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolResult { .. } => "tool_result",
            Self::System { .. } => "system",
            Self::Compact { .. } => "compact",
            Self::UsageSnapshot { .. } => "usage_snapshot",
            Self::PasteRef { .. } => "paste_ref",
            Self::Blob { .. } => "blob",
            Self::SessionEnd { .. } => "session_end",
            Self::Extension { .. } => "extension",
        }
    }
}

/// Outcome recorded by [`LogEntry::SessionEnd`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndState {
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemSubkind {
    LocalCommand,
    Reminder,
    Notice,
}

/// API 返回的 usage 字段在 jsonl 里的形状（与 Anthropic SSE 的 Usage 对齐）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageRecord {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `kind()` is only worth having if it is the tag readers already know.
    #[test]
    fn the_kind_label_is_the_serde_tag() {
        let entries = [
            LogEntry::User { content: vec![] },
            LogEntry::Assistant {
                content: vec![],
                stop_reason: None,
                usage: None,
                model: None,
            },
            LogEntry::ToolResult {
                tool_use_id: "t".into(),
                content: ToolResultContent::Text(String::new()),
                is_error: false,
            },
            LogEntry::System {
                subkind: SystemSubkind::Notice,
                text: String::new(),
            },
            LogEntry::UsageSnapshot {
                total_input: 0,
                total_output: 0,
                total_cache_creation: 0,
                total_cache_read: 0,
                total_cost_usd: 0.0,
            },
            LogEntry::PasteRef {
                paste_id: "p".into(),
            },
            LogEntry::SessionEnd {
                state: SessionEndState::Completed,
            },
            LogEntry::Extension {
                ns: "n".into(),
                event: "e".into(),
                payload: json!({}),
            },
        ];
        for entry in entries {
            let label = entry.kind();
            let tag = serde_json::to_value(&entry).unwrap();
            assert_eq!(tag["kind"], label, "{label} disagrees with what is written");
        }
    }

    #[test]
    fn user_entry_roundtrip() {
        let env = EnvelopedEntry::new(
            SessionId::new(),
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "hi".into(),
                    cache_control: None,
                }],
            },
        );
        let s = serde_json::to_string(&env).unwrap();
        let back: EnvelopedEntry = serde_json::from_str(&s).unwrap();
        match back.entry {
            LogEntry::User { content } => {
                assert_eq!(content.len(), 1);
            }
            _ => panic!(),
        }
        assert_eq!(back.v, CURRENT_SCHEMA_VERSION);
        assert!(back.parent_id.is_none());
        assert!(!back.is_sidechain);
    }

    #[test]
    fn assistant_with_stop_reason_and_usage() {
        let env = EnvelopedEntry::new(
            SessionId::new(),
            LogEntry::Assistant {
                content: vec![ContentBlock::Text {
                    text: "ok".into(),
                    cache_control: None,
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(UsageRecord {
                    input_tokens: 100,
                    output_tokens: 5,
                    ..Default::default()
                }),
                model: Some("claude-sonnet-4-6".into()),
            },
        );
        let s = serde_json::to_string(&env).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "assistant");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 100);
    }

    #[test]
    fn meta_entry_uses_kind_tag() {
        let now = OffsetDateTime::now_utc();
        let env = EnvelopedEntry::new(
            SessionId::new(),
            LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: now,
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                attacode_version: "0.0.1".into(),
                parent_session_id: None,
                scene: Some("coding".into()),
                project_root: Some("/repo".into()),
                session_kind: SessionKind::Primary,
                schema_version: CURRENT_META_SCHEMA_VERSION,
            },
        );
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["kind"], "meta");
        assert_eq!(v["cwd"], "/tmp");
        assert_eq!(v["scene"], "coding");
        assert_eq!(v["session_kind"], "primary");
        assert_eq!(v["schema_version"], 2);
        assert!(v["v"].is_number());
    }

    /// A pre-v2 `Meta` line (no `scene`/`project_root`/`session_kind`/
    /// `schema_version`) must still deserialize — these fields default to
    /// "unknown" rather than failing the whole file to parse. Callers infer
    /// the scene from the resume/fork request instead.
    #[test]
    fn meta_entry_missing_v2_fields_defaults_to_inferred() {
        let session = SessionId::new();
        let raw = json!({
            "id": Id::new(),
            "ts": "2026-05-04T00:00:00Z",
            "session_id": session,
            "kind": "meta",
            "cwd": "/tmp",
            "started_at": "2026-05-04T00:00:00Z",
            "model": "claude-sonnet-4-6",
            "permission_mode": "default",
            "engine_version": "0.0.1",
            "attacode_version": "0.0.1",
        });
        let env: EnvelopedEntry = serde_json::from_value(raw).unwrap();
        match env.entry {
            LogEntry::Meta {
                scene,
                project_root,
                session_kind,
                schema_version,
                parent_session_id,
                ..
            } => {
                assert_eq!(scene, None);
                assert_eq!(project_root, None);
                assert_eq!(session_kind, SessionKind::Primary);
                assert_eq!(schema_version, 1);
                assert_eq!(parent_session_id, None);
            }
            other => panic!("expected LogEntry::Meta, got {other:?}"),
        }
    }

    #[test]
    fn session_end_entry_roundtrips_its_state() {
        let env = EnvelopedEntry::new(
            SessionId::new(),
            LogEntry::SessionEnd {
                state: SessionEndState::Failed,
            },
        );
        let s = serde_json::to_string(&env).unwrap();
        let back: EnvelopedEntry = serde_json::from_str(&s).unwrap();
        match back.entry {
            LogEntry::SessionEnd { state } => assert_eq!(state, SessionEndState::Failed),
            other => panic!("expected LogEntry::SessionEnd, got {other:?}"),
        }
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["kind"], "session_end");
        assert_eq!(v["state"], "failed");
    }

    #[test]
    fn missing_v_treated_as_zero() {
        let session = SessionId::new();
        let id = Id::new();
        let raw = json!({
            "id": id,
            "ts": "2026-05-04T00:00:00Z",
            "session_id": session,
            "kind": "user",
            "content": []
        });
        let env: EnvelopedEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(env.v, 0);
        assert!(env.parent_id.is_none());
        assert!(!env.is_sidechain);
    }

    #[test]
    fn envelope_topology_fields_roundtrip() {
        let parent = Id::new();
        let env = EnvelopedEntry::new(
            SessionId::new(),
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "side".into(),
                    cache_control: None,
                }],
            },
        )
        .with_parent(parent)
        .as_sidechain();

        let s = serde_json::to_string(&env).unwrap();
        let back: EnvelopedEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.parent_id, Some(parent));
        assert!(back.is_sidechain);
    }

    #[test]
    fn tool_result_string_form_decodes() {
        let session = SessionId::new();
        let raw = json!({
            "v": 1,
            "id": Id::new(),
            "ts": "2026-05-04T00:00:00Z",
            "session_id": session,
            "kind": "tool_result",
            "tool_use_id": "toolu_01",
            "content": "stdout",
        });
        let env: EnvelopedEntry = serde_json::from_value(raw).unwrap();
        match env.entry {
            LogEntry::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "toolu_01");
                assert_eq!(content, ToolResultContent::Text("stdout".into()));
                assert!(!is_error);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn compact_with_summary_roundtrips() {
        let session = SessionId::new();
        let env = EnvelopedEntry::new(
            session,
            LogEntry::Compact {
                before_tokens: 100,
                after_tokens: 40,
                summary_block_id: Some(Id::new()),
                replacement_history: Some(vec![Message::User {
                    content: vec![ContentBlock::Text {
                        text: "summary".into(),
                        cache_control: None,
                    }],
                }]),
                summary: Some(vec![ContentBlock::Text {
                    text: "summary".into(),
                    cache_control: None,
                }]),
                snip_removed_uuids: None,
            },
        );
        let s = serde_json::to_string(&env).unwrap();
        let back: EnvelopedEntry = serde_json::from_str(&s).unwrap();
        match back.entry {
            LogEntry::Compact {
                summary,
                replacement_history,
                ..
            } => {
                assert!(replacement_history.is_some());
                assert!(summary.is_some());
            }
            _ => panic!(),
        }
    }
}
