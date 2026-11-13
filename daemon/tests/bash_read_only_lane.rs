//! What the `Bash` read-only lane lets through without asking, seen from a
//! host.
//!
//! A read-only command runs in Default mode with no prompt at all, so a line
//! that only looks read-only — a listed command told by a variable to start a
//! helper program from the project — must reach the host as a prompt, and a
//! denied prompt must mean the helper never ran.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

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

fn bash_round(id: &str, command: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: "Bash".into(),
                input: Value::Null,
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

struct Daemon {
    sock: PathBuf,
    work: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let global = json!({ "sandbox": {"dangerously_disable_sandbox": true} });
        std::fs::write(home.join("settings.json"), global.to_string()).unwrap();

        let sock = dir.path().join("agix.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        config.settings.values.permission_mode = base::settings::PermissionMode::Default;

        let model = Arc::new(MockAnthropicClient::new());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
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
            work,
            model,
            _dir: dir,
        }
    }

    /// Runs a turn, denying every permission prompt it raises; returns the
    /// tool calls the prompts were about.
    async fn turn_denying_prompts(&self) -> Vec<String> {
        let request = json!({"jsonrpc":"2.0","method":"session.run_turn",
                             "params":{"message":"look around"},"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        let mut asked = Vec::new();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(120), reader.read_line(&mut line))
                .await
                .expect("the turn ended")
                .unwrap();
            assert!(n > 0, "connection closed before the turn ended");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                assert!(frame.get("result").is_some(), "turn failed: {frame}");
                return asked;
            }
            let event = &frame["params"]["event"];
            if event["kind"] == "prompt" {
                asked.push(event["tool_use_id"].as_str().unwrap_or("").to_string());
                deny(
                    &self.sock,
                    frame["params"]["session_id"].as_str().unwrap(),
                    event["prompt_id"].as_str().unwrap(),
                )
                .await;
            }
        }
    }
}

async fn deny(sock: &Path, session_id: &str, prompt_id: &str) {
    let request = json!({"jsonrpc":"2.0","method":"session.respondToPrompt","params":{
        "session_id": session_id, "prompt_id": prompt_id,
        "decision": {"type": "deny", "reason": "not read-only"}},"id":2});
    let mut conn = UnixStream::connect(sock).await.unwrap();
    conn.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).await.unwrap();
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["result"]["prompt_id"], prompt_id, "{reply}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_helper_program_a_variable_names_is_asked_about_not_run() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = Daemon::start().await;
    let helper = daemon.work.join("x.sh");
    std::fs::write(&helper, "#!/bin/sh\ntouch helper-ran\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let lines = [
        "HOMEBREW_BROWSER=./x.sh brew info --github wget",
        "SSH_ASKPASS_REQUIRE=force SSH_ASKPASS=./x.sh git remote show http://127.0.0.1:9/r.git",
    ];
    for (k, line) in lines.iter().enumerate() {
        daemon
            .model
            .push_turn(bash_round(&format!("toolu_{k}"), line));
    }
    daemon.model.push_turn(bash_round("toolu_ls", "ls"));
    daemon.model.push_turn(text_round("done"));

    let asked = daemon.turn_denying_prompts().await;
    assert_eq!(
        asked,
        ["toolu_0", "toolu_1"],
        "each helper line is asked about, `ls` is not"
    );
    assert!(
        !daemon.work.join("helper-ran").exists(),
        "a denied prompt ran the helper anyway"
    );
    let shown = serde_json::to_string(&daemon.model.nth_request(3).unwrap()).unwrap();
    assert!(shown.contains("x.sh"), "`ls` ran without asking: {shown}");
}
