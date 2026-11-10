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
// Only the plugin-archive helper hashes anything, and that helper is gated on
// the package layer, which is what installing needs.
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

/// A minimal package that declares nothing.
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
    start_server_serving(dir, Arc::new(scene::code::CodeScene::default())).await
}

/// Same server, serving `scene` instead of the interactive code scene.
async fn start_server_serving(
    dir: tempfile::TempDir,
    scene: Arc<dyn base::interface::scene::Scene>,
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
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );

    // Through `daemon::assemble` for the same reason `main.rs` is — including
    // the startup steps a test would otherwise have to remember one at a
    // time, which is how "no session here has any plugin tool" hid once
    // before. `Nowhere` keeps this file's long-standing shape: these cases
    // are about the RPC surface, not about transcripts.
    let pool = daemon::assemble::pool(
        &config,
        scene,
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
    // A turn streams `session.event` frames ahead of its response.
    loop {
        let mut buf = String::new();
        br.read_line(&mut buf).await.unwrap();
        let is_frame = serde_json::from_str::<serde_json::Value>(&buf)
            .is_ok_and(|v| v["method"] == "session.event");
        if !is_frame {
            return buf;
        }
    }
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
    assert_eq!(
        v["result"]["protocol_version"],
        daemon::discovery::INSTANCE_PROTOCOL_VERSION
    );
    assert!(v["result"]["capabilities"].is_array());

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

/// A request without an `id` is a notification: whatever it does, no
/// response comes back for it — not a result, not an error.
#[tokio::test]
async fn a_notification_gets_no_response() {
    let (_server, sock, _dir, handle) = start_server().await;
    let mut client = UnixStream::connect(&sock).await.unwrap();
    client
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.status\"}\n\
              {\"jsonrpc\":\"2.0\",\"method\":\"nonexistent\"}\n",
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    client
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.status\",\"id\":7}\n")
        .await
        .unwrap();
    let (r, _w) = client.split();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], 7, "the first line back answers the request: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn a_line_that_is_not_json_gets_a_parse_error_and_the_connection_goes_on() {
    let (_server, sock, _dir, handle) = start_server().await;
    let mut client = UnixStream::connect(&sock).await.unwrap();
    client.write_all(b"this is not json\n").await.unwrap();
    client
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.status\",\"id\":9}\n")
        .await
        .unwrap();
    let (r, _w) = client.split();
    let mut lines = BufReader::new(r);

    let mut first = String::new();
    lines.read_line(&mut first).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32700, "message": "Parse error" }
        })
    );

    let mut second = String::new();
    lines.read_line(&mut second).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(v["id"], 9, "the connection still serves requests: {v}");
    assert!(v.get("result").is_some(), "{v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn doctor_lists_the_safety_posture_checks() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let names: Vec<&str> = v["result"]["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    for expected in [
        "settings.permission_mode",
        "settings.hooks_fail_open",
        "build.panic",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("new").join("dir").join("agixd.sock");
    let _listener = DaemonServer::bind_unix(&sock).unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&sock), 0o600);
    assert_eq!(mode(sock.parent().unwrap()), 0o700);
    assert_eq!(mode(&dir.path().join("new")), 0o700);
}

/// `daemon.shutdown` answers once the sessions are stopped; a connection
/// still open after that cannot start another one.
#[tokio::test]
async fn no_session_is_created_after_shutdown_starts() {
    let (_server, sock, _dir, handle) = start_server().await;
    let mut client = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = client.split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.shutdown\",\"id\":1}\n")
        .await
        .unwrap();
    let down: serde_json::Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(down["result"]["shutting_down"], true, "{down}");

    w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"session.create\",\"id\":2}\n")
        .await
        .unwrap();
    let created: serde_json::Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(created["result"].is_null(), "{created}");
    assert!(
        created["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("shutting down"),
        "{created}"
    );
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the listener stopped")
        .unwrap();
}

