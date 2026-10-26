//! A scheduled job is a turn of its session, started by the daemon.
//!
//! It runs whether or not anyone is connected and whether or not the session
//! is in memory, through the same path a client's message takes — so it is
//! announced on `daemon.event` like any other turn, with initiator
//! `"schedule"`. The scheduler's clock is not waited on: `run_due_jobs` is
//! called with a chosen time.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::{DaemonServer, SessionPool};
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const INVALID_PARAMS: i64 = -32602;
const SESSION_NOT_FOUND: i64 = -32000;

fn one_turn() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta { text: "ok".into() },
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

struct Daemon {
    sock: std::path::PathBuf,
    pool: Arc<SessionPool>,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("schedule.sock");
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
        let model = Arc::new(MockAnthropicClient::new());
        model.push_turn(one_turn());
        model.push_turn(one_turn());
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
        let server = Arc::new(DaemonServer::new(pool.clone(), CancellationToken::new()));
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
            pool,
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

    /// Every `daemon.event` this daemon sends from now on.
    async fn watch_events(&self) -> Arc<Mutex<Vec<serde_json::Value>>> {
        let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
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
                let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
                if frame["method"] == "daemon.event" {
                    into.lock().unwrap().push(frame["params"].clone());
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen
    }
}

fn at(rfc3339: &str) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339).unwrap()
}

async fn wait_for(
    events: &Arc<Mutex<Vec<serde_json::Value>>>,
    found: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    for _ in 0..200 {
        if let Some(event) = events.lock().unwrap().iter().find(|e| found(e)) {
            return event.clone();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no matching daemon.event; saw {:?}", events.lock().unwrap());
}

#[tokio::test]
async fn a_scheduled_job_runs_as_a_turn_of_its_session_with_nobody_connected() {
    let daemon = Daemon::start().await;
    let events = daemon.watch_events().await;

    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for(&events, |e| {
        e["kind"] == "session_created" && e["session_id"] == session.as_str()
    })
    .await;

    let job = daemon
        .ok(
            "schedule.create",
            serde_json::json!({
                "session_id": session,
                "cron": "0 9 * * *",
                "prompt": "the nine o'clock report",
            }),
        )
        .await["job"]
        .clone();
    assert_eq!(job["recurring"], true);
    assert_eq!(job["durable"], true);
    let listed = daemon
        .ok("schedule.list", serde_json::json!({"session_id": session}))
        .await;
    assert_eq!(listed["jobs"].as_array().unwrap().len(), 1);

    daemon
        .ok("session.close", serde_json::json!({"session_id": session}))
        .await;
    wait_for(&events, |e| {
        e["kind"] == "session_closed" && e["session_id"] == session.as_str()
    })
    .await;

    daemon.pool.run_due_jobs(at("2026-10-26T09:00:10Z")).await;

    let request = serde_json::to_string(&daemon.model.nth_request(0).expect("a turn ran")).unwrap();
    assert!(request.contains("the nine o'clock report"));
    let started = wait_for(&events, |e| {
        e["kind"] == "turn_started" && e["session_id"] == session.as_str()
    })
    .await;
    assert_eq!(started["initiator"], "schedule");
    let completed = wait_for(&events, |e| {
        e["kind"] == "turn_completed" && e["session_id"] == session.as_str()
    })
    .await;
    assert_eq!(completed["initiator"], "schedule");
    assert_eq!(completed["turn_id"], started["turn_id"]);

    daemon.pool.run_due_jobs(at("2026-10-26T10:00:10Z")).await;
    assert_eq!(daemon.model.calls(), 1, "not due at ten");

    let deleted = daemon
        .ok("schedule.delete", serde_json::json!({"id": job["id"]}))
        .await;
    assert_eq!(deleted["deleted"], true);
    assert!(
        daemon.ok("schedule.list", serde_json::json!({})).await["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_client_turn_is_announced_with_its_initiator() {
    let daemon = Daemon::start().await;
    let events = daemon.watch_events().await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "hello"}),
        )
        .await;
    let completed = wait_for(&events, |e| {
        e["kind"] == "turn_completed" && e["session_id"] == session.as_str()
    })
    .await;
    assert_eq!(completed["initiator"], "client");
}

#[tokio::test]
async fn a_job_needs_a_real_session_and_a_valid_expression() {
    let daemon = Daemon::start().await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let bad_cron = daemon
        .call(
            "schedule.create",
            serde_json::json!({"session_id": session, "cron": "every day", "prompt": "x"}),
        )
        .await;
    assert_eq!(bad_cron["error"]["code"], INVALID_PARAMS, "{bad_cron}");

    let no_session = daemon
        .call(
            "schedule.create",
            serde_json::json!({"session_id": "nope", "cron": "0 9 * * *", "prompt": "x"}),
        )
        .await;
    assert_eq!(
        no_session["error"]["code"], SESSION_NOT_FOUND,
        "{no_session}"
    );

    let missing = daemon
        .call("schedule.create", serde_json::json!({"cron": "0 9 * * *"}))
        .await;
    assert_eq!(missing["error"]["code"], INVALID_PARAMS, "{missing}");
}
