//! Daemon RPC e2e tests.
//!
//! Tests the JSON-RPC session lifecycle via an in-process daemon.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base::id::Id;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::rpc::codes;
use daemon::DaemonServer;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
// Only the plugin-archive helper hashes anything, and that helper is gated —
// on the package layer, which is what installing needs; running a package's
// components is a separate feature and a separate set of tests below.
#[cfg(feature = "plugin-packages")]
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio_util::sync::CancellationToken;

/// Build a zip archive (in memory) around one `plugin.toml`, write it to
/// `dir`, and return `(archive_path, sha256_hex)`.
#[cfg(feature = "plugin-packages")]
fn build_plugin_zip(dir: &std::path::Path, plugin_name: &str, manifest: &str) -> (PathBuf, String) {
    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        writer.start_file("plugin.toml", opts).unwrap();
        use std::io::Write;
        write!(writer, "{manifest}").unwrap();
        writer.finish().unwrap();
    }
    let archive_path = dir.join(format!("{plugin_name}.zip"));
    std::fs::write(&archive_path, &buf).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    let checksum = hex::encode(hasher.finalize());
    (archive_path, checksum)
}

/// A minimal demo plugin declaring one `/name` slash command.
#[cfg(feature = "plugin-packages")]
fn build_demo_plugin_zip(dir: &std::path::Path, plugin_name: &str) -> (PathBuf, String) {
    build_plugin_zip(
        dir,
        plugin_name,
        &format!(
            "[plugin]\nname = \"{plugin_name}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\ndescription = \"demo plugin\"\n"
        ),
    )
}

/// Always-allow permission for tests.
struct AllowAllPermission;
#[async_trait::async_trait]
impl base::interface::permission::Permission for AllowAllPermission {
    async fn check(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &std::path::Path,
        _: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

/// Bind a test server and return (server, socket_path, _tempdir, join_handle).
async fn start_server() -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    start_server_in(tempfile::tempdir().unwrap()).await
}

/// Same server, but rooted at a directory the caller prepared — so a test
/// can install a plugin before the daemon discovers it.
async fn start_server_in(
    dir: tempfile::TempDir,
) -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let sock = dir.path().join("test.sock");

    // 使用 dummy client（不真正调 LLM）
    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());

    let config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "coding",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );

    // Through `daemon::assemble` for the same reason `main.rs` is — including
    // the startup steps a test would otherwise have to remember one at a
    // time, which is how "no session here has any plugin tool" hid once
    // before. `Nowhere` keeps this file's long-standing shape: these cases
    // are about the RPC surface, not about transcripts.
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::scene::coding::CodingScene),
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::Nowhere,
            permission: Some(Arc::new(AllowAllPermission)),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });

    for _ in 0..20 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "socket never bound");
    (server, sock, dir, handle)
}