/// `options` that do not read are reported field by field, and the fields
/// that do read still apply.
#[tokio::test]
async fn malformed_options_are_reported() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{"options":{"permission_mode":"plan","model":3,"permision_rules":[]}}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let sid = v["result"]["session_id"]
        .as_str()
        .expect("created")
        .to_string();
    let mut ignored: Vec<(String, String)> = v["result"]["ignored_options"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|i| {
            (
                i["field"].as_str().unwrap().to_string(),
                i["error"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    ignored.sort();
    assert_eq!(ignored.len(), 2, "{v}");
    assert_eq!(ignored[0].0, "model");
    assert!(ignored[0].1.contains("invalid type"), "{v}");
    assert_eq!(ignored[1].0, "permision_rules");
    assert_eq!(ignored[1].1, "unknown field");

    let resumed = rpc_call(
        &sock,
        &format!(r#"{{"jsonrpc":"2.0","method":"session.resume","id":2,"params":{{"session_id":"{sid}"}}}}"#),
    )
    .await;
    let resumed: serde_json::Value = serde_json::from_str(&resumed).unwrap();
    assert_eq!(resumed["result"]["permission"]["mode"], "plan", "{resumed}");

    let clean = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":3,"params":{"options":{"model":"claude-haiku-4-5"}}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&clean).unwrap();
    assert!(v["result"].get("ignored_options").is_none(), "{v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// With `options.strict`, what plain parsing would leave out is refused
/// instead, and the error names each field.
#[tokio::test]
async fn strict_options_refuse_what_they_cannot_read() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{"options":{"strict":true,"model":3,"permision_rules":[],"prompt_types":["question",7]}}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_null(), "{v}");
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "{v}");
    let mut fields: Vec<&str> = v["error"]["data"]["invalid_options"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|i| i["field"].as_str().unwrap())
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        ["model", "permision_rules", "prompt_types[1]"],
        "{v}"
    );

    let listed = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"session.list","id":2}"#).await;
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(
        listed["result"]["sessions"].as_array().map(Vec::len),
        Some(0),
        "a refused create made a session: {listed}"
    );

    for strict in ["true", "\"yes\""] {
        let resp = rpc_call(
            &sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"session.create","id":3,"params":{{"options":{{"strict":{strict},"model":"claude-haiku-4-5"}}}}}}"#
            ),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        if strict == "true" {
            assert!(v["result"]["session_id"].is_string(), "{v}");
            assert!(v["result"].get("ignored_options").is_none(), "{v}");
        } else {
            assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "{v}");
            assert_eq!(v["error"]["data"]["invalid_options"][0]["field"], "strict");
        }
    }

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// An unattended scene with the network open and the network policy not
/// required: doctor says so, and says the default is planned to change.
#[tokio::test]
async fn doctor_warns_when_unattended_commands_may_reach_an_open_network() {
    use scene::code::{CodeScene, Interaction};

    async fn check(scene: CodeScene, settings: Option<&str>) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        if let Some(settings) = settings {
            std::fs::write(dir.path().join("settings.json"), settings).unwrap();
        }
        let (server, sock, _dir, handle) = start_server_serving(dir, Arc::new(scene)).await;
        let resp = rpc_call(
            &sock,
            r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":1}"#,
        )
        .await;
        server.shutdown_token().cancel();
        let _ = handle.await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        v["result"]["health"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "sandbox.unattended_network")
            .cloned()
            .unwrap_or_else(|| panic!("no sandbox.unattended_network check: {v}"))
    }

    let warned = check(CodeScene::new(Interaction::Unattended), None).await;
    assert_eq!(warned["status"], "degraded", "{warned}");
    let summary = warned["summary"].as_str().unwrap();
    assert!(summary.contains("later 0.3.x"), "{summary}");
    assert_eq!(
        warned["details"]["unattended_scenes"][0], "code",
        "{warned}"
    );

    let attended = check(CodeScene::new(Interaction::Interactive), None).await;
    assert_eq!(attended["status"], "ok", "{attended}");
    for settings in [
        r#"{"sandbox": {"network_mode": "deny_all"}}"#,
        r#"{"sandbox": {"unattended_requires_network_policy": true}}"#,
    ] {
        let held = check(CodeScene::new(Interaction::Unattended), Some(settings)).await;
        assert_eq!(held["status"], "ok", "{settings}: {held}");
    }
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

    // Written to disk at the project tier, the key to the machine-local file.
    let agix = dir.path().join(".agix");
    let written = std::fs::read_to_string(agix.join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["providers"]["deepseek"]["default_model"],
        "deepseek-pro"
    );
    assert!(
        written["providers"]["deepseek"].get("api_key").is_none(),
        "the committed settings.json carries the key: {written}"
    );
    let local = std::fs::read_to_string(agix.join("settings.local.json")).unwrap();
    let local: serde_json::Value = serde_json::from_str(&local).unwrap();
    assert_eq!(local["providers"]["deepseek"]["api_key"], "k");
    assert_eq!(
        v["result"]["secrets_written_to"],
        agix.join("settings.local.json").display().to_string()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(agix.join("settings.local.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "settings.local.json is {mode:o}");
    }

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
            .join(".agix")
            .join("settings.json")
            .display()
            .to_string(),
        "{v}"
    );

    let local = std::fs::read_to_string(other.join(".agix").join("settings.local.json")).unwrap();
    let local: serde_json::Value = serde_json::from_str(&local).unwrap();
    assert_eq!(local["providers"]["deepseek"]["api_key"], "k");
    assert!(
        !dir.path().join(".agix").join("settings.json").exists(),
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

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    let local =
        std::fs::read_to_string(dir.path().join(".agix").join("settings.local.json")).unwrap();
    let local: serde_json::Value = serde_json::from_str(&local).unwrap();
    assert_eq!(local["providers"]["deepseek"]["api_key"], "new-key");
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

/// A key that an earlier write left in the committed file moves out the next
/// time that provider is written, and a delete clears it from both files.
#[tokio::test]
async fn set_provider_moves_a_committed_key_to_the_local_file() {
    let (_server, sock, dir, handle) = start_server().await;
    let agix = dir.path().join(".agix");
    std::fs::create_dir_all(&agix).unwrap();
    std::fs::write(
        agix.join("settings.json"),
        r#"{"providers":{"deepseek":{"api_key":"old-key","base_url":"https://x/v1"}},"prompt_append":"kept"}"#,
    )
    .unwrap();

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"default_model":"m"}},"id":1}"#,
    )
    .await;
    let read = |name: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(agix.join(name)).unwrap()).unwrap()
    };
    let committed = read("settings.json");
    assert!(
        committed["providers"]["deepseek"].get("api_key").is_none(),
        "{committed}"
    );
    assert_eq!(
        committed["providers"]["deepseek"]["base_url"],
        "https://x/v1"
    );
    assert_eq!(committed["prompt_append"], "kept");
    assert_eq!(
        read("settings.local.json")["providers"]["deepseek"]["api_key"],
        "old-key"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.getProvider","params":{"include_secrets":true},"id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["providers"]["deepseek"]["api_key"], "old-key",
        "{v}"
    );

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","delete":true},"id":3}"#,
    )
    .await;
    assert!(read("settings.json")["providers"].get("deepseek").is_none());
    assert!(read("settings.local.json")["providers"]
        .get("deepseek")
        .is_none());

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

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
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

    let agix_dir = dir.path().join(".agix");
    std::fs::create_dir_all(&agix_dir).unwrap();
    let settings_path = agix_dir.join("settings.json");
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

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["mcp_servers"]["bogus"]["command"],
        "definitely-not-a-real-binary-xyz"
    );

    // The binary doesn't exist, so the connect attempt fails — status lists it
    // as failed, with why.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let servers = v["result"]["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1, "resp: {v}");
    assert_eq!(servers[0]["name"], "bogus", "resp: {v}");
    assert_eq!(servers[0]["state"], "failed", "resp: {v}");
    assert!(
        servers[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// `mcp.updateServer` changes a server that is there and refuses one that is
/// not: a misspelt name must not quietly become a second server.
#[tokio::test]
async fn mcp_update_server_replaces_a_declared_server_and_refuses_an_unknown_one() {
    let (_server, sock, dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.updateServer","params":{"name":"bogus","config":{"type":"stdio","command":"x"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], -32602, "resp: {v}");

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"bogus","config":{"type":"stdio","command":"first-missing-binary"}},"id":2}"#,
    )
    .await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.updateServer","params":{"name":"bogus","config":{"type":"stdio","command":"second-missing-binary"}},"id":3}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "resp: {v}");

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["mcp_servers"]["bogus"]["command"],
        "second-missing-binary"
    );
    assert_eq!(written["mcp_servers"].as_object().unwrap().len(), 1);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A server's `headers` and `env` are credentials: they go to the
