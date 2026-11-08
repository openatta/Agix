//! Mailbox for agent-to-agent communication within a team.
//!
//! `MailboxStore` is an `Arc`-shared `HashMap<String, Vec<MailboxMessage>>`,
//! protected by a `Mutex`, with optional JSONL file persistence for cross-turn
//! survival. Three tools provide the agent-facing API:
//!
//! - `SendMessage`: push a message to a peer's mailbox
//! - `ReadMail`: drain received messages from own mailbox
//! - `ListPeers`: list all known agent labels in this team

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, ToolContext, ToolResult, ToolResultContent,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

/// A single message in an agent's mailbox.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MailboxMessage {
    pub from: String,
    pub timestamp_ms: i64,
    pub content: String,
}

/// Thread-safe mailbox store shared across all agents in a team.
/// Supports optional file-based persistence for cross-turn communication.
pub struct MailboxStore {
    inner: Mutex<MailboxInner>,
    /// When Some, messages are also appended to JSONL files in this dir.
    persist_dir: Option<std::path::PathBuf>,
}

struct MailboxInner {
    /// peer_id → FIFO messages
    mailboxes: HashMap<String, Vec<MailboxMessage>>,
    /// known peer labels
    peers: Vec<String>,
}

impl MailboxStore {
    /// Create a new mailbox store for a set of peer agents.
    pub fn new(peers: Vec<String>) -> Self {
        Self {
            inner: Mutex::new(MailboxInner {
                mailboxes: HashMap::new(),
                peers,
            }),
            persist_dir: None,
        }
    }

