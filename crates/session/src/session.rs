//! SessionManager — in-memory conversation state backed by history crate for persistence.
//!
//! Owns the active message buffer and turn counter. Persistence (JSONL append/load,
//! session listing) is delegated to `history::store::HistoryStore`.

use crate::session_memory::SessionMemory;
use base::interface::model::ModelMessage;
use base::session::SessionId;
use history::store::HistoryStore;
pub use history::store::SessionSummary;
use std::sync::Arc;
use std::time::Instant;

/// Session manager. Owns the conversation state; delegates persistence to `HistoryStore`.
pub struct SessionManager {
    pub messages: Vec<ModelMessage>,
    /// Per-message wall-clock timestamps (parallel to `messages`).
    /// Used by time-based micro-compaction to determine message age.
    /// Not serialized — ephemeral for the current session only.
    pub message_timestamps: Vec<Instant>,
    pub turn_count: u32,
    pub session_id: String,
    /// Backing store for JSONL persistence (canonical: `history::JsonlHistoryStore`).
    /// When `None`, persist/resume/list are no-ops.
    history_store: Option<Arc<dyn HistoryStore>>,
    /// Auto-maintenance handle for the `session_memory.md` sidecar file.
    /// When `Some`, the runtime may check staleness and prompt the model to
    /// update its cross-session notes.
    pub session_memory: Option<SessionMemory>,
    /// The session ID that spawned this session (parent-child relationship).
    /// Set when a sub-agent or forked conversation is created from an existing session.
    parent_session_id: Option<String>,
    /// Index into `messages` up to which entries have already been appended
    /// to `history_store` — `persist()` only writes `messages[persisted_up_to..]`
    /// each time it's called, so repeated calls (once per turn) don't
    /// re-append the same messages.
    persisted_up_to: usize,
    /// How this session judges its own size — see
    /// [`SessionManager::set_token_counter`]. `None` means the engine's
    /// default estimator.
    token_counter: Option<Arc<dyn base::interface::token_counter::TokenCounter>>,
}

impl SessionManager {
    /// Create a new session manager.
    /// `history_store`: backing store for persistence. None = no persistence.
    /// `session_id`: optional pre-set ID; if None, a fresh [`SessionId`] is
    /// generated. Must be parseable by [`SessionId::parse`] — `persist()`
    /// and the `session_memory.md` sidecar both key off the parsed form.
    /// `parent_session_id`: optional ID of the session that spawned this one.
    pub fn new(
        history_store: Option<Arc<dyn HistoryStore>>,
        session_id: Option<String>,
        parent_session_id: Option<String>,
    ) -> Self {
        let session_id = session_id.unwrap_or_else(|| SessionId::new().to_string());
        Self {
            messages: Vec::new(),
            message_timestamps: Vec::new(),
            turn_count: 0,
            session_id,
            history_store,
            session_memory: None,
            parent_session_id,
            persisted_up_to: 0,
            token_counter: None,
        }
    }

    /// Convenience: create a no-persistence session manager.
    pub fn in_memory(session_id: Option<String>) -> Self {
        Self::new(None, session_id, None)
    }

    /// Attach a SessionMemory handle. The underlying file is not created until
    /// [`SessionMemory::init_session_memory`] is called explicitly.
    pub fn with_session_memory(mut self, sm: SessionMemory) -> Self {
        self.session_memory = Some(sm);
        self
    }

    // ── In-memory state (Engine uses these directly) ──

    pub fn push_message(&mut self, msg: ModelMessage) {
        self.message_timestamps.push(Instant::now());
        self.messages.push(msg);
    }

    pub fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    /// Estimated input tokens for the current conversation.
    ///
    /// This is the number the runtime compares against
    /// `token_budget().compact_threshold`, so it must agree with the estimate
    /// the compaction strategies use to decide what to drop. It therefore
    /// delegates to the workspace's single tiktoken-based estimator rather than
    /// approximating.
    ///
    /// It used to be `text.len() / 4` with a flat 50 tokens charged to every
    /// non-text block — which valued a 40 KB tool result and an empty one
    /// identically at 50 tokens, i.e. exactly the blocks that fill a context
    /// window were the ones it could not see.
    pub fn token_count(&self) -> usize {
        match &self.token_counter {
            Some(counter) => counter.count_messages(&self.messages),
            None => model::tokens::estimate_message_tokens(&self.messages),
        }
    }

    /// Judge this session's size with `counter` instead of the engine's local
    /// estimate.
    ///
    /// The default is documented to run 5–15% high, because Anthropic does not
    /// publish its tokenizer and a local `cl100k_base` approximation is the
    /// cheap stand-in. A host talking to a provider that does publish one, or
    /// willing to spend a call on `count_tokens`, can be exact — and the
    /// number this returns is the one compaction triggers on, so being exact
    /// here is worth something.
    pub fn set_token_counter(
        &mut self,
        counter: Arc<dyn base::interface::token_counter::TokenCounter>,
    ) {
        self.token_counter = Some(counter);
    }