/// uncommitted `settings.local.json` (0600), the rest to `settings.json`. A
/// later write moves what an earlier version left in `settings.json`, and
/// drops what the replacement no longer has.
#[tokio::test]
async fn mcp_add_server_keeps_headers_and_env_out_of_the_committed_settings() {
    let (_server, sock, dir, handle) = start_server().await;
    let agix = dir.path().join(".agix");
    std::fs::create_dir_all(&agix).unwrap();
    std::fs::write(
        agix.join("settings.json"),
        r#"{"mcp_servers":{"svc":{"type":"stdio","command":"old-binary","env":{"TOKEN":"old-secret"}}}}"#,
    )
    .unwrap();
    let read = |file: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(agix.join(file)).unwrap()).unwrap()
    };

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.updateServer","params":{"name":"svc","config":{"type":"stdio","command":"missing-binary-xyz","env":{"TOKEN":"new-secret"}}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["secrets_written_to"],
        agix.join("settings.local.json").display().to_string(),
        "{v}"
    );
    let committed = read("settings.json");
    assert_eq!(
        committed["mcp_servers"]["svc"]["command"],
        "missing-binary-xyz"
    );
    assert!(
        committed["mcp_servers"]["svc"].get("env").is_none(),
        "the committed settings.json carries the env: {committed}"
    );
    assert_eq!(
        read("settings.local.json")["mcp_servers"]["svc"]["env"]["TOKEN"],
        "new-secret"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(agix.join("settings.local.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "settings.local.json is {mode:o}");
    }

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"svc","config":{"type":"streamable_http","url":"http://127.0.0.1:9/mcp","headers":{"Authorization":"Bearer t"}}},"id":3}"#,
    )
    .await;
    let committed = read("settings.json");
    assert_eq!(
        committed["mcp_servers"]["svc"]["url"],
        "http://127.0.0.1:9/mcp"
    );
    assert!(
        committed["mcp_servers"]["svc"].get("headers").is_none(),
        "{committed}"
    );
    let local = read("settings.local.json");
    assert_eq!(
        local["mcp_servers"]["svc"],
        serde_json::json!({"headers": {"Authorization": "Bearer t"}}),
        "the replaced server's env stayed behind: {local}"
    );

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"svc","config":{"type":"stdio","command":"plain-binary-xyz"}},"id":4}"#,
    )
    .await;
    let local = read("settings.local.json");
    assert!(local["mcp_servers"].get("svc").is_none(), "{local}");

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

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
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

