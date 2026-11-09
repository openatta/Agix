//! A session's life with a real model, as a host drives it: interrupted in
//! the middle of a command and carried on, interrupted while a background
//! sub-agent keeps running, a second message refused, steered in and queued,
//! its model switched between turns, cleared, compacted and renamed over
//! RPC, exported and deleted with its sidechain, its goal paused, edited and
//! cleared, resumed across a daemon restart that took a background task
//! down, given attachments, a 200 KB message, Unicode everywhere, and an
//! empty message.

use std::path::Path;
use std::time::{Duration, Instant};

use live::{LiveRun, Mode, Reply, Watcher};
use serde_json::{json, Value};

const UNICODE: &str =
    "统一码 ✓ — emoji 🦀🎉 — RTL مرحبا بالعالم — combining e\u{301} — zero-width a\u{200b}b";

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

fn trimmed(run: &LiveRun, file: &str) -> Option<String> {
    run.read_file(file).map(|s| s.trim().to_string())
}

fn agent_type(description: &str, model: &str, body: &str) -> String {
    format!("---\ndescription: {description}\nmodel: {model}\n---\n{body}")
}

async fn interrupt(run: &mut LiveRun, session: &str) -> anyhow::Result<()> {
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    anyhow::ensure!(
        resp.result.as_ref().map(|r| &r["interrupted"]) == Some(&json!(true)),
        "{resp:?}"
    );
    Ok(())
}

/// The `tool_use` frame of the session's own (not a sub-agent's) first call
/// to `tool`.
async fn own_call(watcher: &Watcher, tool: &str) -> anyhow::Result<Value> {
    let frame = watcher
        .wait_for(&format!("a {tool} call"), Duration::from_secs(180), |e| {
            e["kind"] == "tool_use" && e["name"] == tool
        })
        .await?;
    Ok(frame["event"].clone())
}

