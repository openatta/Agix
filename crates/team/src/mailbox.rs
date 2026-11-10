//! Mailbox for agent-to-agent communication within a team.
//!
//! `MailboxStore` is an `Arc`-shared `HashMap<String, Vec<MailboxMessage>>`,
//! protected by a `Mutex`, with optional JSONL file persistence for cross-turn
//! survival. Three tools provide the agent-facing API:
//!
//! - `SendMessage`: push a message to a peer's mailbox
//! - `ReadMail`: drain received messages from own mailbox
//! - `ListPeers`: list all known agent labels in this team
//!
//! A label is whatever the model named a member, so mail goes only to labels
//! the team declared, and a label is turned into a file name by
//! [`mailbox_file`] rather than spliced into a path.

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PathStyle, PermissionDecision, ProgressSender, PromptContext, ToolContext, ToolResult,
    ToolResultContent, ToolSpec, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// When Some, each peer's mailbox is also kept in a JSONL file here.
    persist_dir: Option<PathBuf>,
    /// The version of each peer's mailbox last written to its file. Files
    /// are written outside `inner`; a writer that arrives with an older
    /// snapshot than the one already on disk leaves the file alone.
    written: Mutex<HashMap<String, u64>>,
}

struct MailboxInner {
    /// peer_id → FIFO messages
    mailboxes: HashMap<String, Vec<MailboxMessage>>,
    /// known peer labels
    peers: Vec<String>,
    /// Bumped on every change to a peer's mailbox.
    versions: HashMap<String, u64>,
}

/// A peer's mailbox as it stood at one version, to be written to its file.
struct Snapshot {
    peer: String,
    version: u64,
    lines: Vec<String>,
}

impl MailboxInner {
    fn snapshot(&mut self, peer: &str) -> Snapshot {
        let version = self.versions.entry(peer.to_string()).or_default();
        *version += 1;
        let lines = self
            .mailboxes
            .get(peer)
            .map(|msgs| {
                msgs.iter()
                    .filter_map(|m| serde_json::to_string(m).ok())
                    .collect()
            })
            .unwrap_or_default();
        Snapshot {
            peer: peer.to_string(),
            version: *version,
            lines,
        }
    }
}

impl MailboxStore {
    /// Create a new mailbox store for a set of peer agents.
    pub fn new(peers: Vec<String>) -> Self {
        Self {
            inner: Mutex::new(MailboxInner {
                mailboxes: HashMap::new(),
                peers,
                versions: HashMap::new(),
            }),
            persist_dir: None,
            written: Mutex::new(HashMap::new()),
        }
    }