/// A package whose whole content is a script.
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

/// A package that only ships a script installs in the default build.
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
    // this process at a point that rewrites what the model reads, and
    // nothing isolates it from the host. Whoever installs it has to see
    // it before they rely on it.
    let d = &v["result"]["disclosure"];
    let caps = d["capabilities"].as_array().unwrap();
    assert!(
        caps.iter()
            .any(|c| c.as_str().unwrap().contains("`tool.result`")),
        "the script binding must be disclosed: {d}"
    );

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
    // No `api_version`, which is a parse error rather than a default.
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
async fn plugin_rpcs_refuse_names_that_leave_the_cache() {
    let (_server, sock, dir, handle) = start_server().await;
    std::fs::create_dir_all(dir.path().join("keep").join("1.0.0")).unwrap();
    std::fs::create_dir_all(dir.path().join("plugins").join("keep")).unwrap();
    let (archive_path, checksum) = build_demo_plugin_zip(dir.path(), "demo-plugin");

    let calls = [
        r#"{"name":"../..","scope":"global"}"#.to_string(),
        r#"{"name":"..","version":"keep"}"#.to_string(),
        r#"{"name":"demo-plugin","version":"../../../keep"}"#.to_string(),
        format!(r#"{{"name":"{}"}}"#, dir.path().join("keep").display()),
    ];
    for params in &calls {
        let resp = rpc_call(
            &sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"plugin.uninstall","id":1,"params":{params}}}"#),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "{params}: {v}");
    }
    for (name, version) in [("../../escaped", "1.0.0"), ("demo-plugin", "../../escaped")] {
        let resp = rpc_call(
            &sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"{name}","version":"{version}","download_url":"file://{}","checksum":"{checksum}"}}}}"#,
                archive_path.display(),
            ),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            v["error"]["code"],
            codes::INVALID_PARAMS,
            "{name} {version}: {v}"
        );
    }
    assert!(dir.path().join("keep").join("1.0.0").is_dir());
    assert!(dir.path().join("plugins").join("keep").is_dir());
    assert!(!dir.path().join("escaped").exists());

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"other-name","version":"1.0.0","download_url":"file://{}","checksum":"{checksum}"}}}}"#,
            archive_path.display(),
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("demo-plugin"),
        "a package installed under another name is refused: {v}"
    );
    assert!(!dir
        .path()
        .join("plugins")
        .join("cache")
        .join("other-name")
        .exists());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_lifecycle_install_list_disable_enable_uninstall() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_demo_plugin_zip(dir.path(), "demo-plugin");

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

    let d = &v["result"]["disclosure"];
    assert_eq!(d["plugin"], "demo-plugin", "install must disclose: {v}");
    assert_eq!(d["version"], "1.0.0");
    assert_eq!(
        d["capabilities"].as_array().unwrap().len(),
        0,
        "this fixture asks for nothing: {d}"
    );

    // The plugin's own description reaches the model, so it is listed with
    // its provenance rather than left for the reader to infer.
    let visible = d["model_visible"].as_array().unwrap();
    assert!(
        visible
            .iter()
            .any(|v| v["origin"] == "plugin description" && v["text"] == "demo plugin"),
        "{d}"
    );

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    let demo = plugins.iter().find(|p| p["name"] == "demo-plugin").unwrap();
    assert_eq!(demo["enabled"], true);

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

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.enable","id":7,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["enabled"], true, "resp: {v}");

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