    pub fn increment_turn(&mut self) {
        self.turn_count += 1;
    }

    /// Return a vector of (message_index, created_at) pairs for time-based
    /// micro-compaction. Messages without a tracked timestamp get `Instant::now()`
    /// as fallback (effectively treating them as fresh).
    pub fn message_ages(&self) -> Vec<(usize, Instant)> {
        self.messages
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let ts = self
                    .message_timestamps
                    .get(i)
                    .copied()
                    .unwrap_or(Instant::now());
                (i, ts)
            })
            .collect()
    }

    // ── Persistence (delegated to history store) ──

    /// Persist messages pushed since the last `persist()` call via the
    /// history store — incremental append, not a full snapshot. Called once
    /// per turn (see `runtime::turn::run_user_turn`). No-op when no store is
    /// configured, or when nothing new has been pushed since the last call.
    ///
    /// `MessageRole::System` entries are skipped (`ModelMessage` rarely if
    /// ever carries this role — the system prompt is assembled separately
    /// per-turn, not stored as a session message — but this keeps `persist()`
    /// total rather than panicking if one ever does appear).
    pub async fn persist(&mut self) -> Result<(), SessionError> {
        let Some(store) = self.history_store.clone() else {
            return Ok(());
        };
        let sid =
            SessionId::parse(&self.session_id).map_err(|e| SessionError::Id(e.to_string()))?;
        while self.persisted_up_to < self.messages.len() {
            let msg = &self.messages[self.persisted_up_to];
            if let Some(entry) = model_message_to_log_entry(msg) {
                store
                    .append(sid, entry)
                    .await
                    .map_err(|e| SessionError::Store(e.to_string()))?;
            }
            self.persisted_up_to += 1;
        }
        Ok(())
    }

    /// Replace `self.messages` wholesale with a post-compaction list, and
    /// keep persistence consistent with that replacement.
    ///
    /// `persist()`'s incremental scan (`messages[persisted_up_to..]`) assumes
    /// `messages` only ever grows by appending — compaction breaks that
    /// assumption by replacing the whole vec with a shorter one. Left alone,
    /// `persisted_up_to` would stay at its pre-compaction (larger) value and
    /// `persist()`'s `while persisted_up_to < messages.len()` would go
    /// permanently false, silently stopping all further persistence for the
    /// rest of the session (until the message count organically grew back
    /// past the stale cursor). This method resets the cursor to match the
    /// new, shorter list, and — since a `LogEntry::Compact` marker isn't
    /// itself an entry in `messages` and so would never be picked up by
    /// `persist()`'s scan — appends it immediately rather than waiting for
    /// the next `persist()` call.
    ///
    /// If writing that marker fails (disk error, bad session id, ...), the
    /// cursor is left at `0` instead of `messages.len()` — jumping it to the
    /// full length regardless of outcome would permanently hide these
    /// messages from `persist()`'s scan (they'd never be picked up, since
    /// the *only* record of them, the `LogEntry::Compact` entry, didn't
    /// land). Leaving it at `0` means the very next `persist()` call writes
    /// them out as ordinary `User`/`Assistant` entries instead — losing the
    /// "this was a compaction boundary" framing on disk, but not the
    /// content itself.
    ///
    /// No-op on the persistence side (still replaces `self.messages` and
    /// sets the cursor to the full length, since there's nothing to persist
    /// either way) when no `history_store` is configured.
    pub async fn replace_messages_after_compact(
        &mut self,
        new_messages: Vec<ModelMessage>,
        before_tokens: u64,
        after_tokens: u64,
    ) -> Result<(), SessionError> {
        let replacement_history: Vec<base::message::Message> = new_messages
            .iter()
            .filter_map(model_message_to_message)
            .collect();

        self.messages = new_messages;

        let Some(store) = self.history_store.clone() else {
            self.persisted_up_to = self.messages.len();
            return Ok(());
        };

        let result: Result<(), SessionError> = async {
            let sid =
                SessionId::parse(&self.session_id).map_err(|e| SessionError::Id(e.to_string()))?;
            let entry = history::entry::LogEntry::Compact {
                before_tokens,
                after_tokens,
                summary_block_id: None,
                replacement_history: Some(replacement_history),
                summary: None,
                snip_removed_uuids: None,
            };
            store
                .append(sid, entry)
                .await
                .map_err(|e| SessionError::Store(e.to_string()))
        }
        .await;

        self.persisted_up_to = if result.is_ok() {
            self.messages.len()
        } else {
            0
        };
        result
    }

    /// Resume a session by loading messages from the history store and
    /// reconstructing in-memory `ModelMessage`s from the projected transcript
    /// (via `history::transcript::project_messages`, the same projection
    /// `HistoryStore::load_messages` uses). On success, extracts the
    /// `parent_session_id` from the session's Meta entry and marks all
    /// reconstructed messages as already-persisted (so a subsequent
    /// `persist()` call doesn't re-append them).
    pub async fn resume(&mut self, id: &str) -> Result<(), SessionError> {
        let store = match &self.history_store {
            Some(s) => s.clone(),
            None => return Err(SessionError::NotFound(id.to_string())),
        };
        let sid = SessionId::parse(id).map_err(|e| SessionError::Id(e.to_string()))?;
        let entries = store.load(sid).await.map_err(|e| match e {
            history::error::HistoryError::SessionNotFound(id) => SessionError::NotFound(id),
            other => SessionError::Store(other.to_string()),
        })?;
        if entries.is_empty() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        self.session_id = id.to_string();
        self.turn_count = entries.len() as u32;

        // Extract parent_session_id from the Meta entry if present.
        for entry in &entries {
            if let history::entry::LogEntry::Meta {
                parent_session_id, ..
            } = &entry.entry
            {
                self.parent_session_id = parent_session_id.clone();
                break;
            }
        }

        let projected = history::transcript::project_messages_with(&entries, store.projection());
        self.messages = projected
            .iter()
            .filter_map(message_to_model_message)
            .collect();
        self.message_timestamps = vec![Instant::now(); self.messages.len()];
        self.persisted_up_to = self.messages.len();

        Ok(())
    }

    /// Access the session ID (read-only).
    pub fn session_id_str(&self) -> &str {
        &self.session_id
    }

    /// Set/switch the current session ID.
    pub fn set_session_id(&mut self, id: String) {
        self.session_id = id;
    }

    /// Clear all messages and reset turn counter (for `/clear` command).
    ///
    /// Recorded as a compaction to nothing, so a resume after it starts empty
    /// too and later messages keep landing on disk.
    pub async fn clear(&mut self) -> Result<(), SessionError> {
        self.message_timestamps.clear();
        self.turn_count = 0;
        self.replace_messages_after_compact(Vec::new(), 0, 0).await
    }

    // ── Parent session tracking ──

    /// Set the session ID of the parent session that spawned this one.
    pub fn set_parent_session(&mut self, parent_id: String) {
        self.parent_session_id = Some(parent_id);
    }

    /// Return the parent session ID, if set.
    pub fn parent_session_id(&self) -> Option<&str> {
        self.parent_session_id.as_deref()
    }

    /// List all child sessions (sessions whose Meta entry has `parent_session_id`
    /// pointing to this session). Delegates to the HistoryStore.
    pub async fn child_sessions(&self) -> Result<Vec<String>, SessionError> {
        let store = match &self.history_store {
            Some(s) => s,
            None => return Ok(vec![]),
        };
        let children = store
            .child_sessions(&self.session_id)
            .await
            .map_err(|e| SessionError::Store(e.to_string()))?;
        Ok(children.into_iter().map(|s| s.to_string()).collect())
    }

    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: SessionId::parse(&self.session_id).unwrap_or_default(),
            last_modified: String::new(),
            entry_count: self.messages.len(),
            message_count: self.messages.len(),
            preview: String::new(),
            canonical_cwd: None,
            title: None,
            total_input_tokens: None,
            total_output_tokens: None,
            compact_count: 0,
        }
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, SessionError> {
        let store = match &self.history_store {
            Some(s) => s,
            None => return Ok(vec![]),
        };
        let sids = store
            .list_sessions()
            .await
            .map_err(|e| SessionError::Store(e.to_string()))?;
        // For each session, load a summary — simplified for now
        let mut out = Vec::new();
        for sid in sids {
            out.push(SessionSummary {
                session_id: sid,
                last_modified: String::new(),
                entry_count: 0,
                message_count: 0,
                preview: String::new(),
                canonical_cwd: None,
                title: None,
                total_input_tokens: None,
                total_output_tokens: None,
                compact_count: 0,
            });
        }
        Ok(out)
    }

    /// 从 HistoryStore 中删除指定 session 的全部持久化数据。
    /// 库模式下由用户自行管理 Agent 实例生命周期；此方法仅操作磁盘。
    pub async fn delete_session(&self, id: &str) -> Result<(), SessionError> {
        let store = match &self.history_store {
            Some(s) => s,
            None => return Err(SessionError::NotFound(id.to_string())),
        };
        let sid = SessionId::parse(id).map_err(|e| SessionError::Id(e.to_string()))?;
        store
            .delete(sid)
            .await
            .map_err(|e| SessionError::Store(e.to_string()))
    }
}

