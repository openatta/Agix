//! A session's tasks can be listed, read and stopped by its host.
//!
//! What runs on in a session beyond the model's reply — a shell command, a
//! sub-agent, a scheduled job — is listed by `session.tasks`, read by
//! `session.taskOutput`, stopped by `session.stopTask`, and announced to the
//! session's watchers as it starts, changes and ends, outside any turn — with
//! what a command prints streamed to them as it runs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use model::types::MessagesRequest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const SESSION_NOT_FOUND: i64 = -32000;
const TASK_NOT_FOUND: i64 = -32018;

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

fn tool_round(tool: &str, id: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: tool.to_string(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
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
    sock: std::path::PathBuf,
    dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(model: Arc<MockAnthropicClient>) -> Self {
        Self::start_with(model).await
    }

    async fn start_with(model: Arc<dyn AnthropicClient>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("tasks.sock");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&sock),
            "code",
            &StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        config.settings.values.memory_enabled = false;
        config.settings.values.permission_mode = base::settings::PermissionMode::BypassPermissions;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene::default()),
            daemon::Assembly {
                cwd: Some(cwd),
                model_client: Some(model),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let bind = sock.clone();
        tokio::spawn(async move {
            let _ = server.serve_unix(&bind).await;
        });
        for _ in 0..80 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(sock.exists(), "socket never bound");
        Self { sock, dir }
    }

    /// The project root the daemon is configured with, where its commands
    /// run. Their sandbox shows it on every platform; a temporary directory
    /// of the test's own is hidden on Linux behind the command's private
    /// `/tmp`.
    fn project(&self) -> &std::path::Path {
        self.dir.path()
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = serde_json::json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "{method}: connection closed before a response");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                return frame;
            }
        }
    }

    async fn ok(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let frame = self.call(method, params).await;
        assert!(frame.get("result").is_some(), "{method} failed: {frame}");
        frame["result"].clone()
    }

    /// Every `session.event` this connection receives after subscribing.
    async fn watch(&self, session_id: &str) -> Arc<Mutex<Vec<serde_json::Value>>> {
        let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        let request = serde_json::json!({
            "jsonrpc":"2.0","method":"session.subscribe",
            "params":{"session_id": session_id},"id":1
        });
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let into = seen.clone();
        tokio::spawn(async move {
            let (r, _w) = conn.split();
            let mut reader = BufReader::new(r);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
                if frame["method"] == "session.event" {
                    into.lock().unwrap().push(frame["params"].clone());
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen
    }
}

/// Re-evaluate `$probe` (an expression yielding `Option<_>`) until it is
/// `Some`, or fail saying what never happened.
macro_rules! eventually {
    ($what:expr, $probe:expr) => {{
        let mut found = None;
        for _ in 0..2400 {
            if let Some(hit) = $probe {
                found = Some(hit);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        found.unwrap_or_else(|| panic!("never saw {}", $what))
    }};
}

#[tokio::test]
async fn a_running_shell_command_is_listed_announced_and_stopped() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "Bash",
        "toolu_sleep",
        serde_json::json!({"command": "tail -f /dev/null"}),
    ));
    model.push_turn(text_round("stopped"));
    let daemon = Arc::new(Daemon::start(model).await);

    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = daemon.watch(&session).await;
    let turn = {
        let (daemon, session) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            daemon
                .call(
                    "session.run_turn",
                    serde_json::json!({"session_id": session, "message": "run it"}),
                )
                .await
        })
    };

    let shell = eventually!("the shell command listed as running", {
        let tasks = daemon
            .ok("session.tasks", serde_json::json!({"session_id": session}))
            .await;
        assert_eq!(tasks["active"], true);
        tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["kind"] == "shell" && t["status"] == "running")
            .cloned()
    });
    assert_eq!(shell["detail"]["command"], "tail -f /dev/null");

    let started = eventually!("task_started, a second into the command", {
        frames
            .lock()
            .unwrap()
            .iter()
            .find(|f| f["event"]["kind"] == "task_started")
            .cloned()
    });
    assert!(
        started["turn_id"].is_null(),
        "not part of the turn: {started}"
    );
    assert_eq!(started["event"]["task"]["task_id"], shell["task_id"]);

    let stopped = daemon
        .ok(
            "session.stopTask",
            serde_json::json!({"session_id": session, "task_id": shell["task_id"]}),
        )
        .await;
    assert_eq!(stopped["stopped"], true);

    let response = tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("the turn ends once its command is stopped")
        .unwrap();
    assert!(response.get("result").is_some(), "{response}");

    let finished = eventually!("task_finished", {
        frames
            .lock()
            .unwrap()
            .iter()
            .find(|f| f["event"]["kind"] == "task_finished")
            .cloned()
    });
    assert_eq!(finished["event"]["task"]["status"], "stopped");

    let again = daemon
        .ok(
            "session.stopTask",
            serde_json::json!({"session_id": session, "task_id": shell["task_id"]}),
        )
        .await;
    assert_eq!(
        again["stopped"], false,
        "an ended task cannot be stopped twice"
    );
}

