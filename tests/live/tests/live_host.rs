//! The host's side of a session, with a real model: work the host starts
//! under a session without going through its model, and answers the
//! questions of; a session's tasks as a monitoring view sees them, live, and
//! stops; the instance's lifecycle and session list across an engine restart,
//! with a project reached by two paths; a host scene's own tool in the hands
//! of sub-agents; and the context the next call carries, seen before it is
//! made and edited by the host.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base::interface::scene::{Scene, ScenePromptContext, TokenBudget};
use base::prompt::PromptBlock;
use base::tool::{ProgressSender, Tool, ToolContext, ToolResult, ToolResultContent};
use live::{Host, LiveRun, Mode, Watcher};
use serde_json::{json, Value};

fn nonce() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 0xffff_ffff_ffff
    )
}

/// `session.startAgent`, as the result.
async fn start_agent(run: &mut LiveRun, params: Value) -> anyhow::Result<Value> {
    let started = run.client.call("session.startAgent", params).await?;
    anyhow::ensure!(started.error.is_none(), "{started:?}");
    Ok(started.result.unwrap_or_default())
}

/// The frames `watcher` saw about `task_id`, in order.
fn about(watcher: &Watcher, task_id: &str) -> Vec<Value> {
    watcher
        .frames()
        .into_iter()
        .filter(|f| {
            let e = &f["event"];
            e["task_id"] == task_id || e["task"]["task_id"] == task_id
        })
        .collect()
}

fn kind(frame: &Value) -> &str {
    frame["event"]["kind"].as_str().unwrap_or_default()
}

/// Every `daemon.event` a fresh subscriber is sent from now on.
async fn lifecycle(
    run: &LiveRun,
) -> anyhow::Result<(Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>)> {
    let mut subscriber = run.daemon.connect().await?;
    let subscribed = subscriber
        .call("daemon.subscribeEvents", Value::Null)
        .await?;
    anyhow::ensure!(subscribed.error.is_none(), "{subscribed:?}");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let into = seen.clone();
    let reader = tokio::spawn(async move {
        while let Ok(Some(frame)) = subscriber.next_frame().await {
            if frame["method"] == "daemon.event" {
                into.lock().unwrap().push(frame["params"].clone());
            }
        }
    });
    Ok((seen, reader))
}

fn has_event(seen: &Mutex<Vec<Value>>, kind: &str, session: &str) -> bool {
    seen.lock()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == kind && e["session_id"] == session)
}

