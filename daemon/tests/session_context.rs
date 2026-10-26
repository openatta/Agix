//! A host can see what a session's next model call would carry, and change
//! what it remembers.
//!
//! `session.inspectContext` assembles the next request without making it.
//! `session.dropMessages` and `session.editMessage` change a message the
//! engine holds and log the change, so the next call — and every call after a
//! resume — sees the changed context, not the original.

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
const SESSION_BUSY: i64 = -32015;

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

/// A session whose context is: 0 the user's message, 1 the model's `Bash`
/// call, 2 its result (`the-secret-value`, which only the result spells out),
/// 3 the model's reply.
async fn a_session_that_saw_a_secret(model: &Arc<MockAnthropicClient>, daemon: &Daemon) -> String {
    model.push_turn(tool_round(
        "Bash",
        "toolu_echo",
        serde_json::json!({"command": "printf 'the-%s-value' secret"}),
    ));
    model.push_turn(text_round("done"));
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    daemon.turn(&session, "print the secret").await;
    session
}

#[tokio::test]
async fn the_next_call_can_be_seen_without_being_made() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone()).await;
    let session = a_session_that_saw_a_secret(&model, &daemon).await;
    let calls = model.calls();

    let context = daemon
        .ok(
            "session.inspectContext",
            serde_json::json!({"session_id": session}),
        )
        .await;
    assert_eq!(model.calls(), calls, "inspecting calls no model");
    assert_eq!(context["basis"], "next_step_without_new_input");
    assert_eq!(context["messages"]["count"], 4);
    let system = context["system"].as_array().unwrap();
    assert!(!system.is_empty());
    assert!(system.iter().all(|b| b["tokens"].as_u64().is_some()));
    assert!(context["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "Bash"));
    assert!(context["total_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn a_dropped_tool_result_is_gone_from_every_call_after_including_after_a_resume() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone()).await;
    let session = a_session_that_saw_a_secret(&model, &daemon).await;
    assert!(daemon.last_request().contains("the-secret-value"));

    let dropped = daemon
        .ok(
            "session.dropMessages",
            serde_json::json!({"session_id": session, "at": [2], "total": 4}),
        )
        .await;
    assert_eq!(dropped["dropped"], serde_json::json!([2]));

    model.push_turn(text_round("ok"));
    daemon.turn(&session, "go on").await;
    let after = daemon.last_request();
    assert!(
        !after.contains("the-secret-value"),
        "the next call still carries it"
    );
    assert!(after.contains("[removed by the host]"));

    daemon
        .ok("session.close", serde_json::json!({"session_id": session}))
        .await;
    model.push_turn(text_round("ok"));
    daemon.turn(&session, "and again").await;
    let resumed = daemon.last_request();
    assert!(
        !resumed.contains("the-secret-value"),
        "a resume brought it back"
    );
    assert!(resumed.contains("[removed by the host]"));
}

#[tokio::test]
async fn a_users_message_can_be_rewritten_and_the_models_cannot() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone()).await;
    let session = a_session_that_saw_a_secret(&model, &daemon).await;

    let models_own = daemon
        .call(
            "session.editMessage",
            serde_json::json!({"session_id": session, "at": 1, "text": "x"}),
        )
        .await;
    assert_eq!(models_own["error"]["code"], INVALID_PARAMS, "{models_own}");

    let stale = daemon
        .call(
            "session.editMessage",
            serde_json::json!({"session_id": session, "at": 0, "text": "x", "total": 3}),
        )
        .await;
    assert_eq!(stale["error"]["code"], INVALID_PARAMS, "{stale}");

    daemon
        .ok(
            "session.editMessage",
            serde_json::json!({"session_id": session, "at": 0, "text": "say hello instead"}),
        )
        .await;
    model.push_turn(text_round("hello"));
    daemon.turn(&session, "go on").await;
    let after = daemon.last_request();
    assert!(after.contains("say hello instead"));
    assert!(!after.contains("print the secret"));
}

#[tokio::test]
async fn a_session_mid_turn_is_too_busy_to_change() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "Bash",
        "toolu_sleep",
        serde_json::json!({"command": "sleep 30"}),
    ));
    model.push_turn(text_round("stopped"));
    let daemon = Arc::new(Daemon::start(model).await);
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let turn = {
        let (daemon, session) = (daemon.clone(), session.clone());
        tokio::spawn(async move { daemon.turn(&session, "wait").await })
    };

    let mut busy = None;
    for _ in 0..200 {
        let response = daemon
            .call(
                "session.inspectContext",
                serde_json::json!({"session_id": session}),
            )
            .await;
        if response["error"]["code"] == SESSION_BUSY {
            busy = Some(response);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(busy.is_some(), "never refused while the turn ran");

    daemon
        .ok(
            "session.interrupt",
            serde_json::json!({"session_id": session}),
        )
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(10), turn).await;
}
