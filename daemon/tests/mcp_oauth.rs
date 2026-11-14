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

use mcp_oauth_mock::{consent, follow, McpServer};

struct Daemon {
    dir: tempfile::TempDir,
    sock: PathBuf,
    pool: Arc<SessionPool>,
    model: Arc<MockAnthropicClient>,
    stop: CancellationToken,
}

impl Daemon {
    async fn start(servers: serde_json::Value) -> Self {
        Self::start_with_policy(servers, None).await
    }

    async fn start_with_policy(
        servers: serde_json::Value,
        policy: Option<serde_json::Value>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // The user's settings, not the project's: a project's own servers
        // may not read `client_secret_env` from the daemon's environment.
        std::fs::write(
            root.join("settings.json"),
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
        config.settings.values.memory_enabled = false;
        config.settings.values.permission_mode = base::settings::PermissionMode::BypassPermissions;
        let model = Arc::new(MockAnthropicClient::new());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(root.clone()),
                model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
                mcp: daemon::assemble::McpOptions {
                    policy: policy.map(|p| serde_json::from_value(p).expect("a valid policy")),
                    ..Default::default()
                },
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
        "oauth": {"client_id": "pre-1", "client_secret_env": var, "issuer": auth.url,
                  "scopes": ["mcp:read", "mcp:write"]},
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
async fn an_authorization_endpoint_that_is_not_a_web_page_is_never_handed_to_the_host() {
    let (mcp, auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    for endpoint in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "x-apple.systempreferences:com.apple.preference.security",
        "http://as.example.com/authorize",
    ] {
        auth.state.lock().unwrap().authorization_endpoint = Some(endpoint.into());
        let (code, message) = conn
            .err("mcp.auth.start", serde_json::json!({"server": "docs"}))
            .await;
        assert_eq!(code, codes::MCP_AUTH_FAILED as i64, "{endpoint}: {message}");
        assert!(message.contains("https"), "{endpoint}: {message}");
    }
    let requests = auth.state.lock().unwrap().requests.clone();
    assert!(
        !requests.iter().any(|r| r.contains("/register")),
        "nothing was registered: {requests:?}"
    );
    assert_eq!(conn.status("docs").await["auth"]["state"], "required");
}

/// A port the daemon is told to listen on later. One from the ephemeral range
/// (what binding port 0 hands out) can be taken in between as the source port
/// of another test's outgoing connection.
fn free_port_outside_the_ephemeral_range() -> u16 {
    let start = (std::process::id() as u16).wrapping_mul(7919) % 20_000;
    (0..20_000)
        .map(|i| 20_000 + (start + i) % 20_000)
        .find(|&port| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok())
        .expect("a free port between 20000 and 40000")
}

#[tokio::test]
async fn a_sign_in_on_a_fixed_callback_port_can_be_started_again() {
    let (mcp, _auth) = McpServer::start().await;
    let port = free_port_outside_the_ephemeral_range();
    let d = Daemon::start(serde_json::json!({"docs": {
        "type": "streamable_http",
        "url": mcp.url,
        "oauth": {"callback_port": port},
    }}))
    .await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    let redirect = format!("http://127.0.0.1:{port}/callback");

    let first = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    assert_eq!(first["redirect_uri"], redirect.as_str());
    let replaced = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    assert_eq!(replaced["redirect_uri"], redirect.as_str());
    let (code, _) = conn
        .err(
            "mcp.auth.cancel",
            serde_json::json!({"flow_id": first["flow_id"]}),
        )
        .await;
    assert_eq!(code, codes::INVALID_PARAMS as i64, "the first was replaced");

    conn.ok(
        "mcp.auth.cancel",
        serde_json::json!({"flow_id": replaced["flow_id"]}),
    )
    .await;
    let after_cancel = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;

    d.pool.mcp_auth().set_flow_ttl(Duration::from_millis(300));
    conn.ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    conn.event("mcp_auth_failed", "docs").await;
    d.pool.mcp_auth().set_flow_ttl(Duration::from_secs(600));
    let last = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    assert_ne!(last["flow_id"], after_cancel["flow_id"]);

    let location = consent(last["authorize_url"].as_str().unwrap()).await;
    assert!(location.starts_with(&redirect), "{location}");
    let (code, page) = follow(&location).await;
    assert_eq!(code, 200, "{page}");
    conn.event("mcp_connected", "docs").await;
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

/// An EC P-256 key generated for these tests only.
const TEST_SIGNING_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg/ZPFSfCUF/GrtLOT
H09ol9nY5GBMrfhEuil1uXnGJiahRANCAATszeKXDDXvOCL7jv1JTGw6SiWjeN9s
kHMPWaT8lk0SezP/Tc/Vwvu3RMB3rqHok9mE7RViyh/MFhFYFD7eSL6c
-----END PRIVATE KEY-----
";

fn env_var(name: &str, value: &str) -> String {
    let var = format!("AGIX_TEST_{name}_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&var, value);
    var
}

async fn use_tool(d: &Daemon, conn: &mut Conn, tool: &str) -> String {
    d.model.push_turn(tool_round(tool, "t"));
    d.model.push_turn(text_round("ok"));
    run_turn(conn, "go").await;
    serde_json::to_string(&d.model.nth_request(d.model.calls() - 1).unwrap()).unwrap()
}

#[tokio::test]
async fn client_credentials_connect_unattended_and_are_fetched_again_when_refused_or_expiring() {
    let (mcp, auth) = McpServer::start().await;
    auth.preregister("svc", Some("svc-secret"));
    let var = env_var("CC_SECRET", "svc-secret");
    let d = Daemon::start(serde_json::json!({"docs": {
        "type": "streamable_http", "url": mcp.url,
        "oauth": {"grant": "client_credentials", "client_id": "svc", "client_secret_env": var,
                  "issuer": auth.url, "scopes": ["mcp:read"]},
    }}))
    .await;
    let mut conn = d.connect().await;

    let status = conn.wait_state("docs", "connected").await;
    assert_eq!(status["auth"]["state"], "authorized");
    assert_eq!(status["tool_count"], 2);
    assert_eq!(auth.state.lock().unwrap().count("client_credentials"), 1);
    let (code, _) = conn
        .err("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    assert_eq!(code, codes::INVALID_PARAMS as i64, "nothing to sign in to");

    assert!(use_tool(&d, &mut conn, "mcp__docs__echo")
        .await
        .contains("echo: ok"));
    assert_eq!(
        auth.state.lock().unwrap().count("client_credentials"),
        1,
        "a token that lasts is reused"
    );

    auth.state.lock().unwrap().tokens.clear();
    assert!(use_tool(&d, &mut conn, "mcp__docs__echo")
        .await
        .contains("echo: ok"));
    assert_eq!(
        auth.state.lock().unwrap().count("client_credentials"),
        2,
        "a refused token is fetched again and the call retried"
    );

    // Shorter-lived than the margin rmcp keeps before an expiry: the daemon
    // fetching these tokens is what keeps them current, so each is used.
    {
        let mut s = auth.state.lock().unwrap();
        s.token_ttl = 20;
        s.tokens.clear();
    }
    assert!(use_tool(&d, &mut conn, "mcp__docs__echo")
        .await
        .contains("echo: ok"));
    let before = auth.state.lock().unwrap().count("client_credentials");
    assert!(use_tool(&d, &mut conn, "mcp__docs__echo")
        .await
        .contains("echo: ok"));
    assert!(
        auth.state.lock().unwrap().count("client_credentials") > before,
        "a token about to expire is fetched again before it is used"
    );
    assert_eq!(conn.status("docs").await["auth"]["state"], "authorized");
}

#[tokio::test]
async fn client_credentials_can_authenticate_with_a_signed_assertion() {
    let (mcp, auth) = McpServer::start().await;
    auth.preregister("svc-jwt", Some("a secret the daemon does not have"));
    let key = env_var("CC_KEY", TEST_SIGNING_KEY);
    let d = Daemon::start(serde_json::json!({"docs": {
        "type": "streamable_http", "url": mcp.url,
        "oauth": {"grant": "client_credentials", "client_id": "svc-jwt",
                  "private_key_jwt": {"key_env": key, "alg": "ES256"}},
    }}))
    .await;
    let mut conn = d.connect().await;
    let status = conn.wait_state("docs", "connected").await;
    assert_eq!(status["auth"]["state"], "authorized");
    assert_eq!(
        d.record(&mcp.url).await.tokens.unwrap().client_id,
        "svc-jwt"
    );
}

#[tokio::test]
async fn an_enterprise_identity_is_exchanged_for_the_server_token() {
    let (mcp, auth) = McpServer::start().await;
    auth.preregister("ema-client", None);
    let idp = mcp_oauth_mock::AuthServer::start(&mcp.url).await;
    idp.preregister("agix-at-idp", Some("idp-secret"));
    {
        let mut s = idp.state.lock().unwrap();
        s.identities.push("id-token-alice".into());
        s.jag_client_id = "ema-client".into();
    }
    let idp_secret = env_var("IDP_SECRET", "idp-secret");
    let policy = serde_json::json!({"enterprise_idp": {
        "issuer": idp.url, "token_endpoint": format!("{}/token", idp.url),
        "client_id": "agix-at-idp", "client_secret_env": idp_secret,
    }});
    let d = Daemon::start_with_policy(
        serde_json::json!({"docs": {
            "type": "streamable_http", "url": mcp.url,
            "oauth": {"grant": "enterprise", "client_id": "ema-client", "scopes": ["mcp:read"]},
        }}),
        Some(policy),
    )
    .await;
    let mut conn = d.connect().await;
    let waiting = conn.wait_state("docs", "auth_required").await;
    assert!(
        waiting["auth"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("mcp.auth.setIdentity"),
        "{waiting}"
    );

    let out = conn
        .ok(
            "mcp.auth.setIdentity",
            serde_json::json!({"id_token": "someone-else"}),
        )
        .await;
    assert_eq!(out["reconnected"], serde_json::json!(["docs"]));
    let refused = conn.status("docs").await;
    assert_eq!(refused["state"], "auth_required");
    assert!(
        refused["auth"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("refused the identity"),
        "{refused}"
    );

    conn.ok(
        "mcp.auth.setIdentity",
        serde_json::json!({"id_token": "id-token-alice"}),
    )
    .await;
    conn.event("mcp_connected", "docs").await;
    let status = conn.status("docs").await;
    assert_eq!(status["tool_count"], 2);
    assert_eq!(status["auth"]["state"], "authorized");
    assert_eq!(status["auth"]["scopes"], serde_json::json!(["mcp:read"]));
    assert!(
        idp.state
            .lock()
            .unwrap()
            .count(mcp_oauth_mock::TOKEN_EXCHANGE)
            >= 2
    );
    assert_eq!(
        auth.state.lock().unwrap().count(mcp_oauth_mock::JWT_BEARER),
        1
    );
    assert!(use_tool(&d, &mut conn, "mcp__docs__echo")
        .await
        .contains("echo: ok"));

    let (code, _) = conn
        .err(
            "mcp.auth.setIdentity",
            serde_json::json!({"id_token": "a", "refresh_token": "b"}),
        )
        .await;
    assert_eq!(code, codes::INVALID_PARAMS as i64);
}

#[tokio::test]
async fn an_identity_without_an_enterprise_idp_is_refused() {
    let d = Daemon::start(serde_json::json!({})).await;
    let mut conn = d.connect().await;
    let (code, message) = conn
        .err(
            "mcp.auth.setIdentity",
            serde_json::json!({"refresh_token": "rt"}),
        )
        .await;
    assert_eq!(code, codes::INVALID_PARAMS as i64);
    assert!(message.contains("enterprise IdP"), "{message}");
}

#[tokio::test]
async fn the_policy_refuses_an_authorization_server_before_the_user_is_sent_anywhere() {
    let (mcp, auth) = McpServer::start().await;
    let d = Daemon::start_with_policy(
        serde_json::json!({"docs": http_server(&mcp.url)}),
        Some(serde_json::json!({"allowed_authorization_servers": ["https://login.corp.example"]})),
    )
    .await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    let (code, message) = conn
        .err("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    assert_eq!(code, codes::MCP_SERVER_DENIED as i64);
    assert!(
        message.contains("allowed_authorization_servers"),
        "{message}"
    );
    let requests = auth.state.lock().unwrap().requests.clone();
    assert!(
        requests.iter().all(|r| r.contains("/.well-known/")),
        "only metadata was read: {requests:?}"
    );

    let (mcp, _auth) = McpServer::start().await;
    let d = Daemon::start_with_policy(
        serde_json::json!({"docs": http_server(&mcp.url)}),
        Some(serde_json::json!({"allowed_authorization_servers": ["http://127.0.0.1:*"]})),
    )
    .await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    conn.sign_in("docs", serde_json::json!({})).await;
}

async fn turn_on(conn: &mut Conn, session: &str, message: &str) {
    conn.ok(
        "session.run_turn",
        serde_json::json!({"session_id": session, "message": message}),
    )
    .await;
}

async fn new_session(conn: &mut Conn, params: serde_json::Value) -> String {
    conn.ok("session.create", params).await["session_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn last_request(d: &Daemon) -> model::types::MessagesRequest {
    d.model.nth_request(d.model.calls() - 1).unwrap()
}

fn offers_the_server(request: &model::types::MessagesRequest) -> bool {
    request
        .tools
        .iter()
        .any(|t| t.name.starts_with("mcp__docs__"))
}

#[tokio::test]
async fn an_access_token_that_expires_mid_session_is_refreshed_under_the_call() {
    let (mcp, auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    conn.sign_in("docs", serde_json::json!({})).await;
    conn.events.clear();
    let telemetry = d.dir.path().join("telemetry.jsonl");
    let session = new_session(
        &mut conn,
        serde_json::json!({"options": {"telemetry": {"output": telemetry}}}),
    )
    .await;

    let refreshed = auth.state.lock().unwrap().refreshes;
    auth.expire_access_tokens();
    d.model.push_turn(tool_round("mcp__docs__echo", "t1"));
    d.model.push_turn(text_round("done"));
    turn_on(&mut conn, &session, "use docs").await;
    let after = serde_json::to_string(&last_request(&d)).unwrap();
    assert!(after.contains("echo: ok"), "{after}");
    assert!(auth.state.lock().unwrap().refreshes > refreshed);
    assert_eq!(conn.status("docs").await["auth"]["state"], "authorized");
    assert!(
        !conn.events.iter().any(|e| e["kind"] == "mcp_auth_required"),
        "{:?}",
        conn.events
    );

    let export = d.dir.path().join("export");
    conn.ok(
        "session.export",
        serde_json::json!({"session_id": session, "path": export, "format": "dir"}),
    )
    .await;
    assert!(telemetry.exists() && export.exists());
    let store = d.credential_files();
    for token in auth.issued() {
        for file in every_file_under(d.dir.path()) {
            if store.contains(&file) {
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
async fn a_refresh_refused_under_a_call_takes_the_server_off_the_sessions_next_request() {
    let (mcp, auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    conn.sign_in("docs", serde_json::json!({})).await;
    conn.events.clear();
    let session = new_session(&mut conn, serde_json::json!({})).await;
    d.model.push_turn(text_round("ready"));
    turn_on(&mut conn, &session, "hello").await;
    assert!(offers_the_server(&last_request(&d)));

    auth.state.lock().unwrap().reject_refresh = true;
    auth.expire_access_tokens();
    d.model.push_turn(tool_round("mcp__docs__echo", "t1"));
    d.model.push_turn(text_round("it failed"));
    turn_on(&mut conn, &session, "use docs").await;
    conn.event("mcp_auth_required", "docs").await;
    conn.wait_state("docs", "auth_required").await;

    d.model.push_turn(text_round("noted"));
    turn_on(&mut conn, &session, "again").await;
    let next = last_request(&d);
    assert!(
        !offers_the_server(&next),
        "the dropped server is still offered"
    );
    let system = serde_json::to_string(&next.system).unwrap();
    assert!(system.contains("requires the user to sign in"), "{system}");
}

#[tokio::test]
async fn after_logout_a_live_sessions_next_request_offers_none_of_the_servers_tools() {
    let (mcp, _auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    conn.sign_in("docs", serde_json::json!({})).await;
    let session = new_session(&mut conn, serde_json::json!({})).await;
    d.model.push_turn(text_round("ready"));
    turn_on(&mut conn, &session, "hello").await;
    assert!(offers_the_server(&last_request(&d)));

    conn.ok("mcp.auth.logout", serde_json::json!({"server": "docs"}))
        .await;
    d.model.push_turn(text_round("noted"));
    turn_on(&mut conn, &session, "again").await;
    assert!(!offers_the_server(&last_request(&d)));
}

#[tokio::test]
async fn a_forged_loopback_callback_is_refused_and_the_sign_in_keeps_waiting() {
    let (mcp, _auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    let start = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    let location = consent(start["authorize_url"].as_str().unwrap()).await;
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

    let (code, page) = follow(forged.as_str()).await;
    assert_eq!(code, 400, "{page}");
    assert!(page.contains("did not complete"), "{page}");
    assert_eq!(conn.status("docs").await["auth"]["state"], "authorizing");
    let (code, page) = follow(&location).await;
    assert_eq!(code, 200, "{page}");
    conn.event("mcp_connected", "docs").await;
}

#[tokio::test]
async fn an_error_callback_that_is_not_this_sign_ins_is_refused_too() {
    let (mcp, _auth) = McpServer::start().await;
    let d = Daemon::start(serde_json::json!({"docs": http_server(&mcp.url)})).await;
    let mut conn = d.connect().await;
    conn.wait_state("docs", "auth_required").await;
    let start = conn
        .ok("mcp.auth.start", serde_json::json!({"server": "docs"}))
        .await;
    let location = consent(start["authorize_url"].as_str().unwrap()).await;

    for query in ["error=access_denied", "error=access_denied&state=forged"] {
        let mut forged = url::Url::parse(&location).unwrap();
        forged.set_query(Some(query));
        let (code, page) = follow(forged.as_str()).await;
        assert_eq!(code, 400, "{query}: {page}");
        assert_eq!(
            conn.status("docs").await["auth"]["state"],
            "authorizing",
            "{query}"
        );
    }
    let (code, page) = follow(&location).await;
    assert_eq!(code, 200, "{page}");
    conn.event("mcp_connected", "docs").await;
}

#[tokio::test]
async fn a_servers_credentials_and_query_never_show_in_its_status() {
    let d = Daemon::start(serde_json::json!({
        "typo": http_server("http://bob:hunter2@127.0.0.1:99999/mcp?key=QUERY-SECRET"),
    }))
    .await;
    let mut conn = d.connect().await;
    let status = conn.wait_state("typo", "failed").await;
    let shown = status.to_string();
    assert!(shown.contains("127.0.0.1:99999/mcp"), "{shown}");
    for secret in ["bob", "hunter2", "QUERY-SECRET"] {
        assert!(!shown.contains(secret), "{shown}");
    }
}
