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
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
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
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(model: Arc<MockAnthropicClient>) -> Self {
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
        config.settings.memory_enabled = false;
        config.settings.permission_mode =
            base::interface::settings::PermissionMode::BypassPermissions;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::CodeScene),
            daemon::Assembly {
                cwd: Some(cwd),
                model_client: Some(model as Arc<dyn AnthropicClient>),
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
        Self { sock, _dir: dir }
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
        for _ in 0..200 {
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
        serde_json::json!({"command": "sleep 30"}),
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
    assert_eq!(shell["detail"]["command"], "sleep 30");

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

    let response = daemon
        .call(
            "session.taskOutput",
            serde_json::json!({"session_id": session, "task_id": "sh-nope", "tail_bytes": 0}),
        )
        .await;
    assert_eq!(response["error"]["code"], -32602, "{response}");
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
}
