//! A provider that keeps refusing with 429, seen from the RPC side: once the
//! client's backoff gives up, the turn ends with `ENGINE_ERROR` and is not
//! sent again on top of the ladder.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::rpc::codes;
use daemon::DaemonServer;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio_util::sync::CancellationToken;

/// Every request is answered 429 with a `retry-after` past what the ladder
/// waits for, so the client gives up on the first one instead of spending a
/// minute on its steps.
async fn always_rate_limited() -> (u16, Arc<AtomicUsize>) {
    const RESPONSE: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\n\
retry-after: 120\r\n\
content-type: application/json\r\n\
content-length: 69\r\n\
connection: close\r\n\
\r\n\
{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow\"}}";

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    let counter = served.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 65536];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(RESPONSE).await;
            let _ = sock.shutdown().await;
        }
    });
    (port, served)
}

struct Daemon {
    sock: std::path::PathBuf,
    served: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start() -> Self {
        let (port, served) = always_rate_limited().await;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("rate-limit.sock");
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
        let client = HttpAnthropicClient::with_base(
            AuthMode::ApiKey("k".into()),
            url::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
        )
        .unwrap();
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene::default()),
            daemon::Assembly {
                cwd: Some(cwd),
                model_client: Some(Arc::new(client) as Arc<dyn AnthropicClient>),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let bind = sock.clone();
        let handle = tokio::spawn(async move {
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
            served,
            _dir: dir,
            _handle: handle,
        }
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "method": method, "params": params, "id": 1});
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

    async fn failed_turn(&self) -> Value {
        let created = self.call("session.create", json!({})).await;
        let session = created["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{created}"))
            .to_string();
        tokio::time::timeout(
            Duration::from_secs(60),
            self.call(
                "session.run_turn",
                json!({"session_id": session, "message": "hi"}),
            ),
        )
        .await
        .expect("the turn never ended")
    }
}

#[tokio::test]
async fn a_rate_limit_past_the_ladder_ends_the_turn_without_a_resend() {
    let daemon = Daemon::start().await;

    let failed = daemon.failed_turn().await;

    assert_eq!(failed["error"]["code"], codes::ENGINE_ERROR, "{failed}");
    assert_eq!(
        daemon.served.load(Ordering::SeqCst),
        1,
        "the turn sent the request again after the client had given up"
    );
}

#[tokio::test]
#[ignore = "D3 E4"]
async fn a_rate_limit_past_the_ladder_tells_the_host_when_to_come_back() {
    let daemon = Daemon::start().await;

    let failed = daemon.failed_turn().await;

    let data = &failed["error"]["data"];
    assert_eq!(data["kind"], "rate_limited", "{failed}");
    assert_eq!(data["retryable"], true, "{failed}");
    assert_eq!(data["retry_after_ms"], 120_000, "{failed}");
}
