//! A session goal keeps its session working: when a turn ends normally the
//! pool starts the next round itself, announced on `daemon.event` with
//! initiator `"goal"`, until the goal is complete, blocked or out of rounds.
//! Hosts read and change it over `session.goal` / `session.setGoal` /
//! `session.updateGoal`; the model through the `Goal*` tools.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use futures::StreamExt;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use model::types::MessagesRequest;
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

fn round_ending(stop_reason: base::message::StopReason) -> Vec<StreamEvent> {
    let mut round = text_round("working on it");
    if let Some(StreamEvent::MessageDelta { delta, .. }) = round.last_mut() {
        delta.stop_reason = Some(stop_reason);
    }
    round
}

/// Answers every call with the same round, each after `delay` — slow enough
/// for a test to act while a turn is still running.
struct SlowModel {
    round: Vec<StreamEvent>,
    delay: Duration,
}

impl AnthropicClient for SlowModel {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        let (round, delay) = (self.round.clone(), self.delay);
        Box::pin(
            futures::stream::once(tokio::time::sleep(delay))
                .flat_map(move |_| futures::stream::iter(round.clone().into_iter().map(Ok))),
        )
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
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
        let model = Arc::new(MockAnthropicClient::new());
        for round in rounds {
            model.push_turn(round);
        }
        Self::start_with(model).await
    }

    async fn start_with(model: Arc<dyn AnthropicClient>) -> Self {
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
        config.settings.values.memory_enabled = false;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
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
    goal_turn_ids(events, session).len()
}

