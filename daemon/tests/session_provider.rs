//! A session can be pointed at a provider of its own, and a model is checked
//! against the provider it will be sent to.
//!
//! The providers here are configured but never reached: these tests are about
//! which choices the daemon accepts and keeps, not about the HTTP that follows.

use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const INVALID_PARAMS: i64 = -32602;

struct Daemon {
    sock: std::path::PathBuf,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = serde_json::json!({
            "jsonrpc":"2.0","method":method,"params":params,"id":1
        });
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
        let response = self.call(method, params).await;
        assert!(
            response.get("result").is_some(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }

    async fn session(&self, options: serde_json::Value) -> String {
        self.ok("session.create", serde_json::json!({"options": options}))
            .await["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn provider(models: &[&str]) -> base::provider::ProviderConfig {
    base::provider::ProviderConfig {
        api_type: Some("anthropic".into()),
        base_url: Some("http://127.0.0.1:9".into()),
        api_key: Some("unused".into()),
        api_key_env: None,
        default_model: models.first().map(|m| m.to_string()),
        models: models.iter().map(|m| m.to_string()).collect(),
        ..Default::default()
    }
}

async fn start_daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("provider.sock");
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
    config
        .settings
        .providers
        .insert("one".into(), provider(&["one-a"]));
    config
        .settings
        .providers
        .insert("two".into(), provider(&["two-a", "two-b"]));
    config.settings.default_provider = Some("one".into());

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::scene::coding::CodingScene),
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            model_client: Some(Arc::new(MockAnthropicClient::new()) as Arc<dyn AnthropicClient>),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let serving = server.clone();
    let bind = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = serving.serve_unix(&bind).await;
    });
    for _ in 0..80 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");

    Daemon {
        sock,
        _dir: dir,
        _handle: handle,
    }
}

#[tokio::test]
async fn a_provider_nobody_configured_is_refused_at_creation() {
    let daemon = start_daemon().await;
    let response = daemon
        .call(
            "session.create",
            serde_json::json!({"options": {"provider": "nope"}}),
        )
        .await;
    assert_eq!(response["error"]["code"], INVALID_PARAMS, "{response}");
}

#[tokio::test]
async fn a_model_is_checked_against_the_provider_it_would_go_to() {
    let daemon = start_daemon().await;
    let session = daemon.session(serde_json::json!({"provider": "two"})).await;

    let refused = daemon
        .call(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": "one-a"}),
        )
        .await;
    assert_eq!(
        refused["error"]["code"], INVALID_PARAMS,
        "one-a is provider one's model, and this session talks to two: {refused}"
    );

    let accepted = daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": "two-b"}),
        )
        .await;
    assert_eq!(accepted["model"], "two-b");
    assert_eq!(accepted["provider"], "two");
}

#[tokio::test]
async fn a_session_moves_to_another_provider_and_stays_there() {
    let daemon = start_daemon().await;
    let session = daemon.session(serde_json::json!({})).await;

    let switched = daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "provider": "two"}),
        )
        .await;
    assert_eq!(switched["provider"], "two");
    assert!(
        switched["model"].is_null(),
        "no model was chosen for the new provider, so its default applies: {switched}"
    );

    daemon.ok("config.reload", serde_json::json!({})).await;

    let refused = daemon
        .call(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": "one-a"}),
        )
        .await;
    assert_eq!(
        refused["error"]["code"], INVALID_PARAMS,
        "after a reload the session still talks to provider two: {refused}"
    );
}
