//! A session goal keeps its session working: when a turn ends normally the
//! pool starts the next round itself, announced on `daemon.event` with
//! initiator `"goal"`, until the goal is complete, blocked or out of rounds.
//! Hosts read and change it over `session.goal` / `session.setGoal` /
//! `session.updateGoal`; the model through the `Goal*` tools.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const INVALID_PARAMS: i64 = -32602;

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

fn tool_round(tool: &str, id: &str, input: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: tool.to_string(),
                input: Value::Null,
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
    async fn start(rounds: Vec<Vec<StreamEvent>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("goal.sock");
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
        let model = Arc::new(MockAnthropicClient::new());
        for round in rounds {
            model.push_turn(round);
        }
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene::default()),
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

    async fn call(&self, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
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
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                return frame;
            }
        }
    }

    async fn ok(&self, method: &str, params: Value) -> Value {
        let frame = self.call(method, params).await;
        assert!(frame.get("result").is_some(), "{method} failed: {frame}");
        frame["result"].clone()
    }

    /// Every `daemon.event` this daemon sends from now on.
    async fn watch_events(&self) -> Arc<Mutex<Vec<Value>>> {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.subscribeEvents\",\"id\":1}\n")
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
                let frame: Value = serde_json::from_str(&line).unwrap();
                if frame["method"] == "daemon.event" {
                    into.lock().unwrap().push(frame["params"].clone());
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen
    }

    async fn first_turn(&self) -> String {
        self.ok("session.run_turn", json!({"message": "hello"}))
            .await["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Polls `session.goal` until `done` holds for the goal.
    async fn goal_when(&self, session: &str, done: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..200 {
            let goal = self
                .ok("session.goal", json!({"session_id": session}))
                .await["goal"]
                .clone();
            if done(&goal) {
                return goal;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "goal never reached the expected state: {}",
            self.ok("session.goal", json!({"session_id": session}))
                .await
        );
    }
}

fn goal_turns(events: &Arc<Mutex<Vec<Value>>>, session: &str) -> usize {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| {
            e["kind"] == "turn_started" && e["session_id"] == session && e["initiator"] == "goal"
        })
        .count()
}

#[tokio::test]
async fn a_goal_set_by_the_host_runs_its_rounds_and_stops_at_the_limit() {
    let daemon = Daemon::start(vec![
        text_round("hi"),
        text_round("round one"),
        text_round("round two"),
    ])
    .await;
    let events = daemon.watch_events().await;
    let session = daemon.first_turn().await;

    let none = daemon
        .ok("session.goal", json!({"session_id": session}))
        .await;
    assert!(none["goal"].is_null(), "{none}");

    let set = daemon
        .ok(
            "session.setGoal",
            json!({"session_id": session, "objective": "migrate the API", "max_rounds": 2}),
        )
        .await;
    assert_eq!(set["goal"]["phase"], "active", "{set}");
    assert_eq!(set["goal"]["armed"], true, "{set}");

    let blocked = daemon
        .goal_when(&session, |g| g["phase"] == "blocked")
        .await;
    assert_eq!(blocked["rounds_started"], 2, "{blocked}");
    assert!(
        blocked["blocked_reason"]
            .as_str()
            .unwrap()
            .contains("round limit"),
        "{blocked}"
    );
    assert_eq!(goal_turns(&events, &session), 2);

    // Read back from the log once the session has left memory: disarmed.
    daemon
        .ok("session.close", json!({"session_id": session}))
        .await;
    let cold = daemon
        .ok("session.goal", json!({"session_id": session}))
        .await;
    assert_eq!(cold["goal"]["phase"], "blocked", "{cold}");
    assert_eq!(cold["goal"]["armed"], false, "{cold}");
    assert_eq!(cold["goal"]["objective"], "migrate the API", "{cold}");
}

#[tokio::test]
async fn a_host_edits_resumes_and_clears_a_goal() {
    let daemon = Daemon::start(vec![
        text_round("hi"),
        text_round("round one"),
        text_round("round two"),
    ])
    .await;
    let session = daemon.first_turn().await;

    let refused = daemon
        .call(
            "session.updateGoal",
            json!({"session_id": session, "action": "pause"}),
        )
        .await;
    assert_eq!(refused["error"]["code"], INVALID_PARAMS, "{refused}");

    daemon
        .ok(
            "session.setGoal",
            json!({"session_id": session, "objective": "tests green", "max_rounds": 1}),
        )
        .await;
    daemon
        .goal_when(&session, |g| g["phase"] == "blocked")
        .await;

    let edited = daemon
        .ok(
            "session.updateGoal",
            json!({"session_id": session, "action": "edit", "max_rounds": 2}),
        )
        .await;
    assert_eq!(edited["goal"]["max_rounds"], 2, "{edited}");
    assert_eq!(edited["goal"]["phase"], "blocked", "{edited}");

    daemon
        .ok(
            "session.updateGoal",
            json!({"session_id": session, "action": "resume"}),
        )
        .await;
    let again = daemon
        .goal_when(&session, |g| {
            g["phase"] == "blocked" && g["rounds_started"] == 2
        })
        .await;
    assert_eq!(again["armed"], false, "{again}");

    let cleared = daemon
        .ok(
            "session.updateGoal",
            json!({"session_id": session, "action": "clear"}),
        )
        .await;
    assert!(cleared["goal"].is_null(), "{cleared}");
}

#[tokio::test]
async fn a_goal_the_model_sets_continues_after_the_turn() {
    let daemon = Daemon::start(vec![
        tool_round(
            "GoalCreate",
            "toolu_goal",
            json!({"objective": "ship it", "max_rounds": 1}),
        ),
        text_round("goal set"),
        text_round("round one"),
    ])
    .await;
    let events = daemon.watch_events().await;
    let session = daemon.first_turn().await;

    let goal = daemon
        .goal_when(&session, |g| g["phase"] == "blocked")
        .await;
    assert_eq!(goal["objective"], "ship it", "{goal}");
    assert_eq!(goal["rounds_started"], 1, "{goal}");
    assert_eq!(goal_turns(&events, &session), 1);
}