// ── ModelMessage <-> history::LogEntry/Message conversion ──
//
// Two independent, block-level conversions (write direction and read
// direction) rather than a single bidirectional mapping — the storage format
// `base::message::ContentBlock` covers shapes a live `ModelMessage` cannot
// hold (URL images, `CacheEdits`), so the two directions aren't symmetric and
// are clearer written separately.

fn model_content_block_to_content_block(
    b: &base::interface::model::ModelContentBlock,
) -> base::message::ContentBlock {
    use base::interface::model::ModelContentBlock as M;
    use base::message::ContentBlock as C;
    match b {
        M::Text { text } => C::Text {
            text: text.clone(),
            cache_control: None,
        },
        M::Image { media_type, data } => C::Image {
            source: base::message::ImageSource::Base64 {
                media_type: media_type.clone(),
                data: data.clone(),
            },
        },
        M::Thinking {
            text,
            signature,
            provider,
        } => C::Thinking {
            thinking: text.clone(),
            signature: signature.clone(),
            provider: provider.clone(),
        },
        M::RedactedThinking { data, provider } => C::RedactedThinking {
            data: data.clone(),
            provider: provider.clone(),
        },
        M::ToolUse { id, name, input } => C::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        M::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => C::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: base::message::ToolResultContent::Text(content.clone()),
            is_error: is_error.unwrap_or(false),
        },
    }
}

