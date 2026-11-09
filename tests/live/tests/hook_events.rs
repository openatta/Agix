//! Every hook event, fired end to end through the assembled daemon with a
//! scripted model: one command hook bound to all thirty events appends each
//! payload to a ledger, and the cases read the ledger to say which events
//! fired, in what order, and with what payload. Then the hook protocol
//! itself — blocking, rewriting, refusing a message, replacing a result,
//! `once`, `if` — its four backends, and its edges: a hook that times out,
//! hangs, exits non-zero, prints something that is not JSON, and several
//! hooks on one event.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use live::{
    Answer, LiveConfig, LiveRun, LiveRunBuilder, McpToy, Page, PageServer, ProviderStub, Reply,
};
use serde_json::{json, Value};

const EVENTS: [&str; 30] = [
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "TurnStart",
    "TurnComplete",
    "SessionStart",
    "SessionEnd",
    "Stop",
    "StopFailure",
    "UserPromptSubmit",
    "PreCompact",
    "PostCompact",
    "PermissionRequested",
    "PermissionDenied",
    "Setup",
    "Notification",
    "SubagentStop",
    "SubagentStart",
    "TeammateIdle",
    "TaskCreated",
    "TaskCompleted",
    "Elicitation",
    "ElicitationResult",
    "ConfigChange",
    "WorktreeCreate",
    "WorktreeRemove",
    "InstructionsLoaded",
    "CwdChanged",
    "FileChanged",
    "PostSampling",
];

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        live::ModelEndpoint::model(stub),
    ))
}

