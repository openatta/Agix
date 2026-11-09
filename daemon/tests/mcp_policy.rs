//! The MCP policy at each place the pool connects servers: startup,
//! `config.reload`, a project's first session, and `mcp.addServer`; and what
//! a project's own servers may read from the daemon's environment there.
//!
//! Every server here is a shell appending its own name to a log, so the log
//! is a count of the processes the daemon actually started. A denied server
//! must never appear in it.

#![cfg(all(unix, feature = "mcp"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, DaemonConfig, StaticDaemonPaths};
use daemon::session_pool::ProjectSelector;
use daemon::SessionPool;
use mcp::policy::McpPolicy;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};

struct Daemon {
    dir: tempfile::TempDir,
    log: PathBuf,
}

impl Daemon {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("spawns.log");
        Self { dir, log }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// `stray` is started through `/usr/bin/env`, so no rule for `/bin/sh`
    /// admits it.
    fn server(&self, name: &str) -> serde_json::Value {
        self.logging(name, &format!("echo {name}"))
    }

    fn logging(&self, name: &str, script: &str) -> serde_json::Value {
        let script = format!("{script} >> '{}'", self.log.display());
        if name == "stray" {
            serde_json::json!({
                "type": "stdio",
                "command": "/usr/bin/env",
                "args": ["sh", "-c", script],
            })
        } else {
            serde_json::json!({
                "type": "stdio",
                "command": "/bin/sh",
                "args": ["-c", script],
            })
        }
    }

    fn configure(&self, project: &Path, names: &[&str]) {
        let servers: serde_json::Map<String, serde_json::Value> = names
            .iter()
            .map(|n| (n.to_string(), self.server(n)))
            .collect();
        std::fs::create_dir_all(project.join(".agix")).unwrap();
        std::fs::write(
            project.join(".agix").join("settings.json"),
            serde_json::json!({ "mcp_servers": servers }).to_string(),
        )
        .unwrap();
    }

    #[cfg(feature = "plugin-packages")]
    fn install_plugin_server(&self, plugin: &str, server: &str) {
        let dir = self
            .root()
            .join("plugins")
            .join("cache")
            .join(plugin)
            .join("1.0.0");
        std::fs::create_dir_all(dir.join("mcp")).unwrap();
        std::fs::write(
            dir.join("mcp").join("server.json"),
            self.server(&format!("{plugin}-mcp-{server}")).to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            format!(
                "[plugin]\nname = \"{plugin}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
                 description = \"declares an MCP server\"\n\n\
                 [[mcp]]\nname = \"{server}\"\nkind = \"native\"\nconfig = \"mcp/server.json\"\n"
            ),
        )
        .unwrap();
    }

