//! Work the model schedules and paces for itself, with a real model: a
//! `TodoWrite` list kept through a multi-step task, a job it schedules with
//! `CronCreate` firing as a turn of its own until `CronDelete`, the host's
//! `schedule.*` doing the same with nobody connected, a goal ended by its
//! round limit and disarmed by a token budget, `Sleep` held to what was asked
//! and to its cap, background sub-agents driven with `TaskMessage` /
//! `TaskWait` / `TaskOutput` / `TaskStop`, and `Ping` and `PushNotification`
//! from a declarative scene that offers them.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use live::{LiveRun, Page, PageServer};
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

fn agent_type(description: &str, model: &str, body: &str) -> String {
    format!("---\ndescription: {description}\nmodel: {model}\n---\n{body}")
}

/// The input of every call the turn made to `tool`, in order.
fn inputs_of(events: &rpc_client::TurnEvents, tool: &str) -> Vec<Value> {
    events
        .tool_uses
        .iter()
        .filter(|(n, _)| n == tool)
        .map(|(_, input)| input.clone())
        .collect()
}

fn used(events: &rpc_client::TurnEvents, tool: &str) -> bool {
    events.tool_uses.iter().any(|(n, _)| n == tool)
}

fn lines(run: &LiveRun, file: &str) -> usize {
    run.read_file(file)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// Every `daemon.event` from now on.
async fn daemon_events(
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

fn scheduled_turns(seen: &Mutex<Vec<Value>>, session: &str) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|e| {
            e["kind"] == "turn_completed"
                && e["session_id"] == session
                && e["initiator"] == "schedule"
        })
        .count()
}