#[tokio::test]
async fn a_scheduled_job_is_one_of_its_sessions_tasks() {
    let daemon = Daemon::start(Arc::new(MockAnthropicClient::new())).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = daemon
        .ok(
            "schedule.create",
            serde_json::json!({"session_id": session, "cron": "0 9 * * *", "prompt": "report"}),
        )
        .await["job"]
        .clone();

    let tasks = daemon
        .ok("session.tasks", serde_json::json!({"session_id": session}))
        .await;
    let listed = &tasks["tasks"].as_array().unwrap()[0];
    assert_eq!(listed["kind"], "cron");
    assert_eq!(listed["task_id"], job["id"]);
    assert_eq!(listed["detail"]["cron"], "0 9 * * *");

    let stopped = daemon
        .ok(
            "session.stopTask",
            serde_json::json!({"session_id": session, "task_id": job["id"]}),
        )
        .await;
    assert_eq!(stopped["stopped"], true);
    assert!(
        daemon.ok("schedule.list", serde_json::json!({})).await["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn an_unknown_session_has_no_tasks_to_list() {
    let daemon = Daemon::start(Arc::new(MockAnthropicClient::new())).await;
    let response = daemon
        .call("session.tasks", serde_json::json!({"session_id": "nope"}))
        .await;
    assert_eq!(response["error"]["code"], SESSION_NOT_FOUND, "{response}");
}

#[tokio::test]
async fn a_running_shell_commands_output_can_be_read_as_it_runs() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "Bash",
        "toolu_chatty",
        serde_json::json!({"command": "echo first; echo second >&2; sleep 30"}),
    ));
    model.push_turn(text_round("stopped"));
    let daemon = Arc::new(Daemon::start(model).await);

    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let turn = {
        let (daemon, session) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            daemon
                .call(
                    "session.run_turn",
                    serde_json::json!({"session_id": session, "message": "run it"}),
                )
                .await
        })
    };

    let task_id = eventually!("the shell command listed", {
        daemon
            .ok("session.tasks", serde_json::json!({"session_id": session}))
            .await["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["kind"] == "shell")
            .map(|t| t["task_id"].clone())
    });
    let read = |tail: Option<u64>| {
        let (daemon, session, task_id) = (daemon.clone(), session.clone(), task_id.clone());
        async move {
            let mut params = serde_json::json!({"session_id": session, "task_id": task_id});
            if let Some(tail) = tail {
                params["tail_bytes"] = tail.into();
            }
            daemon.ok("session.taskOutput", params).await
        }
    };

    let whole = eventually!("both lines in the output", {
        let out = read(None).await;
        let text = out["output"].as_str().unwrap().to_string();
        (text.contains("first\n") && text.contains("second\n")).then_some(out)
    });
    assert_eq!(whole["status"], "running");
    assert_eq!(whole["truncated"], false);

    let tail = read(Some(7)).await;
    assert_eq!(tail["output"].as_str().unwrap().len(), 7);
    assert_eq!(tail["truncated"], true);

    daemon
        .ok(
            "session.stopTask",
            serde_json::json!({"session_id": session, "task_id": task_id}),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("the turn ends once its command is stopped")
        .unwrap();

    let after = read(None).await;
    assert_eq!(after["status"], "stopped");
    assert!(
        after["output"].as_str().unwrap().contains("first\n"),
        "an ended task keeps its output: {after}"
    );
}

#[tokio::test]
async fn output_is_only_for_a_task_the_engine_knows() {
    let daemon = Daemon::start(Arc::new(MockAnthropicClient::new())).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = daemon
        .ok(
            "schedule.create",
            serde_json::json!({"session_id": session, "cron": "0 9 * * *", "prompt": "report"}),
        )
        .await["job"]["id"]
        .clone();

    for task_id in [serde_json::json!("sh-nope"), job] {
        let response = daemon
            .call(
                "session.taskOutput",
                serde_json::json!({"session_id": session, "task_id": task_id}),
            )
            .await;
        assert_eq!(response["error"]["code"], TASK_NOT_FOUND, "{response}");
    }

    let response = daemon
        .call(
            "session.taskOutput",
            serde_json::json!({"session_id": "nope", "task_id": "sh-nope"}),
        )
        .await;
    assert_eq!(response["error"]["code"], SESSION_NOT_FOUND, "{response}");

    for bad in [
        serde_json::json!({"tail_bytes": 0}),
        serde_json::json!({"tail_bytes": 8, "from_byte": 0}),
        serde_json::json!({"from_byte": -1}),
        serde_json::json!({"from_byte": 1.5}),
    ] {
        let mut params = serde_json::json!({"session_id": session, "task_id": "sh-nope"});
        params
            .as_object_mut()
            .unwrap()
            .extend(bad.as_object().unwrap().clone());
        let response = daemon.call("session.taskOutput", params).await;
        assert_eq!(response["error"]["code"], -32602, "{bad}: {response}");
    }
}

#[tokio::test]
async fn a_sub_agents_output_is_its_answer() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        serde_json::json!({
            "description": "say hello",
            "prompt": "Reply with: hello from below",
            "subagent_type": "general-purpose",
        }),
    ));
    model.push_turn(text_round("hello from below"));
    model.push_turn(text_round("done"));
    let daemon = Daemon::start(model).await;

    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "delegate"}),
        )
        .await;

    let tasks = daemon
        .ok("session.tasks", serde_json::json!({"session_id": session}))
        .await;
    let agent = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["kind"] == "agent")
        .cloned()
        .unwrap_or_else(|| panic!("no agent task: {tasks}"));
    let out = daemon
        .ok(
            "session.taskOutput",
            serde_json::json!({"session_id": session, "task_id": agent["task_id"]}),
        )
        .await;
    assert_eq!(out["status"], "done");
    assert_eq!(out["output"], "hello from below");
}