async fn rpc_call(sock: &std::path::Path, msg: &str) -> String {
    let mut client = UnixStream::connect(sock).await.unwrap();
    client.write_all(msg.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    let mut buf = String::new();
    br.read_line(&mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn status_returns_info() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert!(v["result"]["version"].is_string());
    assert_eq!(v["result"]["sessions"], 0);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn session_list_returns_empty_initially() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"session.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"]["sessions"].is_array());
    assert!(v["result"]["sessions"].as_array().unwrap().is_empty());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn run_turn_without_session_id_creates_new() {
    let (_server, sock, _dir, handle) = start_server().await;
    // session_id 可选，不传则自动创建
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hello"},"id":1}"#,
    )
    .await;
    // 会尝试调用 LLM（但测试环境没有真实 API key），所以应该返回 engine error
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    // 只要有 result 或 error（不是 INVALID_PARAMS），说明 session 创建成功了
    assert!(v["result"].is_object() || v["error"].is_object());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn run_turn_nonexistent_session_errors() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"session_id":"NopeNopeNopeNopeNope12","message":"test"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    // 不存在的 session：新代码尝试创建/恢复，也不会报 SESSION_NOT_FOUND
    // 但最终会尝试调 LLM 而失败
    assert!(v["error"].is_object() || v["result"].is_object());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn unknown_method_returns_error() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"nonexistent","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::METHOD_NOT_FOUND);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn doctor_reports_no_providers_configured_as_ok() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["providers"]["ok"], true, "resp: {v}");
    assert_eq!(
        v["result"]["session_persistence"]["history_store_wired"],
        false
    );
    assert!(v["result"]["settings_tiers"].as_array().unwrap().len() == 3);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_writes_project_settings_and_doctor_sees_it() {
    let (_server, sock, dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","api_key":"k","default_model":"deepseek-pro"},"default_provider":"deepseek"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["default_provider"], "deepseek");

    // Written to disk at the project tier.
    let written = std::fs::read_to_string(dir.path().join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(written["providers"]["deepseek"]["api_key"], "k");

    // Doctor now reflects the newly-written provider.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["providers"]["default_provider"], "deepseek");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_can_target_another_project_this_daemon_serves() {
    let (_server, sock, dir, handle) = start_server().await;

    let other = dir.path().join("other-project");
    std::fs::create_dir_all(&other).unwrap();

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"config.setProvider","params":{{"project_root":"{}","provider_id":"deepseek","config":{{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","api_key":"k","default_model":"deepseek-pro"}},"default_provider":"deepseek"}},"id":1}}"#,
            other.display()
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert_eq!(
        v["result"]["written_to"],
        other
            .join(".atta")
            .join("settings.json")
            .display()
            .to_string(),
        "{v}"
    );

    let written = std::fs::read_to_string(other.join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(written["providers"]["deepseek"]["api_key"], "k");
    assert!(
        !dir.path().join(".atta").join("settings.json").exists(),
        "the daemon's own project must not be written when another one was named"
    );

    // `null` is refused rather than silently writing the global tier: these
    // methods act on the project tier, and a no-project session has none.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"project_root":null,"provider_id":"x","config":{"api_type":"anthropic"}},"id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], -32602, "{v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_partial_patch_does_not_clobber_untouched_fields() {
    let (_server, sock, dir, handle) = start_server().await;

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","default_model":"deepseek-pro"}},"id":1}"#,
    )
    .await;

    // Second call only touches api_key — base_url/default_model must survive.
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_key":"new-key"}},"id":2}"#,
    )
    .await;

    let written = std::fs::read_to_string(dir.path().join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(written["providers"]["deepseek"]["api_key"], "new-key");
    assert_eq!(
        written["providers"]["deepseek"]["base_url"],
        "https://api.deepseek.com/v1"
    );
    assert_eq!(
        written["providers"]["deepseek"]["default_model"],
        "deepseek-pro"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_delete_removes_entry() {
    let (_server, sock, dir, handle) = start_server().await;

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"default_model":"deepseek-pro"}},"id":1}"#,
    )
    .await;
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","delete":true},"id":2}"#,
    )
    .await;

    let written = std::fs::read_to_string(dir.path().join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert!(written["providers"].get("deepseek").is_none());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_rejects_missing_provider_id() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_refuses_to_clobber_a_malformed_existing_settings_json() {
    let (_server, sock, dir, handle) = start_server().await;

    let atta_dir = dir.path().join(".atta");
    std::fs::create_dir_all(&atta_dir).unwrap();
    let settings_path = atta_dir.join("settings.json");
    std::fs::write(&settings_path, "{not valid json").unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"default_model":"deepseek-pro"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    // The original (malformed) content must survive untouched — not get
    // silently replaced with `{}` + the patch.
    let still_there = std::fs::read_to_string(&settings_path).unwrap();
    assert_eq!(still_there, "{not valid json");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn turn_id_is_base58_uuid_22_chars() {
    let id = Id::new().to_string();
    assert!(
        (21..=22).contains(&id.len()),
        "expected 21-22 chars, got {}: {id}",
        id.len()
    );
}

#[tokio::test]
async fn mcp_status_empty_when_nothing_connected() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["servers"].as_array().unwrap().is_empty(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn mcp_add_server_writes_settings_and_reports_connect_failure() {
    let (_server, sock, dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"bogus","config":{"type":"stdio","command":"definitely-not-a-real-binary-xyz"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "resp: {v}");

    let written = std::fs::read_to_string(dir.path().join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["mcp_servers"]["bogus"]["command"],
        "definitely-not-a-real-binary-xyz"
    );

    // The binary doesn't exist, so the connect attempt fails — status must not list it.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["servers"].as_array().unwrap().is_empty(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Where `cargo` put the toy MCP server, having built it.
///
/// Asked rather than reconstructed: a path built from `CARGO_MANIFEST_DIR`
/// guesses the profile and misses a `CARGO_TARGET_DIR` override, and this
/// test is meant to survive both.
fn toy_mcp_server() -> PathBuf {
    let out = std::process::Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "mcp-toy-server",
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .expect("cargo build -p mcp-toy-server runs");
    assert!(
        out.status.success(),
        "cargo build -p mcp-toy-server failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["reason"] == "compiler-artifact" && v["target"]["name"] == "mcp-toy-server")
        .and_then(|v| v["executable"].as_str().map(PathBuf::from))
        .expect("cargo reported an executable for mcp-toy-server")
}

/// The other half of `mcp.addServer`: a server that actually starts.
///
/// Every other MCP case here points the daemon at a binary that does not
/// exist. That covers the failure path twice over and leaves the one a user
/// takes — a server connects, reports its tools, and turns up in
/// `mcp.status` — resting on nothing. The `mcp_connected` notification is
/// checked here for the same reason: it is the half of
/// `daemon.subscribeEvents` a failing connect cannot show.
#[tokio::test]
#[ignore = "builds and spawns the toy MCP server"]
async fn mcp_add_server_connects_a_real_server_and_lists_its_tools() {
    let binary = toy_mcp_server();
    let (_server, sock, dir, handle) = start_server().await;

    let mut sub_client = UnixStream::connect(&sock).await.unwrap();
    sub_client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":1}"#)
        .await
        .unwrap();
    sub_client.write_all(b"\n").await.unwrap();
    let (r, _w) = sub_client.split();
    let mut sub_reader = BufReader::new(r);
    let mut ack_line = String::new();
    sub_reader.read_line(&mut ack_line).await.unwrap();
    let ack: serde_json::Value = serde_json::from_str(&ack_line).unwrap();
    assert_eq!(ack["result"]["subscribed"], true, "ack: {ack}");

    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "mcp.addServer",
        "params": {
            "name": "toy",
            "config": { "type": "stdio", "command": binary.to_string_lossy() }
        }
    });
    let resp = rpc_call(&sock, &request.to_string()).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();

    let connected = |servers: &serde_json::Value| -> bool {
        servers.as_array().is_some_and(|list| {
            list.iter()
                .any(|s| s["name"] == "toy" && s["tool_count"].as_u64().unwrap_or(0) > 0)
        })
    };

    assert!(
        v["result"]["written_to"].is_string(),
        "addServer should report where it wrote: {v}"
    );
    assert!(
        connected(&v["result"]["servers"]),
        "addServer should report the server it just connected, with its tools: {v}"
    );

    let written = std::fs::read_to_string(dir.path().join(".atta").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["mcp_servers"]["toy"]["command"],
        binary.to_string_lossy().as_ref()
    );

    let mut event_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        sub_reader.read_line(&mut event_line),
    )
    .await
    .expect("timed out waiting for the mcp_connected notification")
    .unwrap();
    let event: serde_json::Value = serde_json::from_str(&event_line).unwrap();
    assert_eq!(event["method"], "daemon.event", "event: {event}");
    assert_eq!(event["params"]["kind"], "mcp_connected", "event: {event}");
    assert_eq!(event["params"]["server"], "toy", "event: {event}");

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":3}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        connected(&v["result"]["servers"]),
        "a connected server must still be there when asked separately: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn subscribe_events_receives_mcp_connect_failed_notification() {
    let (_server, sock, _dir, handle) = start_server().await;

    let mut sub_client = UnixStream::connect(&sock).await.unwrap();
    sub_client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":1}"#)
        .await
        .unwrap();
    sub_client.write_all(b"\n").await.unwrap();
    let (r, _w) = sub_client.split();
    let mut sub_reader = BufReader::new(r);

    let mut ack_line = String::new();
    sub_reader.read_line(&mut ack_line).await.unwrap();
    let ack: serde_json::Value = serde_json::from_str(&ack_line).unwrap();
    assert_eq!(ack["result"]["subscribed"], true, "ack: {ack}");

    // Trigger a connect attempt (guaranteed to fail — nonexistent binary)
    // from a second, independent connection.
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"bogus","config":{"type":"stdio","command":"definitely-not-a-real-binary-xyz"}},"id":2}"#,
    )
    .await;

    let mut event_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        sub_reader.read_line(&mut event_line),
    )
    .await
    .expect("timed out waiting for daemon.event notification")
    .unwrap();
    let event: serde_json::Value = serde_json::from_str(&event_line).unwrap();
    assert_eq!(event["method"], "daemon.event", "event: {event}");
    assert_eq!(event["params"]["kind"], "mcp_connect_failed");
    assert_eq!(event["params"]["server"], "bogus");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn get_provider_redacts_api_key_by_default_and_reveals_with_flag() {
    let (_server, sock, _dir, handle) = start_server().await;
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_key":"sk-secret-1234"}},"id":1}"#,
    )
    .await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.getProvider","id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["providers"]["deepseek"]["api_key"], "***1234",
        "resp: {v}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.getProvider","params":{"include_secrets":true},"id":3}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["providers"]["deepseek"]["api_key"], "sk-secret-1234",
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn session_close_removes_unknown_session_gracefully() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.close","params":{"session_id":"doesnotexist"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["closed"], "doesnotexist", "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn session_close_rejects_missing_session_id() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.close","params":{},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Unlike `session.close` (idempotent no-op on an unknown id — see above),
/// answering a prompt for a session that isn't running is a real caller
/// error: there's no pending wait to silently no-op.
#[tokio::test]
async fn respond_to_prompt_rejects_unknown_session() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"session_id":"doesnotexist","prompt_id":"p1","decision":{"type":"permit"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::SESSION_NOT_FOUND, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn respond_to_prompt_rejects_missing_params() {
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"prompt_id":"p1","decision":{"type":"permit"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"],
        codes::INVALID_PARAMS,
        "missing session_id, resp: {v}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"session_id":"x","decision":{"type":"permit"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"],
        codes::INVALID_PARAMS,
        "missing prompt_id, resp: {v}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"session_id":"x","prompt_id":"p1"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"],
        codes::INVALID_PARAMS,
        "missing decision, resp: {v}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"session_id":"x","prompt_id":"p1","prompt_type":"something_else","decision":{"type":"permit"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"],
        codes::INVALID_PARAMS,
        "unsupported prompt_type, resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_list_detects_claude_md_in_project_dir() {
    let (_server, sock, dir, handle) = start_server().await;
    std::fs::write(dir.path().join("CLAUDE.md"), "# hi").unwrap();

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"import.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let sources = v["result"]["sources"].as_array().unwrap();
    assert!(
        sources.iter().any(|s| s["source"] == "claude_code"),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_run_executes_and_writes_agents_md() {
    let (_server, sock, dir, handle) = start_server().await;
    std::fs::write(dir.path().join("CLAUDE.md"), "Be concise.").unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"import.run","params":{"source":"claude_code"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "resp: {v}");

    let agents_md = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    assert!(agents_md.contains("Be concise."));

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_run_unknown_source_errors() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"import.run","params":{"source":"bogus"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_run_source_not_currently_detected_errors() {
    let (_server, sock, _dir, handle) = start_server().await;
    // No CLAUDE.md/.cursorrules/etc written — nothing detected.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"import.run","params":{"source":"claude_code"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn commands_list_returns_builtin_local_commands() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let commands = v["result"]["commands"].as_array().expect("commands array");

    let names: Vec<&str> = commands
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for expected in ["help", "skills", "clear", "compact", "cost"] {
        assert!(
            names.contains(&expected),
            "missing builtin command {expected}: {names:?}"
        );
    }
    let help = commands.iter().find(|c| c["name"] == "help").unwrap();
    assert_eq!(help["kind"], "local");
    assert_eq!(help["source"], "builtin");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A package whose whole content is a script: no components, nothing to
/// compile, nothing that needs a WebAssembly engine.
#[cfg(feature = "plugin-packages")]
fn build_script_plugin_zip(dir: &std::path::Path, plugin_name: &str) -> (PathBuf, String) {
    let mut buf = Vec::new();
    {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        writer.start_file("plugin.toml", opts).unwrap();
        write!(
            writer,
            "[plugin]\nname = \"{plugin_name}\"\nversion = \"1.0.0\"\n\
             api_version = \"0.1\"\ndescription = \"annotates tool results\"\n\n\
             [[script]]\npoint = \"tool.result\"\nentry = \"annotate.js:onResult\"\n"
        )
        .unwrap();
        writer.start_file("annotate.js", opts).unwrap();
        write!(writer, "function onResult(r) {{ return r.text }}").unwrap();
        writer.finish().unwrap();
    }
    let archive_path = dir.join(format!("{plugin_name}.zip"));
    std::fs::write(&archive_path, &buf).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    (archive_path, hex::encode(hasher.finalize()))
}

/// The point of splitting the package layer off the carrier: a package that
/// only ships a script installs in the default build, which carries no
/// WebAssembly at all. This used to answer `PLUGINS_DISABLED` — and on a
/// `plugins` build without the compiler it installed and was then rolled
/// back, for want of a compiler the package gave it nothing to do.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn a_script_only_package_installs_and_discloses_what_it_will_run() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_script_plugin_zip(dir.path(), "annotator");

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"annotator","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["success"], true, "install resp: {v}");

    // The script is a capability, not an implementation detail: it runs in
    // this process at a point that rewrites what the model reads, and the
    // sandbox has nothing to say about that. Whoever installs it has to see
    // it before they rely on it.
    let d = &v["result"]["disclosure"];
    let caps = d["capabilities"].as_array().unwrap();
    assert!(
        caps.iter()
            .any(|c| c.as_str().unwrap().contains("`tool.result`")),
        "the script binding must be disclosed: {d}"
    );
    assert_eq!(d["wasm"]["components"], 0, "{d}");

    // `root` so a host can read what it cares about out of the package
    // without rebuilding the daemon's disk layout for itself.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    let it = plugins.iter().find(|p| p["name"] == "annotator").unwrap();
    assert_eq!(it["enabled"], true);
    let root = std::path::Path::new(it["root"].as_str().expect("list carries the root: {it}"));
    assert!(
        root.join("annotate.js").is_file(),
        "the root must be the unpacked directory: {root:?}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_install_rejects_bad_checksum() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, _correct_checksum) = build_demo_plugin_zip(dir.path(), "demo-plugin");

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"demo-plugin","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            "0".repeat(64),
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("checksum"),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Installing and being found are two different pieces of code, and only one
/// of them reads the manifest. A package that unpacks but will not load is
/// installed in no sense the caller cares about: it never appears in
/// `plugin.list`, contributes nothing, and — before this — reported success
/// on the way in, so nothing anywhere said why.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_install_refuses_a_package_it_cannot_load_back() {
    let (_server, sock, dir, handle) = start_server().await;
    // No `api_version`, which is a parse error rather than a default — the
    // same shape as the committed `demo-plugin` fixture, which spent a
    // release being installed successfully and then skipped by discovery.
    let (archive_path, checksum) = build_plugin_zip(
        dir.path(),
        "unloadable",
        "[plugin]\nname = \"unloadable\"\nversion = \"1.0.0\"\ndescription = \"no api_version\"\n",
    );

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"unloadable","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("api_version"),
        "the refusal does not say what was wrong with the package: {v}"
    );

    // And it is gone, not left unpacked where a later `plugin.list` would
    // keep skipping it in silence.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let names: Vec<&str> = v["result"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"unloadable"),
        "the refused package was left behind: {names:?}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_lifecycle_install_list_disable_enable_uninstall() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_demo_plugin_zip(dir.path(), "demo-plugin");

    // ── install ──
    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"demo-plugin","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["success"], true, "install resp: {v}");

    // ── the install reports what the plugin will contribute ──
    let d = &v["result"]["disclosure"];
    assert_eq!(d["plugin"], "demo-plugin", "install must disclose: {v}");
    assert_eq!(d["version"], "1.0.0");
    assert_eq!(
        d["capabilities"].as_array().unwrap().len(),
        0,
        "this fixture asks for nothing: {d}"
    );
    // Whether this binary will run a package's components is a fact about
    // the build, and the disclosure is where it is answerable — otherwise
    // "installed, and its tool is missing" has no explanation from here.
    assert_eq!(
        d["wasm"]["runnable"],
        cfg!(feature = "plugins"),
        "the disclosure must say whether this build runs components: {d}"
    );
    assert_eq!(d["wasm"]["components"], 0, "this fixture ships none: {d}");

    // The plugin's own description reaches the model, so it is listed with
    // its provenance rather than left for the reader to infer.
    let visible = d["model_visible"].as_array().unwrap();
    assert!(
        visible
            .iter()
            .any(|v| v["origin"] == "plugin description" && v["text"] == "demo plugin"),
        "{d}"
    );

    // ── plugin.list shows it, enabled ──
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    let demo = plugins.iter().find(|p| p["name"] == "demo-plugin").unwrap();
    assert_eq!(demo["enabled"], true);

    // ── disable ──
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.disable","id":4,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["enabled"], false, "resp: {v}");

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":6}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    let demo = plugins.iter().find(|p| p["name"] == "demo-plugin").unwrap();
    assert_eq!(
        demo["enabled"], false,
        "plugin.list still shows enabled: {demo:?}"
    );

    // ── re-enable ──
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.enable","id":7,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["enabled"], true, "resp: {v}");

    // ── uninstall ──
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.uninstall","id":9,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["success"], true, "resp: {v}");

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":10}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    assert!(
        !plugins.iter().any(|p| p["name"] == "demo-plugin"),
        "plugins: {plugins:?}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

// ── resume_or_create panic-avoidance (malformed session_id + real history_store) ──
//
// `SessionPool::resume_or_create` used to `unwrap()` a `SessionId::parse()`
// call and `panic!()` on a second failed retry — both only reachable when a
// real `HistoryStore` is configured (the `start_server()` helper above uses
// `history_store: None`, which never exercises that code path). Production
// `daemon/src/main.rs` does wire up a real store, so this needs its own
// server variant.

async fn start_server_with_history(
    cap: usize,
) -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock");

    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());

    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            dir.path(),
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .unwrap(),
    );

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        None,
        "coding",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = cap;

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::scene::coding::CodingScene),
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::In(store),
            permission: Some(Arc::new(AllowAllPermission)),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });

    for _ in 0..20 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "socket never bound");
    (server, sock, dir, handle)
}

#[tokio::test]
async fn run_turn_with_malformed_session_id_and_history_store_does_not_panic() {
    let (_server, sock, _dir, handle) = start_server_with_history(8).await;

    // Not a valid BASE58(UUID) — used to `unwrap()`-panic inside
    // `resume_or_create` when a real `HistoryStore` is configured. A
    // panicking `tokio::spawn`'d connection task just drops the socket
    // silently, so the regression signature here is "no response at all
    // (empty read)", not a catchable Rust panic in the test process itself.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"session_id":"not-a-valid-id!!","message":"hi"},"id":1}"#,
    )
    .await;
    assert!(
        !resp.is_empty(),
        "connection died with no response — resume_or_create likely panicked on an unparsable session_id"
    );
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

// ── TCP daemon.auth handshake ───────────────────────────────────────────

/// Mirrors `start_server()` but over TCP via `serve_tcp_listener` (bound
/// separately so the test knows the OS-assigned ephemeral port before the
/// accept loop starts — `serve_tcp` itself only takes a fixed `SocketAddr`).
async fn start_tcp_server(
    token: &str,
) -> (
    Arc<DaemonServer>,
    std::net::SocketAddr,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();

    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        None,
        "coding",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = 8;

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::scene::coding::CodingScene),
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::Nowhere,
            permission: Some(Arc::new(AllowAllPermission)),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    server.set_tcp_token(token.to_string()).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server2 = server.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_tcp_listener(listener).await;
    });

    (server, addr, dir, handle)
}

