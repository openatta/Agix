//! What a host adds to a session engine through `Assembly::customize_session`
//! is on every engine the session runs on, including the one rebuilt after a
//! settings change.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base::event::AgentEvent;
use base::interface::event_sink::EventSink;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

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

#[derive(Default)]
struct CountTurns(AtomicUsize);

impl EventSink for CountTurns {
    fn emit(&self, event: &AgentEvent) {
        if matches!(event, AgentEvent::TurnComplete { .. }) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

async fn call(
    sock: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let request = serde_json::json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
    let mut conn = UnixStream::connect(sock).await.unwrap();
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

#[tokio::test]
async fn a_rebuilt_session_still_has_what_the_host_added() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("customize.sock");
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

    let turns = Arc::new(CountTurns::default());
    let builds: Arc<Mutex<Vec<bool>>> = Arc::default();
    let (sink, seen) = (turns.clone(), builds.clone());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::scene::coding::CodingScene),
        daemon::Assembly {
            cwd: Some(cwd),
            model_client: Some(model as Arc<dyn AnthropicClient>),
            customize_session: Some(Arc::new(move |builder, ctx| {
                seen.lock().unwrap().push(ctx.resumed);
                builder.event_sink(sink.clone())
            })),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let serving = server.clone();
    let bind = sock.clone();
    tokio::spawn(async move {
        let _ = serving.serve_unix(&bind).await;
    });
    for _ in 0..80 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let sid = call(&sock, "session.create", serde_json::json!({})).await["session_id"].clone();
    call(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": sid, "message": "first"}),
    )
    .await;
    call(&sock, "config.reload", serde_json::json!({})).await;
    call(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": sid, "message": "second"}),
    )
    .await;

    assert_eq!(
        *builds.lock().unwrap(),
        vec![false, true],
        "the customizer runs on the first build and again on the rebuild"
    );
    for _ in 0..80 {
        if turns.0.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        turns.0.load(Ordering::SeqCst),
        2,
        "the sink the host attached saw the turn served by the rebuilt engine too"
    );
}
