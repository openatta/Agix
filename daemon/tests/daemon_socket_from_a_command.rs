//! A command a session runs cannot act as the daemon's host, nor read the
//! keys the daemon keeps.
//!
//! The daemon's unix socket answers whoever reaches it, and `project.trust`,
//! `mcp.addServer` or `session.respondToPrompt` from a model's own command
//! would undo every other boundary. So a `Bash` call that talks JSON-RPC to
//! the daemon gets nothing out of it: inside the OS sandbox it cannot connect,
//! and outside it the daemon refuses a connection from its own descendant.
//! The host's connection works throughout. Inside the sandbox the state
//! root's settings, where provider keys are written, and its MCP OAuth tokens
//! are not readable either, and under a network allowlist the command runs
//! with the network shut.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const UNAUTHORIZED: i64 = -32003;

const SECRET: &str = "sk-agix-test-secret";
const READABLE: &str = "agix-test-notes";

/// Connects to the socket in `argv[1]` and asks it to trust `argv[2]`.
const CLIENT: &str = r#"
import json, socket, sys
s = socket.socket(socket.AF_UNIX)
try:
    s.connect(sys.argv[1])
except OSError as e:
    print("connect " + "failed:", e)
    sys.exit(0)
request = {"jsonrpc": "2.0", "id": 1, "method": "project.trust",
           "params": {"project_root": sys.argv[2], "trusted": True}}
# A refusal can be written and the socket closed before this request is sent.
try:
    s.sendall((json.dumps(request) + "\n").encode())
except OSError:
    pass
print("re" + "ply:", s.makefile().readline().strip())
"#;

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