/// Connect, send a `daemon.auth` handshake with `token`, and return the open
/// stream plus the parsed handshake response (so callers can keep using the
/// same connection afterward — unlike `rpc_call`, which opens/closes fresh).
async fn tcp_handshake(addr: std::net::SocketAddr, token: &str) -> (TcpStream, serde_json::Value) {
    let mut client = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        r#"{{"jsonrpc":"2.0","method":"daemon.auth","params":{{"token":"{token}"}},"id":0}}"#
    );
    client.write_all(req.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let mut buf = String::new();
    {
        let (r, _) = client.split();
        let mut br = BufReader::new(r);
        br.read_line(&mut buf).await.unwrap();
    }
    (client, serde_json::from_str(&buf).unwrap())
}

#[tokio::test]
async fn tcp_rejects_request_before_handshake_and_closes_connection() {
    let (_server, addr, _dir, handle) = start_tcp_server("s3cr3t").await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    // Skip the handshake — send a normal method call first.
    client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#)
        .await
        .unwrap();
    client.write_all(b"\n").await.unwrap();

    let mut buf = String::new();
    {
        let (r, _) = client.split();
        let mut br = BufReader::new(r);
        br.read_line(&mut buf).await.unwrap();
    }
    let v: serde_json::Value = serde_json::from_str(&buf).unwrap();
    assert_eq!(v["error"]["code"], codes::UNAUTHORIZED, "resp: {v}");

    // The server closes the connection after rejecting the handshake — the
    // next read should see EOF (0 bytes), not a second response, proving
    // `daemon.status` itself was never dispatched.
    let mut trailing = String::new();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    let n = br.read_line(&mut trailing).await.unwrap();
    assert_eq!(n, 0, "connection should be closed after auth rejection");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn tcp_rejects_wrong_token() {
    let (_server, addr, _dir, handle) = start_tcp_server("s3cr3t").await;
    let (_client, v) = tcp_handshake(addr, "wrong-token").await;
    assert_eq!(v["error"]["code"], codes::UNAUTHORIZED, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn tcp_accepts_correct_token_then_dispatches_normally() {
    let (_server, addr, _dir, handle) = start_tcp_server("s3cr3t").await;
    let (mut client, v) = tcp_handshake(addr, "s3cr3t").await;
    assert_eq!(v["result"]["authenticated"], true, "resp: {v}");

    // Same connection, now dispatches like any other request.
    client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.status","id":2}"#)
        .await
        .unwrap();
    client.write_all(b"\n").await.unwrap();
    let mut buf = String::new();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    br.read_line(&mut buf).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&buf).unwrap();
    assert!(v["result"]["version"].is_string(), "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn serve_tcp_listener_bails_without_a_token_even_when_called_directly() {
    // `serve_tcp_listener` is `pub` specifically so tests (and any other
    // caller who already has a bound `TcpListener`, e.g. socket activation)
    // can drive it without going through `serve_tcp`'s addr-binding — but
    // that means it can't just trust `serve_tcp`'s "token is set" check to
    // have already run. This calls it directly, skipping `set_tcp_token`
    // entirely, and asserts it fails loudly instead of silently rejecting
    // every future connection forever.
    let dir = tempfile::tempdir().unwrap();
    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());
    let config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        None,
        "coding",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::scene::coding::CodingScene),
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::Nowhere,
            permission: Some(Arc::new(AllowAllPermission)),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    // Deliberately NOT calling server.set_tcp_token(...).

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = server.serve_tcp_listener(listener).await.unwrap_err();
    assert!(err.to_string().contains("token"), "err: {err}");
}

// ── config.reload + hot-reloadable task routing ─────────────────────────

#[tokio::test]
async fn config_reload_with_no_providers_reports_ok_and_no_router() {
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], false, "resp: {v}");
    assert!(v["result"]["providers"].as_array().unwrap().is_empty());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn config_reload_picks_up_a_hand_edited_settings_file() {
    // The whole point of `config.reload` (vs `config.setProvider`) is
    // supporting a human editing settings.json directly, outside any RPC —
    // this writes the project-tier file by hand (no `config.setProvider`
    // call at all) and confirms `config.reload` alone picks it up.
    let (_server, sock, dir, handle) = start_server().await;

    let settings_path = dir.path().join(".atta").join("settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "providers": {
                "anthropic": {
                    "api_type": "anthropic",
                    "api_key": "sk-ant-hand-edited",
                    "default_model": "claude-sonnet-4-6"
                }
            },
            "default_provider": "anthropic"
        })
        .to_string(),
    )
    .unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert_eq!(v["result"]["default_provider"], "anthropic", "resp: {v}");
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], true, "resp: {v}");
    assert_eq!(
        v["result"]["routing"]["router_error"],
        serde_json::Value::Null,
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn config_reload_attempts_to_connect_a_hand_edited_mcp_server() {
    // Regression: `config.reload` used to re-read `mcp_servers` into the new
    // `Settings` (so `doctor`/`get_providers`-style inspection would show it)
    // but never actually connect it — `apply_reloaded_settings` never
    // touched `self.mcp` at all. A hand-edited `mcp_servers` entry was
    // invisible to every session (including ones later rebuilt via
    // a stale binding, since `create()` sources MCP from the untouched
    // central cache, not from `Settings` directly) — the only way to
    // actually connect it was `mcp.addServer` or a full daemon restart.
    //
    // Uses the same "definitely not a real binary" pattern as
    // `mcp_add_server_writes_settings_and_reports_connect_failure` — the
    // point isn't a successful connection (that needs a real MCP server
    // process), it's proving `config.reload` now genuinely *attempts* one
    // and reports the outcome. Before this fix, `mcp_failed` didn't exist as
    // a field at all: the server would never appear in the response either
    // way, connect attempted or not, so its mere presence here is the proof.
    let (_server, sock, dir, handle) = start_server().await;

    let settings_path = dir.path().join(".atta").join("settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "mcp_servers": {
                "hand-edited": {
                    "type": "stdio",
                    "command": "definitely-not-a-real-binary-xyz"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    let failed = v["result"]["routing"]["mcp_failed"]
        .as_array()
        .expect("mcp_failed should be an array");
    assert!(
        failed.iter().any(|s| s == "hand-edited"),
        "expected 'hand-edited' in mcp_failed, got: {v}"
    );
    assert!(v["result"]["routing"]["mcp_connected"]
        .as_array()
        .unwrap()
        .is_empty());

    // And `mcp.status` — which reads the same `self.mcp` this reconcile call
    // writes to — agrees: still nothing connected (the binary doesn't
    // exist), but the attempt happened, which is what actually matters here.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"]["servers"].as_array().unwrap().is_empty());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_with_openai_compatible_rebuilds_the_router() {
    // N-16: `openai_compatible` now has a runtime `Model` impl
    // (`model::OpenAICompatibleModel`), so configuring a DeepSeek-shaped
    // provider builds and swaps the live router instead of resolving and then
    // failing. This used to be the "valid config, no implementation" case.
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","api_key":"k","default_model":"deepseek-pro"},"default_provider":"deepseek"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], true, "resp: {v}");
    assert!(
        v["result"]["routing"]["router_error"].is_null(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_with_openai_compatible_missing_base_url_reports_a_router_error() {
    // The one thing an `openai_compatible` provider cannot do without: there
    // is no default host to fall back to, and guessing one would surface as a
    // confusing auth failure on the first call instead of a config error now.
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","api_key":"k","default_model":"deepseek-pro"},"default_provider":"deepseek"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], false, "resp: {v}");
    let router_error = v["result"]["routing"]["router_error"].as_str().unwrap();
    assert!(
        router_error.contains("base_url"),
        "router_error: {router_error}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_with_unresolvable_config_reports_routing_not_ok() {
    // Distinguishes the other failure layer: `default_provider` pointing at
    // a provider that doesn't exist is a `resolve_task_models` failure, not
    // a router-build failure — `routing.ok` must be `false` here (unlike
    // the openai_compatible case above).
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"anthropic","config":{"api_type":"anthropic","api_key":"k","default_model":"claude-sonnet-4-6"},"default_provider":"ghost-provider"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["routing"]["ok"], false, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], false, "resp: {v}");
    let error = v["result"]["routing"]["error"].as_str().unwrap();
    assert!(error.contains("ghost-provider"), "error: {error}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn existing_session_survives_a_config_reload_between_turns() {
    // Smoke test for the lazy-recreate path in `run_turn`: a session created
    // before a reload must still be usable (same session_id, no
    // SESSION_NOT_FOUND / crash) for a turn sent after the reload bumped
    // its binding going stale — proving the stale-binding branch in
    // `run_turn` (recreate via `resume_or_create`) doesn't break the normal
    // "session already exists" path. Uses `start_server_with_history()`
    // because the recreate path only engages for HistoryStore-backed
    // sessions (see `run_turn`'s doc comment).
    let (_server, sock, _dir, handle) = start_server_with_history(8).await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hello"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );
    let session_id = {
        let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"session.list","id":2}"#).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        let sessions = v["result"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "resp: {v}");
        sessions[0]["session_id"].as_str().unwrap().to_string()
    };

    // Invalidate every binding without touching this session directly.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":3}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "reload should succeed: {v}");

    // Same session_id, second turn — must not come back SESSION_NOT_FOUND.
    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{session_id}","message":"still there?"}},"id":4}}"#
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_ne!(v["error"]["code"], codes::SESSION_NOT_FOUND, "resp: {v}");
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn recreating_a_stale_session_at_capacity_does_not_evict_an_unrelated_session() {
    // Regression test: `create()`'s capacity check used to run unconditionally
    // before `insert`, not accounting for the config-generation recreate path
    // replacing an *existing* map entry under the same sid rather than adding
    // a new one. At capacity, that spuriously evicted an unrelated session
    // even though the total session count wasn't actually growing.
    let (_server, sock, _dir, handle) = start_server_with_history(2).await;

    async fn session_ids(sock: &std::path::Path) -> std::collections::HashSet<String> {
        let resp = rpc_call(sock, r#"{"jsonrpc":"2.0","method":"session.list","id":99}"#).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        v["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_id"].as_str().unwrap().to_string())
            .collect()
    }

    // Create the two sessions one at a time, diffing `session.list` after
    // each — `self.sessions` is a `HashMap`, so its iteration order carries
    // no creation-order information; this is the only reliable way to know
    // which of the two is objectively older (`id_older`, created first,
    // never touched again) vs the one we're about to recreate (`id_newer`).
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hi 0"},"id":0}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );
    let after_first = session_ids(&sock).await;
    assert_eq!(after_first.len(), 1, "ids: {after_first:?}");
    let id_older = after_first.into_iter().next().unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hi 1"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );
    let after_second = session_ids(&sock).await;
    assert_eq!(after_second.len(), 2, "ids: {after_second:?}");
    let id_newer = after_second
        .into_iter()
        .find(|id| id != &id_older)
        .expect("second session must have a different id than the first");

    // Invalidate every binding, then send a second turn to the *newer* session
    // — this triggers the stale-binding recreate path for it while the
    // pool is already at capacity (2/2). If eviction incorrectly treats this
    // replace-in-place as "adding a new session", it'll evict whatever's
    // globally least-recently-active — `id_older`, which this call never
    // touches.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":11}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "reload should succeed: {v}");

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{id_newer}","message":"again"}},"id":12}}"#
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );

    let final_ids = session_ids(&sock).await;
    assert_eq!(final_ids.len(), 2, "ids: {final_ids:?}");
    assert!(
        final_ids.contains(&id_older),
        "unrelated older session was evicted as a side effect of recreating the newer one: {final_ids:?}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A build without the package layer must say so, not pretend. An empty
/// `plugin.list` would read as "nothing installed", which is a different fact
/// from "this binary cannot load plugins at all".
///
/// Gated on the *package* layer, not on the carrier: a build that reads
/// packages but runs no components manages them perfectly well, and answering
/// `PLUGINS_DISABLED` there was the whole defect — a package with nothing to
/// run was refused for want of an engine it never needed.
#[cfg(not(feature = "plugin-packages"))]
#[tokio::test]
async fn plugin_rpcs_report_plugins_disabled_when_compiled_out() {
    let (_server, sock, _dir, handle) = start_server().await;

    for (method, params) in [
        ("plugin.list", "{}"),
        (
            "plugin.install",
            r#"{"name":"x","version":"1.0.0","download_url":"file:///dev/null"}"#,
        ),
        ("plugin.uninstall", r#"{"name":"x"}"#),
        ("plugin.enable", r#"{"name":"x"}"#),
        ("plugin.disable", r#"{"name":"x"}"#),
        ("plugin.reload", "{}"),
    ] {
        let resp = rpc_call(
            &sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"{method}","id":1,"params":{params}}}"#),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            v["error"]["code"],
            codes::PLUGINS_DISABLED,
            "{method} should report the subsystem as unavailable: {v}"
        );
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("compiled-out"),
            "{method} should say why: {v}"
        );
    }

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Install a plugin that owns a scene directly into the global tier's
/// versioned cache — the layout `plugin::discovery` reads.
#[cfg(feature = "plugins")]
fn install_scene_plugin(root: &std::path::Path, name: &str) {
    let dir = root.join("plugins").join("cache").join(name).join("1.0.0");
    std::fs::create_dir_all(dir.join("scene")).unwrap();
    std::fs::write(
        dir.join("scene/prompt.md"),
        "You are the demo workflow agent.",
    )
    .unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            r#"
[plugin]
name = "{name}"
version = "1.0.0"
api_version = "0.1"
description = "owns a scene"

[scene.own]
name = "Demo workflow"
description = "A plugin-owned scene"
prompt = "scene/prompt.md"
tools = ["Read", "Grep"]
disallowed_tools = ["Bash"]
"#
        ),
    )
    .unwrap();
}

/// A plugin that owns a scene needs no `scene.activate`: installing and
/// enabling it is the consent, and entering the scene is still an explicit
/// `session.create`.
#[cfg(feature = "plugins")]
#[tokio::test]
async fn a_plugin_owned_scene_is_listed_and_can_host_a_session() {
    let dir = tempfile::tempdir().unwrap();
    install_scene_plugin(dir.path(), "demo-scene-plugin");

    let (server, sock, _dir, handle) = start_server_in(dir).await;

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let scenes = v["result"]["scenes"].as_array().expect("scenes array");
    let demo = scenes
        .iter()
        .find(|s| s["scene"] == "plugin:demo-scene-plugin")
        .unwrap_or_else(|| panic!("plugin scene missing from {scenes:?}"));
    assert_eq!(demo["name"], "Demo workflow");
    assert_eq!(
        demo["active"], true,
        "an installed, enabled plugin's scene is servable without a second step"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":2,"params":{"scene":"plugin:demo-scene-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["session_id"].is_string(),
        "a session should be creatable in a plugin scene: {v}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A scene id no plugin owns must still be refused, so a typo does not
/// silently land the user in the default scene.
#[cfg(feature = "plugins")]
#[tokio::test]
async fn an_unknown_plugin_scene_is_refused() {
    let (server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{"scene":"plugin:not-installed"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["error"].is_object(), "expected a refusal, got {v}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Installing must state what a plugin will contribute — above all the text
/// that will reach the model, which no sandbox can vet.
#[cfg(feature = "plugins")]
#[tokio::test]
async fn a_scene_owning_plugin_discloses_its_prompt_and_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    install_scene_plugin(dir.path(), "disclosing-plugin");

    let (server, sock, _dir, handle) = start_server_in(dir).await;

    // Already installed on disk, so ask through the subsystem's own view by
    // reinstalling over it — the response carries the disclosure either way.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "disclosing-plugin"),
        "{v}"
    );

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"scene.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let listed = v["result"]["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["scene"] == "plugin:disclosing-plugin");
    assert!(listed, "the scene the plugin owns should be servable: {v}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A plugin that appeared on disk without going through `plugin.install` —
/// dropped in by hand, or rebuilt in place during development — is picked up
/// by `plugin.reload`.
///
/// Asserted through the plugin's *scene*, not through `plugin.list`:
/// `plugin.list` rescans the directories on every call, so it would report a
/// hand-placed plugin with or without a reload and prove nothing. What needs
/// the reload is everything derived from the installed set — scenes, agent
/// types, commands, and the loaded components themselves.
#[cfg(feature = "plugins")]
#[tokio::test]
async fn plugin_reload_picks_up_a_plugin_that_appeared_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (_server, sock, _dir, handle) = start_server_in(dir).await;

    let scene_named = |v: &serde_json::Value| -> bool {
        v["result"]["scenes"].as_array().is_some_and(|scenes| {
            scenes
                .iter()
                .any(|s| s["scene"] == "plugin:hand-placed-plugin")
        })
    };

    // Straight onto disk, behind the daemon's back.
    install_scene_plugin(&root, "hand-placed-plugin");

    let before = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#).await;
    let before: serde_json::Value = serde_json::from_str(&before).unwrap();
    assert!(
        !scene_named(&before),
        "the daemon adopted a plugin nobody told it about — this test cannot show the reload \
         does anything: {before}"
    );

    let reloaded = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.reload","id":2}"#,
    )
    .await;
    let reloaded: serde_json::Value = serde_json::from_str(&reloaded).unwrap();
    assert!(
        reloaded["result"]["plugins"]
            .as_array()
            .is_some_and(|p| p.iter().any(|x| x["name"] == "hand-placed-plugin")),
        "reload should report the installed set it found: {reloaded}"
    );

    let after = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"scene.list","id":3}"#).await;
    let after: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert!(
        scene_named(&after),
        "the rescan did not take effect — the plugin's scene is still missing: {after}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}