#[tokio::test]
async fn a_background_sub_agents_progress_names_the_call_that_started_it() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("hi"));
    model.push_turn(tool_round(
        "Agent",
        "toolu_bg",
        serde_json::json!({
            "description": "say hello",
            "prompt": "Reply with: hello from below",
            "subagent_type": "general-purpose",
            "background": true,
        }),
    ));
    // The parent's next call and the sub-agent's race for these.
    model.push_turn(text_round("done"));
    model.push_turn(text_round("done"));
    let daemon = Daemon::start(model).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = daemon.watch(&session).await;
    for message in ["hello", "delegate"] {
        daemon
            .ok(
                "session.run_turn",
                serde_json::json!({"session_id": session, "message": message}),
            )
            .await;
    }

    let progress = eventually!(
        "the background sub-agent's progress",
        frames
            .lock()
            .unwrap()
            .iter()
            .find(|f| f["event"]["kind"] == "subagent_progress")
            .cloned()
    );
    assert_eq!(
        progress["event"]["parent_tool_use_id"], "toolu_bg",
        "{progress}"
    );
    assert_eq!(progress["event"]["parent_turn"], 1, "{progress}");
}

/// Keeps talking and never finishes, and counts the calls given up on — a
/// stream dropped unfinished is a model call whose caller was stopped.
#[derive(Default)]
struct NeverFinishes {
    calls: std::sync::atomic::AtomicUsize,
    abandoned: Arc<std::sync::atomic::AtomicUsize>,
}

