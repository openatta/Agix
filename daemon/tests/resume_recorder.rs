//! `recorder` is never written to a session's log, so after the daemon
//! restarts, the call that brings the session back is where it comes from.

use std::path::{Path, PathBuf};
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

/// One daemon process over `root`; a second one over the same root is a restart.
async fn start(
    root: &Path,
    name: &str,
    model: Arc<MockAnthropicClient>,
) -> (PathBuf, CancellationToken) {
    let sock = root.join(format!("{name}.sock"));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "coding",
        &StaticDaemonPaths::new(root.to_path_buf()),
    );
    config.settings.memory_enabled = false;
    config.settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;
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
    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel.clone()));
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
    (sock, cancel)
}

async fn ok(sock: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
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
async fn a_session_brought_back_after_a_restart_records_again_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let recordings = dir.path().join("recordings");
    let recorder = serde_json::json!({"mode": "record", "dir": recordings.display().to_string()});

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("first"));
    let (sock, first) = start(dir.path(), "first", model.clone()).await;
    let session = ok(
        &sock,
        "session.create",
        serde_json::json!({"options": {"recorder": recorder}}),
    )
    .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    ok(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": session, "message": "one"}),
    )
    .await;
    let recording = recordings.join(&session);
    assert!(recording.exists(), "the first daemon did not record");
    first.cancel();
    std::fs::remove_dir_all(&recording).unwrap();

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("second"));
    let (sock, _second) = start(dir.path(), "second", model).await;
    ok(
        &sock,
        "session.resume",
        serde_json::json!({"session_id": session, "options": {"recorder": recorder}}),
    )
    .await;
    ok(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": session, "message": "two"}),
    )
    .await;
    assert!(recording.exists(), "the resumed session did not record");
}