/// `None` for message roles/content blocks with no `LogEntry` equivalent —
/// currently only `MessageRole::System` (see `persist()` doc comment).
fn model_message_to_log_entry(msg: &ModelMessage) -> Option<history::entry::LogEntry> {
    use base::interface::model::MessageRole;
    let content: Vec<base::message::ContentBlock> = msg
        .content
        .iter()
        .map(model_content_block_to_content_block)
        .collect();
    match msg.role {
        MessageRole::User => Some(history::entry::LogEntry::User { content }),
        MessageRole::Assistant => Some(history::entry::LogEntry::Assistant {
            content,
            stop_reason: None,
            usage: None,
            model: None,
        }),
        MessageRole::System => None,
    }
}

/// `None` for `MessageRole::System` — same skip as `model_message_to_log_entry`,
/// which this mirrors except it wraps content in `base::message::Message`
/// (for `LogEntry::Compact.replacement_history`) instead of a bare `LogEntry`.
fn model_message_to_message(msg: &ModelMessage) -> Option<base::message::Message> {
    use base::interface::model::MessageRole;
    let content: Vec<base::message::ContentBlock> = msg
        .content
        .iter()
        .map(model_content_block_to_content_block)
        .collect();
    match msg.role {
        MessageRole::User => Some(base::message::Message::User { content }),
        MessageRole::Assistant => Some(base::message::Message::Assistant {
            content,
            stop_reason: None,
            model: None,
        }),
        MessageRole::System => None,
    }
}

fn content_block_to_model_content_block(
    b: &base::message::ContentBlock,
) -> Option<base::interface::model::ModelContentBlock> {
    use base::interface::model::ModelContentBlock as M;
    use base::message::ContentBlock as C;
    match b {
        C::Text { text, .. } => Some(M::Text { text: text.clone() }),
        C::ToolUse { id, name, input } => Some(M::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        }),
        C::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let content_str = match content {
                base::message::ToolResultContent::Text(s) => s.clone(),
                base::message::ToolResultContent::Blocks(blocks) => {
                    serde_json::to_string(blocks).unwrap_or_default()
                }
            };
            Some(M::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content_str,
                is_error: Some(*is_error),
            })
        }
        C::Image {
            source: base::message::ImageSource::Base64 { media_type, data },
        } => Some(M::Image {
            media_type: media_type.clone(),
            data: data.clone(),
        }),
        C::Thinking {
            thinking,
            signature,
            provider,
        } => Some(M::Thinking {
            text: thinking.clone(),
            signature: signature.clone(),
            provider: provider.clone(),
        }),
        C::RedactedThinking { data, provider } => Some(M::RedactedThinking {
            data: data.clone(),
            provider: provider.clone(),
        }),
        C::Image {
            source: base::message::ImageSource::Url { .. },
        }
        | C::CacheEdits { .. } => None,
    }
}