struct CountsWhenDropped(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for CountsWhenDropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl AnthropicClient for NeverFinishes {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let guard = CountsWhenDropped(self.abandoned.clone());
        let start = StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        };
        let talking = futures::stream::unfold(guard, |guard| async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let delta = StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::TextDelta {
                    text: "still thinking ".into(),
                },
            };
            Some((Ok(delta), guard))
        });
        Box::pin(futures::StreamExt::chain(
            futures::stream::iter([Ok(start)]),
            talking,
        ))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

#[tokio::test]
async fn a_sub_agent_the_host_started_stops_when_its_session_closes() {
    let model = Arc::new(NeverFinishes::default());
    let daemon = Daemon::start_with(model.clone()).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    daemon
        .ok(
            "session.startAgent",
            serde_json::json!({"session_id": session, "prompt": "think about it"}),
        )
        .await;
    eventually!(
        "the sub-agent call the model",
        (model.calls.load(std::sync::atomic::Ordering::SeqCst) > 0).then_some(())
    );
    assert_eq!(model.abandoned.load(std::sync::atomic::Ordering::SeqCst), 0);

    daemon
        .ok("session.close", serde_json::json!({"session_id": session}))
        .await;
    eventually!(
        "the sub-agent stop",
        (model.abandoned.load(std::sync::atomic::Ordering::SeqCst) > 0).then_some(())
    );
}

/// The frames about `task_id`, in the order they arrived.
fn about(
    frames: &Mutex<Vec<serde_json::Value>>,
    task_id: &serde_json::Value,
) -> Vec<serde_json::Value> {
    frames
        .lock()
        .unwrap()
        .iter()
        .filter(|f| f["event"]["task_id"] == *task_id || f["event"]["task"]["task_id"] == *task_id)
        .cloned()
        .collect()
}

async fn run_command(command: &str) -> (Arc<Daemon>, Arc<Mutex<Vec<serde_json::Value>>>, String) {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "Bash",
        "toolu_cmd",
        serde_json::json!({"command": command}),
    ));
    model.push_turn(text_round("ran"));
    let daemon = Arc::new(Daemon::start(model).await);
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = daemon.watch(&session).await;
    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "run it"}),
        )
        .await;
    (daemon, frames, session)
}