fn goal_turn_ids(events: &Arc<Mutex<Vec<Value>>>, session: &str) -> Vec<String> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| {
            e["kind"] == "turn_started" && e["session_id"] == session && e["initiator"] == "goal"
        })
        .map(|e| e["turn_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

async fn until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..400 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never happened: {what}");
}

fn slow_daemon_model() -> Arc<dyn AnthropicClient> {
    Arc::new(SlowModel {
        round: text_round("working on it"),
        delay: Duration::from_millis(150),
    })
}

/// The round number a goal round's turn id carries (`goal-<revision>-<round>-…`).
fn round_of(turn_id: &str) -> u32 {
    turn_id.split('-').nth(2).unwrap().parse().unwrap()
}

/// Run a queued client turn on `session` in the background, once the first
/// goal round is running.
async fn queue_a_client_turn(daemon: &Arc<Daemon>, events: &Arc<Mutex<Vec<Value>>>, session: &str) {
    until("the first goal round starts", || {
        goal_turns(events, session) == 1
    })
    .await;
    let (daemon, session) = (daemon.clone(), session.to_string());
    tokio::spawn(async move {
        daemon
            .ok(
                "session.run_turn",
                json!({"session_id": session, "message": "meanwhile", "on_busy": "queue"}),
            )
            .await;
    });
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

#[tokio::test]
async fn a_turn_queued_behind_a_goal_round_does_not_start_a_second_chain_of_rounds() {
    let daemon = Arc::new(Daemon::start_with(slow_daemon_model()).await);
    let events = daemon.watch_events().await;
    let session = daemon.first_turn().await;

    daemon
        .ok(
            "session.setGoal",
            json!({"session_id": session, "objective": "keep going", "max_rounds": 4}),
        )
        .await;
    queue_a_client_turn(&daemon, &events, &session).await;

    let blocked = daemon
        .goal_when(&session, |g| g["phase"] == "blocked")
        .await;
    assert_eq!(blocked["rounds_started"], 4, "{blocked}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let rounds: Vec<u32> = goal_turn_ids(&events, &session)
        .iter()
        .map(|id| round_of(id))
        .collect();
    assert_eq!(rounds, vec![1, 2, 3, 4], "one round after another");
}

#[tokio::test]
async fn a_paused_goal_starts_no_round_a_queued_turn_left_behind() {
    let daemon = Arc::new(Daemon::start_with(slow_daemon_model()).await);
    let events = daemon.watch_events().await;
    let session = daemon.first_turn().await;

    daemon
        .ok(
            "session.setGoal",
            json!({"session_id": session, "objective": "keep going", "max_rounds": 6}),
        )
        .await;
    queue_a_client_turn(&daemon, &events, &session).await;
    until("the second goal round starts", || {
        goal_turns(&events, &session) == 2
    })
    .await;
    daemon
        .ok(
            "session.updateGoal",
            json!({"session_id": session, "action": "pause"}),
        )
        .await;
    let ran_before_the_pause = goal_turns(&events, &session);

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(goal_turns(&events, &session), ran_before_the_pause);
    let goal = daemon
        .ok("session.goal", json!({"session_id": session}))
        .await["goal"]
        .clone();
    assert_eq!(goal["phase"], "paused", "{goal}");
    assert_eq!(goal["rounds_started"], ran_before_the_pause, "{goal}");
}

#[tokio::test]
async fn a_round_cut_off_at_the_output_limit_keeps_the_goal_going() {
    let daemon = Daemon::start_with(Arc::new(SlowModel {
        round: round_ending(base::message::StopReason::MaxTokens),
        delay: Duration::ZERO,
    }))
    .await;
    let session = daemon.first_turn().await;

    daemon
        .ok(
            "session.setGoal",
            json!({"session_id": session, "objective": "write it all", "max_rounds": 2}),
        )
        .await;
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
}

#[tokio::test]
async fn a_session_rebuilt_for_a_new_prompt_keeps_its_goal_running() {
    let daemon = Arc::new(Daemon::start_with(slow_daemon_model()).await);
    let events = daemon.watch_events().await;
    let session = daemon.first_turn().await;

    daemon
        .ok(
            "session.setGoal",
            json!({"session_id": session, "objective": "keep going", "max_rounds": 3}),
        )
        .await;
    until("the first goal round starts", || {
        goal_turns(&events, &session) == 1
    })
    .await;
    let mut rebuilt = false;
    for _ in 0..400 {
        let reply = daemon
            .call(
                "session.setPrompt",
                json!({"session_id": session, "append": "Answer briefly."}),
            )
            .await;
        if reply.get("result").is_some() {
            rebuilt = true;
            break;
        }
        assert_eq!(
            reply["error"]["code"], -32015,
            "only a busy session refuses: {reply}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(rebuilt, "the session was never idle long enough to rebuild");

    let blocked = daemon
        .goal_when(&session, |g| g["phase"] == "blocked")
        .await;
    assert_eq!(blocked["rounds_started"], 3, "{blocked}");
    assert!(
        blocked["blocked_reason"]
            .as_str()
            .unwrap()
            .contains("round limit"),
        "{blocked}"
    );
}

#[tokio::test]
async fn a_rebuilt_session_tells_its_watchers_where_its_goal_stands() {
    let daemon = Daemon::start(vec![text_round("hi"), text_round("round one")]).await;
    let session = daemon.first_turn().await;
    daemon
        .ok(
            "session.setGoal",
            json!({"session_id": session, "objective": "tests green", "max_rounds": 1}),
        )
        .await;
    daemon
        .goal_when(&session, |g| g["phase"] == "blocked")
        .await;

    let mut watcher = UnixStream::connect(&daemon.sock).await.unwrap();
    let subscribe = json!({"jsonrpc":"2.0","method":"session.subscribe","params":{"session_id": session},"id":1});
    watcher
        .write_all(format!("{subscribe}\n").as_bytes())
        .await
        .unwrap();
    let (r, _w) = watcher.split();
    let mut watched = BufReader::new(r);
    let mut line = String::new();
    watched.read_line(&mut line).await.unwrap();

    daemon
        .ok(
            "session.setPrompt",
            json!({"session_id": session, "append": "Answer briefly."}),
        )
        .await;
    let goal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            assert!(
                watched.read_line(&mut line).await.unwrap() > 0,
                "watcher closed"
            );
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["params"]["event"]["kind"] == "goal_changed" {
                return frame["params"]["event"]["goal"].clone();
            }
        }
    })
    .await
    .expect("no goal_changed after the rebuild");
    assert_eq!(goal["phase"], "blocked", "{goal}");
    assert_eq!(goal["objective"], "tests green", "{goal}");
}