    /// Create a mailbox store with file persistence. Each peer's messages are
    /// kept in one JSONL file (see [`mailbox_file`]) for cross-turn survival.
    pub fn with_persistence(peers: Vec<String>, persist_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&persist_dir);
        // Load existing messages from disk into the in-memory store
        let mut mailboxes: HashMap<String, Vec<MailboxMessage>> = HashMap::new();
        for peer in &peers {
            let file_path = mailbox_file(&persist_dir, peer);
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
            inner: Mutex::new(MailboxInner {
                mailboxes,
                peers,
                versions: HashMap::new(),
            }),
            persist_dir: Some(persist_dir),
            written: Mutex::new(HashMap::new()),
        }
    }

    /// Send a message to a peer's mailbox. A `to` that is not one of the
    /// team's peers is refused. Blocks on file IO when persistence is on —
    /// an async caller runs it on a blocking thread.
    pub fn send(&self, from: &str, to: &str, content: &str) -> Result<(), UnknownPeer> {
        self.check_peer(to)?;
        let msg = MailboxMessage {
            from: from.to_string(),
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            content: content.to_string(),
        };
        let snapshot = {
            let mut inner = self.inner.lock().expect("mailbox lock poisoned");
            inner.mailboxes.entry(to.to_string()).or_default().push(msg);
            self.persist_dir.is_some().then(|| inner.snapshot(to))
        };
        if let Some(snapshot) = snapshot {
            self.persist(snapshot);
        }
        Ok(())
    }

    /// Writes `snapshot` to its peer's file unless a newer one is already
    /// there. `written` is held across the write so two writers for the same
    /// peer cannot land out of order.
    fn persist(&self, snapshot: Snapshot) {
        let Some(dir) = &self.persist_dir else {
            return;
        };
        let mut written = self.written.lock().expect("mailbox lock poisoned");
        let last = written.entry(snapshot.peer.clone()).or_default();
        if *last >= snapshot.version {
            return;
        }
        *last = snapshot.version;
        let file_path = mailbox_file(dir, &snapshot.peer);
        let result = if snapshot.lines.is_empty() {
            match std::fs::remove_file(&file_path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            let tmp = file_path.with_extension("jsonl.tmp");
            std::fs::write(&tmp, snapshot.lines.join("\n") + "\n")
                .and_then(|()| std::fs::rename(&tmp, &file_path))
        };
        if let Err(e) = result {
            tracing::warn!(peer = %snapshot.peer, path = %file_path.display(), error = %e, "mailbox: failed to persist");
        }
    }

    /// `Err` naming the team's peers when `peer` is not one of them.
    pub fn check_peer(&self, peer: &str) -> Result<(), UnknownPeer> {
        let inner = self.inner.lock().expect("mailbox lock poisoned");
        if inner.peers.iter().any(|p| p == peer) {
            return Ok(());
        }
        Err(UnknownPeer {
            peer: peer.to_string(),
            peers: inner.peers.clone(),
        })
    }

    /// Drain up to `max` messages from a peer's mailbox (FIFO, oldest first).
    /// When persistence is enabled, the peer's JSONL file is rewritten to
    /// reflect remaining messages (or deleted if empty) so that a reload via
    /// `with_persistence` does not resurrect already-drained messages. Blocks
    /// on that file IO, like [`Self::send`].
    pub fn drain(&self, peer: &str, max: usize) -> Option<Vec<MailboxMessage>> {
        let (drained, snapshot) = {
            let mut inner = self.inner.lock().expect("mailbox lock poisoned");
            let msgs = inner.mailboxes.get_mut(peer)?;
            let drain_end = max.min(msgs.len());
            let drained: Vec<_> = msgs.drain(..drain_end).collect();
            if msgs.is_empty() {
                inner.mailboxes.remove(peer);
            }
            let snapshot = self.persist_dir.is_some().then(|| inner.snapshot(peer));
            (drained, snapshot)
        };
        if let Some(snapshot) = snapshot {
            self.persist(snapshot);
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

/// A message addressed to a label the team does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPeer {
    pub peer: String,
    pub peers: Vec<String>,
}

impl std::fmt::Display for UnknownPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` is not a peer in this team; peers: {}",
            self.peer,
            self.peers.join(", ")
        )
    }
}

impl std::error::Error for UnknownPeer {}

/// The file `peer`'s mail is kept in, always directly inside `dir`.
///
/// A label made only of ASCII letters, digits, `-`, `_` and `.` (not leading)
/// is its own file name; any other is replaced by a hash of it, so neither a
/// separator nor `..` nor an absolute path in a label can reach the path.
pub fn mailbox_file(dir: &Path, peer: &str) -> PathBuf {
    let plain = !peer.is_empty()
        && peer.len() <= 64
        && !peer.starts_with('.')
        && peer
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if plain {
        return dir.join(format!("{peer}.jsonl"));
    }
    let hash = peer.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    dir.join(format!("~{hash:016x}.jsonl"))
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
    fn description(&self) -> &str {
        ""
    }

    fn spec(&self) -> ToolSpec {
        // Writing to a teammate's in-memory mailbox touches no file, no shell and
        // no network — it is the coordination channel itself, not an effect on
        // the user's workspace. Team members run under `PermissionMode::Plan` by
        // default (see `team::coordinator`), and plan mode exists to stop the
        // agent mutating the workspace before the user has approved a plan; a
        // member that cannot report back to its coordinator is simply broken.
        // See `ToolPermissionSpec::self_allow_overrides_mode`.
        ToolSpec::new().self_allow_overrides_mode(true)
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(SendMessageInput))
            .expect("schemars output is valid JSON")
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(
            "Send a message to another agent in your team.\n\
         \n\
         ```json\n\
         {\"peer\": \"researcher\", \"message\": \"start on task #1\"}\n\
         ```\n\
         \n\
         `peer` is a teammate's label, as `ListPeers` lists them; there is no \
         broadcast. Your plain text output is NOT visible to other agents -- to \
         communicate, you MUST call this tool. Messages sent to you wait in your \
         mailbox until you call `ReadMail`."
                .to_string(),
        )
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<SendMessageInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.peer.trim().is_empty() => ValidationResult::err("peer must not be empty", 1),
            Ok(p) if p.message.trim().is_empty() => {
                ValidationResult::err("message must not be empty", 2)
            }
            Ok(p) => match self.mailbox.check_peer(&p.peer) {
                Ok(()) => ValidationResult::Ok,
                Err(unknown) => ValidationResult::err(unknown.to_string(), 4),
            },
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
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
        let input: SendMessageInput = serde_json::from_value(input)?;
        let (mailbox, from) = (self.mailbox.clone(), self.from_label.clone());
        let (peer, message) = (input.peer.clone(), input.message);
        let sent = tokio::task::spawn_blocking(move || mailbox.send(&from, &peer, &message))
            .await
            .map_err(|e| ToolError::exec(format!("sending the message failed: {e}")))?;
        if let Err(unknown) = sent {
            return Ok(ToolResult::error_text(unknown.to_string()));
        }
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
    fn description(&self) -> &str {
        ""
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(ReadMailInput))
            .expect("schemars output is valid JSON")
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(
            "Read messages sent to you by other agents in the current team.\n\
         \n\
         Usage:\n\
         - Messages are returned and removed from the mailbox (FIFO, oldest first)\n\
         - Returns \"(no new messages)\" when the mailbox is empty\n\
         - Use this after being notified that another agent sent you a message\n\
         \n\
         Input:\n\
         - max (optional, default 10): maximum number of messages to read"
                .to_string(),
        )
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
        let (mailbox, me) = (self.mailbox.clone(), self.my_label.clone());
        let msgs = tokio::task::spawn_blocking(move || mailbox.drain(&me, input.max))
            .await
            .map_err(|e| ToolError::exec(format!("reading the mailbox failed: {e}")))?;
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
    fn description(&self) -> &str {
        ""
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(
            "List all known agent labels in the current team.\n\
         \n\
         - Returns a list of agent names/labels that you can communicate with\n\
         - Use this to discover which agents you can send messages to via `SendMessage`\n\
         - Useful when you join an existing team and need to know your teammates"
                .to_string(),
        )
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
        platform::tool_context_for_test(PathBuf::from("/tmp"))
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
        mb.send("alice", "bob", "hello from alice").unwrap();
        mb.send("alice", "bob", "second message").unwrap();

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
        mb.send("a", "b", "m1").unwrap();
        mb.send("a", "b", "m2").unwrap();
        mb.send("a", "b", "m3").unwrap();

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
        mb.send("a", "b", "msg1").unwrap();
        mb.send("a", "b", "msg2").unwrap();
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
        mb.send("a", "b", "msg1").unwrap();
        mb.send("a", "b", "msg2").unwrap();

        // Reload: persisted messages must appear.
        let mb2 = MailboxStore::with_persistence(peers, persist);
        let msgs = mb2.drain("b", 10).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "msg1");
        assert_eq!(msgs[1].content, "msg2");
    }

    #[test]
    fn concurrent_sends_and_drains_leave_the_file_matching_the_mailbox() {
        let dir = tempfile::TempDir::new().unwrap();
        let persist = dir.path().to_path_buf();
        let peers = vec!["a".to_string(), "b".to_string()];
        let mb = Arc::new(MailboxStore::with_persistence(
            peers.clone(),
            persist.clone(),
        ));

        let workers: Vec<_> = (0..8)
            .map(|t| {
                let mb = mb.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        mb.send("a", "b", &format!("{t}-{i}")).unwrap();
                        if i % 5 == 0 {
                            mb.drain("b", 1);
                        }
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }

        let contents = |mb: &MailboxStore| -> Vec<String> {
            mb.drain("b", usize::MAX)
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.content)
                .collect()
        };
        let on_disk = contents(&MailboxStore::with_persistence(peers, persist));
        let in_memory = contents(&mb);
        assert_eq!(in_memory.len(), 8 * 25 - 8 * 5);
        assert_eq!(on_disk, in_memory);
    }

    #[test]
    fn mailbox_persistence_drain_and_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        let persist = dir.path().to_path_buf();
        let peers = vec!["a".to_string(), "b".to_string()];

        let mb = MailboxStore::with_persistence(peers.clone(), persist.clone());
        mb.send("a", "b", "msg1").unwrap();
        mb.send("a", "b", "msg2").unwrap();
        // Drain all messages — they must not reappear on reload.
        let _ = mb.drain("b", 10).unwrap();

        let mb2 = MailboxStore::with_persistence(peers, persist);
        assert!(
            mb2.drain("b", 10).is_none(),
            "drained messages must not resurrect"
        );
    }

    #[tokio::test]
    async fn mail_to_a_label_the_team_does_not_have_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let persist = dir.path().join("team").join("mailbox");
        let mb = Arc::new(MailboxStore::with_persistence(
            vec!["sender".into(), "receiver".into()],
            persist.clone(),
        ));
        let tool = SendMessageTool::new(mb.clone(), "sender");
        let escape = dir.path().join("escaped");
        for peer in [
            "../../escaped".to_string(),
            escape.display().to_string(),
            "nobody".to_string(),
        ] {
            let input = serde_json::json!({"peer": peer, "message": "x"});
            let checked = tool.validate_input(&input, &ctx()).await;
            assert!(!matches!(checked, ValidationResult::Ok), "{peer} validated");
            let r = tool
                .call(input, ctx(), ProgressSender::noop("t"))
                .await
                .unwrap();
            assert!(r.is_error, "{peer} was sent to");
            assert!(text_content(&r).contains("receiver"), "the peers are named");
        }
        assert!(!dir.path().join("escaped.jsonl").exists());
        let files: Vec<_> = std::fs::read_dir(&persist).unwrap().flatten().collect();
        assert!(files.is_empty(), "nothing was written for a refused peer");
    }

    #[test]
    fn a_label_that_is_not_a_plain_file_name_stays_inside_the_mailbox_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let persist = dir.path().join("mailbox");
        let odd = ["../../x", "/abs/path", "研究员", "a b", ".hidden", ".."];
        let peers: Vec<String> = odd.iter().map(|s| s.to_string()).collect();
        let mb = MailboxStore::with_persistence(peers.clone(), persist.clone());
        for (i, peer) in odd.iter().enumerate() {
            mb.send("lead", peer, &format!("m{i}")).unwrap();
            let file = mailbox_file(&persist, peer);
            assert_eq!(file.parent(), Some(persist.as_path()), "{peer}");
            assert!(file.is_file(), "{peer}");
        }
        let reloaded = MailboxStore::with_persistence(peers, persist);
        for (i, peer) in odd.iter().enumerate() {
            let msgs = reloaded.drain(peer, 10).unwrap();
            assert_eq!(msgs[0].content, format!("m{i}"));
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(
            mailbox_file(Path::new("/m"), "worker-1"),
            Path::new("/m/worker-1.jsonl")
        );
    }

    #[test]
    fn mailbox_persistence_partial_drain() {
        let dir = tempfile::TempDir::new().unwrap();
        let persist = dir.path().to_path_buf();
        let peers = vec!["a".to_string(), "b".to_string()];

        let mb = MailboxStore::with_persistence(peers.clone(), persist.clone());
        mb.send("a", "b", "m1").unwrap();
        mb.send("a", "b", "m2").unwrap();
        mb.send("a", "b", "m3").unwrap();
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