async fn jobs(run: &mut LiveRun, session: &str) -> anyhow::Result<Vec<Value>> {
    let resp = run
        .client
        .call("schedule.list", json!({ "session_id": session }))
        .await?;
    Ok(resp.result.unwrap_or_default()["jobs"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// Wait until `file` has a line, or `timeout` passes: the scheduler checks
/// at the top of every minute.
async fn wait_for_lines(run: &LiveRun, file: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while lines(run, file) == 0 {
        anyhow::ensure!(
            Instant::now() < deadline,
            "{file} still empty after {timeout:?}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}

/// Sleep past the next two minute boundaries the scheduler checks at.
async fn past_two_checks() {
    let into_minute = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() % 60)
        .unwrap_or(0);
    tokio::time::sleep(Duration::from_secs(120 - into_minute + 20)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_todo_list_is_kept_through_a_multi_step_task() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("todos").start().await?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "Work through this with the TodoWrite tool (load it with ToolSearch if needed):\n\
             1. Call TodoWrite once with three todos, all pending: \"write a.txt\", \"write \
             b.txt\", \"write c.txt\".\n\
             2. For a.txt and then b.txt, one at a time: call TodoWrite marking that todo \
             in_progress, write the file with the Write tool containing just its letter, then \
             call TodoWrite marking it completed.\n\
             3. Leave the c.txt todo pending and do not write c.txt. Reply DONE.",
        )
        .await?;
    assert_eq!(
        run.read_file("a.txt").map(|s| s.trim().to_string()),
        Some("a".into())
    );
    assert_eq!(
        run.read_file("b.txt").map(|s| s.trim().to_string()),
        Some("b".into())
    );
    assert!(run.read_file("c.txt").is_none());

    let writes = inputs_of(&events, "TodoWrite");
    assert!(writes.len() >= 3, "{writes:?}");
    let statuses: Vec<Vec<String>> = writes
        .iter()
        .map(|w| {
            w["todos"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| t["status"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .collect();
    eprintln!("[live todos] {statuses:?}");
    assert!(
        statuses
            .first()
            .is_some_and(|s| s.len() == 3 && s.iter().all(|x| x == "pending")),
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .any(|s| s.iter().any(|x| x == "in_progress")),
        "{statuses:?}"
    );

    let archive = run.world.root().join("todos-export");
    let exported = run
        .client
        .call(
            "session.export",
            json!({ "session_id": session, "path": archive.to_string_lossy(), "format": "dir" }),
        )
        .await?;
    assert!(exported.error.is_none(), "{exported:?}");
    let runtime: Value = serde_json::from_str(&std::fs::read_to_string(
        archive.join("sessions").join(&session).join("runtime.json"),
    )?)?;
    let todos = runtime["todos"].as_array().cloned().unwrap_or_default();
    eprintln!("[live todos] runtime: {runtime}");
    assert_eq!(todos.len(), 3, "the session holds the list: {runtime}");
    let pending: Vec<&Value> = todos.iter().filter(|t| t["status"] == "pending").collect();
    assert_eq!(pending.len(), 1, "{runtime}");
    assert!(pending[0]["content"]
        .as_str()
        .unwrap_or_default()
        .contains('c'));
    assert_eq!(
        todos.iter().filter(|t| t["status"] == "completed").count(),
        2,
        "{runtime}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_the_model_schedules_fires_as_a_turn_until_it_deletes_it() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("model-cron").start().await?;
    let session = run.session().await?;
    let (seen, reader) = daemon_events(&run).await?;
    let events = run
        .turn(
            &session,
            "Use the CronCreate tool (load it with ToolSearch if needed) with cron \"* * * * *\", \
             recurring true, and this prompt: \"Run `echo tick >> ticks.log` with the Bash \
             tool, then reply TICKED.\" Then call CronList and reply with the job's id. Do not \
             run the echo yourself.",
        )
        .await?;
    assert!(used(&events, "CronCreate"), "{:?}", events.tool_uses);
    assert!(used(&events, "CronList"), "{:?}", events.tool_uses);
    let scheduled = jobs(&mut run, &session).await?;
    assert_eq!(scheduled.len(), 1, "{scheduled:?}");
    assert_eq!(scheduled[0]["cron"], "* * * * *");
    let tasks = run.tasks(&session).await?;
    assert!(
        tasks
            .iter()
            .any(|t| t["kind"] == "cron" && t["status"] == "waiting"),
        "{tasks:?}"
    );
    assert_eq!(lines(&run, "ticks.log"), 0);

    wait_for_lines(&run, "ticks.log", Duration::from_secs(150)).await?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while scheduled_turns(&seen, &session) == 0 {
        anyhow::ensure!(
            Instant::now() < deadline,
            "no turn_completed with initiator schedule"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let events = run
        .turn(
            &session,
            "Find the scheduled job with CronList and delete it with CronDelete, then reply \
             DONE.",
        )
        .await?;
    assert!(used(&events, "CronDelete"), "{:?}", events.tool_uses);
    assert!(jobs(&mut run, &session).await?.is_empty());
    run.wait_turn_state(&session, "idle", Duration::from_secs(120))
        .await?;
    let (ticks, fired) = (lines(&run, "ticks.log"), scheduled_turns(&seen, &session));
    past_two_checks().await;
    assert_eq!(lines(&run, "ticks.log"), ticks, "a deleted job fired again");
    assert_eq!(scheduled_turns(&seen, &session), fired);
    reader.abort();
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_job_the_host_schedules_runs_once_with_nobody_connected() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("schedule").start().await?;
    let session = run.session().await?;
    let bad = run
        .client
        .call(
            "schedule.create",
            json!({ "session_id": session, "cron": "every minute", "prompt": "x" }),
        )
        .await?;
    assert!(bad.error.is_some(), "{bad:?}");

    let created = run
        .client
        .call(
            "schedule.create",
            json!({
                "session_id": session,
                "cron": "* * * * *",
                "prompt": "Run `echo host >> host.log` with the Bash tool, then reply DONE.",
                "recurring": false,
                "durable": false
            }),
        )
        .await?;
    assert!(created.error.is_none(), "{created:?}");
    let id = created.result.unwrap_or_default()["job"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let (seen, reader) = daemon_events(&run).await?;
    wait_for_lines(&run, "host.log", Duration::from_secs(150)).await?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while scheduled_turns(&seen, &session) == 0 {
        anyhow::ensure!(
            Instant::now() < deadline,
            "no turn_completed with initiator schedule"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        jobs(&mut run, &session).await?.is_empty(),
        "a one-shot job is gone once run"
    );
    let deleted = run
        .client
        .call("schedule.delete", json!({ "id": id }))
        .await?;
    assert_eq!(deleted.result.unwrap_or_default()["deleted"], json!(false));

    past_two_checks().await;
    assert_eq!(lines(&run, "host.log"), 1, "a one-shot job ran once");
    assert_eq!(scheduled_turns(&seen, &session), 1);
    reader.abort();
    run.finish().await;
    Ok(())
}

async fn settled_goal(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let mut goal = Value::Null;
    for _ in 0..600 {
        let resp = run
            .client
            .call("session.goal", json!({ "session_id": session }))
            .await?;
        goal = resp.result.unwrap_or_default()["goal"].clone();
        if goal["phase"] != "active" || goal["armed"] == false {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("[live goal] settled: {goal}");
    Ok(goal)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_goal_out_of_rounds_ends_blocked_and_starts_no_more() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("goal-rounds").start().await?;
    run.write_file("rounds.txt", "")?;
    let session = run.session().await?;
    let set = run
        .client
        .call(
            "session.setGoal",
            json!({
                "session_id": session,
                "objective": "rounds.txt holds ten lines. Each round: append exactly one line \
                              `round` to rounds.txt with Bash `echo round >> rounds.txt`, then \
                              end the round. Never mark the goal complete or blocked yourself.",
                "max_rounds": 2
            }),
        )
        .await?;
    assert!(set.error.is_none(), "{set:?}");
    let goal = settled_goal(&mut run, &session).await?;
    assert_eq!(goal["phase"], "blocked", "{goal}");
    assert_eq!(goal["rounds_started"], 2, "{goal}");
    assert!(
        goal["blocked_reason"]
            .as_str()
            .is_some_and(|r| r.contains("round limit")),
        "{goal}"
    );
    run.wait_turn_state(&session, "idle", Duration::from_secs(60))
        .await?;
    let calls = run.provider.calls().len();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(run.provider.calls().len(), calls, "no round after the last");
    assert_eq!(lines(&run, "rounds.txt"), 2, "one line per round");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_goal_round_that_runs_out_of_token_budget_disarms_the_goal() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("goal-budget")
        .settings(json!({ "execution": { "max_budget_tokens": 1000 } }))
        .start()
        .await?;
    run.write_file("tally.txt", "")?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let set = run
        .client
        .call(
            "session.setGoal",
            json!({
                "session_id": session,
                "objective": "tally.txt holds the lines 1, 2 and 3. Each round append the next \
                              number with Bash `echo N >> tally.txt` and end the round.",
                "max_rounds": 6
            }),
        )
        .await?;
    assert!(set.error.is_none(), "{set:?}");
    let goal = settled_goal(&mut run, &session).await?;
    assert_eq!(goal["phase"], "active", "{goal}");
    assert_eq!(
        goal["armed"], false,
        "a round out of budget leaves it for a person: {goal}"
    );
    assert_eq!(goal["rounds_started"], 1, "{goal}");
    let ended = watcher
        .wait_for("turn_complete", Duration::from_secs(30), |e| {
            e["kind"] == "turn_complete"
        })
        .await?;
    assert_eq!(ended["event"]["stop_reason"], "budget_exceeded", "{ended}");
    let calls = run.provider.calls().len();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        run.provider.calls().len(),
        calls,
        "a disarmed goal starts no round"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn background_agents_are_steered_waited_on_read_and_stopped() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("steer").start().await?;
    let word = format!("LANTERN-{}", nonce());
    let model = run.config().model_at(0).to_string();
    run.write_file(
        ".agix/agents/listener.md",
        &agent_type(
            "Waits for a message from its coordinator",
            &model,
            "Wait for a message from your coordinator: run `sleep 1` with Bash, again and \
             again, up to 200 times, until a message arrives. The message contains a word in \
             capitals with a dash and digits. Write exactly that word to word.txt with the \
             Write tool, then answer with the word and stop.",
        ),
    )?;
    run.write_file(
        ".agix/agents/spinner.md",
        &agent_type(
            "Keeps busy until stopped",
            &model,
            "Run `sleep 1` with Bash, again and again, up to 300 times. Do nothing else and \
             never stop on your own.",
        ),
    )?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Do these steps in order (load any tool you lack with ToolSearch):\n\
                 1. With the Agent tool start two background sub-agents (background true): \
                 subagent_type listener, description listener, prompt \"Wait for your \
                 coordinator.\"; and subagent_type spinner, description spinner, prompt \"Keep \
                 busy.\". Tell neither of them anything else.\n\
                 2. Call TaskMessage on the listener's task with message \"The word is {word}.\"\n\
                 3. Call TaskWait on the listener's task until it has finished, then call \
                 TaskOutput on it.\n\
                 4. Call TaskStop on the spinner's task.\n\
                 5. Reply with the word the listener reported."
            ),
        )
        .await?;
    for tool in ["TaskMessage", "TaskWait", "TaskOutput", "TaskStop"] {
        assert!(used(&events, tool), "{tool}: {:?}", events.tool_uses);
    }
    assert_eq!(
        run.read_file("word.txt").map(|s| s.trim().to_string()),
        Some(word.clone()),
        "the word reached the listener only by TaskMessage"
    );
    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(120))
        .await?;
    let status_of = |agent: &str| {
        tasks
            .iter()
            .find(|t| t["detail"]["agent_type"] == agent)
            .map(|t| t["status"].clone())
            .unwrap_or_default()
    };
    assert_eq!(status_of("listener"), "done", "{tasks:?}");
    assert_eq!(status_of("spinner"), "stopped", "{tasks:?}");
    let spawns = inputs_of(&events, "Agent");
    assert!(
        spawns
            .iter()
            .all(|input| !input.to_string().contains(&word)),
        "the listener was started without the word: {spawns:?}"
    );
    assert!(
        inputs_of(&events, "TaskMessage")
            .iter()
            .any(|input| input.to_string().contains(&word)),
        "{:?}",
        events.tool_uses
    );
    assert!(events.text.contains(&word), "{}", events.text);
    run.finish().await;
    Ok(())
}

const PACED: &[&str] = &[
    "Read",
    "Write",
    "Bash",
    "Sleep",
    "Ping",
    "PushNotification",
    "ToolSearch",
];

fn paced_scene(world: &live::World) -> anyhow::Result<()> {
    world.write_declarative_scene(
        "paced",
        PACED,
        "You are an operator's assistant. Do exactly the steps you are given, with the tools \
         they name.",
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sleep_waits_as_asked_refuses_past_its_cap_and_ends_on_an_interrupt() -> anyhow::Result<()>
{
    let mut run = LiveRun::builder("sleep")
        .before_start(|world, _| paced_scene(world))
        .start()
        .await?;
    let session = run.scene_session("paced", json!({})).await?;
    let events = run
        .turn(
            &session,
            "Call the Sleep tool with duration_ms 3000 and wait for its result. Then call Sleep \
             with duration_ms 45000. If that is refused, do not retry. Reply DONE.",
        )
        .await?;
    let sleeps: Vec<&(String, String, bool)> = events
        .tool_results
        .iter()
        .filter(|(n, _, _)| n == "Sleep")
        .collect();
    assert!(sleeps.len() >= 2, "{:?}", events.tool_results);
    assert!(!sleeps[0].2, "{:?}", sleeps[0]);
    assert!(
        sleeps[1].2 && sleeps[1].1.contains("30000"),
        "{:?}",
        sleeps[1]
    );
    let exchanges = run.provider.exchanges();
    let slept = exchanges
        .windows(2)
        .find(|w| w[1].request.messages_contain("Slept 3000 ms"))
        .map(|w| w[1].at.saturating_sub(w[0].at + w[0].elapsed));
    eprintln!("[live sleep] gap around the sleep: {slept:?}");
    assert!(
        slept.is_some_and(|gap| gap >= Duration::from_millis(2900)),
        "{slept:?}"
    );

    let watcher = run.watch(&session, false).await?;
    let handle = run
        .spawn_turn(
            &session,
            "Call the Sleep tool with duration_ms 30000, then reply DONE.",
            json!({}),
        )
        .await?;
    watcher
        .wait_for("a Sleep call", Duration::from_secs(120), |e| {
            e["kind"] == "tool_use" && e["name"] == "Sleep"
        })
        .await?;
    let interrupted_at = Instant::now();
    run.client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    tokio::time::timeout(Duration::from_secs(20), handle).await???;
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(8),
        "the turn sat out the sleep: {:?}",
        interrupted_at.elapsed()
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_reaches_a_page_and_reports_its_status() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route("/health", Page::text("ok"));
    pages.route("/gone", Page::status(404, "gone"));
    let mut run = LiveRun::builder("ping")
        .before_start(|world, _| paced_scene(world))
        .start()
        .await?;
    let session = run.scene_session("paced", json!({})).await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Call the Ping tool on {} and then on {}, one at a time. Reply with the HTTP \
                 status each one reported.",
                pages.url("/health"),
                pages.url("/gone")
            ),
        )
        .await?;
    let pings: Vec<&(String, String, bool)> = events
        .tool_results
        .iter()
        .filter(|(n, _, _)| n == "Ping")
        .collect();
    assert!(pings.len() >= 2, "{:?}", events.tool_results);
    assert!(pages.hits("/health") >= 1 && pages.hits("/gone") >= 1);
    assert!(
        pings.iter().any(|(_, t, _)| t.contains("HTTP 200")),
        "{pings:?}"
    );
    assert!(
        pings.iter().any(|(_, t, _)| t.contains("HTTP 404")),
        "{pings:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: PushNotification only runs osascript (or prints to stderr) on the daemon's machine — no frame and no Notification hook reaches the host (tools/src/push_notification.rs:93; the hook fires only for permission prompts, runtime/src/turn.rs:3928)"]
async fn a_push_notification_reaches_the_host() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let log = scratch.path().join("notified.jsonl");
    let hook = scratch.path().join("notify.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat >> '{}'\necho >> '{}'\necho '{{\"continue\": true}}'\n",
            log.display(),
            log.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    let mut run = LiveRun::builder("notify")
        .settings(json!({
            "hooks_config": {
                "Notification": [{ "type": "command", "command": hook, "timeout": 5000 }]
            }
        }))
        .before_start(|world, _| paced_scene(world))
        .start()
        .await?;
    let session = run.scene_session("paced", json!({})).await?;
    let token = format!("BUILD-{}", nonce());
    let watcher = run.watch(&session, false).await?;
    let events = run
        .turn(
            &session,
            &format!("Call PushNotification with message \"{token} finished\". Reply DONE."),
        )
        .await?;
    assert!(used(&events, "PushNotification"), "{:?}", events.tool_uses);
    let frames = watcher.frames();
    let in_a_frame = frames.iter().any(|f| {
        f["event"]["kind"] != "tool_use"
            && f["event"]["kind"] != "tool_args_delta"
            && f["event"]["kind"] != "tool_result"
            && f["event"]["kind"] != "text_delta"
            && f.to_string().contains(&token)
    });
    let hooked = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        in_a_frame || hooked.contains(&token),
        "the notification never left the daemon: hook log {hooked:?}"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}