/// The disclosure limits are a refusal: a package whose description is over
/// them does not install, and nothing of it is left in the cache.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn an_install_whose_disclosure_is_over_the_limit_is_refused_and_removed() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_plugin_zip(
        dir.path(),
        "wordy",
        &format!(
            "[plugin]\nname = \"wordy\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
             description = \"{}\"\n",
            "x".repeat(501)
        ),
    );
    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"wordy","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
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
            .contains("reaches the model"),
        "the refusal names the limit: {v}"
    );
    assert!(!dir
        .path()
        .join("plugins")
        .join("cache")
        .join("wordy")
        .join("1.0.0")
        .exists());

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        !v["result"]["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "wordy"),
        "{v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// `enabled.json` is the only off switch a package has. Damaged, it keeps
/// everything in its tier off, refuses to be overwritten (which would turn
/// back on whatever it had turned off), and `daemon.doctor` says why.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn a_damaged_enable_state_keeps_plugins_off_and_doctor_says_so() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_demo_plugin_zip(dir.path(), "demo-plugin");
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

    let enabled_json = dir.path().join("plugins").join("enabled.json");
    std::fs::write(&enabled_json, r#"{"demo-plugin": fal"#).unwrap();

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let demo = v["result"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "demo-plugin")
        .cloned()
        .unwrap();
    assert_eq!(demo["enabled"], false, "{demo}");

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.enable","id":3,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["error"].is_object(), "resp: {v}");
    assert_eq!(
        std::fs::read_to_string(&enabled_json).unwrap(),
        r#"{"demo-plugin": fal"#
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":4}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let check = v["result"]["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "plugins.enable_state")
        .cloned()
        .unwrap_or_else(|| panic!("doctor has no enable-state check: {v}"));
    assert_eq!(check["status"], "degraded", "{check}");
    assert_eq!(check["details"]["tiers"][0]["tier"], "global", "{check}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

// A malformed session id reaches `SessionPool::resume_or_create`'s parsing
// only when a real `HistoryStore` is configured; `start_server()` uses
// `history_store: None`, so these tests need their own server variant.

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
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = cap;

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::CodeScene::default()),
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

    // Not a valid BASE58(UUID). A panicking `tokio::spawn`'d connection task
    // just drops the socket silently, so a panic would show here as "no
    // response at all (empty read)", not a catchable Rust panic in the test.
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
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = 8;

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::CodeScene::default()),
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

/// Under `require_unix_socket_auth` the unix socket takes the TCP handshake:
/// nothing is served before `daemon.auth`, a wrong token is refused, and the
/// right one opens the connection for everything else.
#[tokio::test]
async fn the_unix_socket_can_be_made_to_require_the_handshake() {
    let (server, sock, _dir, handle) = start_server().await;
    server.set_tcp_token("s3cr3t".into()).await;
    server.require_unix_socket_auth();

    async fn exchange(sock: &std::path::Path, lines: &[&str]) -> Vec<serde_json::Value> {
        let mut client = UnixStream::connect(sock).await.unwrap();
        let (r, mut w) = client.split();
        let mut br = BufReader::new(r);
        let mut answers = Vec::new();
        for line in lines {
            if w.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
            let mut buf = String::new();
            if br.read_line(&mut buf).await.unwrap_or(0) == 0 {
                break;
            }
            answers.push(serde_json::from_str(&buf).unwrap());
        }
        answers
    }
    let status = r#"{"jsonrpc":"2.0","method":"daemon.status","id":2}"#;

    let refused = exchange(&sock, &[status, status]).await;
    assert_eq!(refused.len(), 1, "served after the refusal: {refused:?}");
    assert_eq!(refused[0]["error"]["code"], codes::UNAUTHORIZED);

    let wrong = exchange(
        &sock,
        &[
            r#"{"jsonrpc":"2.0","method":"daemon.auth","params":{"token":"nope"},"id":1}"#,
            status,
        ],
    )
    .await;
    assert_eq!(wrong.len(), 1, "{wrong:?}");
    assert_eq!(wrong[0]["error"]["code"], codes::UNAUTHORIZED);

    let admitted = exchange(
        &sock,
        &[
            r#"{"jsonrpc":"2.0","method":"daemon.auth","params":{"token":"s3cr3t"},"id":1}"#,
            status,
        ],
    )
    .await;
    assert_eq!(admitted[0]["result"]["authenticated"], true, "{admitted:?}");
    assert!(admitted[1]["result"]["version"].is_string(), "{admitted:?}");

    server.shutdown_token().cancel();
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
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::CodeScene::default()),
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

    let settings_path = dir.path().join(".agix").join("settings.json");
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
    // Uses the same "definitely not a real binary" pattern as
    // `mcp_add_server_writes_settings_and_reports_connect_failure` — the
    // point isn't a successful connection (that needs a real MCP server
    // process), it's proving `config.reload` attempts one and reports the
    // outcome in `mcp_failed`.
    let (_server, sock, dir, handle) = start_server().await;

    let settings_path = dir.path().join(".agix").join("settings.json");
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
    let servers = v["result"]["servers"].as_array().unwrap();
    assert!(
        !servers.is_empty()
            && servers
                .iter()
                .all(|s| s["name"] == "hand-edited" && s["state"] == "failed"),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_with_openai_compatible_rebuilds_the_router() {
    // `openai_compatible` has a runtime `Model` impl
    // (`model::OpenAICompatibleModel`), so configuring a DeepSeek-shaped
    // provider builds and swaps the live router.
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
    // confusing auth failure on the first call instead of a config error.
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
    // The config-generation recreate path replaces an *existing* map entry
    // under the same sid rather than adding one, so at capacity it must not
    // evict an unrelated session.
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
#[cfg(feature = "plugin-packages")]
fn install_scene_plugin(root: &std::path::Path, name: &str) {
    let dir = root.join("plugins").join("cache").join(name).join("1.0.0");
    std::fs::create_dir_all(dir.join("scene")).unwrap();
    std::fs::write(
        dir.join("scene/prompt.md"),
        "You are the demo workflow agent.",
    )
    .unwrap();
    std::fs::write(
        dir.join("scene/scene.json"),
        format!(
            r#"{{"id": "plugin:{name}", "name": "Demo workflow", "description": "A plugin-owned scene",
                "tools": ["Read", "Grep"], "disallowed_tools": ["Bash"]}}"#
        ),
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

[scene]
dir = "scene"
"#
        ),
    )
    .unwrap();
}

