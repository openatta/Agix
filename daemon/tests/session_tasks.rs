//! A session's tasks can be listed and stopped by its host.
//!
//! What runs on in a session beyond the model's reply — a shell command, a
//! sub-agent, a scheduled job — is listed by `session.tasks`, stopped by
//! `session.stopTask`, and announced to the session's watchers as it starts
//! and ends, outside any turn.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

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
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(model: Arc<MockAnthropicClient>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("tasks.sock");
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
                model_client: Some(model as Arc<dyn AnthropicClient>),
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
        Self { sock, _dir: dir }
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

    /// Every `session.event` this connection receives after subscribing.
    async fn watch(&self, session_id: &str) -> Arc<Mutex<Vec<serde_json::Value>>> {
        let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
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

/// Re-evaluate `$probe` (an expression yielding `Option<_>`) until it is
/// `Some`, or fail saying what never happened.
macro_rules! eventually {
    ($what:expr, $probe:expr) => {{
        let mut found = None;
        for _ in 0..200 {
            if let Some(hit) = $probe {
                found = Some(hit);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        found.unwrap_or_else(|| panic!("never saw {}", $what))
    }};
}

#[tokio::test]
async fn a_running_shell_command_is_listed_announced_and_stopped() {
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
    let frames = daemon.watch(&session).await;
    let turn = {
        let (daemon, session) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            daemon
                .call(
                    "session.run_turn",
                    serde_json::json!({"session_id": session, "message": "run it"}),
                )
                .await
        })
    };

    let shell = eventually!("the shell command listed as running", {
        let tasks = daemon
            .ok("session.tasks", serde_json::json!({"session_id": session}))
            .await;
        assert_eq!(tasks["active"], true);
        tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["kind"] == "shell" && t["status"] == "running")
            .cloned()
    });
    assert_eq!(shell["detail"]["command"], "sleep 30");

    let started = eventually!("task_started, a second into the command", {
        frames
            .lock()
            .unwrap()
            .iter()
            .find(|f| f["event"]["kind"] == "task_started")
            .cloned()
    });
    assert!(
        started["turn_id"].is_null(),
        "not part of the turn: {started}"
    );
    assert_eq!(started["event"]["task"]["task_id"], shell["task_id"]);

    let stopped = daemon
        .ok(
            "session.stopTask",
            serde_json::json!({"session_id": session, "task_id": shell["task_id"]}),
        )
        .await;
    assert_eq!(stopped["stopped"], true);

    let response = tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("the turn ends once its command is stopped")
        .unwrap();
    assert!(response.get("result").is_some(), "{response}");

    let finished = eventually!("task_finished", {
        frames
            .lock()
            .unwrap()
            .iter()
            .find(|f| f["event"]["kind"] == "task_finished")
            .cloned()
    });
    assert_eq!(finished["event"]["task"]["status"], "stopped");

    let again = daemon
        .ok(
            "session.stopTask",
            serde_json::json!({"session_id": session, "task_id": shell["task_id"]}),
        )
        .await;
    assert_eq!(
        again["stopped"], false,
        "an ended task cannot be stopped twice"
    );
}

#[tokio::test]
async fn a_scheduled_job_is_one_of_its_sessions_tasks() {
    let daemon = Daemon::start(Arc::new(MockAnthropicClient::new())).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = daemon
        .ok(
            "schedule.create",
            serde_json::json!({"session_id": session, "cron": "0 9 * * *", "prompt": "report"}),
        )
        .await["job"]
        .clone();

    let tasks = daemon
        .ok("session.tasks", serde_json::json!({"session_id": session}))
        .await;
    let listed = &tasks["tasks"].as_array().unwrap()[0];
    assert_eq!(listed["kind"], "cron");
    assert_eq!(listed["task_id"], job["id"]);
    assert_eq!(listed["detail"]["cron"], "0 9 * * *");

    let stopped = daemon
        .ok(
            "session.stopTask",
            serde_json::json!({"session_id": session, "task_id": job["id"]}),
        )
        .await;
    assert_eq!(stopped["stopped"], true);
    assert!(
        daemon.ok("schedule.list", serde_json::json!({})).await["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn an_unknown_session_has_no_tasks_to_list() {
    let daemon = Daemon::start(Arc::new(MockAnthropicClient::new())).await;
    let response = daemon
        .call("session.tasks", serde_json::json!({"session_id": "nope"}))
        .await;
    assert_eq!(response["error"]["code"], SESSION_NOT_FOUND, "{response}");
}