/// Poll `session.tasks` until a task satisfies `wanted`.
async fn task_where(
    run: &mut LiveRun,
    session: &str,
    what: &str,
    wanted: impl Fn(&Value) -> bool,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let tasks = run.tasks(session).await?;
        if let Some(t) = tasks.iter().find(|t| wanted(t)) {
            return Ok(t.clone());
        }
        anyhow::ensure!(Instant::now() < deadline, "no {what}: {tasks:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn tool_result_ids(request: &live::SeenRequest) -> Vec<String> {
    request.body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| b["tool_use_id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_stops_a_running_command_and_the_session_carries_on() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("interrupt-bash").start().await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let handle = run
        .spawn_turn(
            &session,
            "Run exactly this command with the Bash tool, with timeout 120000: \
             `sleep 8; echo late > late.txt`. Wait for it, then reply DONE.",
            json!({}),
        )
        .await?;
    let bash = own_call(&watcher, "Bash").await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let interrupted_at = Instant::now();
    interrupt(&mut run, &session).await?;
    let events = tokio::time::timeout(Duration::from_secs(20), handle).await???;
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(5),
        "the turn sat out the command: {:?}",
        interrupted_at.elapsed()
    );
    assert!(events.turn_complete, "{:?}", events.events);
    run.wait_turn_state(&session, "idle", Duration::from_secs(10))
        .await?;
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert!(
        run.read_file("late.txt").is_none(),
        "the interrupted command kept running"
    );

    let word = format!("CONTINUED-{}", nonce());
    let before = run.provider.calls().len();
    run.turn(
        &session,
        &format!("Write the word {word} to ok.txt with the Write tool, then reply DONE."),
    )
    .await?;
    assert_eq!(trimmed(&run, "ok.txt"), Some(word));
    let next = &run.provider.calls()[before];
    assert!(
        tool_result_ids(next).contains(&bash["id"].as_str().unwrap_or_default().to_string()),
        "the interrupted call has no result in the next request"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_leaves_a_background_agent_running_until_the_host_stops_it(
) -> anyhow::Result<()> {
    let mut run = LiveRun::builder("interrupt-bg").start().await?;
    run.write_file(
        ".agix/agents/sleeper.md",
        &agent_type(
            "Runs one long command",
            run.config().model_at(0),
            "Run exactly `sleep 90; echo bg > bg.txt` with the Bash tool, with timeout \
             200000. Then answer DONE.",
        ),
    )?;
    let session = run.session().await?;
    run.turn(
        &session,
        "Start one background sub-agent with the Agent tool: subagent_type sleeper, \
         background true, description sleeper, prompt \"Run your command.\". Then reply \
         STARTED without waiting for it.",
    )
    .await?;
    let task = task_where(&mut run, &session, "running sleeper", |t| {
        t["detail"]["agent_type"] == "sleeper" && t["status"] == "running"
    })
    .await?;
    let task_id = task["task_id"].as_str().unwrap_or_default().to_string();

    let watcher = run.watch(&session, false).await?;
    let handle = run
        .spawn_turn(
            &session,
            "Run exactly `sleep 30` with the Bash tool, with timeout 120000, then reply DONE. \
             Do not touch the background agent.",
            json!({}),
        )
        .await?;
    own_call(&watcher, "Bash").await?;
    let interrupted_at = Instant::now();
    interrupt(&mut run, &session).await?;
    tokio::time::timeout(Duration::from_secs(20), handle).await???;
    assert!(interrupted_at.elapsed() < Duration::from_secs(5));
    run.wait_turn_state(&session, "idle", Duration::from_secs(10))
        .await?;
    let still = task_where(&mut run, &session, "the sleeper", |t| {
        t["task_id"] == task_id.as_str()
    })
    .await?;
    assert_eq!(still["status"], "running", "{still}");

    let stopped = run
        .client
        .call(
            "session.stopTask",
            json!({ "session_id": session, "task_id": task_id }),
        )
        .await?;
    assert!(stopped.error.is_none(), "{stopped:?}");
    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(60))
        .await?;
    let ended = tasks
        .iter()
        .find(|t| t["task_id"] == task_id.as_str())
        .cloned()
        .unwrap_or_default();
    assert_eq!(ended["status"], "stopped", "{ended}");
    assert!(run.read_file("bg.txt").is_none());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_session_refuses_takes_a_steer_and_queues_the_next_turn() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("busy").start().await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let (first_word, steered) = (format!("FIRST-{}", nonce()), format!("STEER-{}", nonce()));
    let first = run
        .spawn_turn(
            &session,
            &format!(
                "Do these steps in order, one tool call at a time:\n\
                 1. Run exactly `sleep 12` with the Bash tool, with timeout 60000.\n\
                 2. Write the word {first_word} to a.txt with the Write tool.\n\
                 Do both steps even if the operator sends you more work meanwhile. Then \
                 reply DONE."
            ),
            json!({ "turn_id": "busy-first" }),
        )
        .await?;
    own_call(&watcher, "Bash").await?;

    let refused = run
        .spawn_turn(
            &session,
            "this one should be refused",
            json!({ "turn_id": "busy-refused", "on_busy": "reject" }),
        )
        .await?
        .await??;
    let error = refused.response.and_then(|r| r.error).expect("refused");
    assert_eq!(error.code, -32015, "{error:?}");
    assert_eq!(
        error.data.as_ref().map(|d| &d["current_turn_id"]),
        Some(&json!("busy-first"))
    );
    let other = run.config().model_at(1).to_string();
    let switch = run.client.session_set_model(&session, &other).await?;
    assert_eq!(
        switch.error.map(|e| e.code),
        Some(-32015),
        "setModel mid-turn"
    );

    let steer = run
        .client
        .call(
            "session.run_turn",
            json!({
                "session_id": session,
                "turn_id": "busy-steer",
                "on_busy": "steer",
                "message": format!(
                    "Operator: keep doing your steps as given. In addition, before you \
                     reply, write the word {steered} to s.txt with the Write tool."
                ),
            }),
        )
        .await?;
    let steer = steer.result.unwrap_or_default();
    assert_eq!(steer["delivery"], "steer", "{steer}");
    assert_eq!(steer["into_turn_id"], "busy-first");

    let queued = run
        .spawn_turn(
            &session,
            "Read a.txt and write exactly its content to q.txt with the Write tool, then \
             reply DONE.",
            json!({ "turn_id": "busy-queued", "on_busy": "queue" }),
        )
        .await?;
    first.await??;
    let queued = queued.await??;
    assert_eq!(
        queued.response.and_then(|r| r.result).unwrap_or_default()["delivery"],
        "turn"
    );

    assert_eq!(trimmed(&run, "a.txt"), Some(first_word.clone()));
    assert_eq!(trimmed(&run, "s.txt"), Some(steered));
    assert_eq!(trimmed(&run, "q.txt"), Some(first_word));
    let frames = watcher.frames();
    let position = |turn: &str, kind: &str| {
        frames
            .iter()
            .position(|f| f["turn_id"] == turn && f["event"]["kind"] == kind)
    };
    assert!(
        position("busy-first", "user_message_delivered").is_some(),
        "no user_message_delivered in the first turn"
    );
    let first_done = position("busy-first", "turn_complete").expect("first turn_complete");
    let queued_start = position("busy-queued", "turn_start").expect("queued turn_start");
    assert!(first_done < queued_start, "the queued turn ran early");
    assert!(
        frames.iter().all(|f| f["turn_id"] != "busy-refused"),
        "the refused message started a turn"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_moves_the_next_turn_to_another_model_with_the_same_history() -> anyhow::Result<()>
{
    let mut run = LiveRun::builder("set-model").start().await?;
    let (first, second) = (
        run.config().model.clone(),
        run.config().model_at(1).to_string(),
    );
    assert_ne!(first, second, "AGIX_LIVE_MODELS needs a second Flash name");
    let session = run.session().await?;
    let word = format!("CODEWORD-{}", nonce());
    run.turn(
        &session,
        &format!("Remember the codeword {word}. Reply OK and nothing else."),
    )
    .await?;
    let before = run.provider.calls().len();
    let switched = run.client.session_set_model(&session, &second).await?;
    let switched = switched.result.unwrap_or_default();
    assert_eq!(switched["model"], second.as_str(), "{switched}");
    assert_eq!(switched["applies"], "next_turn");
    run.turn(
        &session,
        "Write the codeword I gave you earlier to w.txt with the Write tool, then reply DONE.",
    )
    .await?;
    assert_eq!(trimmed(&run, "w.txt"), Some(word));
    let calls = run.provider.calls();
    let turns = |range: &[live::SeenRequest]| -> Vec<String> {
        range
            .iter()
            .filter(|c| c.purpose() == live::Purpose::Turn)
            .map(|c| c.model().to_string())
            .collect()
    };
    let (earlier, later) = (turns(&calls[..before]), turns(&calls[before..]));
    assert!(
        !earlier.is_empty() && earlier.iter().all(|m| *m == first),
        "{earlier:?}"
    );
    assert!(
        !later.is_empty() && later.iter().all(|m| *m == second),
        "{later:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn compact_clear_and_rename_over_rpc() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("compact-clear").start().await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let word = format!("SECRET-{}", nonce());
    run.turn(
        &session,
        &format!("The codeword is {word}. Reply OK and nothing else."),
    )
    .await?;
    run.write_file("notes.txt", &"a line of notes for the record\n".repeat(200))?;
    run.turn(
        &session,
        "Read notes.txt with the Read tool and reply with its line count only.",
    )
    .await?;

    let total = |resp: daemon::rpc::RpcResponse| resp.result.unwrap_or_default()["total"].clone();
    let history = json!({ "session_id": session, "offset": 0, "limit": 1 });
    let before = total(run.client.call("session.history", history.clone()).await?);
    let compacted = run
        .client
        .call("session.compact", json!({ "session_id": session }))
        .await?;
    assert!(compacted.error.is_none(), "{compacted:?}");
    let frame = watcher
        .wait_for("a compact frame", Duration::from_secs(30), |e| {
            e["kind"] == "compact"
        })
        .await?;
    let compact = &frame["event"];
    assert!(
        compact["messages_after"].as_u64() < compact["messages_before"].as_u64(),
        "{compact}"
    );
    assert!(run
        .provider
        .calls()
        .iter()
        .any(|c| c.purpose() == live::Purpose::Compact));
    let after = total(run.client.call("session.history", history.clone()).await?);
    assert!(after.as_u64() < before.as_u64(), "{before} → {after}");

    let cleared = run
        .client
        .call("session.clear", json!({ "session_id": session }))
        .await?;
    assert!(cleared.error.is_none(), "{cleared:?}");
    assert_eq!(
        total(run.client.call("session.history", history).await?),
        json!(0)
    );
    let since_clear = run.provider.calls().len();
    run.turn(
        &session,
        "If an earlier message in this conversation gave you a codeword, write it to c.txt; \
         otherwise write NONE to c.txt. Use the Write tool, then reply DONE.",
    )
    .await?;
    assert!(
        run.provider.calls()[since_clear..]
            .iter()
            .all(|c| !c.messages_contain(&word)),
        "the codeword reached the model after session.clear"
    );
    assert!(trimmed(&run, "c.txt").is_some_and(|c| !c.contains(&word)));

    let name = "发布评审 🦀 مراجعة";
    let renamed = run.client.session_rename(&session, Some(name)).await?;
    assert_eq!(renamed.result.unwrap_or_default()["name"], name);
    let got = run
        .client
        .session_get(&session)
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(got["name"], name);
    let listed = run.client.session_list().await?.result.unwrap_or_default();
    assert!(
        listed["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["session_id"] == session.as_str() && s["name"] == name),
        "{listed}"
    );
    run.client.session_rename(&session, None).await?;
    let got = run
        .client
        .session_get(&session)
        .await?
        .result
        .unwrap_or_default();
    assert!(got["name"].is_null(), "{got}");
    run.finish().await;
    Ok(())
}

fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn zip_entry(path: &Path, name: &str) -> anyhow::Result<String> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let mut entry = archive.by_name(name)?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut entry, &mut text)?;
    Ok(text)
}

#[tokio::test(flavor = "multi_thread")]
async fn export_carries_the_sidechain_and_delete_takes_it_along() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("export").start().await?;
    let token = format!("SIDE-{}", nonce());
    run.write_file("sub.txt", &format!("{token}\n"))?;
    run.write_file(
        ".agix/agents/reader.md",
        &agent_type(
            "Reads one file",
            run.config().model_at(0),
            "Read the file you are told to read with the Read tool and answer with its \
             content only.",
        ),
    )?;
    let session = run.session().await?;
    run.turn(
        &session,
        "Call the Agent tool once: subagent_type reader, description reader, prompt \"Read \
         sub.txt\". Do not read the file yourself. Write the agent's answer to answer.txt \
         with the Write tool, then reply DONE.",
    )
    .await?;
    assert_eq!(trimmed(&run, "answer.txt"), Some(token.clone()));
    let children = run
        .client
        .call("session.list", json!({ "parent_session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    let child = children["sessions"][0]["session_id"]
        .as_str()
        .expect("a sidechain")
        .to_string();

    let out = tempfile::tempdir()?;
    let zip_path = out.path().join("s.agix.zip");
    let exported = run
        .client
        .session_export(&session, &zip_path, None, None)
        .await?;
    let exported = exported.result.unwrap_or_default();
    assert_eq!(exported["format"], "zip", "{exported}");
    let ids: Vec<&str> = exported["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["session_id"].as_str())
        .collect();
    assert!(
        ids.contains(&session.as_str()) && ids.contains(&child.as_str()),
        "{ids:?}"
    );
    let manifest: Value = serde_json::from_str(&zip_entry(&zip_path, "manifest.json")?)?;
    assert_eq!(manifest["root_session_id"], session.as_str());
    assert!(
        zip_entry(&zip_path, &format!("sessions/{child}/transcript.jsonl"))?.contains(&token),
        "the sidechain's transcript is not the one that read the file"
    );
    zip_entry(&zip_path, &format!("sessions/{session}/history.json"))?;

    let dir_path = out.path().join("s-dir");
    let exported = run
        .client
        .session_export(&session, &dir_path, Some("dir"), None)
        .await?;
    assert!(exported.error.is_none(), "{exported:?}");
    assert!(dir_path.join("manifest.json").is_file());
    assert!(dir_path
        .join(format!("sessions/{child}/transcript.jsonl"))
        .is_file());
    let again = run
        .client
        .session_export(&session, &zip_path, None, None)
        .await?;
    assert_eq!(
        again.error.map(|e| e.code),
        Some(-32602),
        "overwrote an export"
    );

    let transcripts = run.world.transcript_dir(&run.project);
    let mentioning = |id: &str| {
        files_under(&transcripts)
            .into_iter()
            .filter(|p| p.to_string_lossy().contains(id))
            .count()
    };
    assert!(mentioning(&session) > 0 && mentioning(&child) > 0);
    let dry = run
        .client
        .call(
            "session.delete",
            json!({ "session_id": session, "dry_run": true }),
        )
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(dry["deleted"], false, "{dry}");
    assert!(dry["sidechain_ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|i| i == child.as_str())));
    assert!(mentioning(&session) > 0, "a dry run deleted");

    let mut events = run.daemon.connect().await?;
    events.call("daemon.subscribeEvents", Value::Null).await?;
    let deleted = run
        .client
        .call("session.delete", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(deleted["deleted"], true, "{deleted}");
    assert!(deleted["sidechains_deleted"].as_u64() >= Some(1));
    assert_eq!(mentioning(&session), 0, "the transcript is still there");
    assert_eq!(mentioning(&child), 0, "the sidechain is still there");
    let gone = run.client.session_get(&session).await?;
    assert_eq!(gone.error.map(|e| e.code), Some(-32000));
    let mut deleted_events = Vec::new();
    while deleted_events.len() < 2 {
        let Ok(Ok(Some(frame))) =
            tokio::time::timeout(Duration::from_secs(5), events.next_frame()).await
        else {
            break;
        };
        if frame["params"]["kind"] == "session_deleted" {
            deleted_events.push(frame["params"].clone());
        }
    }
    assert!(
        deleted_events.iter().any(
            |e| e["session_id"] == child.as_str() && e["parent_session_id"] == session.as_str()
        ),
        "{deleted_events:?}"
    );
    assert!(deleted_events
        .iter()
        .any(|e| e["session_id"] == session.as_str()));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hosts_goal_can_be_paused_edited_and_cleared() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("goal-host").start().await?;
    let session = run.session().await?;
    let set = run
        .client
        .call(
            "session.setGoal",
            json!({
                "session_id": session,
                "objective": "Write the word GOAL-ONE to goal.txt with the Write tool, then \
                              mark the goal complete with GoalUpdate.",
                "max_rounds": 3,
            }),
        )
        .await?;
    assert!(set.error.is_none(), "{set:?}");
    let update = |action: &str, extra: Value| {
        let mut params = json!({ "session_id": session, "action": action });
        if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
            p.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        params
    };
    let paused = run
        .client
        .call("session.updateGoal", update("pause", json!({})))
        .await?;
    let paused = paused.result.unwrap_or_default();
    assert_eq!(paused["goal"]["phase"], "paused", "{paused}");
    run.wait_turn_state(&session, "idle", Duration::from_secs(60))
        .await?;

    let edited = run
        .client
        .call(
            "session.updateGoal",
            update(
                "edit",
                json!({ "objective": "EDITED-OBJECTIVE: never run", "max_rounds": 2 }),
            ),
        )
        .await?;
    let edited = edited.result.unwrap_or_default();
    assert_eq!(
        edited["goal"]["objective"], "EDITED-OBJECTIVE: never run",
        "{edited}"
    );
    assert_eq!(edited["goal"]["phase"], "paused");
    assert_eq!(edited["goal"]["max_rounds"], 2);
    let calls = run.provider.calls().len();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        run.provider.calls().len(),
        calls,
        "a paused goal ran a round"
    );

    let cleared = run
        .client
        .call("session.updateGoal", update("clear", json!({})))
        .await?;
    assert!(cleared.result.unwrap_or_default()["goal"].is_null());
    let goal = run
        .client
        .call("session.goal", json!({ "session_id": session }))
        .await?;
    assert!(goal.result.unwrap_or_default()["goal"].is_null());
    assert!(run
        .provider
        .calls()
        .iter()
        .all(|c| !c.messages_contain("EDITED-OBJECTIVE")));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_takes_a_background_task_down_and_the_session_resumes() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("restart-bg")
        .mode(Mode::Spawned)
        .start()
        .await?;
    run.write_file(
        ".agix/agents/sleeper.md",
        &agent_type(
            "Runs one long command",
            run.config().model_at(0),
            "Run exactly `sleep 20; echo bg > bg.txt` with the Bash tool, with timeout \
             200000. Then answer DONE.",
        ),
    )?;
    let session = run.session().await?;
    let word = format!("BEFORE-{}", nonce());
    run.turn(
        &session,
        &format!(
            "Do these steps in order:\n\
             1. Write the word {word} to before.txt with the Write tool.\n\
             2. Start one background sub-agent with the Agent tool: subagent_type sleeper, \
             background true, description sleeper, prompt \"Run your command.\".\n\
             Then reply STARTED without waiting for it."
        ),
    )
    .await?;
    let task = task_where(&mut run, &session, "running sleeper", |t| {
        t["detail"]["agent_type"] == "sleeper" && t["status"] == "running"
    })
    .await?;
    let child = task["detail"]["agent_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let restarted_at = Instant::now();
    run.restart().await?;

    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(resumed["status"], "resumed", "{resumed}");
    let tasks = run.tasks(&session).await?;
    assert!(
        tasks.iter().all(|t| t["status"] != "running"),
        "a task survived the restart as running: {tasks:?}"
    );
    run.turn(
        &session,
        "Read before.txt and write exactly its content to after.txt with the Write tool, \
         then reply DONE.",
    )
    .await?;
    assert_eq!(trimmed(&run, "after.txt"), Some(word));
    let children = run
        .client
        .call("session.list", json!({ "parent_session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert!(
        children["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["session_id"] == child.as_str()),
        "the sidechain is gone: {children}"
    );
    let wait = Duration::from_secs(25).saturating_sub(restarted_at.elapsed());
    tokio::time::sleep(wait).await;
    assert!(
        run.read_file("bg.txt").is_none(),
        "the background command outlived its daemon"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_text_attachment_reaches_the_model_without_being_in_the_message() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("attach-text").start().await?;
    let session = run.session().await?;
    let token = format!("ATT-{}", nonce());
    run.turn_with(
        &session,
        "The attached selection contains one word that starts with ATT-. Write that word to \
         att.txt with the Write tool, then reply DONE. Do not read any files.",
        json!({ "attachments": [{
            "kind": "text",
            "path": "editor/selection.rs",
            "content": format!("fn main() {{\n    // marker: {token}\n}}\n"),
        }] }),
    )
    .await?;
    assert_eq!(trimmed(&run, "att.txt"), Some(token.clone()));
    let first = &run.provider.calls()[0];
    assert!(first.messages_contain(&token));
    run.finish().await;
    Ok(())
}

/// A `width`×`height` PNG of one colour, stored without compression.
fn solid_png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    let row: Vec<u8> = std::iter::once(0)
        .chain((0..width).flat_map(|_| rgb))
        .collect();
    let raw = row.repeat(height as usize);
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        zlib.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

#[tokio::test(flavor = "multi_thread")]
async fn an_image_attachment_reaches_the_endpoint_as_an_image_block() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("attach-image").start().await?;
    let session = run.session().await?;
    let image = run.project.join("swatch.png");
    std::fs::write(&image, solid_png(64, 64, [20, 40, 230]))?;
    let (turn_id, message) = (
        "attach-image-t1",
        "Look at the attached image. Write the one lowercase English word naming its colour \
         (red, green or blue) to colour.txt with the Write tool, then reply DONE. Do not read \
         any files.",
    );
    let events = run
        .client
        .run_turn_with(json!({
            "session_id": session,
            "turn_id": turn_id,
            "message": message,
            "attachments": [{ "kind": "file", "path": image.to_string_lossy() }],
        }))
        .await?;
    let first = run.provider.calls()[0].clone();
    let images: Vec<Value> = first.body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "image")
        .collect();
    assert_eq!(images.len(), 1, "{}", first.body["messages"]);
    assert_eq!(images[0]["source"]["media_type"], "image/png");
    match events.response.as_ref().and_then(|r| r.error.as_ref()) {
        Some(error) => {
            eprintln!("[live attach-image] the endpoint refused the image: {error:?}");
            assert_eq!(error.code, -32002, "{error:?}");
            run.wait_turn_state(&session, "idle", Duration::from_secs(10))
                .await?;
        }
        None => assert_eq!(trimmed(&run, "colour.txt").as_deref(), Some("blue")),
    }
    let fresh = run.session().await?;
    run.turn(&fresh, "Reply with the word READY and nothing else.")
        .await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_200_kb_message_reaches_the_model_whole() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("big-input").start().await?;
    let session = run.session().await?;
    let token = format!("BIG-{}", nonce());
    let mut message = String::from(
        "Below is a long log. Its very last line is one word that starts with BIG-. Write \
         that word to big.txt with the Write tool, then reply DONE. Do not read any files.\n\n",
    );
    let mut i = 0;
    while message.len() < 200_000 {
        message.push_str(&format!(
            "{i:06} INFO worker heartbeat ok queue=0 latency_ms=12\n"
        ));
        i += 1;
    }
    message.push_str(&token);
    run.turn(&session, &message).await?;
    assert_eq!(trimmed(&run, "big.txt"), Some(token));
    let first = &run.provider.calls()[0];
    assert!(
        first.messages_contain(&message),
        "the message did not arrive whole"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn unicode_in_messages_file_names_and_contents_survives_byte_for_byte() -> anyhow::Result<()>
{
    let mut run = LiveRun::builder("unicode").start().await?;
    let session = run.session().await?;
    let edited_name = "笔记 🦀 ملاحظات.md";
    let original = format!("# {UNICODE}\n\n{UNICODE}\nSTATUS: draft\n{UNICODE}\n");
    run.write_file(edited_name, &original)?;
    let written_name = "résumé ✓ 日本.txt";
    let word = format!("W-{}", nonce());
    let message = format!(
        "请按顺序做两件事 ✅ (مرحبا):\n\
         1. 用 Edit 工具把文件「{edited_name}」里的 `STATUS: draft` 改成 `STATUS: final`，\
         别的字节一个都不要动。\n\
         2. 用 Write 工具新建文件「{written_name}」，内容只有 {word}。\n\
         然后回复 DONE。"
    );
    run.turn(&session, &message).await?;
    assert_eq!(
        run.read_file(edited_name),
        Some(original.replace("STATUS: draft", "STATUS: final")),
        "the edit touched other bytes"
    );
    assert_eq!(trimmed(&run, written_name), Some(word));
    assert!(
        run.provider.calls()[0].messages_contain(&message),
        "the message changed on the way to the model"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_message_is_refused_without_calling_the_model() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("empty").start().await?;
    let session = run.session().await?;
    for message in ["", "   \n\t "] {
        let events = run
            .client
            .run_turn_with(json!({ "session_id": session, "message": message }))
            .await?;
        let error = events.response.and_then(|r| r.error);
        assert_eq!(
            error.as_ref().map(|e| e.code),
            Some(-32602),
            "{message:?}: {error:?}"
        );
    }
    assert!(
        run.provider.calls().is_empty(),
        "an empty message reached the model"
    );
    run.turn(&session, "Reply with the word READY and nothing else.")
        .await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_ends_a_turn_still_waiting_for_the_model() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("interrupt-headers").start().await?;
    let session = run.session().await?;
    run.provider.inject_failures(
        1,
        Reply::delayed(
            Duration::from_secs(30),
            Reply::status(
                529,
                "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"held\"}}",
            ),
        ),
    );
    let handle = run
        .spawn_turn(&session, "Reply with the word READY.", json!({}))
        .await?;
    run.wait_turn_state(&session, "running", Duration::from_secs(10))
        .await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let interrupted_at = Instant::now();
    interrupt(&mut run, &session).await?;
    tokio::time::timeout(Duration::from_secs(60), handle).await???;
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(5),
        "the turn waited for the held request: {:?}",
        interrupted_at.elapsed()
    );
    run.finish().await;
    Ok(())
}