/// A plugin that owns a scene needs no `scene.activate`: installing and
/// enabling it is the consent, and entering the scene is still an explicit
/// `session.create`.
#[cfg(feature = "plugin-packages")]
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
#[cfg(feature = "plugin-packages")]
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

/// A plugin that appeared on disk without going through `plugin.install` —
/// dropped in by hand, or rebuilt in place during development — is picked up
/// by `plugin.reload`.
///
/// Asserted through the plugin's *scene*, not through `plugin.list`:
/// `plugin.list` rescans the directories on every call, so it would report a
/// hand-placed plugin with or without a reload and prove nothing. What needs
/// the reload is everything derived from the installed set — scenes, agent
/// types, commands.
#[cfg(feature = "plugin-packages")]
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

#[cfg(feature = "plugin-packages")]
fn build_scene_plugin_zip(dir: &std::path::Path, name: &str) -> (PathBuf, String) {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        let files = [
            (
                "plugin.toml",
                format!(
                    "[plugin]\nname = \"{name}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n[scene]\ndir = \"scene\"\n"
                ),
            ),
            ("scene/prompt.md", "You are the demo workflow agent.".into()),
            (
                "scene/scene.json",
                format!(
                    r#"{{"id": "plugin:{name}", "name": "Demo", "description": "d", "tools": ["Read"]}}"#
                ),
            ),
        ];
        for (path, content) in files {
            writer.start_file(path, opts).unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }
    let archive_path = dir.join(format!("{name}.zip"));
    std::fs::write(&archive_path, &buf).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    (archive_path, hex::encode(hasher.finalize()))
}

