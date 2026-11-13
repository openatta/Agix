//! A provider that keeps refusing, seen from the RPC side: once the client's
//! backoff gives up, the turn ends with `ENGINE_ERROR`, is not sent again on
//! top of the ladder, and `error.data` tells the host whether and when to
//! come back.

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

/// A `retry-after` past what the ladder waits for, so the client gives up on
/// the first refusal instead of spending a minute on its steps.
const RATE_LIMITED: Refusal = Refusal {
    status: "429 Too Many Requests",
    retry_after: Some(120),
    error_type: "rate_limit_error",
};

const OVERLOADED: Refusal = Refusal {
    status: "529 Overloaded",
    retry_after: Some(120),
    error_type: "overloaded_error",
};

const BAD_KEY: Refusal = Refusal {
    status: "401 Unauthorized",
    retry_after: None,
    error_type: "authentication_error",
};

struct Refusal {
    status: &'static str,
    retry_after: Option<u64>,
    error_type: &'static str,
}

impl Refusal {
    fn response(&self) -> Vec<u8> {
        let body = format!(
            r#"{{"type":"error","error":{{"type":"{}","message":"no"}}}}"#,
            self.error_type
        );
        let retry_after = self
            .retry_after
            .map(|secs| format!("retry-after: {secs}\r\n"))
            .unwrap_or_default();
        format!(
            "HTTP/1.1 {}\r\n{retry_after}content-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            self.status,
            body.len()
        )
        .into_bytes()
    }
}

/// Every request is answered with `refusal`.
async fn always_refusing(refusal: &Refusal) -> (u16, Arc<AtomicUsize>) {
    let response = refusal.response();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    let counter = served.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 65536];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(&response).await;
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
    async fn start(refusal: &Refusal) -> Self {
        let (port, served) = always_refusing(refusal).await;
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
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
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
    let daemon = Daemon::start(&RATE_LIMITED).await;

    let failed = daemon.failed_turn().await;

    assert_eq!(failed["error"]["code"], codes::ENGINE_ERROR, "{failed}");
    assert_eq!(
        daemon.served.load(Ordering::SeqCst),
        1,
        "the turn sent the request again after the client had given up"
    );
}

#[tokio::test]
async fn a_rate_limit_past_the_ladder_tells_the_host_when_to_come_back() {
    let daemon = Daemon::start(&RATE_LIMITED).await;

    let failed = daemon.failed_turn().await;

    let data = &failed["error"]["data"];
    assert_eq!(data["kind"], "rate_limited", "{failed}");
    assert_eq!(data["retryable"], true, "{failed}");
    assert_eq!(data["retry_after_ms"], 120_000, "{failed}");
}

#[tokio::test]
async fn an_overload_with_no_fallback_model_ends_the_turn_as_retryable() {
    let daemon = Daemon::start(&OVERLOADED).await;

    let failed = daemon.failed_turn().await;

    assert_eq!(failed["error"]["code"], codes::ENGINE_ERROR, "{failed}");
    assert_eq!(
        failed["error"]["data"],
        json!({"kind": "overloaded", "retryable": true, "retry_after_ms": 120_000,
               "side_effects": false}),
        "{failed}"
    );
    assert_eq!(daemon.served.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_rejected_key_ends_the_turn_as_not_retryable() {
    let daemon = Daemon::start(&BAD_KEY).await;

    let failed = daemon.failed_turn().await;

    assert_eq!(failed["error"]["code"], codes::ENGINE_ERROR, "{failed}");
    assert_eq!(
        failed["error"]["data"],
        json!({"kind": "auth", "retryable": false, "retry_after_ms": null,
               "side_effects": false}),
        "{failed}"
    );
}