    /// Create a mailbox store with file persistence. Messages are appended
    /// to JSONL files (`<persist_dir>/<peer>.jsonl`) for cross-turn survival.
    pub fn with_persistence(peers: Vec<String>, persist_dir: std::path::PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&persist_dir);
        // Load existing messages from disk into the in-memory store
        let mut mailboxes: HashMap<String, Vec<MailboxMessage>> = HashMap::new();
        for peer in &peers {
            let file_path = persist_dir.join(format!("{peer}.jsonl"));
            if let Ok(contents) = std::fs::read_to_string(&file_path) {
                let mut msgs = Vec::new();
                for line in contents.lines() {
                    if let Ok(msg) = serde_json::from_str::<MailboxMessage>(line) {
                        msgs.push(msg);
                    }
                }
                if !msgs.is_empty() {
                    mailboxes.insert(peer.clone(), msgs);
                }
            }
        }
        Self {
            inner: Mutex::new(MailboxInner { mailboxes, peers }),
            persist_dir: Some(persist_dir),
        }
    }

    /// Send a message to a peer's mailbox (locked; persists to file under same lock).
    pub fn send(&self, from: &str, to: &str, content: &str) {
        let msg = MailboxMessage {
            from: from.to_string(),
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            content: content.to_string(),
        };
        // Serialize outside the lock so the lock region is tight.
        let serialized = self.persist_dir.as_ref().map(|dir| {
            let file_path = dir.join(format!("{to}.jsonl"));
            let json = serde_json::to_string(&msg);
            (file_path, json)
        });
        let mut inner = self.inner.lock().expect("mailbox lock poisoned");
        inner.mailboxes.entry(to.to_string()).or_default().push(msg);
        // Persist to file under the same lock so drain() always sees a consistent
        // view: either both in-memory + on-disk, or neither.
        if let Some((file_path, Ok(json))) = serialized {
            if let Err(e) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file_path)
                .and_then(|mut f| {
                    use std::io::Write;
                    writeln!(f, "{json}")
                })
            {
                tracing::warn!(peer = %to, path = %file_path.display(), error = %e, "mailbox: failed to persist message");
            }
        } else if let Some((_, Err(e))) = serialized {
            tracing::warn!(peer = %to, error = %e, "mailbox: failed to serialize message");
        }
    }

    /// Drain up to `max` messages from a peer's mailbox (FIFO, oldest first).
    /// When persistence is enabled, the peer's JSONL file is rewritten to
    /// reflect remaining messages (or deleted if empty) so that a reload via
    /// `with_persistence` does not resurrect already-drained messages.
    pub fn drain(&self, peer: &str, max: usize) -> Option<Vec<MailboxMessage>> {
        let mut inner = self.inner.lock().expect("mailbox lock poisoned");
        let msgs = inner.mailboxes.get_mut(peer)?;
        let drain_end = max.min(msgs.len());
        let drained: Vec<_> = msgs.drain(..drain_end).collect();
        let is_empty = msgs.is_empty();
        if is_empty {
            inner.mailboxes.remove(peer);
        }
        // Sync persisted JSONL file with remaining mailbox contents.
        if let Some(ref dir) = self.persist_dir {
            let file_path = dir.join(format!("{peer}.jsonl"));
            if is_empty {
                let _ = std::fs::remove_file(&file_path);
            } else if let Some(msgs) = inner.mailboxes.get(peer) {
                let lines: Vec<String> = msgs
                    .iter()
                    .filter_map(|m| serde_json::to_string(m).ok())
                    .collect();
                if let Err(e) = std::fs::write(&file_path, lines.join("\n") + "\n") {
                    tracing::warn!(peer = %peer, path = %file_path.display(), error = %e, "mailbox: failed to sync after drain");
                }
            }
        }
        Some(drained)
    }

    /// Get the list of known peer labels.
    pub fn peers(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("mailbox lock poisoned")
            .peers
            .clone()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageInput {
    /// Target agent label (must be a peer in the current team)
    pub peer: String,
    /// Message body
    pub message: String,
}

pub struct SendMessageTool {
    mailbox: std::sync::Arc<MailboxStore>,
    from_label: String,
}

impl SendMessageTool {
    pub fn new(mailbox: std::sync::Arc<MailboxStore>, from_label: impl Into<String>) -> Self {
        Self {
            mailbox,
            from_label: from_label.into(),
        }
    }
}

#[async_trait]
impl base::tool::Tool for SendMessageTool {
    fn name(&self) -> &str {
        "SendMessage"
    }

    fn is_deferred(&self) -> bool {
        false
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(SendMessageInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        "Send a message to another agent.\n\
         \n\
         ```json\n\
         {\"to\": \"researcher\", \"summary\": \"assign task 1\", \"message\": \"start on task #1\"}\n\
         ```\n\
         \n\
         | `to` | |\n\
         |---|---|\n\
         | `\"researcher\"` | Teammate by name |\n\
         | `\"*\"` | Broadcast to all teammates |\n\
         Your plain text output is NOT visible to other agents -- to communicate, \
         you MUST call this tool. Messages from teammates are delivered \
         automatically; use `ReadMail` to retrieve them. Refer to teammates by \
         name, never by UUID."
            .to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false // SendMessage is not read-only, cannot be concurrency-safe
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<SendMessageInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.peer.trim().is_empty() => ValidationResult::err("peer must not be empty", 1),
            Ok(p) if p.message.trim().is_empty() => {
                ValidationResult::err("message must not be empty", 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    /// Writing to a teammate's in-memory mailbox touches no file, no shell and
    /// no network — it is the coordination channel itself, not an effect on
    /// the user's workspace. Team members run under `PermissionMode::Plan` by
    /// default (see `team::coordinator`), and plan mode exists to stop the
    /// agent mutating the workspace before the user has approved a plan; a
    /// member that cannot report back to its coordinator is simply broken.
    /// See `Tool::self_allow_overrides_mode`.
    fn self_allow_overrides_mode(&self) -> bool {
        true
    }

    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: SendMessageInput = serde_json::from_value(input)?;
        self.mailbox
            .send(&self.from_label, &input.peer, &input.message);
        Ok(ToolResult {
            content: ToolResultContent::Text(format!("Message sent to {}", input.peer)),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMailInput {
    /// Max messages to return (drains oldest first). Defaults to 10.
    #[serde(default = "default_read_max")]
    pub max: usize,
}

fn default_read_max() -> usize {
    10
}

pub struct ReadMailTool {
    mailbox: std::sync::Arc<MailboxStore>,
    my_label: String,
}

impl ReadMailTool {
    pub fn new(mailbox: std::sync::Arc<MailboxStore>, my_label: impl Into<String>) -> Self {
        Self {
            mailbox,
            my_label: my_label.into(),
        }
    }
}

#[async_trait]
impl base::tool::Tool for ReadMailTool {
    fn name(&self) -> &str {
        "ReadMail"
    }

    fn is_deferred(&self) -> bool {
        false
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ReadMailInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        "Read messages sent to you by other agents in the current team.\n\
         \n\
         Usage:\n\
         - Messages are returned and removed from the mailbox (FIFO, oldest first)\n\
         - Returns \"(no new messages)\" when the mailbox is empty\n\
         - Use this after being notified that another agent sent you a message\n\
         \n\
         Input:\n\
         - max (optional, default 10): maximum number of messages to read"
            .to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<ReadMailInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.max == 0 => ValidationResult::err("max must be >= 1", 1),
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ReadMailInput = serde_json::from_value(input)?;
        let msgs = self.mailbox.drain(&self.my_label, input.max);
        let msgs = match msgs {
            Some(v) => v,
            None => {
                return Ok(ToolResult {
                    content: ToolResultContent::Text("(no new messages)".into()),
                    is_error: false,
                    structured_content: None,
                    mcp_meta: None,
                    new_messages: None,
                });
            }
        };
        let mut result = String::new();
        for msg in &msgs {
            result.push_str(&format!(
                "**From {}** (at {}):\n{}\n\n",
                msg.from, msg.timestamp_ms, msg.content
            ));
        }
        Ok(ToolResult {
            content: ToolResultContent::Text(result),
            is_error: false,
            structured_content: Some(serde_json::to_value(&msgs).unwrap_or_default()),
            mcp_meta: None,
            new_messages: None,
        })
    }
}

pub struct ListPeersTool {
    mailbox: std::sync::Arc<MailboxStore>,
}

impl ListPeersTool {
    pub fn new(mailbox: std::sync::Arc<MailboxStore>) -> Self {
        Self { mailbox }
    }
}

#[async_trait]
impl base::tool::Tool for ListPeersTool {
    fn name(&self) -> &str {
        "ListPeers"
    }

    fn is_deferred(&self) -> bool {
        false
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        "List all known agent labels in the current team.\n\
         \n\
         - Returns a list of agent names/labels that you can communicate with\n\
         - Use this to discover which agents you can send messages to via `SendMessage`\n\
         - Useful when you join an existing team and need to know your teammates"
            .to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        _input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let peers = self.mailbox.peers();
        if peers.is_empty() {
            return Ok(ToolResult {
                content: ToolResultContent::Text("(no peers in current team)".into()),
                is_error: false,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            });
        }
        let result = peers.join("\n");
        Ok(ToolResult {
            content: ToolResultContent::Text(result),
            is_error: false,
            structured_content: Some(serde_json::json!({"peers": peers})),
            mcp_meta: None,
            new_messages: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::{Tool, ToolContext};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn ctx() -> ToolContext {
        ToolContext::for_test(PathBuf::from("/tmp"))
    }

    fn text_content(r: &ToolResult) -> &str {
        match &r.content {
            ToolResultContent::Text(t) => t,
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn mailbox_send_and_drain() {
        let mb = MailboxStore::new(vec!["alice".into(), "bob".into()]);
        mb.send("alice", "bob", "hello from alice");
        mb.send("alice", "bob", "second message");

        let msgs = mb.drain("bob", 10).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].from, "alice");
        assert_eq!(msgs[0].content, "hello from alice");
        assert_eq!(msgs[1].content, "second message");

        assert!(mb.drain("bob", 10).is_none());
    }

    #[test]
    fn mailbox_drain_nonexistent_peer() {
        let mb = MailboxStore::new(vec![]);
        assert!(mb.drain("nobody", 10).is_none());
    }

    #[test]
    fn mailbox_drain_respects_max() {
        let mb = MailboxStore::new(vec!["a".into(), "b".into()]);
        mb.send("a", "b", "m1");
        mb.send("a", "b", "m2");
        mb.send("a", "b", "m3");

        let msgs = mb.drain("b", 2).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "m1");
        assert_eq!(msgs[1].content, "m2");

        let remaining = mb.drain("b", 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].content, "m3");
    }

    #[test]
    fn mailbox_peers_list() {
        let mb = MailboxStore::new(vec!["a".into(), "b".into()]);
        let peers = mb.peers();
        assert_eq!(peers, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn send_message_tool_basic() {
        let mb = Arc::new(MailboxStore::new(vec!["sender".into(), "receiver".into()]));
        let tool = SendMessageTool::new(mb.clone(), "sender");
        let r = tool
            .call(
                serde_json::json!({"peer": "receiver", "message": "hi"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(text_content(&r).contains("sent"));

        let msgs = mb.drain("receiver", 10).unwrap();
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[0].from, "sender");
    }

    #[tokio::test]
    async fn read_mail_tool_empty() {
        let mb = Arc::new(MailboxStore::new(vec!["me".into()]));
        let tool = ReadMailTool::new(mb.clone(), "me");
        let r = tool
            .call(
                serde_json::json!({"max": 10}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(text_content(&r).contains("no new messages"));
    }

    #[tokio::test]
    async fn read_mail_tool_with_messages() {
        let mb = Arc::new(MailboxStore::new(vec!["a".into(), "b".into()]));
        mb.send("a", "b", "msg1");
        mb.send("a", "b", "msg2");
        let tool = ReadMailTool::new(mb.clone(), "b");
        let r = tool
            .call(
                serde_json::json!({"max": 10}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let text = text_content(&r);
        assert!(text.contains("msg1"));
        assert!(text.contains("msg2"));
        assert!(text.contains("From a"));
    }

    #[tokio::test]
    async fn list_peers_tool() {
        let mb = Arc::new(MailboxStore::new(vec!["alice".into(), "bob".into()]));
        let tool = ListPeersTool::new(mb);
        let r = tool
            .call(serde_json::json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(!r.is_error);
        let text = text_content(&r);
        assert!(text.contains("alice"));
        assert!(text.contains("bob"));
    }

    #[tokio::test]
    async fn list_peers_empty() {
        let mb = Arc::new(MailboxStore::new(vec![]));
        let tool = ListPeersTool::new(mb);
        let r = tool
            .call(serde_json::json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(text_content(&r).contains("no peers"));
    }

    #[test]
    fn validate_send_message_empty_peer() {
        let mb = Arc::new(MailboxStore::new(vec![]));
        let tool = SendMessageTool::new(mb, "me");
        let r = futures::executor::block_on(
            tool.validate_input(&serde_json::json!({"peer": "", "message": "hi"}), &ctx()),
        );
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[test]
    fn validate_read_mail_zero_max() {
        let mb = Arc::new(MailboxStore::new(vec![]));
        let tool = ReadMailTool::new(mb, "me");
        let r = futures::executor::block_on(
            tool.validate_input(&serde_json::json!({"max": 0}), &ctx()),
        );
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[test]
    fn validate_send_message_invalid_json() {
        let mb = Arc::new(MailboxStore::new(vec![]));
        let tool = SendMessageTool::new(mb, "me");
        let r = futures::executor::block_on(
            tool.validate_input(&serde_json::json!({"peer": 123}), &ctx()),
        );
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[test]
    fn mailbox_persistence_send_and_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        let persist = dir.path().to_path_buf();
        let peers = vec!["a".to_string(), "b".to_string()];

        let mb = MailboxStore::with_persistence(peers.clone(), persist.clone());
        mb.send("a", "b", "msg1");
        mb.send("a", "b", "msg2");

        // Reload: persisted messages must appear.
        let mb2 = MailboxStore::with_persistence(peers, persist);
        let msgs = mb2.drain("b", 10).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "msg1");
        assert_eq!(msgs[1].content, "msg2");
    }

    #[test]
    fn mailbox_persistence_drain_and_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        let persist = dir.path().to_path_buf();
        let peers = vec!["a".to_string(), "b".to_string()];

        let mb = MailboxStore::with_persistence(peers.clone(), persist.clone());
        mb.send("a", "b", "msg1");
        mb.send("a", "b", "msg2");
        // Drain all messages — they must not reappear on reload.
        let _ = mb.drain("b", 10).unwrap();

        let mb2 = MailboxStore::with_persistence(peers, persist);
        assert!(
            mb2.drain("b", 10).is_none(),
            "drained messages must not resurrect"
        );
    }

    #[test]
    fn mailbox_persistence_partial_drain() {
        let dir = tempfile::TempDir::new().unwrap();
        let persist = dir.path().to_path_buf();
        let peers = vec!["a".to_string(), "b".to_string()];

        let mb = MailboxStore::with_persistence(peers.clone(), persist.clone());
        mb.send("a", "b", "m1");
        mb.send("a", "b", "m2");
        mb.send("a", "b", "m3");
        // Drain only one message.
        let drained = mb.drain("b", 1).unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].content, "m1");

        // Reload: remaining 2 must be present.
        let mb2 = MailboxStore::with_persistence(peers, persist);
        let msgs = mb2.drain("b", 10).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "m2");
        assert_eq!(msgs[1].content, "m3");
    }
}