#[tokio::test]
async fn what_a_running_command_prints_streams_to_the_sessions_watchers() {
    let (_daemon, frames, _session) =
        run_command("for i in 1 2 3 4 5 6; do echo line-$i; sleep 0.5; done").await;

    let finished = eventually!("task_finished", {
        frames
            .lock()
            .unwrap()
            .iter()
            .find(|f| f["event"]["kind"] == "task_finished")
            .cloned()
    });
    let task_id = finished["event"]["task"]["task_id"].clone();
    let seen = about(&frames, &task_id);
    let kinds: Vec<&str> = seen
        .iter()
        .map(|f| f["event"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.first(), Some(&"task_started"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"task_finished"), "{kinds:?}");
    let progress: Vec<&serde_json::Value> = seen
        .iter()
        .filter(|f| f["event"]["kind"] == "task_progress")
        .collect();
    assert!(
        progress.len() >= 2,
        "a three-second command streams more than once a run: {kinds:?}"
    );
    assert!(progress
        .iter()
        .all(|f| f["turn_id"].is_null() && f["event"]["skipped_bytes"] == 0));
    let streamed: String = progress
        .iter()
        .map(|f| f["event"]["appended"].as_str().unwrap())
        .collect();
    assert_eq!(
        streamed, "line-1\nline-2\nline-3\nline-4\nline-5\nline-6\n",
        "every line once, in order, the last before task_finished"
    );
    let mut next = 0;
    for frame in &progress {
        assert_eq!(frame["event"]["from_byte"], next, "{frame}");
        next = frame["event"]["next_byte"].as_u64().unwrap();
        assert_eq!(
            next - frame["event"]["from_byte"].as_u64().unwrap(),
            frame["event"]["appended"].as_str().unwrap().len() as u64,
            "{frame}"
        );
    }
    assert_eq!(next, streamed.len() as u64);
}

#[tokio::test]
async fn a_quick_command_streams_nothing() {
    let (_daemon, frames, _session) = run_command("echo quick").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let task_frames: Vec<_> = frames
        .lock()
        .unwrap()
        .iter()
        .filter(|f| {
            f["event"]["kind"]
                .as_str()
                .unwrap_or("")
                .starts_with("task_")
        })
        .cloned()
        .collect();
    assert!(task_frames.is_empty(), "{task_frames:?}");
}

#[tokio::test]
async fn a_flood_of_output_streams_as_its_tail_and_says_what_it_left_out() {
    let (daemon, frames, session) =
        run_command("sleep 1.2; head -c 40000 /dev/zero | tr '\\0' x; echo; sleep 1.2").await;
    let finished = eventually!("task_finished", {
        frames
            .lock()
            .unwrap()
            .iter()
            .find(|f| f["event"]["kind"] == "task_finished")
            .cloned()
    });
    let task_id = finished["event"]["task"]["task_id"].clone();
    let progress: Vec<serde_json::Value> = about(&frames, &task_id)
        .into_iter()
        .filter(|f| f["event"]["kind"] == "task_progress")
        .collect();
    let (shown, skipped) = progress.iter().fold((0, 0), |(shown, skipped), f| {
        let appended = f["event"]["appended"].as_str().unwrap().len();
        assert!(appended <= 8 * 1024, "a frame carries at most 8 KiB");
        (
            shown + appended,
            skipped + f["event"]["skipped_bytes"].as_u64().unwrap() as usize,
        )
    });
    assert!(skipped > 0, "{progress:?}");
    assert_eq!(
        shown + skipped,
        40_001,
        "every byte is either sent or counted"
    );

    let whole = daemon
        .ok(
            "session.taskOutput",
            serde_json::json!({"session_id": session, "task_id": task_id, "tail_bytes": 100_000}),
        )
        .await;
    assert_eq!(
        whole["output"].as_str().unwrap().len(),
        40_001,
        "what a frame left out is still there to read"
    );
}

/// Start a turn that runs `command` with Bash, and hand back the shell
/// task's id once it is listed, with the turn still running.
async fn start_command(
    command: &str,
) -> (
    Arc<Daemon>,
    String,
    serde_json::Value,
    tokio::task::JoinHandle<serde_json::Value>,
) {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "Bash",
        "toolu_cmd",
        serde_json::json!({"command": command}),
    ));
    model.push_turn(text_round("ran"));
    let daemon = Arc::new(Daemon::start(model).await);
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let turn = {
        let (daemon, session) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            daemon
                .call(
                    "session.run_turn",
                    serde_json::json!({"session_id": session, "message": "run it"}),
                )
                .await
        })
    };
    let task_id = eventually!("the shell command listed", {
        daemon
            .ok("session.tasks", serde_json::json!({"session_id": session}))
            .await["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["kind"] == "shell")
            .map(|t| t["task_id"].clone())
    });
    (daemon, session, task_id, turn)
}

async fn read_from(
    daemon: &Daemon,
    session: &str,
    task_id: &serde_json::Value,
    from_byte: u64,
) -> serde_json::Value {
    daemon
        .ok(
            "session.taskOutput",
            serde_json::json!({"session_id": session, "task_id": task_id, "from_byte": from_byte}),
        )
        .await
}