    fn config(&self) -> DaemonConfig {
        load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            None,
            "code",
            &StaticDaemonPaths::new(self.root().to_path_buf()),
        )
    }

    async fn start(&self, policy: Option<McpPolicy>) -> Arc<SessionPool> {
        self.start_with(self.config(), policy).await
    }

    async fn start_with(
        &self,
        config: DaemonConfig,
        policy: Option<McpPolicy>,
    ) -> Arc<SessionPool> {
        let client: Arc<dyn AnthropicClient> =
            Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("unused".into())).unwrap());
        daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene::default()),
            daemon::Assembly {
                cwd: Some(self.root().to_path_buf()),
                model_client: Some(client),
                transcripts: daemon::Transcripts::Nowhere,
                mcp_policy: policy,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }

    fn spawned(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    async fn wait_for_spawn(&self, name: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !self.spawned().iter().any(|n| n == name) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "`{name}` was never started; started: {:?}",
                self.spawned()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn policy(json: serde_json::Value) -> McpPolicy {
    serde_json::from_value(json).unwrap()
}

/// `kept` is allowed; `blocked` is allowed too but also denied, and deny wins;
/// `stray` is in no allow rule.
fn allow_kept_and_blocked_deny_blocked() -> McpPolicy {
    policy(serde_json::json!({
        "allowed_servers": [{"command": "/bin/sh"}],
        "denied_servers": [{"name": "blocked"}],
    }))
}

async fn denied_in_status(pool: &SessionPool) -> Vec<(String, String)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let denied: Vec<(String, String)> = pool
            .mcp_status()
            .await
            .iter()
            .filter(|s| s["state"] == "denied")
            .map(|s| {
                (
                    s["name"].as_str().unwrap().to_string(),
                    s["error"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        if !denied.is_empty() || tokio::time::Instant::now() > deadline {
            let mut denied = denied;
            denied.sort();
            return denied;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One `mcp_connect_failed` per server, however often it was sent: writing
/// settings also wakes the config watcher, which reloads on its own.
async fn failed_events(
    events: &mut tokio::sync::broadcast::Receiver<serde_json::Value>,
    expected: usize,
) -> Vec<(String, String)> {
    let mut failed = std::collections::BTreeMap::new();
    while failed.len() < expected {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("timed out waiting for mcp_connect_failed")
            .unwrap();
        if event["kind"] == "mcp_connect_failed" {
            failed.insert(
                event["server"].as_str().unwrap().to_string(),
                event["error"].as_str().unwrap().to_string(),
            );
        }
    }
    failed.into_iter().collect()
}

#[tokio::test]
async fn startup_never_starts_a_denied_server_and_lists_it_as_denied() {
    let d = Daemon::new();
    d.configure(d.root(), &["blocked", "stray"]);
    let pool = d.start(Some(allow_kept_and_blocked_deny_blocked())).await;

    let denied = denied_in_status(&pool).await;
    assert_eq!(
        denied.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["blocked", "stray"]
    );
    assert!(denied[0].1.contains("denied_servers rule"), "{denied:?}");
    assert!(
        denied[1].1.contains("no allowed_servers rule"),
        "{denied:?}"
    );
    assert!(d.spawned().is_empty(), "started: {:?}", d.spawned());
}

#[tokio::test]
async fn startup_starts_an_allowed_server() {
    let d = Daemon::new();
    d.configure(d.root(), &["kept"]);
    let _pool = d.start(Some(allow_kept_and_blocked_deny_blocked())).await;
    d.wait_for_spawn("kept").await;
}

#[tokio::test]
async fn reload_never_starts_a_denied_server_and_reports_it_failed() {
    let d = Daemon::new();
    let pool = d.start(Some(allow_kept_and_blocked_deny_blocked())).await;
    let mut events = pool.subscribe_events();
    d.configure(d.root(), &["blocked", "stray"]);

    let report = pool.reload_settings(None, "code").await;
    let mut failed: Vec<&str> = report["routing"]["mcp_failed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    failed.sort();
    assert_eq!(failed, ["blocked", "stray"], "{report}");

    let events = failed_events(&mut events, 2).await;
    assert!(
        events
            .iter()
            .all(|(_, e)| e.contains("the host's MCP policy")),
        "{events:?}"
    );
    assert_eq!(denied_in_status(&pool).await.len(), 2);
    assert!(d.spawned().is_empty(), "started: {:?}", d.spawned());
}

#[tokio::test]
async fn reload_starts_an_allowed_server() {
    let d = Daemon::new();
    let pool = d.start(Some(allow_kept_and_blocked_deny_blocked())).await;
    d.configure(d.root(), &["kept"]);
    let reloading = tokio::spawn({
        let pool = pool.clone();
        async move { pool.reload_settings(None, "code").await }
    });
    d.wait_for_spawn("kept").await;
    reloading.abort();
}

#[tokio::test]
async fn another_projects_first_session_never_starts_a_denied_server() {
    let d = Daemon::new();
    let other = d.root().join("other");
    d.configure(&other, &["blocked", "stray"]);
    let pool = d.start(Some(allow_kept_and_blocked_deny_blocked())).await;
    let mut events = pool.subscribe_events();

    pool.create_session(None, ProjectSelector::Path(other), None)
        .await
        .unwrap();
    let events = failed_events(&mut events, 2).await;
    assert_eq!(
        events.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["blocked", "stray"]
    );
    assert!(d.spawned().is_empty(), "started: {:?}", d.spawned());
}

#[tokio::test]
async fn another_projects_first_session_starts_an_allowed_server() {
    let d = Daemon::new();
    let other = d.root().join("other");
    d.configure(&other, &["kept"]);
    let pool = d.start(Some(allow_kept_and_blocked_deny_blocked())).await;
    let creating = tokio::spawn({
        let pool = pool.clone();
        async move {
            let _ = pool
                .create_session(None, ProjectSelector::Path(other), None)
                .await;
        }
    });
    d.wait_for_spawn("kept").await;
    creating.abort();
}

#[tokio::test]
async fn add_server_refuses_a_denied_server_before_writing_settings() {
    let d = Daemon::new();
    let pool = d.start(Some(allow_kept_and_blocked_deny_blocked())).await;
    let settings = d.root().join(".agix").join("settings.json");

    for name in ["blocked", "stray"] {
        let (code, message) = pool
            .add_mcp_server(None, "code", name, d.server(name))
            .await
            .unwrap_err();
        assert_eq!(code, daemon::rpc::codes::MCP_SERVER_DENIED, "{message}");
        assert!(message.contains("the host's MCP policy"), "{message}");
    }
    assert!(
        !settings.exists(),
        "a denied server was written to settings"
    );
    assert!(d.spawned().is_empty(), "started: {:?}", d.spawned());

    pool.add_mcp_server(None, "code", "kept", d.server("kept"))
        .await
        .unwrap();
    assert_eq!(d.spawned(), ["kept"]);
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert!(written["mcp_servers"]["kept"].is_object(), "{written}");

    let (code, _) = pool
        .update_mcp_server(None, "code", "blocked", d.server("blocked"))
        .await
        .unwrap_err();
    assert_eq!(code, daemon::rpc::codes::MCP_SERVER_DENIED);
}

#[tokio::test]
async fn an_unparseable_policy_file_denies_every_server_and_doctor_says_so() {
    let d = Daemon::new();
    d.configure(d.root(), &["kept"]);
    let policy_file = d.root().join("mcp-policy.json");
    std::fs::write(&policy_file, r#"{"allowed_servers": [{"name": "kept"}"#).unwrap();
    let mut config = d.config();
    config.mcp_policy_path = Some(policy_file.clone());
    let pool = d.start_with(config, None).await;

    let denied = denied_in_status(&pool).await;
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert!(denied[0].1.contains("unreadable"), "{denied:?}");
    assert!(
        denied[0].1.contains(&policy_file.display().to_string()),
        "{denied:?}"
    );

    let (code, _) = pool
        .add_mcp_server(None, "code", "kept", d.server("kept"))
        .await
        .unwrap_err();
    assert_eq!(code, daemon::rpc::codes::MCP_SERVER_DENIED);

    let other = d.root().join("other");
    d.configure(&other, &["kept"]);
    pool.create_session(None, ProjectSelector::Path(other), None)
        .await
        .unwrap();
    pool.reload_settings(None, "code").await;
    assert!(d.spawned().is_empty(), "started: {:?}", d.spawned());

    let doctor = pool.doctor_report().await;
    let check = doctor["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "mcp.policy")
        .unwrap_or_else(|| panic!("no mcp.policy check: {doctor}"))
        .clone();
    assert_eq!(check["status"], "failing", "{check}");
    assert_eq!(doctor["health"]["status"], "failing", "{doctor}");
}

#[tokio::test]
async fn a_named_policy_file_that_is_missing_denies_every_server() {
    let d = Daemon::new();
    d.configure(d.root(), &["kept"]);
    let mut config = d.config();
    config.mcp_policy_path = Some(d.root().join("absent.json"));
    let pool = d.start_with(config, None).await;
    let denied = denied_in_status(&pool).await;
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert!(d.spawned().is_empty(), "started: {:?}", d.spawned());
}

#[tokio::test]
async fn a_host_policy_wins_over_the_policy_file() {
    let d = Daemon::new();
    d.configure(d.root(), &["kept"]);
    let policy_file = d.root().join("mcp-policy.json");
    std::fs::write(&policy_file, r#"{"allowed_servers": []}"#).unwrap();
    let mut config = d.config();
    config.mcp_policy_path = Some(policy_file);
    let pool = d
        .start_with(config, Some(allow_kept_and_blocked_deny_blocked()))
        .await;
    d.wait_for_spawn("kept").await;

    let doctor = pool.doctor_report().await;
    let check = doctor["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "mcp.policy")
        .unwrap()
        .clone();
    assert_eq!(check["status"], "ok", "{check}");
    assert_eq!(check["details"]["source"], "host", "{check}");
}

#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_servers_are_admitted_on_reload_too() {
    let d = Daemon::new();
    d.install_plugin_server("p", "srv");
    let pool = d
        .start(Some(policy(serde_json::json!({
            "denied_servers": [{"name": "p-mcp-srv"}],
        }))))
        .await;
    let denied = denied_in_status(&pool).await;
    assert_eq!(denied.len(), 1, "{denied:?}");

    let report = pool.reload_settings(None, "code").await;
    assert_eq!(
        report["routing"]["mcp_failed"],
        serde_json::json!(["p-mcp-srv"]),
        "{report}"
    );
    let denied = denied_in_status(&pool).await;
    assert_eq!(
        denied.len(),
        1,
        "the plugin's server vanished on reload: {denied:?}"
    );
    assert_eq!(denied[0].0, "p-mcp-srv");
    assert!(d.spawned().is_empty(), "started: {:?}", d.spawned());
}

#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_servers_reach_another_project() {
    let d = Daemon::new();
    d.install_plugin_server("p", "srv");
    let other = d.root().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let pool = d
        .start(Some(policy(serde_json::json!({
            "denied_servers": [{"name": "p-mcp-srv"}],
        }))))
        .await;
    denied_in_status(&pool).await;
    let mut events = pool.subscribe_events();

    pool.create_session(None, ProjectSelector::Path(other), None)
        .await
        .unwrap();
    let events = failed_events(&mut events, 1).await;
    assert_eq!(events[0].0, "p-mcp-srv", "{events:?}");
    assert!(d.spawned().is_empty(), "started: {:?}", d.spawned());
}

async fn wait_for_line(d: &Daemon, prefix: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(line) = d.spawned().into_iter().find(|l| l.starts_with(prefix)) {
            return line;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nothing logged `{prefix}`; logged: {:?}",
            d.spawned()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_projects_own_server_does_not_expand_the_daemons_environment() {
    std::env::set_var("AGIX_E2E_PROJECT_SECRET", "s3cret");
    let d = Daemon::new();
    let mut project = d.logging(
        "proj",
        r#"echo "proj:[$AGIX_E2E_PROJECT_SECRET][${OWN}][${AGIX_E2E_PROJECT_SECRET:-unset}]""#,
    );
    project["env"] = serde_json::json!({"OWN": "mine"});
    std::fs::create_dir_all(d.root().join(".agix")).unwrap();
    std::fs::write(
        d.root().join(".agix").join("settings.json"),
        serde_json::json!({"mcp_servers": {"proj": project}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        d.root().join("settings.json"),
        serde_json::json!({"mcp_servers": {
            "user": d.logging("user", r#"echo "user:[$AGIX_E2E_PROJECT_SECRET]""#),
        }})
        .to_string(),
    )
    .unwrap();

    let _pool = d.start(None).await;
    assert_eq!(wait_for_line(&d, "proj:").await, "proj:[][mine][unset]");
    assert_eq!(wait_for_line(&d, "user:").await, "user:[s3cret]");
}

#[tokio::test]
async fn a_server_added_over_rpc_is_the_projects_own() {
    std::env::set_var("AGIX_E2E_ADDED_SECRET", "s3cret");
    let d = Daemon::new();
    let pool = d.start(None).await;

    pool.add_mcp_server(
        None,
        "code",
        "added",
        d.logging("added", r#"echo "added:[$AGIX_E2E_ADDED_SECRET]""#),
    )
    .await
    .unwrap();
    assert_eq!(wait_for_line(&d, "added:").await, "added:[]");

    let (code, message) = pool
        .add_mcp_server(
            None,
            "code",
            "remote",
            serde_json::json!({
                "type": "streamable_http",
                "url": "https://mcp.example/mcp",
                "oauth": {"client_id": "c", "client_secret_env": "AGIX_E2E_ADDED_SECRET"},
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(code, daemon::rpc::codes::INVALID_PARAMS, "{message}");
    assert!(message.contains("client_secret_env"), "{message}");
}
