//! What an `acceptEdits` session reads without asking, seen from a host: the
//! project, and nothing outside it that `sandbox.read_directories` does not
//! name — the same as `default`.

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

fn stop(reason: base::message::StopReason) -> StreamEvent {
    StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: Some(reason),
            stop_sequence: None,
        },
        usage: Some(Usage::default()),
    }
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
        stop(base::message::StopReason::EndTurn),
    ]
}

fn tool_round(id: &str, tool: &str, input: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: tool.into(),
                input: Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        stop(base::message::StopReason::ToolUse),
    ]
}

struct Daemon {
    sock: PathBuf,
    work: PathBuf,
    /// Outside the project and every directory the instance reads anyway — a
    /// stand-in for `~/Documents`.
    elsewhere: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(read_directories: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work, elsewhere) = (
            dir.path().join("home"),
            dir.path().join("work"),
            dir.path().join("elsewhere"),
        );
        for d in [&home, &work, &elsewhere] {
            std::fs::create_dir_all(d).unwrap();
        }
        let listed: Vec<PathBuf> = read_directories
            .iter()
            .map(|d| dir.path().join(d))
            .collect();
        let global = json!({ "sandbox": {
            "dangerously_disable_sandbox": true,
            "read_directories": listed,
        }});
        std::fs::write(home.join("settings.json"), global.to_string()).unwrap();

        let sock = dir.path().join("accept-edits.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.memory_enabled = false;
        config.settings.permission_mode = base::interface::settings::PermissionMode::AcceptEdits;

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
            work,
            elsewhere,
            model,
            _dir: dir,
        }
    }

    /// Runs a turn, denying every permission prompt it raises; returns the
    /// tool calls the prompts were about.
    async fn turn_denying_prompts(&self) -> Vec<String> {
        let request = json!({"jsonrpc":"2.0","method":"session.run_turn",
                             "params":{"message":"read the notes"},"id":1});
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
                assert_eq!(event["prompt_type"], "permission", "{event}");
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
        "decision": {"type": "deny", "reason": "outside the project"}},"id":2});
    let mut conn = UnixStream::connect(sock).await.unwrap();
    conn.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).await.unwrap();
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["result"]["prompt_id"], prompt_id, "{reply}");
}

fn read(path: &Path) -> Value {
    json!({ "file_path": path.display().to_string() })
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_outside_the_project_is_asked_about_and_inside_it_is_not() {
    let daemon = Daemon::start(&[]).await;
    let inside = daemon.work.join("notes.txt");
    let outside = daemon.elsewhere.join("notes.txt");
    std::fs::write(&inside, "inside-the-project").unwrap();
    std::fs::write(&outside, "outside-the-project").unwrap();

    daemon
        .model
        .push_turn(tool_round("toolu_inside", "Read", read(&inside)));
    daemon
        .model
        .push_turn(tool_round("toolu_outside", "Read", read(&outside)));
    daemon.model.push_turn(tool_round(
        "toolu_cat",
        "Bash",
        json!({ "command": format!("cat {}", outside.display()) }),
    ));
    daemon.model.push_turn(text_round("done"));

    let asked = daemon.turn_denying_prompts().await;
    assert_eq!(asked, ["toolu_outside", "toolu_cat"]);
    let shown = serde_json::to_string(&daemon.model.nth_request(1).unwrap()).unwrap();
    assert!(
        shown.contains("inside-the-project"),
        "the project file was not read: {shown}"
    );
    let last = serde_json::to_string(&daemon.model.nth_request(3).unwrap()).unwrap();
    assert!(
        !last.contains("outside-the-project"),
        "a denied read reached the model: {last}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_read_directory_is_read_without_asking() {
    let daemon = Daemon::start(&["elsewhere"]).await;
    let outside = daemon.elsewhere.join("notes.txt");
    std::fs::write(&outside, "listed-directory").unwrap();

    daemon
        .model
        .push_turn(tool_round("toolu_listed", "Read", read(&outside)));
    daemon.model.push_turn(text_round("done"));

    let asked = daemon.turn_denying_prompts().await;
    assert!(asked.is_empty(), "{asked:?}");
    let shown = serde_json::to_string(&daemon.model.nth_request(1).unwrap()).unwrap();
    assert!(shown.contains("listed-directory"), "{shown}");
}