#[tokio::test]
async fn reads_from_each_next_byte_join_into_a_commands_whole_output() {
    let (daemon, session, task_id, turn) =
        start_command("for i in 1 2 3 4 5; do echo line-$i; sleep 0.5; done").await;

    let mut joined = String::new();
    let mut next = 0;
    let first = eventually!("the first line", {
        let out = read_from(&daemon, &session, &task_id, 0).await;
        (!out["output"].as_str().unwrap().is_empty()).then_some(out)
    });
    let second = {
        tokio::time::sleep(Duration::from_millis(700)).await;
        read_from(
            &daemon,
            &session,
            &task_id,
            first["next_byte"].as_u64().unwrap(),
        )
        .await
    };
    tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("the command ends")
        .unwrap();
    let third = read_from(
        &daemon,
        &session,
        &task_id,
        second["next_byte"].as_u64().unwrap(),
    )
    .await;
    for read in [&first, &second, &third] {
        assert_eq!(read["from_byte"], next, "{read}");
        assert_eq!(read["dropped"], 0, "{read}");
        assert_eq!(read["truncated"], false, "{read}");
        joined.push_str(read["output"].as_str().unwrap());
        next = read["next_byte"].as_u64().unwrap();
        assert_eq!(
            read["from_byte"].as_u64().unwrap() + read["output"].as_str().unwrap().len() as u64,
            next
        );
    }
    assert_eq!(joined, "line-1\nline-2\nline-3\nline-4\nline-5\n");
    assert_eq!(first["final"], false);
    assert_eq!(third["final"], true);
    assert_eq!(third["status"], "done");

    let after = read_from(&daemon, &session, &task_id, next).await;
    assert_eq!(after["output"], "");
    assert_eq!(
        after["next_byte"], next,
        "an ended task's output does not grow"
    );
    assert_eq!(after["final"], true);

    let past = daemon
        .call(
            "session.taskOutput",
            serde_json::json!({"session_id": session, "task_id": task_id, "from_byte": next + 1}),
        )
        .await;
    assert_eq!(past["error"]["code"], -32602, "{past}");
    assert!(
        past["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&format!("next_byte is {next}")),
        "{past}"
    );

    let tail = daemon
        .ok(
            "session.taskOutput",
            serde_json::json!({"session_id": session, "task_id": task_id, "tail_bytes": 7}),
        )
        .await;
    assert_eq!(tail["output"], "line-5\n");
    assert_eq!(tail["from_byte"], next - 7);
    assert_eq!(tail["next_byte"], next);
    assert_eq!(tail["final"], true);
}

#[tokio::test]
async fn a_read_after_more_than_is_kept_says_how_much_was_dropped() {
    let (daemon, session, task_id, turn) =
        start_command("echo start; while [ ! -e go ]; do sleep 0.05; done; yes x | head -c 600000")
            .await;
    let first = eventually!("the first line", {
        let out = read_from(&daemon, &session, &task_id, 0).await;
        (out["output"] == "start\n").then_some(out)
    });
    assert_eq!(first["next_byte"], 6);
    std::fs::write(daemon.project().join("go"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(120), turn)
        .await
        .expect("the command ends")
        .unwrap();

    let second = read_from(&daemon, &session, &task_id, 6).await;
    let (from, next) = (
        second["from_byte"].as_u64().unwrap(),
        second["next_byte"].as_u64().unwrap(),
    );
    assert_eq!(next, 6 + 600_000);
    let kept = 256 * 1024;
    assert_eq!(from, next - kept, "the output starts at what is still kept");
    assert_eq!(second["dropped"], from - 6, "{}", second["dropped"]);
    assert_eq!(second["truncated"], true);
    assert_eq!(second["output"].as_str().unwrap().len() as u64, next - from);
}

#[tokio::test]
async fn a_monitor_runs_beside_the_session_and_its_events_reach_the_model() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "Monitor",
        "toolu_watch",
        serde_json::json!({
            "command": "for i in 1 2 3; do echo tick-$i; sleep 0.4; done; sleep 60",
            "description": "ticks",
            "persistent": true
        }),
    ));
    model.push_turn(text_round("watching"));
    model.push_turn(text_round("saw the ticks"));
    let daemon = Daemon::start(model.clone()).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = daemon.watch(&session).await;

    let started = std::time::Instant::now();
    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "watch it"}),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the turn did not wait for the monitor"
    );

    let task_id = eventually!("the monitor's third tick streamed to the host", {
        let frames = frames.lock().unwrap();
        frames
            .iter()
            .find(|f| {
                f["event"]["kind"] == "task_progress"
                    && f["event"]["appended"]
                        .as_str()
                        .is_some_and(|a| a.contains("tick-3"))
            })
            .map(|f| f["event"]["task_id"].clone())
    });
    let tasks = daemon
        .ok("session.tasks", serde_json::json!({"session_id": session}))
        .await;
    let monitor = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"] == task_id)
        .cloned()
        .unwrap();
    assert_eq!(monitor["kind"], "monitor");
    assert_eq!(monitor["status"], "running");
    assert_eq!(monitor["detail"]["description"], "ticks");

    // Past the window lines are batched in, so the last tick has been posted.
    tokio::time::sleep(Duration::from_millis(500)).await;
    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "anything new?"}),
        )
        .await;
    let asked =
        serde_json::to_string(&model.nth_request(2).expect("the second turn's call")).unwrap();
    assert!(
        asked.contains("<kind>monitor</kind>")
            && asked.contains("tick-1\\ntick-2\\ntick-3\\n</events>"),
        "the events opened the next turn: {asked}"
    );

    let stopped = daemon
        .ok(
            "session.stopTask",
            serde_json::json!({"session_id": session, "task_id": task_id}),
        )
        .await;
    assert_eq!(stopped["stopped"], true);
    let finished = eventually!("the monitor's end", {
        frames
            .lock()
            .unwrap()
            .iter()
            .find(|f| {
                f["event"]["kind"] == "task_finished" && f["event"]["task"]["task_id"] == task_id
            })
            .cloned()
    });
    assert_eq!(finished["event"]["task"]["status"], "stopped");
}