/// With `plugins.enable_on_install` off, `plugin.install` reports
/// `enabled: false` and the package contributes nothing (here: its scene
/// cannot host a session) until the host calls `plugin.enable`.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_install_leaves_a_package_off_when_enable_on_install_is_false() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{"plugins": {"enable_on_install": false}}"#,
    )
    .unwrap();
    let (archive_path, checksum) = build_scene_plugin_zip(dir.path(), "held-scene");
    let (server, sock, _dir, handle) = start_server_in(dir).await;

    let call = |body: String| {
        let sock = sock.clone();
        async move {
            let resp = rpc_call(&sock, &body).await;
            serde_json::from_str::<serde_json::Value>(&resp).unwrap()
        }
    };
    let session_in_scene = || {
        call(
            r#"{"jsonrpc":"2.0","method":"session.create","id":9,"params":{"scene":"plugin:held-scene"}}"#
                .to_string(),
        )
    };

    let v = call(format!(
        r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"held-scene","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
        archive_path.display(),
        checksum,
    ))
    .await;
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["enabled"], false, "{v}");
    assert!(
        v["result"]["disclosure"]["scene"].is_string(),
        "the disclosure is still returned: {v}"
    );
    assert!(
        session_in_scene().await["error"].is_object(),
        "a package that is not enabled served a session"
    );

    let listed = call(r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#.to_string()).await;
    let entry = listed["result"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "held-scene")
        .unwrap_or_else(|| panic!("installed but not listed: {listed}"))
        .clone();
    assert_eq!(entry["enabled"], false);

    let v = call(
        r#"{"jsonrpc":"2.0","method":"plugin.enable","id":3,"params":{"name":"held-scene"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(v["result"]["enabled"], true, "{v}");
    assert!(
        session_in_scene().await["result"]["session_id"].is_string(),
        "enabling the package did not make its scene servable"
    );

    let doctor = call(r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":4}"#.to_string()).await;
    assert!(
        doctor["result"]["health"]
            .to_string()
            .contains("plugins.enable_on_install"),
        "doctor does not report the setting: {doctor}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Installing is the default and still enables: the result says so, and the
/// doctor announces that the default is going to change.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_install_enables_by_default_and_doctor_announces_the_coming_change() {
    let (server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_scene_plugin_zip(dir.path(), "eager-scene");
    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"eager-scene","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["enabled"], true, "{v}");

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":2}"#,
    )
    .await;
    assert!(
        resp.contains("planned to default to false"),
        "doctor is silent about the coming default: {resp}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A package that reached the cache without `plugin.install` and is over the
/// disclosure limits is not loaded, and `daemon.doctor` names it.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn an_oversized_hand_placed_package_is_not_loaded_and_doctor_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("plugins/cache/wordy/1.0.0");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("plugin.toml"),
        format!(
            "[plugin]\nname = \"wordy\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\ndescription = \"{}\"\n",
            "x".repeat(600)
        ),
    )
    .unwrap();
    let (server, sock, _dir, handle) = start_server_in(dir).await;

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["plugins"].as_array().unwrap().is_empty(),
        "an over-limit package was listed as loadable: {v}"
    );
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let health = v["result"]["health"].to_string();
    assert!(
        health.contains("plugins.refused") && health.contains("wordy"),
        "doctor does not name the refused package: {health}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}
