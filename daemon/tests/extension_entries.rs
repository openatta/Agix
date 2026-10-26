//! A host's own entries in a session's log: written over the protocol, read
//! back in order with where they sit among the messages, pushed to whoever
//! watches the session, kept by a fork and a resume — and never shown to the
//! model. Also: a session brought back after a restart can be given its
//! recorder again.

use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const INVALID_PARAMS: i64 = -32602;
const SESSION_NOT_FOUND: i64 = -32000;

fn text_round(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta { text: text.into() },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::EndTurn),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

#[allow(dead_code)]
fn tool_round(tool: &str, id: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: tool.to_string(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::ToolUse),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

struct Daemon {
    sock: std::path::PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(model: Arc<MockAnthropicClient>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("context.sock");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&sock),
            "coding",
            &StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        config.settings.memory_enabled = false;
        config.settings.permission_mode =
            base::interface::settings::PermissionMode::BypassPermissions;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::scene::coding::CodingScene),
            daemon::Assembly {
                cwd: Some(cwd),
                model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let bind = sock.clone();
        tokio::spawn(async move {
            let _ = server.serve_unix(&bind).await;
        });
        for _ in 0..80 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(sock.exists(), "socket never bound");
        Self {
            sock,
            model,
            _dir: dir,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = serde_json::json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "{method}: connection closed before a response");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                return frame;
            }
        }
    }

    async fn ok(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let frame = self.call(method, params).await;
        assert!(frame.get("result").is_some(), "{method} failed: {frame}");
        frame["result"].clone()
    }

    async fn turn(&self, session: &str, message: &str) {
        self.ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": message}),
        )
        .await;
    }

    /// The last request the model was sent, as text.
    fn last_request(&self) -> String {
        let n = self.model.calls();
        serde_json::to_string(&self.model.nth_request(n - 1).expect("a call was made")).unwrap()
    }
}

impl Daemon {
    async fn session_after_a_turn(&self) -> String {
        self.model.push_turn(text_round("hello back"));
        let session = self.ok("session.create", serde_json::json!({})).await["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        self.turn(&session, "hello").await;
        session
    }

    async fn append(
        &self,
        session: &str,
        event: &str,
        payload: serde_json::Value,
    ) -> serde_json::Value {
        self.ok(
            "session.appendEntry",
            serde_json::json!({"session_id": session, "ns": "com.example.cards", "event": event, "payload": payload}),
        )
        .await
    }

    async fn entries(&self, session: &str) -> Vec<serde_json::Value> {
        self.ok(
            "session.entries",
            serde_json::json!({"session_id": session}),
        )
        .await["entries"]
            .as_array()
            .unwrap()
            .clone()
    }

    async fn watch(&self, session_id: &str) -> Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
        let seen: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        let request = serde_json::json!({
            "jsonrpc":"2.0","method":"session.subscribe",
            "params":{"session_id": session_id},"id":1
        });
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let into = seen.clone();
        tokio::spawn(async move {
            let (r, _w) = conn.split();
            let mut reader = BufReader::new(r);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
                if frame["method"] == "session.event" {
                    into.lock().unwrap().push(frame["params"].clone());
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen
    }
}

#[tokio::test]
async fn an_entry_is_listed_where_it_was_written_and_the_model_never_sees_it() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone()).await;
    let session = daemon.session_after_a_turn().await;

    let written = daemon
        .append(
            &session,
            "job_done",
            serde_json::json!({"title": "card-marker-7f3"}),
        )
        .await;
    assert_eq!(written["at"], 2, "after the user's message and the reply");
    assert_eq!(written["ns"], "com.example.cards");

    model.push_turn(text_round("ok"));
    daemon.turn(&session, "go on").await;
    assert!(
        !daemon.last_request().contains("card-marker-7f3"),
        "the model was sent the host's entry"
    );
    daemon
        .append(&session, "second", serde_json::json!(null))
        .await;

    let listed = daemon.entries(&session).await;
    let events: Vec<_> = listed
        .iter()
        .map(|e| (e["event"].clone(), e["at"].clone()))
        .collect();
    assert_eq!(
        events,
        vec![
            (serde_json::json!("job_done"), serde_json::json!(2)),
            (serde_json::json!("second"), serde_json::json!(4)),
        ],
        "in log order, the engine's own `agix.*` entries left out"
    );

    let only = daemon
        .ok(
            "session.entries",
            serde_json::json!({"session_id": session, "namespaces": ["com.other"]}),
        )
        .await;
    assert_eq!(only["entries"], serde_json::json!([]));
}

#[tokio::test]
async fn the_engines_namespaces_and_unknown_sessions_are_refused() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone()).await;
    let session = daemon.session_after_a_turn().await;

    let engines = daemon
        .call(
            "session.appendEntry",
            serde_json::json!({"session_id": session, "ns": "agix.session", "event": "spec"}),
        )
        .await;
    assert_eq!(engines["error"]["code"], INVALID_PARAMS, "{engines}");

    let missing = base::session::SessionId::new().to_string();
    let nowhere = daemon
        .call(
            "session.appendEntry",
            serde_json::json!({"session_id": missing, "ns": "com.example.cards", "event": "x"}),
        )
        .await;
    assert_eq!(nowhere["error"]["code"], SESSION_NOT_FOUND, "{nowhere}");
    let listed = daemon
        .call(
            "session.entries",
            serde_json::json!({"session_id": missing}),
        )
        .await;
    assert_eq!(listed["error"]["code"], SESSION_NOT_FOUND, "{listed}");
}