fn sh(project: &std::path::Path, script: &str) -> String {
    std::process::Command::new("sh")
        .arg(script)
        .current_dir(project)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_runs_a_fix_beside_the_session_and_answers_its_questions() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "flow",
        json!({ "permission_mode": "default" }),
        Mode::InProcess,
    )
    .await?;
    let ok = format!("CHECK-OK-{}", nonce());
    run.write_file("calc.sh", "add() {\n  echo $(($1 - $2))\n}\n")?;
    let check = format!(". ./calc.sh\n[ \"$(add 2 3)\" = 5 ] && echo {ok} || echo CHECK-FAILED\n");
    run.write_file("check.sh", &check)?;
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;

    let fix = start_agent(
        &mut run,
        json!({
            "session_id": session,
            "prompt": "calc.sh defines add(), and check.sh tests it but reports CHECK-FAILED. \
                       Fix add() in calc.sh without touching check.sh, then run `sh check.sh` \
                       with the Bash tool and answer with what it printed.",
            "description": "fix add",
            "tag": "flow:fix",
            "permission_rules": [
                { "source": "session", "behavior": "deny", "tool_name": "Write", "rule_content": "**/check.sh" },
                { "source": "session", "behavior": "deny", "tool_name": "Edit", "rule_content": "**/check.sh" }
            ]
        }),
    )
    .await?;
    let fix_id = fix["task_id"].as_str().unwrap_or_default().to_string();
    let fix_session = fix["agent_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let fixed = watcher
        .wait_for("the fix to finish", Duration::from_secs(300), |e| {
            e["kind"] == "task_finished" && e["task"]["task_id"] == fix_id.as_str()
        })
        .await?;
    let task = &fixed["event"]["task"];
    assert_eq!(task["status"], "done", "{task}");
    assert_eq!(task["detail"]["tag"], "flow:fix");
    assert!(
        sh(&run.project, "check.sh").contains(&ok),
        "the fix is real: {:?}",
        run.read_file("calc.sh")
    );
    assert_eq!(run.read_file("check.sh").as_deref(), Some(check.as_str()));

    let (calls, input, output) = run.provider.spend();
    assert_eq!(task["usage"]["input_tokens"], input, "the session sat idle");
    assert_eq!(task["usage"]["output_tokens"], output);

    let asked = watcher.asked();
    assert!(!asked.is_empty(), "running a command asks in default mode");
    for question in &asked {
        let prompt_id = &question.prompt["event"]["prompt_id"];
        assert_eq!(question.prompt["turn_id"], Value::Null);
        let message = question.prompt["event"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            message.starts_with(&format!("[sub-agent {fix_session}]")),
            "{message}"
        );
        let listed = question
            .tasks
            .iter()
            .find(|t| t["task_id"] == fix_id.as_str())
            .cloned()
            .unwrap_or_default();
        assert_eq!(listed["status"], "waiting", "{listed}");
        assert!(
            listed["waiting_for"]
                .as_array()
                .is_some_and(|ids| ids.contains(prompt_id)),
            "{listed}"
        );
    }

    let seen = about(&watcher, &fix_id);
    let updates: Vec<&Value> = seen.iter().filter(|f| kind(f) == "task_updated").collect();
    let mut usage_changes = 0;
    let mut last_usage = &json!({});
    for update in &updates {
        let usage = &update["event"]["task"]["usage"];
        if usage != last_usage {
            usage_changes += 1;
            last_usage = usage;
        }
    }
    assert_eq!(usage_changes, calls, "the usage moved once per model call");
    assert_eq!(
        updates.last().map(|f| &f["event"]["task"]["usage"]),
        Some(&task["usage"])
    );
    assert!(updates
        .iter()
        .any(|f| f["event"]["task"]["status"] == "waiting"));

    let progress = watcher.events("subagent_progress");
    assert!(progress
        .iter()
        .all(|f| f["event"]["task_id"] == fix_id.as_str()));
    assert!(
        progress
            .iter()
            .any(|f| f["event"]["event"]["kind"] == "tool_use"
                && f["event"]["event"]["name"] == "Bash"),
        "the host saw which tool the agent was on"
    );

    let mut host = run.daemon.connect().await?;
    let explain = "Has the background fix finished? Then read calc.sh and say in one sentence \
                   what add() returns for 2 and 3.";
    let (turn, review) = tokio::join!(run.turn(&session, explain), async {
        watcher
            .wait_for("the turn to start", Duration::from_secs(60), |e| {
                e["kind"] == "step_start"
            })
            .await?;
        host.call(
            "session.startAgent",
            json!({
                "session_id": session,
                "prompt": "Read calc.sh and check.sh and decide whether add() is now correct. \
                           Hand in your verdict with the StructuredOutput tool.",
                "description": "review add",
                "tag": "flow:review",
                "notify_parent": false,
                "output_schema": {
                    "type": "object",
                    "required": ["verdict", "reason"],
                    "properties": {
                        "verdict": { "type": "string", "enum": ["pass", "fail"] },
                        "reason": { "type": "string" }
                    }
                }
            }),
        )
        .await
    });
    let turn = turn?;
    assert!(turn.text.contains('5'), "{}", turn.text);
    let review = review?;
    assert!(review.error.is_none(), "{review:?}");
    let review_id = review.result.unwrap_or_default()["task_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let order: Vec<String> = watcher
        .frames()
        .iter()
        .filter_map(|f| match kind(f) {
            "task_started" if f["event"]["task"]["task_id"] == review_id.as_str() => {
                Some("review".to_string())
            }
            "turn_complete" => Some("turn".to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        order.first().map(String::as_str),
        Some("review"),
        "started while the session's turn ran: {order:?}"
    );

    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(300))
        .await?;
    let reviewed = tasks
        .iter()
        .find(|t| t["task_id"] == review_id.as_str())
        .cloned()
        .unwrap_or_default();
    assert_eq!(reviewed["status"], "done", "{reviewed}");
    assert_eq!(
        reviewed["structured_output"]["verdict"], "pass",
        "{reviewed}"
    );

    run.turn(
        &session,
        "Did any other background work report since? Reply in one line.",
    )
    .await?;
    let calls = run.provider.calls();
    let told = calls
        .iter()
        .find(|c| {
            c.messages_text()
                .contains("Has the background fix finished")
        })
        .map(|c| c.messages_text())
        .unwrap_or_default();
    assert!(
        told.contains("<task-notification>") && told.contains(&fix_id),
        "the fix reached the idle session as a notification"
    );
    let last = calls.last().map(|c| c.messages_text()).unwrap_or_default();
    assert!(
        !last.contains(&review_id),
        "notify_parent: false kept the review out of the session"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_monitoring_view_follows_a_build_and_the_list_survives_a_restart() -> anyhow::Result<()> {
    let mut run = LiveRun::start("build", json!({}), Mode::InProcess).await?;
    let done = format!("BUILD-OK-{}", nonce());
    run.write_file(
        "build.sh",
        &format!(
            "for step in 1 2 3 4 5; do\n  echo \"step $step/5: compiling module $step\"\n  \
             sleep 0.7\ndone\nseq 1 600 | sed 's/^/  linking object /'\nsleep 0.7\necho {done}\n"
        ),
    )?;
    let alias = run.world.root().join("build-alias");
    std::os::unix::fs::symlink(&run.project, &alias)?;
    let canonical = run.project.canonicalize()?;

    let (instance, reader) = lifecycle(&run).await?;
    let by_alias = run
        .session_with(json!({ "project_root": alias.to_string_lossy() }))
        .await?;
    let watcher = run.watch(&by_alias, true).await?;
    let built = run
        .turn(
            &by_alias,
            "Run exactly `sh build.sh` with the Bash tool — no pipes, no other command, it \
             takes a few seconds — and reply with the last line it printed, nothing else.",
        )
        .await?;
    assert!(built.text.contains(&done), "{}", built.text);

    let finished = watcher
        .wait_for("the build to finish", Duration::from_secs(30), |e| {
            e["kind"] == "task_finished"
                && e["task"]["kind"] == "shell"
                && e["task"]["detail"]["command"]
                    .as_str()
                    .is_some_and(|c| c.contains("build.sh"))
        })
        .await?;
    let build_id = finished["event"]["task"]["task_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let seen = about(&watcher, &build_id);
    assert_eq!(seen.first().map(kind), Some("task_started"));
    assert_eq!(seen.last().map(kind), Some("task_finished"));
    assert!(seen.iter().all(|f| f["turn_id"].is_null()));
    let progress: Vec<&Value> = seen.iter().filter(|f| kind(f) == "task_progress").collect();
    assert!(
        progress.len() >= 3,
        "a five-second build streams as it goes: `{}` sent {progress:?}",
        finished["event"]["task"]["detail"]["command"]
    );
    let appended = |f: &Value| {
        f["event"]["appended"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    assert!(
        appended(progress[0]).starts_with("step 1/5"),
        "{}",
        appended(progress[0])
    );
    assert!(appended(progress[progress.len() - 1]).ends_with(&format!("{done}\n")));
    let (shown, skipped) = progress.iter().fold((0, 0), |(shown, skipped), f| {
        let len = appended(f).len();
        assert!(len <= 8 * 1024, "a frame carries at most 8 KiB");
        (
            shown + len,
            skipped + f["event"]["skipped_bytes"].as_u64().unwrap_or(0) as usize,
        )
    });
    assert!(skipped > 0, "the link burst was more than a frame");
    let whole = run
        .client
        .call(
            "session.taskOutput",
            json!({ "session_id": by_alias, "task_id": build_id, "tail_bytes": 1_000_000 }),
        )
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(
        whole["output"].as_str().map(str::len),
        Some(shown + skipped),
        "every byte was either streamed or counted"
    );
    drop(watcher);

    let by_path = run
        .session_with(json!({ "project_root": canonical.to_string_lossy() }))
        .await?;
    run.turn(&by_path, "Reply with the single word READY.")
        .await?;
    let listed_once = |list: Value| {
        for id in [&by_alias, &by_path] {
            let n = list["sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["session_id"] == id.as_str())
                .count();
            assert_eq!(n, 1, "{id} listed {n} times: {list}");
        }
    };
    listed_once(session_list(&mut run).await?);
    for id in [&by_alias, &by_path] {
        assert!(has_event(&instance, "session_created", id));
    }
    let turns: Vec<Value> = instance
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "turn_started" || e["kind"] == "turn_completed")
        .map(|e| json!([e["kind"], e["turn_id"], e["initiator"]]))
        .collect();
    assert_eq!(
        turns,
        [
            json!(["turn_started", "build-t1", "client"]),
            json!(["turn_completed", "build-t1", "client"]),
            json!(["turn_started", "build-t2", "client"]),
            json!(["turn_completed", "build-t2", "client"]),
        ],
        "the instance view sees every turn without subscribing to a session"
    );
    reader.abort();

    run.restart().await?;
    let (instance, reader) = lifecycle(&run).await?;
    listed_once(session_list(&mut run).await?);
    let recalled = run
        .turn(
            &by_alias,
            "What was the last line build.sh printed earlier? Reply with it only, without \
             running anything.",
        )
        .await?;
    assert!(recalled.text.contains(&done), "{}", recalled.text);
    assert!(
        has_event(&instance, "session_resumed", &by_alias),
        "the turn brought the session back from disk"
    );
    assert!(!has_event(&instance, "session_resumed", &by_path));
    listed_once(session_list(&mut run).await?);
    reader.abort();
    run.finish().await;
    Ok(())
}

async fn session_list(run: &mut LiveRun) -> anyhow::Result<Value> {
    Ok(run
        .client
        .call("session.list", json!({}))
        .await?
        .result
        .unwrap_or_default())
}

/// A production service's status, as an on-call host's own tool reports it.
/// Notes which session asked, so a case can tell a sub-agent's call from the
/// session's own.
struct ServiceStatus {
    incident: String,
    asked_by: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl Tool for ServiceStatus {
    fn name(&self) -> &str {
        "ServiceStatus"
    }
    fn description(&self) -> &str {
        "The current status of one production service, with its open incident if any."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["service"],
            "properties": { "service": { "type": "string", "description": "The service's name" } }
        })
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        let service = input["service"].as_str().unwrap_or_default().to_string();
        self.asked_by
            .lock()
            .unwrap()
            .push((ctx.session_id.clone(), service.clone()));
        Ok(ToolResult {
            content: ToolResultContent::Text(format!(
                "{service}: degraded since 09:14, incident {}-{service}",
                self.incident
            )),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }
}

/// The scene an on-call host serves: the code scene, plus the host's own
/// `ServiceStatus` — a new instance each time a session is built, as a host
/// scene hands them out.
struct OnCallScene {
    incident: String,
    asked_by: Arc<Mutex<Vec<(String, String)>>>,
}

impl Scene for OnCallScene {
    fn id(&self) -> &str {
        "oncall"
    }
    fn name(&self) -> &str {
        "On-call"
    }
    fn description(&self) -> &str {
        "Production on-call, with the host's service status tool"
    }
    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        scene::code::CodeScene::default().build_system_prompt(ctx)
    }
    fn tools(&self) -> Vec<String> {
        Vec::new()
    }
    fn token_budget(&self) -> TokenBudget {
        scene::code::CodeScene::default().token_budget()
    }
    fn extra_tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(ServiceStatus {
            incident: self.incident.clone(),
            asked_by: self.asked_by.clone(),
        })]
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sub_agents_in_a_host_scene_use_the_scenes_own_tool() -> anyhow::Result<()> {
    let incident = format!("INC-{}", nonce());
    let asked_by = Arc::new(Mutex::new(Vec::new()));
    let scene = Arc::new(OnCallScene {
        incident: incident.clone(),
        asked_by: asked_by.clone(),
    });
    let mut run = LiveRun::start_with_host(
        "oncall",
        json!({}),
        Host {
            scenes: vec![scene],
            ..Default::default()
        },
    )
    .await?;
    let session = run.session_with(json!({ "scene": "oncall" })).await?;

    let answer = run
        .turn(
            &session,
            "Delegate, and do not call ServiceStatus yourself. In a single message, call the \
             Agent tool twice: one sub-agent in the foreground with prompt \"Check the payments \
             service with the ServiceStatus tool and report its incident code\", and one with \
             background true and prompt \"Check the search service with the ServiceStatus tool \
             and report its incident code\". Then reply with the payments incident code.",
        )
        .await?;
    assert!(
        answer.text.contains(&format!("{incident}-payments")),
        "{}",
        answer.text
    );
    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(300))
        .await?;
    let agents: Vec<&Value> = tasks.iter().filter(|t| t["kind"] == "agent").collect();
    assert!(
        agents.len() >= 2 && agents.iter().all(|t| t["status"] == "done"),
        "both sub-agents were built in the host scene and ran: {tasks:?}"
    );
    let sidechains: Vec<&str> = agents
        .iter()
        .filter_map(|t| t["detail"]["agent_session_id"].as_str())
        .collect();
    let asked = asked_by.lock().unwrap().clone();
    for service in ["payments", "search"] {
        assert!(
            asked
                .iter()
                .any(|(by, s)| s == service && sidechains.contains(&by.as_str())),
            "a sub-agent called the scene's tool for {service}: {asked:?}"
        );
    }
    assert!(agents.iter().any(|t| t["detail"]["background"] == true));
    assert!(agents.iter().any(|t| t["detail"]["background"] == false));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_stops_a_runaway_command_and_the_turn_carries_on() -> anyhow::Result<()> {
    let mut run = LiveRun::start("runaway", json!({}), Mode::InProcess).await?;
    run.write_file(
        "serve.sh",
        "echo $$ > serve.pid\nwhile true; do echo \"heartbeat $(date +%s)\"; sleep 0.5; done\n",
    )?;
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    let mut host = run.daemon.connect().await?;

    let (turn, stopped) = tokio::join!(
        run.turn(
            &session,
            "Start the dev server with `sh serve.sh` using the Bash tool, in the foreground. \
             Then tell me in one line how it went.",
        ),
        async {
            let beat = watcher
                .wait_for("the server's heartbeat", Duration::from_secs(120), |e| {
                    e["kind"] == "task_progress"
                        && e["appended"]
                            .as_str()
                            .is_some_and(|a| a.contains("heartbeat"))
                })
                .await?;
            let task_id = beat["event"]["task_id"].clone();
            let stopped = host
                .call(
                    "session.stopTask",
                    json!({ "session_id": session, "task_id": task_id }),
                )
                .await?;
            anyhow::Ok((task_id, stopped))
        }
    );
    let (task_id, stopped) = stopped?;
    assert_eq!(stopped.result.unwrap_or_default()["stopped"], true);
    let turn = turn?;
    assert!(
        turn.turn_complete,
        "the turn went on after the command was stopped"
    );
    assert!(turn.tool_results.iter().any(|(name, _, _)| name == "Bash"));

    let ended = watcher
        .wait_for("the server to end", Duration::from_secs(10), |e| {
            e["kind"] == "task_finished" && e["task"]["task_id"] == task_id
        })
        .await?;
    assert_eq!(ended["event"]["task"]["status"], "stopped");
    let pid = run.read_file("serve.pid").unwrap_or_default();
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(!alive, "the server's process is gone with its task");
    let again = run
        .client
        .call(
            "session.stopTask",
            json!({ "session_id": session, "task_id": task_id }),
        )
        .await?;
    assert_eq!(again.result.unwrap_or_default()["stopped"], false);
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// The messages of `session`'s transcript, as `session.history` projects them.
async fn history(run: &mut LiveRun, session: &str) -> anyhow::Result<Vec<Value>> {
    let page = run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "limit": 500 }),
        )
        .await?
        .result
        .unwrap_or_default();
    Ok(page["messages"].as_array().cloned().unwrap_or_default())
}

fn tool_names(tools: &Value) -> Vec<String> {
    let mut names: Vec<String> = tools
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

/// What a request carries on the user's side — what was said and what tools
/// answered — leaving out the model's own earlier replies, which a host's
/// edits do not touch.
fn user_side(request: &live::SeenRequest) -> String {
    request.body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_edits_what_the_session_carries_and_sees_the_next_call_first() -> anyhow::Result<()>
{
    // Without thinking, so the model's own earlier reasoning — which a host's
    // edits cannot reach — does not carry the stale codename forward.
    let mut run = LiveRun::start(
        "context",
        json!({ "model": { "thinking_mode": "off" } }),
        Mode::InProcess,
    )
    .await?;
    let n = nonce();
    let (stale, corrected, key) = (
        format!("MAPLE-{n}"),
        format!("CEDAR-{n}"),
        format!("CACHE-{n}"),
    );
    run.write_file("notes.txt", &format!("build cache key: {key}\n"))?;
    let session = run.session().await?;
    run.turn(
        &session,
        &format!("Our release codename is {stale}. Reply with OK only."),
    )
    .await?;
    let mut read = false;
    for _ in 0..2 {
        let turn = run
            .turn(
                &session,
                "Read notes.txt with the Read tool and reply with OK only.",
            )
            .await?;
        read = turn.tool_uses.iter().any(|(name, _)| name == "Read");
        if read {
            break;
        }
    }
    anyhow::ensure!(read, "the model never read notes.txt");
    std::fs::remove_file(run.project.join("notes.txt"))?;

    let messages = history(&mut run, &session).await?;
    let total = messages.len();
    let seq_of = |needle: &str, origin: &str| {
        messages
            .iter()
            .find(|m| m["origin"] == origin && m["content"].to_string().contains(needle))
            .map(|m| m["seq"].clone())
    };
    let said = seq_of(&stale, "user").expect("the codename message");
    let read = seq_of(&key, "tool_result").expect("the file's content");
    let edited = run
        .client
        .call(
            "session.editMessage",
            json!({ "session_id": session, "at": said, "total": total,
                    "text": format!("Our release codename is {corrected}. Reply with OK only.") }),
        )
        .await?;
    assert!(edited.error.is_none(), "{edited:?}");
    let dropped = run
        .client
        .call(
            "session.dropMessages",
            json!({ "session_id": session, "at": [read], "total": total }),
        )
        .await?;
    assert!(dropped.error.is_none(), "{dropped:?}");
    let card = run
        .client
        .call(
            "session.appendEntry",
            json!({ "session_id": session, "ns": "com.example.cards", "event": "codename_changed",
                    "payload": { "from": stale, "to": corrected } }),
        )
        .await?;
    assert!(card.error.is_none(), "{card:?}");

    let inspected = run
        .client
        .call("session.inspectContext", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    let asked_before = run.provider.calls().len();
    let answer = run
        .turn(
            &session,
            "Without reading any file or running anything: what is our release codename, and \
             what build cache key was in notes.txt? Write UNKNOWN for whichever you do not know.",
        )
        .await?;
    let next = run.provider.calls()[asked_before].clone();
    let sent = user_side(&next);
    assert!(sent.contains(&corrected));
    assert!(sent.contains("[removed by the host]"));
    for gone in [stale.as_str(), key.as_str(), "codename_changed"] {
        let at = sent.find(gone).unwrap_or(0);
        assert!(
            !sent.contains(gone),
            "{gone} reached the model: …{}…",
            &sent[at.saturating_sub(300)..(at + 100).min(sent.len())]
        );
    }
    assert!(answer.text.contains(&corrected), "{}", answer.text);
    assert_eq!(inspected["model"], next.model(), "{inspected}");
    assert_eq!(
        tool_names(&inspected["tools"]),
        tool_names(&next.body["tools"]),
        "inspectContext named the tools the call carried"
    );

    let entries = run
        .client
        .call("session.entries", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert!(
        entries.to_string().contains("codename_changed"),
        "{entries}"
    );
    let after = history(&mut run, &session).await?;
    let at = said.as_u64().unwrap_or_default() as usize;
    assert!(after[at]["content"].to_string().contains(&corrected));
    run.finish().await;
    Ok(())
}