/// `None` for `Message::System` — matches `model_message_to_log_entry`'s
/// symmetric skip on the write side.
fn message_to_model_message(msg: &base::message::Message) -> Option<ModelMessage> {
    use base::interface::model::MessageRole;
    match msg {
        base::message::Message::User { content } => Some(ModelMessage {
            role: MessageRole::User,
            content: content
                .iter()
                .filter_map(content_block_to_model_content_block)
                .collect(),
        }),
        base::message::Message::Assistant { content, .. } => Some(ModelMessage {
            role: MessageRole::Assistant,
            content: content
                .iter()
                .filter_map(content_block_to_model_content_block)
                .collect(),
        }),
        base::message::Message::System { .. } => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("invalid id: {0}")]
    Id(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{MessageRole, ModelContentBlock};

    fn make_text_msg(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn new_session_starts_empty() {
        let mgr = SessionManager::in_memory(None);
        assert_eq!(mgr.messages.len(), 0);
        assert_eq!(mgr.turn_count, 0);
        assert!(!mgr.session_id.is_empty());
    }

    #[test]
    fn new_with_session_id_preserves_it() {
        let mgr = SessionManager::in_memory(Some("test-session-1".into()));
        assert_eq!(mgr.session_id, "test-session-1");
    }

    #[test]
    fn push_message_appends() {
        let mut mgr = SessionManager::in_memory(None);
        mgr.push_message(make_text_msg("hello"));
        assert_eq!(mgr.messages.len(), 1);
        mgr.push_message(make_text_msg("world"));
        assert_eq!(mgr.messages.len(), 2);
    }

    #[test]
    fn messages_method_returns_all() {
        let mut mgr = SessionManager::in_memory(None);
        mgr.push_message(make_text_msg("a"));
        mgr.push_message(make_text_msg("b"));
        assert_eq!(mgr.messages().len(), 2);
    }

    #[test]
    fn increment_turn_monotonic() {
        let mut mgr = SessionManager::in_memory(None);
        assert_eq!(mgr.turn_count, 0);
        mgr.increment_turn();
        assert_eq!(mgr.turn_count, 1);
        mgr.increment_turn();
        assert_eq!(mgr.turn_count, 2);
    }

    #[test]
    fn token_count_empty_is_zero() {
        let mgr = SessionManager::in_memory(None);
        assert_eq!(mgr.token_count(), 0);
    }

    #[test]
    fn token_count_approximates_text_length() {
        let mut mgr = SessionManager::in_memory(None);
        // 40 chars → 40/4 = 10 tokens
        mgr.push_message(make_text_msg("hello world this is a test message here"));
        assert!(mgr.token_count() > 0);
    }

    #[test]
    fn summary_reflects_state() {
        let mut mgr = SessionManager::in_memory(Some("summary-test".into()));
        mgr.push_message(make_text_msg("hi"));
        let s = mgr.summary();
        assert_eq!(s.entry_count, 1);
        assert_eq!(s.message_count, 1);
    }

    #[test]
    fn set_session_id_updates() {
        let mut mgr = SessionManager::in_memory(None);
        mgr.set_session_id("new-id".into());
        assert_eq!(mgr.session_id_str(), "new-id");
    }

    #[tokio::test]
    async fn persist_without_store_is_noop() {
        let mut mgr = SessionManager::in_memory(None);
        let r = mgr.persist().await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn resume_without_store_returns_not_found() {
        let mut mgr = SessionManager::in_memory(None);
        let r = mgr.resume("nonexistent").await;
        assert!(r.is_err());
        match r {
            Err(SessionError::NotFound(_)) => {}
            _ => panic!("expected NotFound"),
        }
    }

    #[tokio::test]
    async fn list_sessions_without_store_returns_empty() {
        let mgr = SessionManager::in_memory(None);
        let sessions = mgr.list_sessions().await.unwrap();
        assert!(sessions.is_empty());
    }

    /// A caller that supplies no id must still get one `persist()` accepts.
    /// The generated default used to be a hyphenated UUID, which
    /// `SessionId::parse` rejects — so every `persist()` bailed before
    /// writing a byte and the only trace was a per-turn `warn!`.
    #[tokio::test]
    async fn default_session_id_is_one_persist_accepts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = SessionManager::new(Some(real_store(tmp.path()).await), None, None);
        SessionId::parse(&mgr.session_id).expect("the generated default must be a valid SessionId");

        mgr.push_message(make_text_msg("hello"));
        mgr.persist().await.expect("persist must not fail");

        let sessions = mgr.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1, "the turn must have reached disk");
    }

    async fn real_store(tmp: &std::path::Path) -> Arc<dyn HistoryStore> {
        let cwd = tmp.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &cwd,
                history::path::HistoryRoots::under(tmp),
            )
            .await
            .unwrap(),
        )
    }

    /// Wraps a real store; `append` fails while `fail_appends` is true and
    /// otherwise delegates. Lets a test simulate "the disk write for this
    /// one `LogEntry::Compact` marker failed" without the store failing
    /// forever (which would make it impossible to observe the fallback
    /// recovery — the *next* successful `persist()` writing the content out
    /// as ordinary entries).
    struct FlakyAppendStore {
        inner: Arc<dyn HistoryStore>,
        fail_appends: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl HistoryStore for FlakyAppendStore {
        async fn append(
            &self,
            session: SessionId,
            entry: history::entry::LogEntry,
        ) -> Result<(), history::error::HistoryError> {
            if self.fail_appends.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(history::error::HistoryError::Path(
                    "simulated disk failure".to_string(),
                ));
            }
            self.inner.append(session, entry).await
        }
        async fn load(
            &self,
            session: SessionId,
        ) -> Result<Vec<history::entry::EnvelopedEntry>, history::error::HistoryError> {
            self.inner.load(session).await
        }
        async fn list_sessions(&self) -> Result<Vec<SessionId>, history::error::HistoryError> {
            self.inner.list_sessions().await
        }
        async fn delete(&self, session: SessionId) -> Result<(), history::error::HistoryError> {
            self.inner.delete(session).await
        }
    }

    /// Regression guard for the `persisted_up_to` desync bug: after
    /// `replace_messages_after_compact` shrinks `messages`, the cursor must
    /// track the *new* (shorter) length, and a `LogEntry::Compact` marker
    /// must land on disk immediately — not wait for the next `persist()`.
    #[tokio::test]
    async fn replace_messages_after_compact_resets_cursor_and_writes_compact_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let sid = SessionId::new();
        let mut mgr = SessionManager::new(Some(store.clone()), Some(sid.to_string()), None);

        // 5 messages, all persisted (persisted_up_to == 5).
        for i in 0..5 {
            mgr.push_message(make_text_msg(&format!("msg {i}")));
        }
        mgr.persist().await.unwrap();
        assert_eq!(store.load(sid).await.unwrap().len(), 5);

        // Compaction shrinks 5 messages down to 1.
        let compacted = vec![make_text_msg("summary of earlier conversation")];
        mgr.replace_messages_after_compact(compacted, 1000, 100)
            .await
            .unwrap();

        assert_eq!(mgr.messages.len(), 1, "in-memory messages replaced");

        let entries = store.load(sid).await.unwrap();
        assert_eq!(
            entries.len(),
            6,
            "the 5 pre-compaction entries plus one new Compact marker"
        );
        match &entries[5].entry {
            history::entry::LogEntry::Compact {
                before_tokens,
                after_tokens,
                replacement_history,
                ..
            } => {
                assert_eq!(*before_tokens, 1000);
                assert_eq!(*after_tokens, 100);
                assert_eq!(replacement_history.as_ref().map(|h| h.len()), Some(1));
            }
            other => panic!("expected LogEntry::Compact, got {other:?}"),
        }

        // The cursor must now track the *new* (shorter) length — pushing
        // and persisting one more message must append exactly one more
        // entry, not silently no-op (the bug this guards against: a stale
        // cursor larger than the new `messages.len()` makes `persist()`'s
        // `while persisted_up_to < messages.len()` permanently false).
        mgr.push_message(make_text_msg("new message after compaction"));
        mgr.persist().await.unwrap();
        let entries = store.load(sid).await.unwrap();
        assert_eq!(
            entries.len(),
            7,
            "persist() must resume appending after the cursor reset"
        );
    }

    /// End-to-end: after a real compaction event, resuming the session from
    /// disk must reconstruct the *compacted* state, not replay the original
    /// pre-compaction messages — proving the write side
    /// (`replace_messages_after_compact`) and the pre-existing read side
    /// (`transcript::project_messages`'s `LogEntry::Compact` handling,
    /// which wholesale-replaces the projection when `replacement_history`
    /// is present) actually agree with each other.
    #[tokio::test]
    async fn resume_after_compaction_reconstructs_compacted_state_not_original() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let sid = SessionId::new();

        {
            let mut writer = SessionManager::new(Some(store.clone()), Some(sid.to_string()), None);
            for i in 0..5 {
                writer.push_message(make_text_msg(&format!("original msg {i}")));
            }
            writer.persist().await.unwrap();

            let compacted = vec![make_text_msg("compacted summary")];
            writer
                .replace_messages_after_compact(compacted, 2000, 50)
                .await
                .unwrap();

            // A message produced *after* compaction, in the same session.
            writer.push_message(make_text_msg("post-compaction message"));
            writer.persist().await.unwrap();
        }

        let mut reader = SessionManager::new(Some(store.clone()), None, None);
        reader.resume(&sid.to_string()).await.unwrap();

        // Only the compacted summary + the post-compaction message should
        // survive projection — none of the 5 original messages.
        assert_eq!(reader.messages().len(), 2, "got: {:?}", reader.messages());
        let text_of = |m: &ModelMessage| match &m.content[0] {
            ModelContentBlock::Text { text } => text.clone(),
            other => panic!("unexpected content block: {other:?}"),
        };
        assert_eq!(text_of(&reader.messages()[0]), "compacted summary");
        assert_eq!(text_of(&reader.messages()[1]), "post-compaction message");
        for i in 0..5 {
            assert!(
                !reader
                    .messages()
                    .iter()
                    .any(|m| text_of(m) == format!("original msg {i}")),
                "pre-compaction message {i} must not survive projection"
            );
        }
    }

    #[tokio::test]
    async fn replace_messages_after_compact_without_store_still_resets_in_memory_state() {
        let mut mgr = SessionManager::in_memory(None);
        mgr.push_message(make_text_msg("a"));
        mgr.push_message(make_text_msg("b"));
        mgr.persist().await.unwrap();

        let compacted = vec![make_text_msg("summary")];
        let r = mgr.replace_messages_after_compact(compacted, 500, 50).await;
        assert!(r.is_ok(), "no store configured must not error");
        assert_eq!(mgr.messages.len(), 1);
    }

    /// If the `LogEntry::Compact` marker fails to write, the compacted
    /// content must not be permanently lost: the cursor falls back to `0`
    /// (instead of jumping to the full length) so the *next* successful
    /// `persist()` call writes it out as ordinary entries. Guards against
    /// the failure-path bug the design review flagged: unconditionally
    /// setting `persisted_up_to = messages.len()` regardless of write
    /// outcome would silently and permanently drop these messages from disk.
    #[tokio::test]
    async fn replace_messages_after_compact_disk_failure_falls_back_to_next_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = real_store(tmp.path()).await;
        let flaky = Arc::new(FlakyAppendStore {
            inner: inner.clone(),
            fail_appends: std::sync::atomic::AtomicBool::new(true),
        });
        let sid = SessionId::new();
        let mut mgr: SessionManager = SessionManager::new(
            Some(flaky.clone() as Arc<dyn HistoryStore>),
            Some(sid.to_string()),
            None,
        );

        let compacted = vec![make_text_msg("compacted while disk is down")];
        let r = mgr
            .replace_messages_after_compact(compacted, 1000, 100)
            .await;
        assert!(r.is_err(), "disk failure must surface as an error");
        assert_eq!(
            mgr.messages.len(),
            1,
            "in-memory state still reflects the compaction despite the write failure"
        );
        // Nothing was ever successfully appended, so the session file was
        // never created — `load()` reports that as `SessionNotFound`, not
        // `Ok(vec![])`.
        match inner.load(sid).await {
            Err(history::error::HistoryError::SessionNotFound(_)) => {}
            other => panic!("expected SessionNotFound (nothing persisted yet), got {other:?}"),
        }

        // Disk recovers. The stale-content-loss bug this test guards
        // against would leave `persisted_up_to` at 1 (== messages.len()),
        // making this `persist()` a permanent no-op; the fix leaves it at
        // 0, so this call must write the compacted message out.
        flaky
            .fail_appends
            .store(false, std::sync::atomic::Ordering::SeqCst);
        mgr.persist().await.unwrap();
        let entries = inner.load(sid).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "the compacted message must be recoverable once persist() succeeds again"
        );
    }

    /// Two compactions back to back — each one's `replace_messages_after_compact`
    /// call must produce its own independent `Compact` marker, and the
    /// cursor must end up correctly tracking the *second* (final) message
    /// list, not some stale mix of the two.
    #[tokio::test]
    async fn two_consecutive_compactions_each_persist_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let sid = SessionId::new();
        let mut mgr = SessionManager::new(Some(store.clone()), Some(sid.to_string()), None);

        for i in 0..5 {
            mgr.push_message(make_text_msg(&format!("msg {i}")));
        }
        mgr.persist().await.unwrap();

        mgr.replace_messages_after_compact(vec![make_text_msg("first summary")], 2000, 200)
            .await
            .unwrap();
        assert_eq!(store.load(sid).await.unwrap().len(), 6);

        // Grow again, then compact a second time before the first
        // compaction's content has grown past its own cursor.
        for i in 0..3 {
            mgr.push_message(make_text_msg(&format!("post-first-compact msg {i}")));
        }
        mgr.replace_messages_after_compact(vec![make_text_msg("second summary")], 800, 50)
            .await
            .unwrap();

        assert_eq!(mgr.messages.len(), 1);
        let entries = store.load(sid).await.unwrap();
        assert_eq!(
            entries.len(),
            7,
            "5 original + first Compact marker + second Compact marker \
             (the 3 post-first-compact messages were never individually \
             persisted — folded into the second marker's replacement_history)"
        );
        match &entries[6].entry {
            history::entry::LogEntry::Compact {
                replacement_history,
                ..
            } => {
                assert_eq!(replacement_history.as_ref().map(|h| h.len()), Some(1));
            }
            other => panic!("expected LogEntry::Compact, got {other:?}"),
        }

        // Cursor must track the *second* compaction's shorter list, not the
        // 3 intermediate messages that were pushed but never persisted.
        mgr.push_message(make_text_msg("new message after second compact"));
        mgr.persist().await.unwrap();
        assert_eq!(store.load(sid).await.unwrap().len(), 8);
    }

    #[tokio::test]
    async fn persist_writes_new_messages_and_does_not_reappend_on_second_call() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let sid = SessionId::new();
        let mut mgr = SessionManager::new(Some(store.clone()), Some(sid.to_string()), None);

        mgr.push_message(make_text_msg("hello"));
        mgr.persist().await.unwrap();

        let entries = store.load(sid).await.unwrap();
        assert_eq!(entries.len(), 1, "one message should have been appended");

        // Calling persist() again with no new messages must not duplicate.
        mgr.persist().await.unwrap();
        let entries = store.load(sid).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "persist() must not re-append already-persisted messages"
        );

        // Pushing one more message and persisting appends exactly one more entry.
        mgr.push_message(make_text_msg("second message"));
        mgr.persist().await.unwrap();
        let entries = store.load(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn resume_reconstructs_messages_and_marks_them_already_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let sid = SessionId::new();

        // Write a session with one writer...
        {
            let mut writer = SessionManager::new(Some(store.clone()), Some(sid.to_string()), None);
            writer.push_message(make_text_msg("first"));
            writer.persist().await.unwrap();
        }

        // ...and reconstruct it with a fresh SessionManager via resume().
        let mut reader = SessionManager::new(Some(store.clone()), None, None);
        reader.resume(&sid.to_string()).await.unwrap();
        assert_eq!(reader.messages().len(), 1);
        match &reader.messages()[0].content[0] {
            ModelContentBlock::Text { text } => assert_eq!(text, "first"),
            other => panic!("unexpected content block: {other:?}"),
        }

        // resume() must mark reconstructed messages as already-persisted so a
        // subsequent persist() doesn't re-append them.
        reader.persist().await.unwrap();
        let entries = store.load(sid).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "resume()'d messages must not be re-persisted"
        );
    }

    /// The write side always stored these; the read side used to drop them,
    /// so a resumed session lost its reasoning (and with it, on Anthropic, the
    /// signed blocks the next tool-use request has to echo back) and its
    /// images.
    #[tokio::test]
    async fn resume_keeps_thinking_redacted_thinking_and_images() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let sid = SessionId::new();
        let assistant = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                ModelContentBlock::Thinking {
                    text: "signed".into(),
                    signature: Some("sig".into()),
                    provider: None,
                },
                ModelContentBlock::Thinking {
                    text: "unsigned".into(),
                    signature: None,
                    provider: Some("deepseek".into()),
                },
                ModelContentBlock::RedactedThinking {
                    data: "opaque".into(),
                    provider: Some("anthropic".into()),
                },
                ModelContentBlock::Text {
                    text: "answer".into(),
                },
            ],
        };
        let user = ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }],
        };
        {
            let mut writer = SessionManager::new(Some(store.clone()), Some(sid.to_string()), None);
            writer.push_message(user.clone());
            writer.push_message(assistant.clone());
            writer.persist().await.unwrap();
        }

        let mut reader = SessionManager::new(Some(store.clone()), None, None);
        reader.resume(&sid.to_string()).await.unwrap();
        let contents: Vec<_> = reader.messages().iter().map(|m| &m.content).collect();
        assert_eq!(contents, [&user.content, &assistant.content]);
    }

    #[tokio::test]
    async fn resume_missing_session_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = real_store(tmp.path()).await;
        let mut mgr = SessionManager::new(Some(store), None, None);
        let err = mgr.resume(&SessionId::new().to_string()).await.unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }
}

#[cfg(test)]
mod token_counter_tests {
    use super::*;
    use base::interface::model::{MessageRole, ModelContentBlock};
    use base::interface::token_counter::FixedTokenCounter;

    fn say(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: text.into() }],
        }
    }

    /// The number a host substitutes here is the number compaction triggers
    /// on, which is the reason the seam is worth having.
    #[test]
    fn a_substituted_counter_is_what_the_session_reports() {
        let mut session = SessionManager::in_memory(None);
        session.messages.push(say("a conversation of some length"));

        let default_estimate = session.token_count();
        assert!(default_estimate > 0);

        session.set_token_counter(Arc::new(FixedTokenCounter::new(0, 1_000)));
        assert_eq!(
            session.token_count(),
            1_000,
            "the session must ask the counter it was given, not the built-in one"
        );
    }
}
