//! The deployment's managed settings over the wire: a `deny` in them holds in a
//! `bypassPermissions` session, `disable_bypass_permissions_mode` takes that
//! mode away, a file that cannot be honoured refuses every tool call, and
//! `daemon.doctor` / `config.get` say where the layer came from.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use base::interface::settings::PermissionMode;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use model::types::MessagesRequest;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

/// Replays one canned round per model call, then repeats the last.
struct Scripted {
    rounds: StdMutex<std::collections::VecDeque<Vec<StreamEvent>>>,
    last: Vec<StreamEvent>,
}

impl AnthropicClient for Scripted {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        let events = self
            .rounds
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.last.clone());
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

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

/// The model runs `touch <marker>` once, then says it is done.
fn touch_rounds(marker: &str) -> Vec<Vec<StreamEvent>> {
    vec![
        vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ToolUse {
                    id: "tool-1".into(),
                    name: "Bash".into(),
                    input: serde_json::Value::Null,
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::InputJsonDelta {
                    partial_json: json!({ "command": format!("touch {marker}") }).to_string(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            stop(base::message::StopReason::ToolUse),
        ],
        text_round("done"),
    ]
}

struct Daemon {
    sock: PathBuf,
    work: PathBuf,
    managed: Option<PathBuf>,
    server: Arc<DaemonServer>,
    _dir: tempfile::TempDir,
    handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// A daemon whose own settings say `bypassPermissions`, reading its managed
    /// settings from `managed` (written to a file first) when given.
    async fn start(managed: Option<&str>, prompt_timeout: Duration) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let sock = dir.path().join("managed.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.memory_enabled = false;
        config.settings.permission_mode = PermissionMode::BypassPermissions;
        let managed = managed.map(|content| {
            let path = dir.path().join("it").join("managed-settings.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        });
        config.managed_settings_path = managed.clone();

        let rounds = touch_rounds(&work.join("marker").display().to_string());
        let client = Arc::new(Scripted {
            last: rounds.last().cloned().unwrap(),
            rounds: StdMutex::new(rounds.into()),
        });
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene::default()),
            daemon::Assembly {
                cwd: Some(work.clone()),
                permission_prompt_timeout: prompt_timeout,
                model_client: Some(client as Arc<dyn AnthropicClient>),
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
            work,
            managed,
            server,
            _dir: dir,
            handle,
        }
    }

    async fn stop(self) {
        self.server.shutdown_token().cancel();
        let _ = self.handle.await;
    }

    fn marker(&self) -> PathBuf {
        self.work.join("marker")
    }

    /// One request; every frame before the response, and the response.
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> (Vec<serde_json::Value>, serde_json::Value) {
        let mut stream = UnixStream::connect(&self.sock).await.unwrap();
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut lines = BufReader::new(stream).lines();
        let mut frames = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value.get("id").is_some() {
                return (frames, value);
            }
            frames.push(value);
        }
        panic!("connection closed before a response to {method}");
    }

    async fn run_turn(&self, options: serde_json::Value) -> Vec<serde_json::Value> {
        let (frames, response) = self
            .call(
                "session.run_turn",
                json!({"message": "make a file", "options": options}),
            )
            .await;
        assert!(response["result"].is_object(), "turn failed: {response}");
        frames
    }
}

fn events<'a>(frames: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    frames
        .iter()
        .filter(|f| f["params"]["event"]["kind"] == kind)
        .map(|f| &f["params"]["event"])
        .collect()
}

fn tool_results(frames: &[serde_json::Value]) -> String {
    serde_json::to_string(&events(frames, "tool_result")).unwrap()
}

fn check<'a>(doctor: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    doctor["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no {name} check: {doctor}"))
}

const DENY_TOUCH: &str = r#"{"permission_rules": [{"tool": "Bash(touch:*)", "action": "deny"}]}"#;

#[tokio::test]
async fn without_managed_settings_a_bypass_session_runs_the_command() {
    let d = Daemon::start(None, Duration::from_secs(30)).await;
    let frames = d.run_turn(json!({})).await;
    assert!(events(&frames, "prompt").is_empty(), "{frames:?}");
    assert!(d.marker().exists(), "{}", tool_results(&frames));
    d.stop().await;
}

#[tokio::test]
async fn a_managed_deny_refuses_the_call_in_a_bypass_session() {
    for spelling in ["bypassPermissions", "auto", "yolo"] {
        let d = Daemon::start(Some(DENY_TOUCH), Duration::from_secs(30)).await;
        let frames = d.run_turn(json!({"permission_mode": spelling})).await;
        let results = tool_results(&frames);
        assert!(
            !d.marker().exists(),
            "{spelling}: the command ran: {results}"
        );
        assert!(results.contains("Bash(touch:*)"), "{spelling}: {results}");
        assert!(
            events(&frames, "prompt").is_empty(),
            "{spelling}: {frames:?}"
        );
        d.stop().await;
    }
}

#[tokio::test]
async fn a_managed_ask_still_asks_in_a_bypass_session() {
    let d = Daemon::start(
        Some(r#"{"permission_rules": [{"tool": "Bash(touch:*)", "action": "ask"}]}"#),
        Duration::from_secs(1),
    )
    .await;
    let frames = d.run_turn(json!({})).await;
    assert!(!d.marker().exists(), "{}", tool_results(&frames));
    assert!(!events(&frames, "prompt").is_empty(), "{frames:?}");
    d.stop().await;
}

async fn reported_permission(d: &Daemon) -> serde_json::Value {
    let (_, created) = d.call("session.create", json!({})).await;
    let session_id = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, resumed) = d
        .call("session.resume", json!({"session_id": session_id}))
        .await;
    resumed["result"]["permission"].clone()
}

#[tokio::test]
async fn a_bypass_session_reports_prompts_only_when_the_managed_layer_can_ask() {
    let plain = Daemon::start(None, Duration::from_secs(30)).await;
    let permission = reported_permission(&plain).await;
    assert_eq!(permission["mode"], "bypassPermissions", "{permission}");
    assert_eq!(permission["prompts"], false, "{permission}");
    plain.stop().await;

    let asks = Daemon::start(
        Some(r#"{"permission_rules": [{"tool": "Bash(touch:*)", "action": "ask"}]}"#),
        Duration::from_secs(30),
    )
    .await;
    let permission = reported_permission(&asks).await;
    assert_eq!(permission["mode"], "bypassPermissions", "{permission}");
    assert_eq!(permission["prompts"], true, "{permission}");
    asks.stop().await;
}

#[tokio::test]
async fn disabling_bypass_turns_a_bypass_session_into_one_that_asks() {
    let d = Daemon::start(
        Some(r#"{"disable_bypass_permissions_mode": true}"#),
        Duration::from_secs(1),
    )
    .await;
    let frames = d
        .run_turn(json!({"permission_mode": "bypassPermissions"}))
        .await;
    assert!(!d.marker().exists(), "{}", tool_results(&frames));
    assert!(!events(&frames, "prompt").is_empty(), "{frames:?}");
    d.stop().await;
}

#[tokio::test]
async fn a_managed_file_with_a_key_it_may_not_set_refuses_every_tool_call() {
    let d = Daemon::start(
        Some(r#"{"permission_mode": "bypassPermissions", "permission_rules": []}"#),
        Duration::from_secs(30),
    )
    .await;
    let frames = d.run_turn(json!({})).await;
    let results = tool_results(&frames);
    assert!(!d.marker().exists(), "the command ran: {results}");
    assert!(results.contains("managed settings"), "{results}");

    let (_, doctor) = d.call("daemon.doctor", json!({})).await;
    let managed = check(&doctor["result"], "settings.managed");
    assert_eq!(managed["status"], "failing", "{managed}");
    assert!(
        managed["summary"]
            .as_str()
            .unwrap()
            .contains("permission_mode"),
        "{managed}"
    );
    d.stop().await;
}

#[tokio::test]
async fn doctor_and_config_get_name_the_managed_layer_and_where_it_came_from() {
    let d = Daemon::start(Some(DENY_TOUCH), Duration::from_secs(30)).await;
    let origin = d.managed.as_deref().map(Path::display).unwrap().to_string();

    let (_, doctor) = d.call("daemon.doctor", json!({})).await;
    let managed = check(&doctor["result"], "settings.managed");
    assert_eq!(managed["status"], "ok", "{managed}");
    assert_eq!(managed["details"]["present"], true, "{managed}");
    assert_eq!(managed["details"]["origin"], origin.as_str(), "{managed}");

    let (_, config) = d
        .call("config.get", json!({"scene": "code", "tier": "effective"}))
        .await;
    let layer = &config["result"]["managed"];
    assert_eq!(layer["origin"], origin.as_str(), "{config}");
    assert_eq!(
        layer["permission_rules"][0]["tool"], "Bash(touch:*)",
        "{config}"
    );
    d.stop().await;
}

#[tokio::test]
async fn doctor_says_where_it_looked_when_there_are_no_managed_settings() {
    let d = Daemon::start(None, Duration::from_secs(30)).await;
    let (_, doctor) = d.call("daemon.doctor", json!({})).await;
    let managed = check(&doctor["result"], "settings.managed");
    assert_eq!(managed["status"], "ok", "{managed}");
    assert_eq!(managed["details"]["present"], false, "{managed}");
    assert!(managed["details"]["path"].is_string(), "{managed}");
    d.stop().await;
}