fn executable(path: &Path, body: &str) -> anyhow::Result<()> {
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn command_hook(script: &Path, timeout_ms: u64) -> Value {
    json!({ "type": "command", "command": script, "timeout": timeout_ms })
}

/// A command hook that appends every payload it is given to `ledger.jsonl`.
/// On `Stop` it answers `continue: false` once if `stop-now` exists, which
/// is how a case ends one particular turn by hook.
struct Ledger {
    dir: tempfile::TempDir,
}

impl Ledger {
    fn new() -> anyhow::Result<Self> {
        let ledger = Self {
            dir: tempfile::tempdir()?,
        };
        let stop = ledger.dir.path().join("stop-now");
        executable(
            &ledger.script(),
            &format!(
                "#!/bin/sh\npayload=$(cat)\nprintf '%s\\n' \"$payload\" >> '{}'\n\
                 case \"$payload\" in\n  *'\"hook_event_name\":\"Stop\"'*)\n    \
                 if [ -f '{stop}' ]; then rm -f '{stop}'; \
                 echo '{{\"continue\": false, \"message\": \"LEDGER-STOPPED\"}}'; exit 0; fi ;;\n\
                 esac\necho '{{}}'\n",
                ledger.file().display(),
                stop = stop.display(),
            ),
        )?;
        Ok(ledger)
    }

    fn script(&self) -> PathBuf {
        self.dir.path().join("ledger.sh")
    }

    fn file(&self) -> PathBuf {
        self.dir.path().join("ledger.jsonl")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn stop_next_turn(&self) -> anyhow::Result<()> {
        std::fs::write(self.path("stop-now"), "")?;
        Ok(())
    }

    fn hook(&self) -> Value {
        command_hook(&self.script(), 30_000)
    }

    fn on_every_event(&self) -> Value {
        Value::Object(
            EVENTS
                .iter()
                .map(|e| (e.to_string(), json!([self.hook()])))
                .collect(),
        )
    }

    fn entries(&self) -> Vec<Value> {
        std::fs::read_to_string(self.file())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn names(&self) -> Vec<String> {
        names_of(&self.entries())
    }

    async fn wait_for(&self, event: &str, timeout: Duration) -> anyhow::Result<Value> {
        self.wait_until(event, |e| e["hook_event_name"] == event, timeout)
            .await
    }

    async fn wait_until(
        &self,
        what: &str,
        wanted: impl Fn(&Value) -> bool,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(found) = self.entries().into_iter().find(|e| wanted(e)) {
                return Ok(found);
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "no {what} within {timeout:?}; the ledger has {:?}",
                self.names()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Events fired from a task of their own rather than in line with what
/// triggered them, so they land in the ledger at no fixed place.
const FIRED_ASIDE: [&str; 2] = ["TaskCreated", "Elicitation"];

fn in_line(e: &Value) -> bool {
    !FIRED_ASIDE.iter().any(|name| e["hook_event_name"] == *name)
}

/// A task the ledger saw start, of `kind`.
fn task_created_of(kind: &str) -> impl Fn(&Value) -> bool + '_ {
    move |e| {
        e["hook_event_name"] == "TaskCreated"
            && e["reason"]
                .as_str()
                .and_then(|r| serde_json::from_str::<Value>(r).ok())
                .is_some_and(|task| task["kind"] == kind)
    }
}

fn names_of(entries: &[Value]) -> Vec<String> {
    entries
        .iter()
        .map(|e| e["hook_event_name"].as_str().unwrap_or("?").to_string())
        .collect()
}

fn position(entries: &[Value], wanted: impl Fn(&Value) -> bool) -> Option<usize> {
    entries.iter().position(wanted)
}

fn event_is<'a>(name: &'a str) -> impl Fn(&Value) -> bool + 'a {
    move |e| e["hook_event_name"] == name
}

fn event_on<'a>(name: &'a str, tool: &'a str) -> impl Fn(&Value) -> bool + 'a {
    move |e| e["hook_event_name"] == name && e["tool_name"] == tool
}

fn in_order(
    entries: &[Value],
    first: impl Fn(&Value) -> bool,
    then: impl Fn(&Value) -> bool,
) -> bool {
    match (position(entries, first), position(entries, then)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_wired_event_fires_at_its_moment() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let mut run = stubbed("ledger", &stub)
        .settings(json!({
            "permission_mode": "default",
            "permission_rules": [
                { "tool": "Agent", "action": "allow" },
                { "tool": "Read(**/forbidden.txt)", "action": "deny" }
            ],
            "mcp_servers": { "demo": McpToy::stdio_config(&[])? },
            "hooks_config": ledger.on_every_event(),
        }))
        .before_start(|_, project| {
            std::fs::write(project.join("AGENTS.md"), "# Project rules\nBe brief.\n")?;
            std::fs::write(project.join("forbidden.txt"), "no\n")?;
            std::fs::create_dir_all(project.join("a-directory"))?;
            Ok(())
        })
        .start()
        .await?;

    let session = run.session().await?;
    let _watcher = run
        .watch_with(&session, |e| {
            (e["prompt_type"] == "permission").then(Answer::permit)
        })
        .await?;
    ledger
        .wait_for("CwdChanged", Duration::from_secs(60))
        .await?;
    let opening = ledger.entries();
    assert!(
        in_order(&opening, event_is("Setup"), event_is("SessionStart"))
            && in_order(&opening, event_is("SessionStart"), event_is("CwdChanged")),
        "a session opens with Setup, SessionStart, CwdChanged: {:?}",
        names_of(&opening)
    );
    assert!(
        position(&opening, event_is("TurnStart")).is_none(),
        "{:?}",
        names_of(&opening)
    );

    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls("b1", "Bash", json!({ "command": "echo made > made.txt" })),
            Reply::calls("r1", "Read", json!({ "file_path": "forbidden.txt" })),
            Reply::calls("r2", "Read", json!({ "file_path": "a-directory" })),
            Reply::calls("e1", "mcp__demo__elicit", json!({ "reason": "ledger" })),
            Reply::calls(
                "a1",
                "Agent",
                json!({ "prompt": "say SUB-DONE", "description": "a helper" }),
            ),
            Reply::text("turn one done"),
        ],
    );
    stub.script([Reply::text("SUB-DONE")]);
    run.turn(&session, "PARENT-MARK turn one").await?;
    ledger
        .wait_for("Elicitation", Duration::from_secs(60))
        .await?;
    let tasked = ledger
        .wait_until(
            "the sub-agent's TaskCreated",
            task_created_of("agent"),
            Duration::from_secs(60),
        )
        .await?;
    let all = ledger.entries();
    let turn_one: Vec<Value> = all[opening.len()..]
        .iter()
        .filter(|e| e["session_id"] == session.as_str() || e["hook_event_name"] == "Elicitation")
        .cloned()
        .collect();
    let names = names_of(&turn_one);

    assert!(
        in_order(
            &turn_one,
            event_is("UserPromptSubmit"),
            event_is("TurnStart")
        ) && in_order(
            &turn_one,
            event_is("TurnStart"),
            event_is("InstructionsLoaded")
        ) && in_order(
            &turn_one,
            event_is("InstructionsLoaded"),
            event_is("PreToolUse")
        ),
        "{names:?}"
    );
    let prompt = turn_one
        .iter()
        .find(|e| e["hook_event_name"] == "UserPromptSubmit")
        .cloned()
        .unwrap_or_default();
    assert_eq!(prompt["user_prompt"], "PARENT-MARK turn one");
    assert_eq!(prompt["delivery"], "turn");

    assert!(
        in_order(
            &turn_one,
            event_on("PreToolUse", "Bash"),
            event_on("PermissionRequested", "Bash")
        ) && in_order(
            &turn_one,
            event_on("PermissionRequested", "Bash"),
            event_on("Notification", "Bash")
        ) && in_order(
            &turn_one,
            event_on("Notification", "Bash"),
            event_on("PostToolUse", "Bash")
        ),
        "an asked call: PreToolUse, PermissionRequested, Notification, PostToolUse: {names:?}"
    );
    let denied = turn_one
        .iter()
        .find(|e| e["hook_event_name"] == "PermissionDenied")
        .cloned()
        .unwrap_or_default();
    assert_eq!(denied["tool_name"], "Read", "{denied}");
    assert!(
        denied["reason"]
            .as_str()
            .is_some_and(|r| r.contains("denied by rule")),
        "{denied}"
    );
    assert!(
        !turn_one
            .iter()
            .any(|e| e["hook_event_name"] == "PostToolUse" && e["tool_use_id"] == "r1"),
        "a denied call never ran, so nothing comes after it"
    );
    assert!(
        in_order(&turn_one, event_on("PostToolUseFailure", "Read"), |e| {
            e["hook_event_name"] == "PostToolUse" && e["tool_use_id"] == "r2"
        }),
        "a failed call: PostToolUseFailure, then PostToolUse: {names:?}"
    );
    let failed = turn_one
        .iter()
        .find(|e| e["hook_event_name"] == "PostToolUse" && e["tool_use_id"] == "r2")
        .cloned()
        .unwrap_or_default();
    assert_eq!(failed["is_error"], true, "{failed}");
    let elicited = turn_one
        .iter()
        .find(|e| e["hook_event_name"] == "Elicitation")
        .cloned()
        .unwrap_or_default();
    assert!(
        elicited["tool_input"]["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("elicitation://toy/")),
        "{elicited}"
    );
    assert_eq!(elicited["tool_input"]["server_name"], "demo");

    let task: Value = serde_json::from_str(tasked["reason"].as_str().unwrap_or("null"))?;
    assert_eq!(task["kind"], "agent", "{tasked}");
    assert!(
        in_order(&all, event_is("SubagentStart"), event_is("SubagentStop"))
            && in_order(
                &all,
                event_is("SubagentStop"),
                event_on("PostToolUse", "Agent")
            ),
        "{:?}",
        names_of(&all)
    );
    let ending: Vec<&str> = turn_one
        .iter()
        .filter(|e| e["session_id"] == session.as_str() && in_line(e))
        .map(|e| e["hook_event_name"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        ending.ends_with(&["PostSampling", "Stop", "TurnComplete"]),
        "a turn ends PostSampling, Stop, TurnComplete: {ending:?}"
    );
    let complete = turn_one
        .iter()
        .rposition(|e| e["hook_event_name"] == "TurnComplete");
    let completed = &turn_one[complete.unwrap_or_default()];
    assert_eq!(completed["turn_id"], "ledger-t1");
    assert_eq!(completed["stop_reason"], "end_turn");
    let sampled = names.iter().filter(|n| *n == "PostSampling").count();
    assert_eq!(sampled, 6, "one PostSampling per model response: {names:?}");
    assert!(position(&all, event_is("PreCompact")).is_none());

    let before_compact = ledger.entries().len();
    stub.script([Reply::text("SUMMARY: one turn happened.")]);
    run.turn(&session, "/compact").await?;
    let compacting = ledger.entries()[before_compact..].to_vec();
    let pre = compacting
        .iter()
        .find(|e| e["hook_event_name"] == "PreCompact")
        .cloned()
        .unwrap_or_default();
    assert_eq!(pre["tool_input"]["trigger"], "manual", "{compacting:?}");
    assert!(in_order(
        &compacting,
        event_is("PreCompact"),
        event_is("PostCompact")
    ));
    assert_eq!(
        compacting
            .iter()
            .find(|e| e["hook_event_name"] == "PostCompact")
            .map(|e| e["tool_input"]["strategy"].clone()),
        Some(json!("FullCompact"))
    );

    let before_stop = ledger.entries().len();
    ledger.stop_next_turn()?;
    stub.script_when("PARENT-MARK", [Reply::text("turn three done")]);
    let stopped = run.turn(&session, "PARENT-MARK turn three").await?;
    let ended = stopped.turn_complete_event.clone().unwrap_or_default();
    assert_eq!(ended["stop_reason"], "stopped_by_hook", "{ended}");
    assert_eq!(ended["message"], "LEDGER-STOPPED", "{ended}");
    let stopping: Vec<Value> = ledger.entries()[before_stop..]
        .iter()
        .filter(|e| in_line(e))
        .cloned()
        .collect();
    let stopping = names_of(&stopping);
    assert!(
        stopping.ends_with(&["Stop".to_string(), "StopFailure".to_string()]),
        "a Stop hook that ends the turn: StopFailure, and no TurnComplete: {stopping:?}"
    );

    run.client.call("config.reload", json!({})).await?;
    assert!(position(&ledger.entries(), event_is("SessionEnd")).is_none());
    run.client
        .call("session.close", json!({ "session_id": session }))
        .await?;
    let end = ledger
        .wait_for("SessionEnd", Duration::from_secs(60))
        .await?;
    assert_eq!(end["session_id"], session.as_str());

    let fired = ledger.names();
    for unwired in ["ConfigChange", "ElicitationResult"] {
        assert!(
            !fired.iter().any(|n| n == unwired),
            "{unwired} is listed as never fired: {fired:?}"
        );
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn worktree_events_bracket_a_worktree() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let mut run = stubbed("wt", &stub)
        .settings(json!({
            "hooks_config": ledger.on_every_event(),
            "permission_rules": [
                { "tool": "EnterWorktree", "action": "allow" },
                { "tool": "ExitWorktree", "action": "allow" }
            ]
        }))
        .before_start(|world, _| {
            world.git_project("wt")?;
            world.write_declarative_scene(
                "worktrees",
                &["Read", "Write", "EnterWorktree", "ExitWorktree"],
                "Worktree scene.\n",
            )?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.scene_session("worktrees", json!({})).await?;
    stub.script([
        Reply::calls("w1", "EnterWorktree", json!({ "slug": "probe" })),
        Reply::calls("w2", "EnterWorktree", json!({ "slug": "../escape" })),
        Reply::calls(
            "w3",
            "ExitWorktree",
            json!({ "action": "remove", "discard_changes": true }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "worktree").await?;
    assert!(
        !events.tool_results[0].2 && events.tool_results[1].2,
        "{:?}",
        events.tool_results
    );
    let entries = ledger.entries();
    let created: Vec<&Value> = entries
        .iter()
        .filter(|e| e["hook_event_name"] == "WorktreeCreate")
        .collect();
    assert_eq!(
        created.len(),
        1,
        "only the worktree that was made: {created:?}"
    );
    assert_eq!(created[0]["tool_input"]["slug"], "probe");
    assert!(
        in_order(
            &entries,
            event_on("PostToolUse", "EnterWorktree"),
            event_is("WorktreeCreate")
        ) && in_order(
            &entries,
            event_is("WorktreeCreate"),
            event_is("WorktreeRemove")
        ),
        "{:?}",
        names_of(&entries)
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_notification_is_a_frame_and_a_notification_hook() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let mut run = stubbed("push", &stub)
        .settings(json!({ "hooks_config": ledger.on_every_event() }))
        .before_start(|world, _| {
            world.write_declarative_scene("push", &["PushNotification"], "Push scene.\n")?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.scene_session("push", json!({})).await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([
        Reply::calls(
            "p1",
            "PushNotification",
            json!({ "message": "PUSHED-7 build finished", "status": "proactive" }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "notify").await?;
    assert!(!events.tool_results[0].2, "{:?}", events.tool_results);

    let frames = watcher.frames();
    let notified: Vec<&Value> = frames
        .iter()
        .filter(|f| f["event"]["kind"] == "notification")
        .collect();
    assert_eq!(notified.len(), 1, "{frames:?}");
    assert_eq!(notified[0]["event"]["message"], "PUSHED-7 build finished");
    assert_eq!(notified[0]["event"]["status"], "proactive");
    assert!(notified[0]["turn_id"].is_string());

    let entries = ledger.entries();
    let hooked: Vec<&Value> = entries
        .iter()
        .filter(|e| e["hook_event_name"] == "Notification")
        .collect();
    assert_eq!(hooked.len(), 1, "{:?}", names_of(&entries));
    assert_eq!(hooked[0]["reason"], "PUSHED-7 build finished");
    assert_eq!(hooked[0]["tool_name"], "PushNotification");
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// The OS file watcher can lag well behind a write on a loaded machine, so
/// the file is edited again each second until the event shows up.
async fn edit_until_file_changed(ledger: &Ledger, file: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut edits = 0;
    loop {
        edits += 1;
        std::fs::write(file, format!("# Rules\nEdit {edits}.\n"))?;
        if ledger
            .wait_for("FileChanged", Duration::from_secs(1))
            .await
            .is_ok()
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no FileChanged after {edits} edits of {}",
            file.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn file_changed_fires_when_the_configured_instruction_file_is_edited() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let notes = ledger.path("RULES.md");
    std::fs::write(&notes, "# Rules\nBe brief.\n")?;
    let mut run = stubbed("watched", &stub)
        .settings(json!({
            "instruction_file": notes,
            "hooks_config": { "FileChanged": [ledger.hook()] },
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "hello").await?;
    assert!(ledger.entries().is_empty());
    edit_until_file_changed(&ledger, &notes).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn file_changed_fires_when_the_agents_md_a_session_loaded_is_edited() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let mut run = stubbed("watched-agents", &stub)
        .settings(json!({ "hooks_config": { "FileChanged": [ledger.hook()], "InstructionsLoaded": [ledger.hook()] } }))
        .before_start(|_, project| {
            std::fs::write(project.join("AGENTS.md"), "# Rules\nBe brief.\n")?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "hello").await?;
    ledger
        .wait_for("InstructionsLoaded", Duration::from_secs(60))
        .await?;
    edit_until_file_changed(&ledger, &run.project.join("AGENTS.md")).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_member_finishing_fires_task_completed_and_teammate_idle() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let mut run = stubbed("teamed", &stub)
        .settings(json!({
            "execution": { "team_enabled": true },
            "hooks_config": {
                "TaskCompleted": [ledger.hook()],
                "TeammateIdle": [ledger.hook()]
            },
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "t1",
                "TeamCreate",
                json!({ "name": "crew", "agents": [{ "label": "solo", "prompt": "MEMBER-MARK say hi" }] }),
            ),
            Reply::text("team done"),
        ],
    );
    stub.script_when("MEMBER-MARK", [Reply::text("hi from the member")]);
    run.turn(&session, "PARENT-MARK run the team").await?;
    for event in ["TaskCompleted", "TeammateIdle"] {
        let fired = ledger.wait_for(event, Duration::from_secs(60)).await?;
        assert_eq!(fired["team_name"], "crew", "{fired}");
        assert_eq!(fired["teammate_name"], "solo", "{fired}");
        assert_ne!(
            fired["session_id"],
            session.as_str(),
            "the member's own session: {fired}"
        );
    }
    let names = ledger.names();
    assert_eq!(
        names,
        ["TaskCompleted", "TeammateIdle"],
        "the lead is not a member: {names:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn every_way_of_starting_a_sub_agent_is_bracketed_by_subagent_hooks() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let mut run = stubbed("bracketed", &stub)
        .settings(json!({
            "execution": { "team_enabled": true },
            "permission_rules": [{ "tool": "Agent", "action": "allow" }],
            "hooks_config": {
                "SubagentStart": [ledger.hook()],
                "SubagentStop": [ledger.hook()],
            },
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "a1",
                "Agent",
                json!({ "prompt": "BACKGROUND-MARK say hi", "description": "bg", "background": true }),
            ),
            Reply::calls(
                "a2",
                "Agent",
                json!({ "prompt": "MEMBER-MARK say hi", "description": "member",
                        "team_name": "crew", "name": "solo" }),
            ),
            Reply::text("done"),
        ],
    );
    stub.script_when("BACKGROUND-MARK", [Reply::text("hi from the background")]);
    stub.script_when("MEMBER-MARK", [Reply::text("hi from the member")]);
    run.turn(&session, "PARENT-MARK go").await?;

    let deadline = Instant::now() + Duration::from_secs(60);
    let entries = loop {
        let entries = ledger.entries();
        let starts = entries
            .iter()
            .filter(|e| e["hook_event_name"] == "SubagentStart")
            .count();
        let stops = entries
            .iter()
            .filter(|e| e["hook_event_name"] == "SubagentStop")
            .count();
        if starts >= 2 && stops >= 1 {
            break entries;
        }
        anyhow::ensure!(Instant::now() < deadline, "{:?}", names_of(&entries));
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let started: Vec<&Value> = entries
        .iter()
        .filter(|e| e["hook_event_name"] == "SubagentStart")
        .collect();
    assert_eq!(started.len(), 2, "{:?}", names_of(&entries));
    for e in &started {
        assert_ne!(e["session_id"], session.as_str(), "{e}");
        assert_ne!(e["cwd"], "", "{e}");
        assert_ne!(e["permission_mode"], "", "{e}");
    }
    let background = entries
        .iter()
        .find(|e| e["hook_event_name"] == "SubagentStop")
        .cloned()
        .unwrap_or_default();
    assert_eq!(background["stop_reason"], "end_turn", "{background}");
    assert!(
        started
            .iter()
            .any(|e| e["session_id"] == background["session_id"]),
        "the background run's start and stop name the same sub-agent: {entries:?}"
    );
    run.finish().await;
    Ok(())
}

/// A sub-agent shares its parent's hooks rather than building its own, so
/// the instruction file is watched once however many sub-agents ran.
#[tokio::test(flavor = "multi_thread")]
async fn one_edit_fires_file_changed_once_after_sub_agents_have_run() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let notes = ledger.path("RULES.md");
    std::fs::write(&notes, "# Rules\nBe brief.\n")?;
    let mut run = stubbed("watched-once", &stub)
        .settings(json!({
            "instruction_file": notes,
            "permission_rules": [{ "tool": "Agent", "action": "allow" }],
            "hooks_config": { "FileChanged": [ledger.hook()] },
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "a1",
                "Agent",
                json!({ "prompt": "SUB-MARK one", "description": "one" }),
            ),
            Reply::calls(
                "a2",
                "Agent",
                json!({ "prompt": "SUB-MARK two", "description": "two" }),
            ),
            Reply::text("done"),
        ],
    );
    stub.script_when(
        "SUB-MARK",
        [Reply::text("sub done"), Reply::text("sub done")],
    );
    run.turn(&session, "PARENT-MARK go").await?;
    assert!(ledger.entries().is_empty(), "{:?}", ledger.names());

    std::fs::write(&notes, "# Rules\nBe very brief.\n")?;
    let changed = ledger
        .wait_for("FileChanged", Duration::from_secs(30))
        .await?;
    assert_eq!(changed["session_id"], session.as_str(), "{changed}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(ledger.names(), ["FileChanged"], "one edit, one event");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn every_payload_names_the_sessions_cwd_and_permission_mode() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let mut run = stubbed("payloads", &stub)
        .settings(json!({
            "permission_mode": "default",
            "permission_rules": [{ "tool": "Agent", "action": "allow" }],
            "hooks_config": ledger.on_every_event(),
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let _watcher = run.watch(&session, true).await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls("b1", "Bash", json!({ "command": "echo made > made.txt" })),
            Reply::calls(
                "a1",
                "Agent",
                json!({ "prompt": "say hi", "description": "helper" }),
            ),
            Reply::text("done"),
        ],
    );
    stub.script([Reply::text("hi")]);
    run.turn(&session, "PARENT-MARK go").await?;
    let wrong: Vec<String> = ledger
        .entries()
        .iter()
        .filter(|e| e["cwd"] == "" || e["permission_mode"] == "")
        .map(|e| {
            format!(
                "{} cwd={} mode={}",
                e["hook_event_name"], e["cwd"], e["permission_mode"]
            )
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pre_tool_use_hook_that_exits_2_blocks_the_call_whatever_it_printed() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let refuse = ledger.path("refuse.sh");
    executable(
        &refuse,
        "#!/bin/sh\ncat > /dev/null\necho 'not json {{'\necho NO-BASH-TODAY >&2\nexit 2\n",
    )?;
    let mut run = stubbed("exit-code", &stub)
        .settings(json!({ "hooks_config": { "PreToolUse": [command_hook(&refuse, 30_000)] } }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "echo x > blocked.txt" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "go").await?;
    assert!(run.read_file("blocked.txt").is_none());
    let (_, out, is_error) = &events.tool_results[0];
    assert!(*is_error && out.contains("NO-BASH-TODAY"), "{out}");
    assert!(
        stub.calls()[1].messages_contain("NO-BASH-TODAY"),
        "the model was told why"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_that_fails_hangs_or_prints_garbage_changes_nothing() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let garbage = ledger.path("garbage.sh");
    executable(&garbage, "#!/bin/sh\ncat > /dev/null\necho 'not json {{'\n")?;
    let failing = ledger.path("failing.sh");
    executable(&failing, "#!/bin/sh\ncat > /dev/null\necho oops\nexit 1\n")?;
    let silent = ledger.path("silent.sh");
    executable(
        &silent,
        "#!/bin/sh\ncat > /dev/null\necho went-wrong >&2\nexit 3\n",
    )?;
    let pid = ledger.path("hang.pid");
    let hang = ledger.path("hang.sh");
    executable(
        &hang,
        &format!(
            "#!/bin/sh\necho $$ > '{}'\ncat > /dev/null\nexec sleep 30\n",
            pid.display()
        ),
    )?;
    let mut run = stubbed("bad-hooks", &stub)
        .settings(json!({
            "hooks_config": {
                "PreToolUse": [
                    command_hook(&garbage, 30_000),
                    command_hook(&failing, 30_000),
                    command_hook(&silent, 30_000),
                    command_hook(&hang, 3000),
                    ledger.hook()
                ]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "echo ran > ran.txt" })),
        Reply::text("carried on"),
    ]);
    let started = Instant::now();
    let events = run.turn(&session, "go").await?;
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the hang was cut short"
    );
    assert_eq!(
        run.read_file("ran.txt").as_deref(),
        Some("ran\n"),
        "{:?}",
        events.tool_results
    );
    assert!(events.text.contains("carried on") && events.turn_complete);
    assert_eq!(
        ledger.names(),
        ["PreToolUse"],
        "the well-behaved hook beside them ran"
    );

    // On a loaded machine the hook can be killed before it writes its pid;
    // then nothing of it is left to check.
    let pid = std::fs::read_to_string(&pid).unwrap_or_default();
    let pid = pid.trim();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pid.is_empty() {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()?
            .success();
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the timed-out hook {pid} is still running"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_hook_with_on_error_block_blocks_the_call() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let hang = ledger.path("hang.sh");
    executable(&hang, "#!/bin/sh\ncat > /dev/null\nexec sleep 30\n")?;
    let failing = ledger.path("failing.sh");
    executable(&failing, "#!/bin/sh\ncat > /dev/null\nexit 1\n")?;
    let mut hung = command_hook(&hang, 1000);
    hung["on_error"] = json!("block");
    hung["if"] = json!("Bash(echo one*)");
    let mut broken = command_hook(&failing, 30_000);
    broken["on_error"] = json!("block");
    broken["if"] = json!("Bash(echo two*)");
    let mut run = stubbed("on-error-block", &stub)
        .settings(json!({ "hooks_config": { "PreToolUse": [hung, broken] } }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "echo one > one.txt" })),
        Reply::calls("b2", "Bash", json!({ "command": "echo two > two.txt" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "go").await?;
    assert!(run.read_file("one.txt").is_none() && run.read_file("two.txt").is_none());
    let (_, timed_out, is_error) = &events.tool_results[0];
    assert!(
        *is_error && timed_out.contains("hang.sh") && timed_out.contains("timed out"),
        "{timed_out}"
    );
    let (_, exited, is_error) = &events.tool_results[1];
    assert!(
        *is_error && exited.contains("failing.sh") && exited.contains("exited 1"),
        "{exited}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn every_hook_on_an_event_runs_the_first_rewrite_wins_and_a_block_wins_over_both(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let rewrite_to = |name: &str, target: &str| -> anyhow::Result<PathBuf> {
        let script = ledger.path(name);
        executable(
            &script,
            &format!(
                "#!/bin/sh\ncat > /dev/null\ntouch '{}'\n\
                 echo '{{\"updated_input\": {{\"file_path\": \"{target}\", \"content\": \"rewritten\"}}}}'\n",
                ledger.path(&format!("{name}.ran")).display()
            ),
        )?;
        Ok(script)
    };
    let first = rewrite_to("first.sh", "first.txt")?;
    let second = rewrite_to("second.sh", "second.txt")?;
    let block = ledger.path("block.sh");
    executable(
        &block,
        "#!/bin/sh\ncat > /dev/null\necho '{\"decision\": \"block\", \"message\": \"BASH-BLOCKED\"}'\n",
    )?;
    let mut run = stubbed("many-hooks", &stub)
        .settings(json!({
            "hooks_config": {
                "PreToolUse": [
                    { "type": "command", "command": first, "if": "Write" },
                    { "type": "command", "command": second, "if": "Write" },
                    ledger.hook(),
                    { "type": "command", "command": block, "if": "Bash" }
                ]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::calls(
            "w1",
            "Write",
            json!({ "file_path": "original.txt", "content": "original" }),
        ),
        Reply::calls("b1", "Bash", json!({ "command": "echo x > bash.txt" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "go").await?;
    assert_eq!(
        run.read_file("first.txt").as_deref(),
        Some("rewritten"),
        "{:?}",
        events.tool_results
    );
    assert!(run.read_file("second.txt").is_none() && run.read_file("original.txt").is_none());
    assert!(ledger.path("first.sh.ran").exists() && ledger.path("second.sh.ran").exists());
    let seen = ledger.entries();
    assert_eq!(
        seen[0]["tool_input"]["file_path"], "original.txt",
        "hooks see the model's input"
    );
    assert!(run.read_file("bash.txt").is_none());
    assert!(
        events.tool_results[1].1.contains("BASH-BLOCKED"),
        "{:?}",
        events.tool_results
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_prompt_submit_hook_refuses_one_message_and_rewrites_another() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let screen = ledger.path("screen.sh");
    executable(
        &screen,
        "#!/bin/sh\npayload=$(cat)\ncase \"$payload\" in\n  \
         *FORBIDDEN*) echo '{\"decision\": \"block\", \"message\": \"REFUSED-BY-HOOK\"}' ;;\n  \
         *) echo '{\"updated_input\": \"REWRITTEN-BY-HOOK\"}' ;;\nesac\n",
    )?;
    let mut run = stubbed("prompt-screen", &stub)
        .settings(
            json!({ "hooks_config": { "UserPromptSubmit": [command_hook(&screen, 30_000)] } }),
        )
        .start()
        .await?;
    let session = run.session().await?;
    let refused = run.turn(&session, "FORBIDDEN request").await?;
    let ended = refused.turn_complete_event.clone().unwrap_or_default();
    assert_eq!(ended["stop_reason"], "stopped_by_hook", "{ended}");
    assert_eq!(ended["message"], "REFUSED-BY-HOOK", "{ended}");
    assert_eq!(
        stub.call_count(),
        0,
        "a refused message never reaches the model"
    );

    stub.script([Reply::text("ok")]);
    run.turn(&session, "ORIGINAL-WORDING please").await?;
    let sent = &stub.calls()[0];
    assert!(sent.messages_contain("REWRITTEN-BY-HOOK"));
    assert!(!sent.messages_contain("ORIGINAL-WORDING"));
    assert!(
        !sent.messages_contain("FORBIDDEN"),
        "the refused message is not in the transcript"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_post_tool_use_hook_withholds_a_result_or_ends_the_turn() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let withhold = ledger.path("withhold.sh");
    executable(
        &withhold,
        "#!/bin/sh\ncat > /dev/null\necho '{\"decision\": \"block\", \"message\": \"RESULT-WITHHELD\"}'\n",
    )?;
    let end = ledger.path("end.sh");
    executable(
        &end,
        "#!/bin/sh\ncat > /dev/null\necho '{\"continue\": false}'\n",
    )?;
    let mut run = stubbed("post-hooks", &stub)
        .settings(json!({
            "hooks_config": {
                "PostToolUse": [
                    { "type": "command", "command": withhold, "if": "Read" },
                    { "type": "command", "command": end, "if": "Bash" },
                    ledger.hook()
                ]
            }
        }))
        .start()
        .await?;
    run.write_file("secret.txt", "SECRET-IN-FILE-881\n")?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("r1", "Read", json!({ "file_path": "secret.txt" })),
        Reply::calls("b1", "Bash", json!({ "command": "echo ran > ran.txt" })),
        Reply::text("NEVER-SENT"),
    ]);
    let events = run.turn(&session, "go").await?;
    let seen = ledger.entries();
    assert!(
        seen[0]["tool_result"]
            .as_str()
            .is_some_and(|r| r.contains("SECRET-IN-FILE-881")),
        "PostToolUse sees what the tool returned: {}",
        seen[0]
    );
    let second = &stub.calls()[1];
    assert!(second.messages_contain("RESULT-WITHHELD"));
    assert!(
        !second.messages_contain("SECRET-IN-FILE-881"),
        "the model never sees it"
    );
    assert_eq!(
        run.read_file("ran.txt").as_deref(),
        Some("ran\n"),
        "the tool had already run"
    );
    assert_eq!(
        stub.call_count(),
        2,
        "continue:false ended the turn after the call"
    );
    assert!(!events.text.contains("NEVER-SENT"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_once_hook_runs_once_per_session() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let mut run = stubbed("once", &stub)
        .settings(json!({
            "hooks_config": {
                "PreToolUse": [{ "type": "command", "command": ledger.script(), "once": true }]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "true" })),
        Reply::calls("b2", "Bash", json!({ "command": "true" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "go").await?;
    assert_eq!(ledger.names(), ["PreToolUse"]);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_hook_that_blocks_sends_the_model_back_once() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let ledger = Ledger::new()?;
    let marker = ledger.path("sent-back");
    let keep_going = ledger.path("keep-going.sh");
    executable(
        &keep_going,
        &format!(
            "#!/bin/sh\ncat > /dev/null\nif [ -f '{m}' ]; then echo '{{}}'; exit 0; fi\ntouch '{m}'\n\
             echo '{{\"decision\": \"block\", \"message\": \"KEEP-GOING: write done.txt\"}}'\n",
            m = marker.display()
        ),
    )?;
    let mut run = stubbed("stop-block", &stub)
        .settings(json!({ "hooks_config": { "Stop": [command_hook(&keep_going, 30_000)] } }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::text("I am done"),
        Reply::calls(
            "w1",
            "Write",
            json!({ "file_path": "done.txt", "content": "done" }),
        ),
        Reply::text("now done"),
    ]);
    run.turn(&session, "go").await?;
    assert!(stub
        .calls()
        .get(1)
        .is_some_and(|c| c.messages_contain("KEEP-GOING")));
    assert_eq!(run.read_file("done.txt").as_deref(), Some("done"));
    assert_eq!(stub.call_count(), 3, "sent back once, not forever");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_hook_asks_a_model_and_its_verdict_blocks_the_call() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("prompt-hook", &stub)
        .settings(json!({
            "hooks_config": {
                "PreToolUse": [{
                    "type": "prompt",
                    "prompt": "HOOK-JUDGE: answer with a JSON hook response for this tool call."
                }]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "HOOK-JUDGE",
        [Reply::text(
            "```json\n{\"decision\": \"block\", \"message\": \"JUDGE-SAYS-NO\"}\n```",
        )],
    );
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "echo x > judged.txt" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "go").await?;
    assert!(run.read_file("judged.txt").is_none());
    assert!(
        events.tool_results[0].1.contains("JUDGE-SAYS-NO"),
        "{:?}",
        events.tool_results
    );
    let judged = stub
        .calls()
        .into_iter()
        .find(|c| c.system_contains("HOOK-JUDGE"))
        .expect("the hook's own model call");
    assert!(
        !judged.has_tools(),
        "a prompt hook's call is one tool-less question"
    );
    assert!(
        judged.messages_contain("echo x > judged.txt"),
        "it is shown the call"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_hook_runs_a_sub_agent_whose_verdict_blocks_the_call() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("agent-hook", &stub)
        .settings(json!({
            "hooks_config": {
                "PreToolUse": [{
                    "type": "agent",
                    "prompt": "AGENT-JUDGE: decide whether this call may run; answer {\"ok\": bool, \"reason\": str}.",
                    "timeout": 30000
                }]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "AGENT-JUDGE",
        [Reply::text(
            "{\"ok\": false, \"reason\": \"AGENT-SAYS-NO\"}",
        )],
    );
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "echo x > judged.txt" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "go").await?;
    assert!(run.read_file("judged.txt").is_none());
    assert!(
        events.tool_results[0].1.contains("AGENT-SAYS-NO"),
        "{:?}",
        events.tool_results
    );
    run.finish().await;
    Ok(())
}

async fn http_hook_hits(host: &str) -> anyhow::Result<usize> {
    let stub = ProviderStub::start().await?;
    let pages = PageServer::start().await?;
    pages.route(
        "/hook",
        Page::json(&json!({ "decision": "block", "message": "HTTP-HOOK-SAYS-NO" })),
    );
    let local = pages.url("/hook");
    let url = local.replacen("127.0.0.1", host, 1);
    let mut run = stubbed("http-hook", &stub)
        .settings(json!({
            "hooks_config": { "PreToolUse": [{ "type": "http", "url": url, "timeout": 3000 }] }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "echo x > ran.txt" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "go").await?;
    let hits = pages.hits("/hook");
    if hits == 0 {
        assert_eq!(
            run.read_file("ran.txt").as_deref(),
            Some("x\n"),
            "a refused hook neither blocks nor stops the turn: {:?}",
            events.tool_results
        );
    }
    run.finish().await;
    Ok(hits)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_http_hook_pointed_at_this_machine_is_refused_and_the_turn_goes_on() -> anyhow::Result<()>
{
    assert_eq!(http_hook_hits("127.0.0.1").await?, 0);
    assert_eq!(http_hook_hits("localhost").await?, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_http_hook_cannot_reach_this_machine_by_another_spelling() -> anyhow::Result<()> {
    for host in [
        "0.0.0.0",
        "[::ffff:127.0.0.1]",
        "[::ffff:7f00:1]",
        "127.1",
        "2130706433",
        "LOCALHOST.",
        "hook@127.0.0.1",
        "user:pass@localhost",
    ] {
        assert_eq!(http_hook_hits(host).await?, 0, "{host}");
    }
    Ok(())
}