#[tokio::test]
async fn a_running_program_is_a_code_task_that_wraps_up_when_stopped() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "RunCode",
        "toolu_code",
        serde_json::json!({
            "description": "count until told to stop",
            "code": "let n = 0;
                while (!stopRequested()) {
                    await tools.Bash({command: 'sleep 0.05'}); n++;
                    log('round ' + n);
                }
                return {rounds: n};"
        }),
    ));
    model.push_turn(text_round("stopped"));
    let daemon = Arc::new(Daemon::start(model).await);

    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let frames = daemon.watch(&session).await;
    let turn = {
        let (daemon, session) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            daemon
                .call(
                    "session.run_turn",
                    serde_json::json!({"session_id": session, "message": "run it"}),
                )
                .await
        })
    };

    let program = eventually!("the program listed as a running code task", {
        let tasks = daemon
            .ok("session.tasks", serde_json::json!({"session_id": session}))
            .await;
        tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["kind"] == "code" && t["status"] == "running")
            .cloned()
    });
    assert_eq!(program["detail"]["description"], "count until told to stop");

    let output = eventually!("its call log as it runs", {
        let out = daemon
            .ok(
                "session.taskOutput",
                serde_json::json!({"session_id": session, "task_id": program["task_id"]}),
            )
            .await;
        out["output"]
            .as_str()
            .filter(|o| o.contains("round 2"))
            .map(str::to_string)
    });
    assert!(output.contains("→ Bash(command: sleep 0.05)"), "{output}");

    daemon
        .ok(
            "session.stopTask",
            serde_json::json!({"session_id": session, "task_id": program["task_id"]}),
        )
        .await;
    let response = tokio::time::timeout(Duration::from_secs(20), turn)
        .await
        .expect("the turn ends once the program wraps up")
        .unwrap();
    assert!(response.get("result").is_some(), "{response}");

    let out = daemon
        .ok(
            "session.taskOutput",
            serde_json::json!({"session_id": session, "task_id": program["task_id"]}),
        )
        .await;
    let output = out["output"].as_str().unwrap();
    assert!(
        output.contains("\"rounds\":"),
        "it returned what it had: {output}"
    );
    assert_eq!(out["status"], "stopped");
    assert_eq!(out["final"], true, "{out}");

    eventually!("task_finished", {
        about(&frames, &program["task_id"])
            .into_iter()
            .find(|f| f["event"]["kind"] == "task_finished")
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let seen = about(&frames, &program["task_id"]);
    let kinds: Vec<&str> = seen
        .iter()
        .map(|f| f["event"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds.last(),
        Some(&"task_finished"),
        "nothing follows task_finished: {kinds:?}"
    );
    let finished = seen.last().unwrap();
    assert_eq!(finished["event"]["task"]["status"], "stopped");
    let last_progress = seen
        .iter()
        .rev()
        .find(|f| f["event"]["kind"] == "task_progress")
        .expect("what the program printed was streamed");
    assert_eq!(
        last_progress["event"]["next_byte"], out["next_byte"],
        "the wrap-up went out before task_finished"
    );
    assert!(
        last_progress["event"]["appended"]
            .as_str()
            .unwrap()
            .contains("\"rounds\":"),
        "{last_progress}"
    );
}

fn ids_of(tasks: &serde_json::Value) -> Vec<serde_json::Value> {
    tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task_id"].clone())
        .collect()
}

#[tokio::test]
async fn a_finished_task_and_its_notice_outlast_a_rebuild() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("the sub-agent's answer"));
    model.push_turn(text_round("noted"));
    let daemon = Daemon::start(model.clone()).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let started = daemon
        .ok(
            "session.startAgent",
            serde_json::json!({"session_id": session, "prompt": "look into it"}),
        )
        .await;
    let task_id = started["task_id"].clone();
    eventually!("the sub-agent finish", {
        let tasks = daemon
            .ok("session.tasks", serde_json::json!({"session_id": session}))
            .await;
        tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["task_id"] == task_id && t["status"] == "done")
            .then_some(())
    });

    daemon.ok("config.reload", serde_json::json!({})).await;
    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "anything new?"}),
        )
        .await;

    let task = task_id.as_str().unwrap();
    let told = (0..model.calls()).any(|i| {
        let request = serde_json::to_string(&model.nth_request(i).unwrap()).unwrap();
        request.contains("<task-notification>")
            && request.contains(task)
            && request.contains("anything new?")
    });
    assert!(
        told,
        "the turn after the rebuild was not told the sub-agent ended"
    );
    let tasks = daemon
        .ok("session.tasks", serde_json::json!({"session_id": session}))
        .await;
    assert!(ids_of(&tasks).contains(&task_id), "{tasks}");
    let out = daemon
        .ok(
            "session.taskOutput",
            serde_json::json!({"session_id": session, "task_id": task_id}),
        )
        .await;
    assert_eq!(out["status"], "done");
    assert_eq!(out["output"], "the sub-agent's answer");
}

