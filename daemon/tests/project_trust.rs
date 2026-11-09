//! Workspace trust over the wire: what an untrusted project's own settings,
//! hooks and MCP servers do (nothing that loosens), `project.trust` /
//! `project.status`, the `project_trust_changed` event, and what
//! `daemon.doctor` says — including that the 0.3.x default keeps today's
//! behavior.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use model::types::MessagesRequest;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const INVALID_PARAMS: i64 = -32602;

/// Says "ok" and stops, every time it is asked.
struct SaysOk;

impl AnthropicClient for SaysOk {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::TextDelta { text: "ok".into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: Some(Usage::default()),
            },
        ];
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

/// `home` is the global and scene tier, `work` the daemon's own project.
struct Daemon {
    sock: PathBuf,
    home: PathBuf,
    work: PathBuf,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// Writes the two settings files first: the daemon reads its own pair's
    /// settings once, at startup.
    async fn start(global: serde_json::Value, project: serde_json::Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(work.join(".agix")).unwrap();
        std::fs::write(home.join("settings.json"), global.to_string()).unwrap();
        std::fs::write(work.join(".agix/settings.json"), project.to_string()).unwrap();
        Self::serve(dir, home, work).await
    }

    async fn serve(dir: tempfile::TempDir, home: PathBuf, work: PathBuf) -> Self {
        let sock = dir.path().join("trust.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.memory_enabled = false;

        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene::default()),
            daemon::Assembly {
                cwd: Some(work.clone()),
                model_client: Some(Arc::new(SaysOk) as Arc<dyn AnthropicClient>),
                transcripts: daemon::Transcripts::Nowhere,
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
        Self {
            sock,
            home,
            work,
            _dir: dir,
            _handle: handle,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(60), reader.read_line(&mut line))
                .await
                .unwrap_or_else(|_| panic!("{method}: no response"))
                .unwrap();
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

    fn project(&self) -> serde_json::Value {
        json!({ "project_root": self.work })
    }

    async fn trust(&self, trusted: bool) -> serde_json::Value {
        let mut params = self.project();
        params["trusted"] = json!(trusted);
        self.ok("project.trust", params).await
    }

    async fn status(&self) -> serde_json::Value {
        self.ok("project.status", self.project()).await
    }

    async fn effective(&self) -> serde_json::Value {
        let mut params = self.project();
        params["scene"] = json!("code");
        self.ok("config.get", params).await["settings"].clone()
    }

    async fn doctor_check(&self, name: &str) -> serde_json::Value {
        let report = self.ok("daemon.doctor", json!({})).await;
        report["health"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("no `{name}` check: {report}"))
            .clone()
    }

    /// A turn on a fresh session in the daemon's own project.
    async fn turn(&self) {
        self.ok("session.run_turn", json!({"message": "hi"})).await;
    }
}

fn strings(v: &serde_json::Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect()
}

fn untrusted_by_default() -> serde_json::Value {
    json!({"project_trust": {"default": "untrusted"}})
}

#[tokio::test]
async fn by_default_a_project_is_trusted_and_the_doctor_says_what_that_lets_through() {
    let daemon = Daemon::start(
        json!({}),
        json!({
            "permission_mode": "bypassPermissions",
            "hooks_config": {"Stop": [{"type": "command", "command": "true"}]},
        }),
    )
    .await;
    std::fs::create_dir_all(daemon.work.join(".agix/agents")).unwrap();
    std::fs::write(
        daemon.work.join(".agix/agents/explore.md"),
        "---\ndescription: the project's explore\n---\nbody",
    )
    .unwrap();
    std::fs::create_dir_all(daemon.work.join(".agents/skills/deploy")).unwrap();
    std::fs::write(
        daemon.work.join(".agents/skills/deploy/SKILL.md"),
        "---\nname: deploy\ndescription: deploys\n---\nBranch: !`git branch --show-current`\n",
    )
    .unwrap();

    assert_eq!(
        daemon.effective().await["permission_mode"],
        "bypassPermissions"
    );

    let status = daemon.status().await;
    assert_eq!(status["trust"], "trusted", "{status}");
    assert_eq!(status["default"], "trusted");
    assert!(status["matched"].is_null());
    let gated = strings(&status["requires_trust"]);
    assert!(gated.contains(&"permission_mode"), "{status}");
    assert!(gated.contains(&"hooks_config"), "{status}");
    assert_eq!(
        gated.contains(&"agents/explore"),
        cfg!(feature = "agents"),
        "{status}"
    );
    assert_eq!(
        gated.contains(&"skills/deploy"),
        cfg!(feature = "skills"),
        "{status}"
    );

    let check = daemon.doctor_check("project.trust").await;
    assert_eq!(check["status"], "degraded", "{check}");
    let summary = check["summary"].as_str().unwrap();
    assert!(
        summary.contains("project_trust.default") && summary.contains("0.4"),
        "{check}"
    );
    assert_eq!(check["details"]["trust"], "trusted");
}

#[tokio::test]
async fn an_untrusted_project_cannot_loosen_permissions() {
    let daemon = Daemon::start(
        untrusted_by_default(),
        json!({
            "permission_mode": "bypassPermissions",
            "permission_rules": [{"tool": "Bash", "action": "allow"}],
            "sandbox": {"read_anywhere": true},
        }),
    )
    .await;

    let settings = daemon.effective().await;
    assert_eq!(settings["permission_mode"], "default", "{settings}");
    assert_eq!(settings["sandbox"]["read_anywhere"], false);

    let check = daemon.doctor_check("project.trust").await;
    assert_eq!(check["status"], "degraded", "{check}");
    assert_eq!(
        strings(&check["details"]["requires_trust"]),
        [
            "permission_mode",
            "sandbox.read_anywhere",
            "permission_rules.allow(Bash)"
        ]
    );
}

#[tokio::test]
async fn project_trust_and_status_round_trip() {
    let daemon = Daemon::start(
        untrusted_by_default(),
        json!({"permission_mode": "bypassPermissions"}),
    )
    .await;

    let mut events = UnixStream::connect(&daemon.sock).await.unwrap();
    events
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.subscribeEvents\",\"id\":1}\n")
        .await
        .unwrap();
    let mut events = BufReader::new(events);
    let mut line = String::new();
    events.read_line(&mut line).await.unwrap();

    let before = daemon.status().await;
    assert_eq!(before["trust"], "untrusted", "{before}");
    assert_eq!(strings(&before["requires_trust"]), ["permission_mode"]);

    let trusted = daemon.trust(true).await;
    assert_eq!(trusted["trust"], "trusted", "{trusted}");
    assert_eq!(trusted["matched"], json!(daemon.work));
    assert_eq!(
        trusted["written_to"],
        json!(daemon.home.join("settings.local.json"))
    );
    let written: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(daemon.home.join("settings.local.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(written["project_trust"]["trusted"], json!([daemon.work]));
    assert_eq!(daemon.status().await["trust"], "trusted");
    assert_eq!(
        daemon.effective().await["permission_mode"],
        "bypassPermissions"
    );

    let event = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut line = String::new();
            events.read_line(&mut line).await.unwrap();
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame["params"]["kind"] == "project_trust_changed" {
                return frame["params"].clone();
            }
        }
    })
    .await
    .expect("no project_trust_changed event");
    assert_eq!(event["project_root"], json!(daemon.work));
    assert_eq!(event["trust"], "trusted");

