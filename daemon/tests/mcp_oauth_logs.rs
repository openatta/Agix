//! No OAuth token reaches the daemon's log.
//!
//! A test binary of its own: the log is captured by installing the process's
//! global `tracing` subscriber, which the shared integration binary cannot
//! do reliably while its other tests install theirs. The whole sign-in runs
//! at `DEBUG` — discovery, registration, the code exchange, a refresh, the
//! connection, a tool call — and every token the authorization server issued
//! is then looked for in what was logged.

#![cfg(all(unix, feature = "mcp"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use mcp_oauth_mock::{consent, follow, McpServer};
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn round(
    block: ContentBlockStart,
    delta: BlockDelta,
    stop: base::message::StopReason,
) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: block,
        },
        StreamEvent::ContentBlockDelta { index: 0, delta },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(stop),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

async fn rpc(sock: &std::path::Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    let mut conn = UnixStream::connect(sock).await.unwrap();
    let request =
        serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params, "id": 1});
    conn.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let (r, _w) = conn.split();
    let mut reader = BufReader::new(r);
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).await.unwrap() > 0);
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        if frame.get("id").is_some() {
            assert!(frame.get("error").is_none(), "{method}: {frame}");
            return frame["result"].clone();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_token_is_ever_logged() {
    let logs = Captured::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();

    let (mcp, auth) = McpServer::start().await;
    auth.state.lock().unwrap().token_ttl = 5;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join(".agix")).unwrap();
    std::fs::write(
        root.join(".agix/settings.json"),
        serde_json::json!({"mcp_servers": {"docs": {"type": "streamable_http", "url": mcp.url}}})
            .to_string(),
    )
    .unwrap();
    let sock = root.join("d.sock");
    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(root.clone()),
    );
    config.settings.values.memory_enabled = false;
    config.settings.values.permission_mode = base::settings::PermissionMode::BypassPermissions;
    let model = Arc::new(MockAnthropicClient::new());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(root.clone()),
            model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let stop = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool.clone(), stop.clone()));
    let bind = sock.clone();
    tokio::spawn(async move {
        let _ = server.serve_unix(&bind).await;
    });
    while !sock.exists() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let wait_for = |state: &'static str| {
        let sock = sock.clone();
        async move {
            for _ in 0..600 {
                let status = rpc(&sock, "mcp.status", serde_json::json!({})).await;
                if status["servers"][0]["state"] == state {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("docs never became {state}");
        }
    };
    wait_for("auth_required").await;
    let start = rpc(
        &sock,
        "mcp.auth.start",
        serde_json::json!({"server": "docs"}),
    )
    .await;
    let location = consent(start["authorize_url"].as_str().unwrap()).await;
    assert_eq!(follow(&location).await.0, 200);
    wait_for("connected").await;

    model.push_turn(round(
        ContentBlockStart::ToolUse {
            id: "t1".into(),
            name: "mcp__docs__echo".into(),
            input: serde_json::Value::Null,
        },
        BlockDelta::InputJsonDelta {
            partial_json: "{}".into(),
        },
        base::message::StopReason::ToolUse,
    ));
    model.push_turn(round(
        ContentBlockStart::Text {
            text: String::new(),
        },
        BlockDelta::TextDelta {
            text: "done".into(),
        },
        base::message::StopReason::EndTurn,
    ));
    let session = rpc(&sock, "session.create", serde_json::json!({})).await;
    rpc(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": session["session_id"], "message": "use docs"}),
    )
    .await;
    stop.cancel();

    let issued = auth.state.lock().unwrap().issued.clone();
    assert!(
        auth.state.lock().unwrap().refreshes >= 1,
        "a refresh was logged through too"
    );
    assert!(!mcp.state.lock().unwrap().tokens_seen.is_empty());
    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("DEBUG"), "the capture saw debug output");
    for token in &issued {
        if let Some(line) = logged.lines().find(|l| l.contains(token.as_str())) {
            panic!("a token reached the log: {line}");
        }
    }
}
