//! What a running tool call reports reaches the session's watchers as
//! `tool_progress`, between its `tool_use` and its `tool_result`, and never
//! the transcript — a sub-agent's inside its `subagent_progress`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

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
        let sock = dir.path().join("progress.sock");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&sock),
            "code",
            &StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        config.settings.memory_enabled = false;
        config.settings.permission_mode = base::settings::PermissionMode::BypassPermissions;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene::default()),
            daemon::Assembly {
                cwd: Some(cwd),
                model_client: Some(model),
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

    async fn ok(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
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
                assert!(frame.get("result").is_some(), "{method} failed: {frame}");
                return frame["result"].clone();
            }
        }
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
        let (subscribed_tx, subscribed) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (r, _w) = conn.split();
            let mut reader = BufReader::new(r);
            let mut subscribed_tx = Some(subscribed_tx);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
                if frame.get("id").is_some() {
                    if let Some(tx) = subscribed_tx.take() {
                        let _ = tx.send(());
                    }
                }
                if frame["method"] == "session.event" {
                    into.lock().unwrap().push(frame["params"].clone());
                }
            }
        });
        subscribed.await.expect("session.subscribe answered");
        seen
    }

    async fn session(&self) -> String {
        self.ok("session.create", serde_json::json!({})).await["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn run_turn(&self, session: &str) {
        self.ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "go"}),
        )
        .await;
    }
}

/// The session's frames up to and including `turn_complete`.
async fn until_turn_complete(frames: &Mutex<Vec<serde_json::Value>>) -> Vec<serde_json::Value> {
    for _ in 0..400 {
        {
            let seen = frames.lock().unwrap();
            if seen.iter().any(|f| f["event"]["kind"] == "turn_complete") {
                return seen.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("turn_complete never arrived");
}

const LINES: &str = "printf 'one\\ntwo\\nthree\\n'";

#[tokio::test]
async fn a_tool_calls_progress_reaches_its_watchers_and_not_the_transcript() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "Bash",
        "toolu_lines",
        serde_json::json!({"command": LINES}),
    ));
    model.push_turn(text_round("done"));
    let daemon = Daemon::start(model).await;
    let session = daemon.session().await;
    let frames = daemon.watch(&session).await;
    daemon.run_turn(&session).await;
    let frames = until_turn_complete(&frames).await;

    let at = |kind: &str| {
        frames
            .iter()
            .position(|f| f["event"]["kind"] == kind && f["event"]["id"] == "toolu_lines")
            .unwrap_or_else(|| panic!("no {kind}: {frames:#?}"))
    };
    let (used, answered) = (at("tool_use"), at("tool_result"));
    let progress: Vec<(usize, &serde_json::Value)> = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f["event"]["kind"] == "tool_progress")
        .collect();
    assert!(!progress.is_empty(), "no tool_progress: {frames:#?}");
    for (i, frame) in &progress {
        assert!(used < *i && *i < answered, "out of order: {frames:#?}");
        assert_eq!(frame["event"]["tool_use_id"], "toolu_lines", "{frame}");
        assert_eq!(frame["event"]["name"], "Bash", "{frame}");
        assert_eq!(frame["session_id"], session.as_str());
        assert!(frame["turn_id"].is_string(), "{frame}");
    }
    let reported: u64 = progress
        .iter()
        .map(|(_, f)| 1 + f["event"]["dropped"].as_u64().expect("dropped is a count"))
        .sum();
    assert_eq!(reported, 3, "every line is sent or counted: {progress:#?}");
    let last = &progress.last().unwrap().1["event"];
    assert_eq!(last["message"], "three", "the last line goes out: {last}");
    assert_eq!(last["progress"], 3.0, "{last}");

    let history = daemon
        .ok(
            "session.history",
            serde_json::json!({"session_id": session, "limit": 100}),
        )
        .await;
    let blocks: Vec<&str> = history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().unwrap())
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        blocks.iter().filter(|b| **b != "text").collect::<Vec<_>>(),
        [&"tool_use", &"tool_result"],
        "{history}"
    );
    assert!(
        !history.to_string().contains("tool_progress"),
        "progress is not in the transcript: {history}"
    );
}

#[tokio::test]
async fn a_sub_agents_tool_progress_arrives_inside_its_subagent_progress() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        serde_json::json!({
            "description": "print lines",
            "prompt": "Run the command",
            "subagent_type": "general-purpose",
        }),
    ));
    model.push_turn(tool_round(
        "Bash",
        "toolu_inner",
        serde_json::json!({"command": LINES}),
    ));
    model.push_turn(text_round("printed"));
    model.push_turn(text_round("done"));
    let daemon = Daemon::start(model).await;
    let session = daemon.session().await;
    let frames = daemon.watch(&session).await;
    daemon.run_turn(&session).await;
    let frames = until_turn_complete(&frames).await;

    assert!(
        !frames.iter().any(|f| f["event"]["kind"] == "tool_progress"),
        "a sub-agent's progress is not the parent's own: {frames:#?}"
    );
    let wrapped = frames
        .iter()
        .find(|f| {
            f["event"]["kind"] == "subagent_progress"
                && f["event"]["event"]["kind"] == "tool_progress"
        })
        .unwrap_or_else(|| panic!("no wrapped tool_progress: {frames:#?}"));
    assert_eq!(wrapped["event"]["parent_tool_use_id"], "toolu_agent");
    let inner = &wrapped["event"]["event"];
    assert_eq!(inner["tool_use_id"], "toolu_inner", "{inner}");
    assert_eq!(inner["name"], "Bash", "{inner}");
    assert!(inner["dropped"].is_u64(), "{inner}");
}

#[tokio::test]
async fn the_daemon_says_it_streams_tool_progress() {
    let daemon = Daemon::start(Arc::new(MockAnthropicClient::new())).await;
    let ping = daemon.ok("daemon.ping", serde_json::json!({})).await;
    assert!(
        ping["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "tool_progress"),
        "{ping}"
    );
}