fn bash_round(command: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: "toolu_daemon".into(),
                name: "Bash".into(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: json!({ "command": command }).to_string(),
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

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn has_python() -> bool {
    std::process::Command::new("python3")
        .arg("-c")
        .arg("pass")
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether this build has a sandbox backend and this machine runs it. The
/// sandboxed cases say so and skip where either is missing, unless
/// `AGIX_REQUIRE_SANDBOX` is set — CI sets it on the runner that installs
/// bubblewrap.
fn sandbox_runs() -> bool {
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        (
            "sandbox-exec",
            &["-p", "(version 1)(allow default)", "true"],
        )
    } else if cfg!(target_os = "linux") {
        ("bwrap", &["--ro-bind", "/", "/", "true"])
    } else {
        ("", &[])
    };
    let runs = cfg!(feature = "sandbox")
        && !program.is_empty()
        && std::process::Command::new(program)
            .args(args)
            .status()
            .is_ok_and(|s| s.success());
    if !runs {
        assert!(
            std::env::var_os("AGIX_REQUIRE_SANDBOX").is_none(),
            "AGIX_REQUIRE_SANDBOX is set and no sandbox backend runs here"
        );
        eprintln!(
            "skipped: no sandbox backend runs here (Linux: install bubblewrap and allow \
             unprivileged user namespaces)"
        );
    }
    runs
}

/// The socket is outside the state root, where `--socket` may put it: only
/// the daemon having named it keeps a command off it. It is in the project,
/// which a command sees under every backend — bwrap gives a command a `/tmp`
/// of its own, where a socket beside the project could be out of sight
/// without anything having fenced it.
struct Daemon {
    sock: PathBuf,
    home: PathBuf,
    work: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(sandboxed: bool) -> Self {
        Self::start_with(json!({"dangerously_disable_sandbox": !sandboxed})).await
    }

    async fn start_with(sandbox: serde_json::Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(work.join(".agix")).unwrap();
        let global = json!({
            "project_trust": {"default": "untrusted"},
            "sandbox": sandbox,
        });
        std::fs::write(home.join("settings.json"), global.to_string()).unwrap();
        // Where `config.setProvider` puts a key; any value stands in for one,
        // and a provider here would take the turn away from the mock model.
        let keys = json!({
            "permission_rules": [{"tool": format!("Bash(echo {SECRET})"), "action": "deny"}],
        });
        std::fs::write(home.join("settings.local.json"), keys.to_string()).unwrap();
        let oauth = home.join("scenes").join("code").join("mcp").join("oauth");
        std::fs::create_dir_all(&oauth).unwrap();
        let token = json!({ "access_token": SECRET });
        std::fs::write(oauth.join("server.json"), token.to_string()).unwrap();
        std::fs::create_dir_all(home.join("scratchpad")).unwrap();
        std::fs::write(home.join("scratchpad").join("notes.md"), READABLE).unwrap();

        let sock = work.join("agix.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        config.settings.values.permission_mode = base::settings::PermissionMode::BypassPermissions;

        let model = Arc::new(MockAnthropicClient::new());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene::default()),
            daemon::Assembly {
                cwd: Some(work.clone()),
                model_client: Some(model.clone()),
                transcripts: daemon::Transcripts::Nowhere,
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");

        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let listener = DaemonServer::bind_unix(&sock).unwrap();
        tokio::spawn(async move {
            let _ = server.serve_unix_listener(listener).await;
        });
        Self {
            sock,
            home,
            work,
            model,
            _dir: dir,
        }
    }

    async fn ok(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(120), reader.read_line(&mut line))
                .await
                .unwrap_or_else(|_| panic!("{method}: no response"))
                .unwrap();
            assert!(n > 0, "{method}: connection closed before a response");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                assert!(frame.get("result").is_some(), "{method} failed: {frame}");
                return frame["result"].clone();
            }
        }
    }

    fn project(&self) -> serde_json::Value {
        json!({ "project_root": self.work })
    }

    /// Has the model run the client against the daemon's socket and returns
    /// what the command printed, as the model was shown it.
    async fn command_calls_the_daemon(&self) -> String {
        let command = format!(
            "python3 -c {} {} {}",
            shell_quote(CLIENT),
            shell_quote(&self.sock.display().to_string()),
            shell_quote(&self.work.display().to_string()),
        );
        let shown = self.command_shows(&command).await;
        let start = shown
            .find("connect failed:")
            .or_else(|| shown.find("reply:"))
            .unwrap_or_else(|| panic!("the command printed nothing: {shown}"));
        shown[start..].chars().take(400).collect()
    }

    /// Has the model run `command` and returns the request that showed it
    /// the result.
    async fn command_shows(&self, command: &str) -> String {
        self.model.push_turn(bash_round(command));
        self.model.push_turn(text_round("done"));
        self.ok("session.run_turn", json!({"message": "run it"}))
            .await;
        let shown = self.model.nth_request(1).expect("the model saw the result");
        serde_json::to_string(&shown).unwrap()
    }

    async fn assert_still_untrusted(&self) {
        let status = self.ok("project.status", self.project()).await;
        assert_eq!(status["trust"], "untrusted", "{status}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sandboxed_command_cannot_reach_the_daemon() {
    if !has_python() {
        eprintln!("skipped: no python3");
        return;
    }
    let daemon = Daemon::start(true).await;
    daemon.assert_still_untrusted().await;
    let printed = daemon.command_calls_the_daemon().await;
    assert!(
        !printed.contains("\\\"result\\\""),
        "the command got an answer: {printed}"
    );
    #[cfg(target_os = "macos")]
    if sandbox_runs() {
        assert!(
            printed.starts_with("connect failed:"),
            "the sandbox let it connect: {printed}"
        );
    }
    #[cfg(target_os = "linux")]
    if sandbox_runs() {
        assert!(
            printed.starts_with("connect failed:"),
            "bwrap let it connect: {printed}"
        );
    }
    daemon.assert_still_untrusted().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_refuses_a_command_it_started_outside_the_sandbox() {
    if !has_python() {
        eprintln!("skipped: no python3");
        return;
    }
    let daemon = Daemon::start(false).await;
    let printed = daemon.command_calls_the_daemon().await;
    assert!(
        printed.starts_with("reply:") && printed.contains(&UNAUTHORIZED.to_string()),
        "the daemon did not refuse it: {printed}"
    );
    assert!(!printed.contains("\\\"result\\\""), "{printed}");
    daemon.assert_still_untrusted().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sandboxed_command_cannot_read_the_keys_the_daemon_keeps() {
    if !sandbox_runs() {
        return;
    }
    let daemon = Daemon::start(true).await;
    // Through a variable: the gate refuses a command that names a credential
    // before it runs, and this is about what the sandbox lets a command read.
    let command = std::iter::once(format!(
        "H={}",
        shell_quote(&daemon.home.display().to_string())
    ))
    .chain(
        [
            "settings.local.json",
            "scenes/code/mcp/oauth/server.json",
            "scratchpad/notes.md",
        ]
        .map(|f| format!("cat \"$H/{f}\"")),
    )
    .collect::<Vec<_>>()
    .join("; ");
    let shown = daemon.command_shows(&command).await;
    assert!(
        shown.contains(READABLE),
        "the scratchpad was not read: {shown}"
    );
    assert!(!shown.contains(SECRET), "the command read a key: {shown}");
}

/// An allowlist of domains no backend can filter by: the command still runs,
/// with the network shut — a local port the allowlist does not name among
/// it — and doctor says the domains are out of a sandboxed command's reach.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_runs_under_an_allowlist_of_domains_with_the_network_shut() {
    if !sandbox_runs() {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            drop(conn);
        }
    });
    let daemon = Daemon::start_with(json!({
        "network_mode": "allowlist",
        "allowed_domains": ["registry.npmjs.org"],
    }))
    .await;
    let command = format!(
        "echo ran-$((40+2)); (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null \
         && echo reached-$((1+1)) || echo shut-$((2+2))"
    );
    let shown = daemon.command_shows(&command).await;
    assert!(shown.contains("ran-42"), "the command did not run: {shown}");
    assert!(
        shown.contains("shut-4"),
        "127.0.0.1:{port} was reached: {shown}"
    );

    let doctor = daemon.ok("daemon.doctor", json!({})).await;
    let check = doctor["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "sandbox.network_allowlist")
        .cloned()
        .unwrap_or_else(|| panic!("no sandbox.network_allowlist check: {doctor}"));
    assert_eq!(check["status"], "degraded", "{check}");
    assert_eq!(
        check["details"]["unreachable_from_the_sandbox"],
        json!(["registry.npmjs.org"]),
        "{check}"
    );
}
