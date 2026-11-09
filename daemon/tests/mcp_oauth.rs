//! MCP OAuth through the daemon's JSON-RPC surface: a server that answers 401
//! waits for a sign-in, `mcp.auth.*` runs it against a mock authorization
//! server, and what comes out — the connection, its tools, `mcp.status`,
//! the `daemon.event`s, the credential file — is checked from outside.

#![cfg(all(unix, feature = "mcp"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::rpc::codes;
use daemon::{DaemonServer, SessionPool};
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

use crate::mcp_oauth_mock::{consent, follow, McpServer};

struct Daemon {
    dir: tempfile::TempDir,
    sock: PathBuf,
    pool: Arc<SessionPool>,
    model: Arc<MockAnthropicClient>,
    stop: CancellationToken,
}

impl Daemon {
    async fn start(servers: serde_json::Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".agix")).unwrap();
        std::fs::write(
            root.join(".agix").join("settings.json"),
            serde_json::json!({ "mcp_servers": servers }).to_string(),
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
        config.settings.memory_enabled = false;
        config.settings.permission_mode =
            base::interface::settings::PermissionMode::BypassPermissions;
        let model = Arc::new(MockAnthropicClient::new());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene::default()),
            daemon::Assembly {
                cwd: Some(root.clone()),
                model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let stop = CancellationToken::new();
        let server = Arc::new(DaemonServer::new(pool.clone(), stop.clone()));
        let bind = sock.clone();
        tokio::spawn(async move {
            let _ = server.serve_unix(&bind).await;
        });
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(sock.exists(), "socket never bound");
        Self {
            dir,
            sock,
            pool,
            model,
            stop,
        }
    }

    async fn connect(&self) -> Conn {
        let (r, w) = UnixStream::connect(&self.sock).await.unwrap().into_split();
        let mut conn = Conn {
            reader: BufReader::new(r),
            writer: w,
            next_id: 0,
            events: Vec::new(),
        };
        conn.ok("daemon.subscribeEvents", serde_json::json!({}))
            .await;
        conn
    }

    fn credential_files(&self) -> Vec<PathBuf> {
        let auth = self.pool.mcp_auth();
        let dir = auth.credential_dir().expect("the default store is files");
        std::fs::read_dir(dir)
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default()
    }

    async fn record(&self, url: &str) -> mcp::oauth::McpCredentials {
        self.pool
            .mcp_auth()
            .store()
            .load(url)
            .await
            .unwrap()
            .expect("a stored sign-in")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    events: Vec<serde_json::Value>,
}