/// Answers every call at once except those whose request holds `hold`,
/// which talk forever; counts those given up on.
struct HoldsOne {
    hold: &'static str,
    held: std::sync::atomic::AtomicUsize,
    abandoned: Arc<std::sync::atomic::AtomicUsize>,
}

impl AnthropicClient for HoldsOne {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        let request = serde_json::to_string(&req).unwrap_or_default();
        if !request.contains(self.hold) {
            return Box::pin(futures::stream::iter(text_round("ok").into_iter().map(Ok)));
        }
        self.held.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        NeverFinishes {
            calls: Default::default(),
            abandoned: self.abandoned.clone(),
        }
        .stream_messages(req)
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

#[tokio::test]
async fn a_session_running_background_work_is_not_rebuilt_under_it() {
    let model = Arc::new(HoldsOne {
        hold: "keep thinking",
        held: Default::default(),
        abandoned: Default::default(),
    });
    let daemon = Daemon::start_with(model.clone()).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let task_id = daemon
        .ok(
            "session.startAgent",
            serde_json::json!({"session_id": session, "prompt": "keep thinking"}),
        )
        .await["task_id"]
        .clone();
    eventually!(
        "the sub-agent call the model",
        (model.held.load(std::sync::atomic::Ordering::SeqCst) > 0).then_some(())
    );

    daemon.ok("config.reload", serde_json::json!({})).await;
    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "hello"}),
        )
        .await;
    let refused = daemon
        .call(
            "session.setPrompt",
            serde_json::json!({"session_id": session, "append": "Be brief."}),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32015, "{refused}");
    assert_eq!(
        refused["error"]["data"]["running_task_ids"],
        serde_json::json!([task_id]),
        "{refused}"
    );

    let tasks = daemon
        .ok("session.tasks", serde_json::json!({"session_id": session}))
        .await;
    let task = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"] == task_id)
        .cloned()
        .unwrap_or_else(|| panic!("the sub-agent is gone: {tasks}"));
    assert_eq!(task["status"], "running", "{task}");
    assert_eq!(model.abandoned.load(std::sync::atomic::Ordering::SeqCst), 0);
}