    let again = daemon.trust(true).await;
    assert_eq!(again["trust"], "trusted");
    let written: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(daemon.home.join("settings.local.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        written["project_trust"]["trusted"],
        json!([daemon.work]),
        "trusting twice lists it once"
    );

    let revoked = daemon.trust(false).await;
    assert_eq!(revoked["trust"], "untrusted", "{revoked}");
    assert_eq!(daemon.effective().await["permission_mode"], "default");

    let null_root = daemon
        .call(
            "project.trust",
            json!({"project_root": null, "trusted": true}),
        )
        .await;
    assert_eq!(null_root["error"]["code"], INVALID_PARAMS, "{null_root}");
    let no_answer = daemon.call("project.trust", daemon.project()).await;
    assert_eq!(no_answer["error"]["code"], INVALID_PARAMS, "{no_answer}");
}

#[tokio::test]
async fn trust_is_set_only_in_the_global_tier() {
    let daemon = Daemon::start(
        untrusted_by_default(),
        json!({
            "project_trust": {"default": "trusted", "trusted": ["/**"]},
            "permission_mode": "bypassPermissions",
        }),
    )
    .await;

    assert_eq!(daemon.status().await["trust"], "untrusted");
    assert_eq!(daemon.effective().await["permission_mode"], "default");
}

#[cfg(unix)]
#[tokio::test]
async fn an_untrusted_projects_hooks_do_not_run() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("hooks.log");
    let mut global = untrusted_by_default();
    global["hooks_config"] = json!({
        "UserPromptSubmit": [{"type": "command", "command": "trust-probe"}],
    });
    let daemon = Daemon::start(
        global,
        json!({"hooks_config": {
            "Stop": [{"type": "command", "command": format!("echo project-config >> '{}'", log.display())}],
        }}),
    )
    .await;
    let probe = daemon.work.join(".agix/hooks/trust-probe");
    std::fs::create_dir_all(probe.parent().unwrap()).unwrap();
    std::fs::write(
        &probe,
        format!("#!/bin/sh\necho project-script >> '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let logged = || std::fs::read_to_string(&log).unwrap_or_default();

    daemon.turn().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        logged(),
        "",
        "neither the project's own hook nor its hooks/ script runs before it is trusted"
    );

    daemon.trust(true).await;
    daemon.turn().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !(logged().contains("project-config") && logged().contains("project-script")) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "once trusted both run; log: {:?}",
            logged()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(all(unix, feature = "mcp"))]
#[tokio::test]
async fn a_projects_own_mcp_servers_wait_for_trust() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("spawns.log");
    let server = |name: &str| {
        json!({"type": "stdio", "command": "/bin/sh",
               "args": ["-c", format!("echo {name} >> '{}'", log.display())]})
    };
    let daemon = Daemon::start(
        untrusted_by_default(),
        json!({"mcp_servers": {"fs": server("fs")}}),
    )
    .await;
    let spawned = || std::fs::read_to_string(&log).unwrap_or_default();

    let status = daemon.ok("mcp.status", json!({})).await;
    let fs = status["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "fs")
        .unwrap_or_else(|| panic!("fs not listed: {status}"))
        .clone();
    assert_eq!(fs["state"], "untrusted", "{fs}");
    assert_eq!(fs["transport"], "stdio");

    let added = daemon
        .call(
            "mcp.addServer",
            json!({"name": "other", "config": server("other")}),
        )
        .await;
    assert_eq!(
        added["error"]["code"],
        daemon::rpc::codes::PROJECT_UNTRUSTED,
        "{added}"
    );
    let on_disk = std::fs::read_to_string(daemon.work.join(".agix/settings.json")).unwrap();
    assert!(!on_disk.contains("other"), "nothing written: {on_disk}");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(spawned(), "", "an untrusted project's server never starts");

    daemon.trust(true).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !spawned().contains("fs") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "trusting the project starts its server"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status = daemon.ok("mcp.status", json!({})).await;
    assert!(
        status["servers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["state"] != "untrusted"),
        "{status}"
    );
}