#[tokio::test]
async fn watchers_are_told_of_an_entry_as_it_is_written() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone()).await;
    let session = daemon.session_after_a_turn().await;
    let seen = daemon.watch(&session).await;

    daemon
        .append(&session, "job_done", serde_json::json!({"n": 1}))
        .await;
    let mut frame = None;
    for _ in 0..200 {
        frame = seen
            .lock()
            .unwrap()
            .iter()
            .find(|f| f["event"]["kind"] == "extension_entry")
            .cloned();
        if frame.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let frame = frame.expect("no extension_entry frame");
    assert_eq!(frame["turn_id"], serde_json::Value::Null);
    assert_eq!(frame["event"]["entry"]["event"], "job_done");
    assert_eq!(
        frame["event"]["entry"]["payload"],
        serde_json::json!({"n": 1})
    );
}

#[tokio::test]
async fn a_whole_fork_keeps_the_entries_after_the_last_message_and_a_cut_one_does_not() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone()).await;
    let session = daemon.session_after_a_turn().await;
    daemon
        .append(&session, "job_done", serde_json::json!({}))
        .await;

    let whole = daemon
        .ok("session.fork", serde_json::json!({"session_id": session}))
        .await;
    let whole_entries = daemon.entries(whole["session_id"].as_str().unwrap()).await;
    assert_eq!(whole_entries.len(), 1, "{whole_entries:?}");
    assert_eq!(whole_entries[0]["at"], 2);

    let cut = daemon
        .ok(
            "session.fork",
            serde_json::json!({"session_id": session, "at_message": 1}),
        )
        .await;
    assert!(daemon
        .entries(cut["session_id"].as_str().unwrap())
        .await
        .is_empty());
}

#[tokio::test]
async fn entries_outlive_the_session_leaving_memory_and_can_be_written_to_a_cold_one() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone()).await;
    let session = daemon.session_after_a_turn().await;
    daemon
        .append(&session, "before", serde_json::json!({}))
        .await;
    daemon
        .ok("session.close", serde_json::json!({"session_id": session}))
        .await;

    daemon
        .append(&session, "while_cold", serde_json::json!({}))
        .await;
    let listed = daemon.entries(&session).await;
    assert_eq!(listed.len(), 2, "{listed:?}");

    model.push_turn(text_round("back"));
    daemon.turn(&session, "again").await;
    let listed = daemon.entries(&session).await;
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|e| e["at"] == 2), "{listed:?}");
}