impl Conn {
    async fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        let request =
            serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params, "id": id});
        self.writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let frame = self.frame().await;
            if frame["id"] == id {
                return frame;
            }
        }
    }

    async fn ok(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let frame = self.call(method, params).await;
        assert!(frame.get("error").is_none(), "{method} failed: {frame}");
        frame["result"].clone()
    }

    async fn err(&mut self, method: &str, params: serde_json::Value) -> (i64, String) {
        let frame = self.call(method, params).await;
        let error = &frame["error"];
        assert!(error.is_object(), "{method} should have failed: {frame}");
        (
            error["code"].as_i64().unwrap(),
            error["message"].as_str().unwrap_or_default().to_string(),
        )
    }

    async fn frame(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(60), self.reader.read_line(&mut line))
            .await
            .expect("the daemon went quiet")
            .unwrap();
        assert!(n > 0, "the daemon closed the connection");
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        if frame["method"] == "daemon.event" {
            self.events.push(frame["params"].clone());
        }
        frame
    }

    /// The first `daemon.event` of `kind` for `server`, seen before or after
    /// this call.
    async fn event(&mut self, kind: &str, server: &str) -> serde_json::Value {
        let matches = |e: &serde_json::Value| e["kind"] == kind && e["server"] == server;
        loop {
            if let Some(i) = self.events.iter().position(matches) {
                return self.events.remove(i);
            }
            self.frame().await;
        }
    }

    async fn status(&mut self, server: &str) -> serde_json::Value {
        let result = self.ok("mcp.status", serde_json::json!({})).await;
        result["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == server)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    async fn wait_state(&mut self, server: &str, state: &str) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.status(server).await;
            if status["state"] == state {
                return status;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "`{server}` never became {state}: {status}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Sign in to `server` through the loopback listener.
    async fn sign_in(&mut self, server: &str, params: serde_json::Value) -> serde_json::Value {
        let mut params = params;
        params["server"] = server.into();
        let start = self.ok("mcp.auth.start", params).await;
        let location = consent(start["authorize_url"].as_str().unwrap()).await;
        let (status, page) = follow(&location).await;
        assert_eq!(status, 200, "{page}");
        assert!(page.contains(&format!("Signed in to {server}")), "{page}");
        self.event("mcp_auth_completed", server).await;
        self.event("mcp_connected", server).await
    }
}

fn http_server(url: &str) -> serde_json::Value {
    serde_json::json!({"type": "streamable_http", "url": url})
}

fn tool_round(tool: &str, id: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: tool.into(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: "{}".into(),
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

async fn run_turn(conn: &mut Conn, message: &str) -> String {
    let session = conn.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    conn.ok(
        "session.run_turn",
        serde_json::json!({"session_id": session, "message": message}),
    )
    .await;
    session
}

fn every_file_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out
}

/// A token appears in the credential file and nowhere else the daemon writes
/// or answers; `mcp_oauth_logs` checks its log the same way.
#[tokio::test]
async fn a_server_that_asks_for_authorization_connects_after_a_loopback_sign_in() {
    let (mcp, auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;

    let waiting = conn.wait_state("docs", "auth_required").await;
    assert_eq!(waiting["tool_count"], 0);
    assert_eq!(waiting["auth"]["state"], "required");
    assert_eq!(waiting["auth"]["scopes"], serde_json::json!(["mcp:read"]));

    d.model.push_turn(text_round("noted"));
    run_turn(&mut conn, "hello").await;
    let system = serde_json::to_string(&d.model.nth_request(0).unwrap().system).unwrap();
    assert!(
        system.contains("docs") && system.contains("requires the user to sign in"),
        "the model is told the server needs a sign-in: {system}"
    );

    let start = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    let redirect = start["redirect_uri"].as_str().unwrap();
    assert!(redirect.starts_with("http://127.0.0.1:"), "{start}");
    assert!(redirect.ends_with("/callback"), "{start}");
    assert_eq!(start["expires_in"], 600);
    assert_eq!(conn.status("docs").await["auth"]["state"], "authorizing");

    let authorize: url::Url = start["authorize_url"].as_str().unwrap().parse().unwrap();
    let query: std::collections::HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["resource"], mcp.url);
    assert!(query["scope"].contains("mcp:read"), "{query:?}");

    let location = consent(authorize.as_str()).await;
    assert!(location.starts_with(redirect), "{location}");
    let (code, page) = follow(&location).await;
    assert_eq!(code, 200, "{page}");
    assert!(page.contains("Signed in to docs"), "{page}");
    conn.event("mcp_auth_completed", "docs").await;
    let connected = conn.event("mcp_connected", "docs").await;
    assert_eq!(connected["tool_count"], 2);

    let status = conn.wait_state("docs", "connected").await;
    assert_eq!(status["tool_count"], 2);
    assert_eq!(status["auth"]["state"], "authorized");
    assert_eq!(status["auth"]["issuer"], auth.url);
    assert!(status["auth"]["expires_at"].is_string(), "{status}");
    assert_eq!(auth.state.lock().unwrap().registrations, 1);

    d.model.push_turn(tool_round("mcp__docs__echo", "t1"));
    d.model.push_turn(text_round("done"));
    run_turn(&mut conn, "use docs").await;
    let after_tool = serde_json::to_string(&d.model.nth_request(2).unwrap()).unwrap();
    assert!(after_tool.contains("echo: ok"), "{after_tool}");
    assert!(!mcp.state.lock().unwrap().tokens_seen.is_empty());

    let files = d.credential_files();
    assert_eq!(files.len(), 1, "{files:?}");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let stored = std::fs::read_to_string(&files[0]).unwrap();
    let issued = auth.state.lock().unwrap().issued.clone();
    assert!(issued.iter().any(|t| stored.contains(t.as_str())));

    let rpc_seen = serde_json::to_string(&conn.events).unwrap()
        + &conn
            .ok("mcp.status", serde_json::json!({}))
            .await
            .to_string()
        + &start.to_string();
    for token in &issued {
        assert!(!rpc_seen.contains(token.as_str()), "a token reached RPC");
        for file in every_file_under(d.dir.path()) {
            if file == files[0] {
                continue;
            }
            let text = String::from_utf8_lossy(&std::fs::read(&file).unwrap()).to_string();
            assert!(
                !text.contains(token.as_str()),
                "a token reached {}",
                file.display()
            );
        }
    }
}

#[tokio::test]
async fn an_external_redirect_with_a_preregistered_client_completes_over_rpc() {
    let (mcp, auth) = McpServer::start().await;
    auth.preregister("pre-1", Some("s3cret"));
    mcp.state.lock().unwrap().challenge_scope = None;
    let var = format!("AGIX_TEST_MCP_SECRET_{}", std::process::id());
    std::env::set_var(&var, "s3cret");
    let d = Daemon::start(serde_json::json!({"docs": {
        "type": "streamable_http", "url": mcp.url,
        "oauth": {"client_id": "pre-1", "client_secret_env": var, "scopes": ["mcp:read", "mcp:write"]},
    }}))
    .await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;

    let (code, _) = conn
        .err(
            "mcp.auth.start",
            serde_json::json!({"server": "docs", "redirect": "external"}),
        )
        .await;
    assert_eq!(code, codes::INVALID_PARAMS as i64);
    let (code, _) = conn
        .err(
            "mcp.auth.start",
            serde_json::json!({"server": "docs", "redirect": "external", "redirect_uri": "http://app.example/cb"}),
        )
        .await;
    assert_eq!(
        code,
        codes::INVALID_PARAMS as i64,
        "plain http off loopback"
    );

    let start = conn
        .ok(
            "mcp.auth.start",
            serde_json::json!({"server": "docs", "redirect": "external", "redirect_uri": "https://app.example/oauth/done"}),
        )
        .await;
    assert_eq!(start["redirect_uri"], "https://app.example/oauth/done");
    let authorize: url::Url = start["authorize_url"].as_str().unwrap().parse().unwrap();
    let scope = authorize
        .query_pairs()
        .find(|(k, _)| k == "scope")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    assert!(
        scope.contains("mcp:read") && scope.contains("mcp:write"),
        "{scope}"
    );

    let location = consent(authorize.as_str()).await;
    let mut forged = url::Url::parse(&location).unwrap();
    let pairs: Vec<(String, String)> = forged
        .query_pairs()
        .map(|(k, v)| {
            let v = if k == "state" {
                "forged".into()
            } else {
                v.into_owned()
            };
            (k.into_owned(), v)
        })
        .collect();
    forged.query_pairs_mut().clear().extend_pairs(pairs);
    let (code, message) = conn
        .err(
            "mcp.auth.complete",
            serde_json::json!({"flow_id": start["flow_id"], "callback_url": forged.as_str()}),
        )
        .await;
    assert_eq!(code, codes::MCP_AUTH_FAILED as i64, "{message}");
    assert!(message.contains("state"), "{message}");

    let done = conn
        .ok(
            "mcp.auth.complete",
            serde_json::json!({"flow_id": start["flow_id"], "callback_url": location}),
        )
        .await;
    assert_eq!(done["server"], "docs");
    assert!(
        done["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "mcp:write"),
        "{done}"
    );
    conn.event("mcp_connected", "docs").await;
    assert_eq!(auth.state.lock().unwrap().registrations, 0);
    assert_eq!(d.record(&mcp.url).await.tokens.unwrap().client_id, "pre-1");

    let (code, _) = conn
        .err(
            "mcp.auth.complete",
            serde_json::json!({"flow_id": start["flow_id"], "callback_url": location}),
        )
        .await;
    assert_eq!(
        code,
        codes::INVALID_PARAMS as i64,
        "a finished flow is gone"
    );
}

#[tokio::test]
async fn an_authorization_server_without_s256_is_refused() {
    let (mcp, auth) = McpServer::start().await;
    auth.state.lock().unwrap().advertise_s256 = false;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    let (code, message) = conn
        .err("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    assert_eq!(code, codes::MCP_AUTH_FAILED as i64);
    assert!(message.contains("S256"), "{message}");
    let requests = auth.state.lock().unwrap().requests.clone();
    assert!(
        !requests.iter().any(|r| r.contains("/register")),
        "nothing was registered: {requests:?}"
    );
}

#[tokio::test]
async fn tokens_refresh_rotate_and_survive_a_transient_failure() {
    let (mcp, auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    auth.state.lock().unwrap().token_ttl = 5;
    conn.sign_in("docs", serde_json::json!({})).await;

    let refreshed = auth.state.lock().unwrap().refreshes;
    assert!(
        refreshed >= 1,
        "a token about to expire is refreshed before use"
    );
    let before = d.record(&mcp.url).await.tokens.unwrap();
    assert!(
        auth.state
            .lock()
            .unwrap()
            .tokens
            .contains_key(&before.access_token),
        "the stored token is the rotated one"
    );

    auth.state.lock().unwrap().token_ttl = 3600;
    mcp.state.lock().unwrap().unavailable = 2;
    conn.ok("config.reload", serde_json::json!({})).await;
    conn.wait_state("docs", "connected").await;
    assert_eq!(
        mcp.state.lock().unwrap().unavailable,
        0,
        "both 503s were hit"
    );
    let after = d.record(&mcp.url).await;
    assert!(after.tokens.is_some(), "a 503 does not cost the sign-in");
    assert_eq!(auth.state.lock().unwrap().registrations, 1);
    assert_eq!(conn.status("docs").await["auth"]["state"], "authorized");
}

#[tokio::test]
async fn a_rejected_refresh_drops_the_tokens_and_asks_again() {
    let (mcp, auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    conn.sign_in("docs", serde_json::json!({})).await;
    conn.events.clear();

    {
        let mut s = auth.state.lock().unwrap();
        s.tokens.clear();
        s.reject_refresh = true;
    }
    conn.ok("config.reload", serde_json::json!({})).await;
    conn.event("mcp_auth_required", "docs").await;
    let status = conn.wait_state("docs", "auth_required").await;
    assert_eq!(status["auth"]["state"], "required");
    let record = d.record(&mcp.url).await;
    assert!(record.tokens.is_none(), "rejected tokens are forgotten");
    assert!(
        record.client.is_some(),
        "the client registration is kept for the next sign-in"
    );

    auth.state.lock().unwrap().reject_refresh = false;
    conn.sign_in("docs", serde_json::json!({})).await;
    assert_eq!(
        auth.state.lock().unwrap().registrations,
        1,
        "signing in again reuses the registration"
    );
}

#[tokio::test]
async fn insufficient_scope_asks_for_more_and_keeps_the_server() {
    let (mcp, _auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    conn.sign_in("docs", serde_json::json!({"scopes": ["mcp:read"]}))
        .await;
    conn.events.clear();

    d.model.push_turn(tool_round("mcp__docs__write", "t1"));
    d.model.push_turn(text_round("it needs more access"));
    run_turn(&mut conn, "write it").await;
    let required = conn.event("mcp_auth_required", "docs").await;
    let scopes = required["scopes"].as_array().unwrap();
    assert!(scopes.iter().any(|s| s == "mcp:read"), "{required}");
    assert!(scopes.iter().any(|s| s == "mcp:write"), "{required}");
    let status = conn.status("docs").await;
    assert_eq!(status["state"], "connected", "{status}");
    assert_eq!(status["auth"]["state"], "required");

    let start = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    let authorize: url::Url = start["authorize_url"].as_str().unwrap().parse().unwrap();
    let scope = authorize
        .query_pairs()
        .find(|(k, _)| k == "scope")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    assert!(
        scope.contains("mcp:read") && scope.contains("mcp:write"),
        "{scope}"
    );
    let (code, page) = follow(&consent(authorize.as_str()).await).await;
    assert_eq!(code, 200, "{page}");
    conn.event("mcp_connected", "docs").await;

    d.model.push_turn(tool_round("mcp__docs__write", "t2"));
    d.model.push_turn(text_round("written"));
    run_turn(&mut conn, "write it now").await;
    let last = d.model.calls() - 1;
    let request = serde_json::to_string(&d.model.nth_request(last).unwrap()).unwrap();
    assert!(request.contains("write: ok"), "{request}");
}

#[tokio::test]
async fn logout_revokes_and_disconnects_but_keeps_the_registration() {
    let (mcp, auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    conn.sign_in("docs", serde_json::json!({})).await;
    let tokens = d.record(&mcp.url).await.tokens.unwrap();

    let out = conn
        .ok("mcp.auth.logout", serde_json::json!({"server": "docs"}))
        .await;
    assert_eq!(out["server"], "docs");
    let revoked = auth.state.lock().unwrap().revoked.clone();
    assert!(revoked.contains(&tokens.access_token), "{revoked:?}");
    assert!(
        revoked.contains(tokens.refresh_token.as_ref().unwrap()),
        "{revoked:?}"
    );
    let status = conn.wait_state("docs", "auth_required").await;
    assert_eq!(status["auth"]["state"], "required");
    assert_eq!(status["tool_count"], 0);
    let record = d.record(&mcp.url).await;
    assert!(record.tokens.is_none());
    assert!(record.client.is_some());
}

#[tokio::test]
async fn a_flow_expires_and_a_cancelled_one_is_gone() {
    let (mcp, _auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;

    let start = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    let out = conn
        .ok(
            "mcp.auth.cancel",
            serde_json::json!({"flow_id": start["flow_id"]}),
        )
        .await;
    assert_eq!(out["server"], "docs");
    assert_eq!(conn.status("docs").await["auth"]["state"], "required");
    let (code, _) = conn
        .err(
            "mcp.auth.cancel",
            serde_json::json!({"flow_id": start["flow_id"]}),
        )
        .await;
    assert_eq!(code, codes::INVALID_PARAMS as i64);

    d.pool.mcp_auth().set_flow_ttl(Duration::from_millis(300));
    let start = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    assert_eq!(start["expires_in"], 0);
    let failed = conn.event("mcp_auth_failed", "docs").await;
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("not completed in time"),
        "{failed}"
    );
    assert_eq!(conn.status("docs").await["auth"]["state"], "failed");
    let location = consent(start["authorize_url"].as_str().unwrap()).await;
    let (code, _) = conn
        .err(
            "mcp.auth.complete",
            serde_json::json!({"flow_id": start["flow_id"], "callback_url": location}),
        )
        .await;
    assert_eq!(code, codes::INVALID_PARAMS as i64);
}

#[tokio::test]
async fn two_sign_ins_run_side_by_side() {
    let (docs, _a) = McpServer::start().await;
    let (wiki, _b) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({
        "docs": http_server(&docs.url),
        "wiki": http_server(&wiki.url),
    }))
    .await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    conn.wait_state("wiki", "auth_required").await;

    let first = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    let second = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "wiki"}))
        .await;
    assert_ne!(first["flow_id"], second["flow_id"]);
    assert_ne!(first["redirect_uri"], second["redirect_uri"]);

    let wiki_back = consent(second["authorize_url"].as_str().unwrap()).await;
    let docs_back = consent(first["authorize_url"].as_str().unwrap()).await;
    assert_eq!(follow(&wiki_back).await.0, 200);
    conn.event("mcp_connected", "wiki").await;
    assert_eq!(conn.status("docs").await["auth"]["state"], "authorizing");
    assert_eq!(follow(&docs_back).await.0, 200);
    conn.event("mcp_connected", "docs").await;
    assert_eq!(conn.status("docs").await["state"], "connected");
    assert_eq!(conn.status("wiki").await["state"], "connected");
}

#[tokio::test]
async fn a_server_added_over_rpc_gets_the_same_sign_in() {
    let (mcp, _auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({})).await;
    let mut conn = d.connect().await;

    let (code, message) = conn
        .err(
            "mcp.addServer",
            serde_json::json!({"name": "docs", "config": {
                "type": "streamable_http", "url": mcp.url, "oauth_provider": "github"
            }}),
        )
        .await;
    assert_eq!(code, codes::INVALID_PARAMS as i64);
    assert!(message.contains("\"oauth\": {"), "{message}");

    let added = conn
        .ok(
            "mcp.addServer",
            serde_json::json!({"name": "docs", "config": http_server(&mcp.url)}),
        )
        .await;
    let entry = added["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "docs")
        .cloned()
        .unwrap();
    assert_eq!(entry["state"], "auth_required", "{added}");
    conn.event("mcp_auth_required", "docs").await;

    conn.sign_in("docs", serde_json::json!({})).await;
    assert_eq!(conn.status("docs").await["tool_count"], 2);

    conn.ok(
        "mcp.addServer",
        serde_json::json!({"name": "docs-renamed", "config": http_server(&mcp.url)}),
    )
    .await;
    let renamed = conn.event("mcp_connected", "docs-renamed").await;
    assert_eq!(
        renamed["tool_count"], 2,
        "a server renamed in settings keeps its sign-in"
    );
}
