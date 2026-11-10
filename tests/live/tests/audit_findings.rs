//! Defects an architecture review inferred from reading the runtime and the
//! extension points, each reproduced from outside: a client over JSON-RPC
//! and a scripted model. They stay ignored until fixed, then become the
//! regression cases for those fixes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use live::{
    LiveConfig, LiveRun, LiveRunBuilder, McpToy, ProviderStub, Purpose, Reply, SeenRequest,
};
use serde_json::{json, Value};

const SUMMARY: &str = "You are compacting an AI coding agent's conversation";
const TOO_LONG: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 212034 tokens > 200000 maximum"}}"#;

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        live::ModelEndpoint::model(stub),
    ))
}

fn turns_with(stub: &ProviderStub, mark: &str, not: &[&str]) -> Vec<SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .filter(|c| c.messages_contain(mark) && not.iter().all(|n| !c.messages_contain(n)))
        .collect()
}

fn tool_result_in(calls: &[SeenRequest], tool_use_id: &str) -> Option<Value> {
    calls.iter().find_map(|c| {
        c.body["messages"]
            .as_array()?
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == tool_use_id)
            .cloned()
    })
}

fn executable(dir: &Path, name: &str, body: &str) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

async fn wait_until(what: &str, timeout: Duration, done: impl Fn() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while !done() {
        anyhow::ensure!(Instant::now() < deadline, "{what} never happened");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

async fn interrupt(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    Ok(resp.result.unwrap_or_default())
}

async fn runtime_of(run: &mut LiveRun, session: &str, name: &str) -> anyhow::Result<Value> {
    let archive = run.world.root().join(format!("export-{name}"));
    let exported = run
        .client
        .session_export(session, &archive, Some("dir"), None)
        .await?;
    anyhow::ensure!(exported.error.is_none(), "{exported:?}");
    Ok(serde_json::from_str(&std::fs::read_to_string(
        archive.join("sessions").join(session).join("runtime.json"),
    )?)?)
}

/// `rounds` rounds of file reads, so a compaction has something to summarize.
async fn a_few_rounds(
    run: &mut LiveRun,
    stub: &ProviderStub,
    session: &str,
    rounds: usize,
) -> anyhow::Result<()> {
    let mut script = Vec::new();
    for i in 0..rounds {
        let body: String = (0..200)
            .map(|l| format!("file {i} line {l} of filler\n"))
            .collect();
        let path = run.write_file(&format!("f{i}.txt"), &body)?;
        script.push(Reply::calls(
            &format!("r{i}"),
            "Read",
            json!({ "file_path": path.to_string_lossy() }),
        ));
    }
    script.push(Reply::text("read them"));
    stub.script(script);
    run.turn(session, "read the files").await?;
    Ok(())
}

/// An agent type's `mcp_servers` grant: the sub-agent is advertised the
/// granted server's tools from its first request on, and can call them in
/// every round, not only the first.
#[tokio::test(flavor = "multi_thread")]
async fn audit_a_sub_agent_granted_an_mcp_server_is_shown_and_keeps_its_tools() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let log = scratch.path().join("calls.log");
    let mut run = stubbed("mcp-grant", &stub)
        .settings(json!({
            "mcp_servers": { "toy": McpToy::stdio_config(&["--calls", &log.to_string_lossy()])? },
            "permission_rules": [
                { "tool": "Agent", "action": "allow" },
                { "tool": "mcp__toy__ping", "action": "allow" }
            ]
        }))
        .before_start(|world, _| {
            world.write_project_file(
                "mcp-grant",
                ".agix/agents/pinger.md",
                "---\ndescription: pings the toy server\nmcp_servers: [toy]\n---\nYou ping.\n",
            )?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "a-1",
                "Agent",
                json!({
                    "prompt": "CHILD-MARK ping the toy server twice",
                    "description": "ping",
                    "subagent_type": "pinger"
                }),
            ),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "CHILD-MARK",
        [
            Reply::calls("c-ping-1", "mcp__toy__ping", json!({ "message": "first" })),
            Reply::calls("c-ping-2", "mcp__toy__ping", json!({ "message": "second" })),
            Reply::text("child done"),
        ],
    );
    run.turn(&session, "PARENT-MARK delegate the pings").await?;

    let parent = turns_with(&stub, "PARENT-MARK", &[]);
    assert!(
        parent[0].tool_names().iter().any(|n| n == "mcp__toy__ping"),
        "the parent itself has no toy tools, so this case proves nothing: {:?}",
        parent[0].tool_names()
    );
    let child = turns_with(&stub, "CHILD-MARK", &["PARENT-MARK"]);
    assert!(child.len() >= 3, "the child made {} requests", child.len());
    let advertised: Vec<String> = child[0]
        .tool_names()
        .into_iter()
        .filter(|n| n.starts_with("mcp__toy__"))
        .collect();
    let pings = McpToy::calls(&log)
        .into_iter()
        .filter(|(_, tool)| tool == "ping")
        .count();
    let second = tool_result_in(&child, "c-ping-2");
    assert!(
        !advertised.is_empty() && pings == 2,
        "the child's first request advertised toy tools {advertised:?}; the toy server saw \
         {pings} ping(s); the second ping answered {second:?}"
    );
    run.finish().await;
    Ok(())
}

/// A `context: fork` skill with `agent: explore` runs on the explore type's
/// read-only tools, and at the depth limit has no `Agent` — not the parent's,
/// either, which would count the grandchild against the parent's depth.
#[tokio::test(flavor = "multi_thread")]
async fn audit_a_forked_skill_gets_its_agent_types_tools_and_no_parents_agent() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("fork-explore", &stub)
        .settings(json!({
            "execution": { "max_agent_depth": 1 },
            "permission_rules": [
                { "tool": "Skill", "action": "allow" },
                { "tool": "Agent", "action": "allow" }
            ]
        }))
        .start()
        .await?;
    run.write_file(
        ".agents/skills/looker/SKILL.md",
        "---\nname: looker\ndescription: looks around\ncontext: fork\nagent: explore\n---\n\
         FORK-MARK look around the project\n",
    )?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls("s-1", "Skill", json!({ "skill": "looker" })),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "FORK-MARK",
        [
            Reply::calls(
                "f-1",
                "Agent",
                json!({ "prompt": "GRANDCHILD-MARK go deeper", "description": "deeper" }),
            ),
            Reply::text("fork done"),
        ],
    );
    stub.script_when("GRANDCHILD-MARK", [Reply::text("grandchild done")]);
    run.turn(&session, "PARENT-MARK use the looker skill")
        .await?;

    let fork = turns_with(&stub, "FORK-MARK", &["PARENT-MARK"]);
    assert!(!fork.is_empty(), "the skill never forked");
    let names = fork[0].tool_names();
    let mut problems = Vec::new();
    let writers: Vec<&String> = names
        .iter()
        .filter(|n| ["Write", "Edit", "Bash"].contains(&n.as_str()))
        .collect();
    if !writers.is_empty() {
        problems.push(format!("the explore fork was offered {writers:?}"));
    }
    if names.iter().any(|n| n == "Agent") {
        problems.push("the fork at the depth limit was offered Agent".to_string());
    }
    let grandchild = turns_with(&stub, "GRANDCHILD-MARK", &["FORK-MARK"]);
    if !grandchild.is_empty() {
        problems.push(format!(
            "the fork's Agent call spawned a grandchild past max_agent_depth 1 ({} request(s)); \
             its result: {:?}",
            grandchild.len(),
            tool_result_in(&fork, "f-1")
        ));
    }
    assert!(problems.is_empty(), "{problems:#?}\nfork tools: {names:?}");
    run.finish().await;
    Ok(())
}

/// A turn submitted while the engine is still between turns (here: re-running
/// an async-rewake hook) and interrupted before it starts is the turn that
/// gets interrupted — not the finished one before it, nor the background
/// command that one started.
#[tokio::test(flavor = "multi_thread")]
async fn audit_an_interrupt_sent_between_turns_reaches_the_turn_it_was_sent_for(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let (go, stalled, release) = (
        scratch.path().join("go"),
        scratch.path().join("stalled"),
        scratch.path().join("release"),
    );
    let hook = executable(
        scratch.path(),
        "rewake.sh",
        &format!(
            "#!/bin/sh\ninput=$(cat)\necho \"$input\" >> '{log}'\ncase \"$input\" in\n  *WakeEvent*)\n    touch '{stalled}'\n    \
             while [ ! -e '{release}' ]; do sleep 0.05; done\n    echo '{{}}' ;;\n  *)\n    \
             echo '{{\"rewake\":true}}' ;;\nesac\n",
            stalled = stalled.display(),
            release = release.display(),
            log = scratch.path().join("stalled.log").display(),
        ),
    )?;
    let mut run = stubbed("rewake-interrupt", &stub)
        .settings(json!({
            "hooks_config": { "SessionStart": [{
                "type": "command", "command": hook, "async_rewake": true, "timeout": 120_000
            }], "TaskCreated": [{ "type": "command", "command": hook }] },
            "permission_rules": [{ "tool": "Monitor", "action": "allow" }]
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::Blocks(vec![
            live::Block::tool(
                "m-quick",
                "Monitor",
                json!({
                    "command": format!(
                        "while [ ! -e '{}' ]; do sleep 0.05; done; echo finished",
                        go.display()
                    ),
                    "description": "quick"
                }),
            ),
            live::Block::tool(
                "m-long",
                "Monitor",
                json!({ "command": "sleep 600", "description": "long" }),
            ),
        ]),
        Reply::text("t1 done"),
    ]);
    run.turn(&session, "start two monitors").await?;
    let task_of = |tasks: &[Value], description: &str| {
        tasks
            .iter()
            .find(|t| t.to_string().contains(description))
            .cloned()
    };
    let tasks = run.tasks(&session).await?;
    anyhow::ensure!(
        task_of(&tasks, "long").is_some() && task_of(&tasks, "quick").is_some(),
        "the monitors did not start: {tasks:?}"
    );
    let log = scratch.path().join("stalled.log");
    wait_until(
        "the quick monitor announced",
        Duration::from_secs(20),
        || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .any(|l| l.contains("TaskCreated") && l.contains("quick"))
        },
    )
    .await?;
    std::fs::write(&go, "")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let tasks = run.tasks(&session).await?;
        if task_of(&tasks, "quick").is_some_and(|t| t["status"] != "running") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the quick monitor never ended");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    stub.script([Reply::text("T2-ANSWER")]);
    let second = run
        .spawn_turn(&session, "T2-MARK filler", json!({}))
        .await?;
    wait_until("the rewake hook stalling", Duration::from_secs(20), || {
        stalled.exists()
    })
    .await?;
    let (handle, mark) = if turns_with(&stub, "T2-MARK", &[]).is_empty() {
        (second, "T2-MARK")
    } else {
        second.await??;
        stub.script([Reply::text("T3-ANSWER")]);
        (
            run.spawn_turn(&session, "T3-MARK the turn to interrupt", json!({}))
                .await?,
            "T3-MARK",
        )
    };
    run.wait_turn_state(&session, "running", Duration::from_secs(10))
        .await?;
    let interrupted = interrupt(&mut run, &session).await?;
    assert_eq!(interrupted["interrupted"], json!(true), "{interrupted}");
    std::fs::write(&release, "")?;
    let events = handle.await??;

    let mut problems = Vec::new();
    let stop = events
        .turn_complete_event
        .as_ref()
        .map(|e| e["stop_reason"].clone())
        .unwrap_or_default();
    if !turns_with(&stub, mark, &[]).is_empty() && stop == "end_turn" {
        problems.push(format!(
            "the interrupted turn ({mark}) still reached the model and ended {stop}: {:?}",
            events.text
        ));
    }
    let long = task_of(&run.tasks(&session).await?, "long");
    if long.as_ref().is_none_or(|t| t["status"] != "running") {
        problems.push(format!(
            "the previous turn's monitor was stopped by this interrupt: {long:?}"
        ));
    }
    assert!(problems.is_empty(), "{problems:#?}");
    run.finish().await;
    Ok(())
}

/// A size refusal sets off a compaction before the retry; a host interrupt
/// while its summary is being written ends the turn the way any interrupt
/// does, not with an engine error.
#[tokio::test(flavor = "multi_thread")]
async fn audit_an_interrupt_during_the_compaction_for_a_size_refusal_is_not_an_engine_error(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("retry-interrupt", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session, 12).await?;
    stub.script_when(
        SUMMARY,
        [Reply::delayed(
            Duration::from_secs(60),
            Reply::text("SUMMARY-TOO-LATE"),
        )],
    );
    stub.inject_failures(1, Reply::status(400, TOO_LONG));
    stub.script([Reply::text("ANSWERED-AFTER-COMPACTING")]);
    let handle = run.spawn_turn(&session, "and now?", json!({})).await?;
    wait_until("a summary request", Duration::from_secs(30), || {
        stub.calls().iter().any(|c| c.purpose() == Purpose::Compact)
    })
    .await?;
    let interrupted = interrupt(&mut run, &session).await?;
    assert_eq!(interrupted["interrupted"], json!(true), "{interrupted}");
    let events = handle.await??;
    let error = events.response.as_ref().and_then(|r| r.error.as_ref());
    assert!(
        error.is_none(),
        "the interrupted turn failed: {error:?}; turn_complete {:?}",
        events.turn_complete_event
    );
    run.finish().await;
    Ok(())
}

/// `session.compact` is a turn of its own; an interrupt while its summary is
/// being written ends it, and the late summary is never applied.
#[tokio::test(flavor = "multi_thread")]
async fn audit_an_interrupt_stops_session_compact_while_its_summary_is_written(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("compact-interrupt", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session, 4).await?;
    stub.script_when(
        SUMMARY,
        [Reply::delayed(
            Duration::from_secs(8),
            Reply::text("SUMMARY-WRITTEN-LATE"),
        )],
    );
    let mut compactor = run.daemon.connect().await?;
    let id = session.clone();
    let compacting = tokio::spawn(async move {
        compactor
            .call("session.compact", json!({ "session_id": id }))
            .await
    });
    wait_until("a summary request", Duration::from_secs(30), || {
        stub.calls().iter().any(|c| c.purpose() == Purpose::Compact)
    })
    .await?;
    let interrupted = interrupt(&mut run, &session).await?;
    assert_eq!(interrupted["interrupted"], json!(true), "{interrupted}");
    let compacted = compacting.await??;
    let history = run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "limit": 100 }),
        )
        .await?
        .result
        .unwrap_or_default()
        .to_string();
    assert!(
        !history.contains("SUMMARY-WRITTEN-LATE"),
        "the summary written after the interrupt was applied; session.compact answered {:?}",
        compacted
            .result
            .or(compacted.error.map(|e| json!(e.message)))
    );
    run.finish().await;
    Ok(())
}

/// A transcript that exists but fails to load is a failed resume, not a
/// session with no history: the turn is refused, the model is never asked,
/// and the log is left exactly as it was — no second `Meta`, no new turn.
/// Two ways to break one: a whole line that is not UTF-8 (a newline-ended
/// line was written in full, so it is not a torn tail the reader may drop),
/// and a file this process may write but not read.
#[tokio::test(flavor = "multi_thread")]
async fn audit_a_transcript_that_fails_to_load_is_not_resumed_as_an_empty_session(
) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("unloadable", &stub).start().await?;
    let breakages: [(&str, &[u8], bool); 2] = [
        ("a line that is not UTF-8", b"\xff\xfe\n", false),
        ("a write-only file", b"", true),
    ];
    for (n, (how, appended, write_only)) in breakages.into_iter().enumerate() {
        let session = run.session().await?;
        stub.script([Reply::text("first answer")]);
        run.turn(&session, &format!("FIRST-MARK-{n} hello")).await?;
        let closed = run.client.session_close(&session).await?;
        anyhow::ensure!(closed.error.is_none(), "{closed:?}");
        let transcript = run
            .world
            .transcript_dir(&run.project)
            .join(format!("{session}.jsonl"));
        anyhow::ensure!(
            transcript.exists(),
            "no transcript at {}",
            transcript.display()
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)?
            .write_all(appended)?;
        let broken = std::fs::read(&transcript)?;
        if write_only {
            std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o200))?;
            if std::fs::read(&transcript).is_ok() {
                eprintln!("skipping {how}: this process reads files it has no read permission on");
                std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o600))?;
                continue;
            }
        }

        stub.script([Reply::text("second answer")]);
        let second_mark = format!("SECOND-MARK-{n}");
        let resumed = run
            .client
            .run_turn_with(
                json!({ "session_id": session, "message": format!("{second_mark} again") }),
            )
            .await?;
        if write_only {
            std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o600))?;
        }

        let error = resumed.response.as_ref().and_then(|r| r.error.as_ref());
        assert!(
            error.is_some_and(|e| e.message.contains("could not be loaded")),
            "{how}: the turn on a transcript that does not load was not refused: {:?}",
            resumed.response
        );
        assert!(
            turns_with(&stub, &second_mark, &[]).is_empty(),
            "{how}: the model was asked to go on from a transcript that did not load"
        );
        assert!(
            std::fs::read(&transcript)? == broken,
            "{how}: the refused resume wrote to the transcript"
        );
    }
    run.finish().await;
    Ok(())
}

/// A settings change rebuilds an idle session's engine before its next turn;
/// what the session was in — plan mode, its todo list, the deferred tools it
/// loaded — is still what it is in afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn audit_a_rebuilt_session_keeps_plan_mode_todos_and_loaded_tools() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let builds = scratch.path().join("builds.log");
    let hook = executable(
        scratch.path(),
        "start.sh",
        &format!(
            "#!/bin/sh\ncat >/dev/null\necho started >> '{}'\necho '{{}}'\n",
            builds.display()
        ),
    )?;
    let mut run = stubbed("rebuilt", &stub)
        .settings(json!({
            "hooks_config": { "SessionStart": [{ "type": "command", "command": hook }] }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("ts-1", "ToolSearch", json!({ "query": "select:TodoWrite" })),
        Reply::calls(
            "td-1",
            "TodoWrite",
            json!({ "todos": [{ "content": "step one", "status": "pending", "active_form": "doing step one" }] }),
        ),
        Reply::calls("pm-1", "EnterPlanMode", json!({})),
        Reply::text("planned"),
    ]);
    run.turn(&session, "plan it").await?;
    let before = runtime_of(&mut run, &session, "before").await?;
    anyhow::ensure!(
        before["permission_mode"] == "plan"
            && before["todos"].as_array().is_some_and(|t| t.len() == 1),
        "the session never got into the state this case checks: {before}"
    );
    let starts = || {
        std::fs::read_to_string(&builds)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let starts_before = starts();

    let reloaded = run.client.config_reload(&rpc_client::Target::own()).await?;
    anyhow::ensure!(reloaded.error.is_none(), "{reloaded:?}");
    stub.script([Reply::text("after the reload")]);
    run.turn(&session, "carry on").await?;
    anyhow::ensure!(
        starts() > starts_before,
        "the session was not rebuilt ({starts_before} -> {} starts)",
        starts()
    );
    let after = runtime_of(&mut run, &session, "after").await?;

    stub.script_when(SUMMARY, [Reply::text("1. User intent — plan it.")]);
    let compacted = run
        .client
        .call("session.compact", json!({ "session_id": session }))
        .await?;
    anyhow::ensure!(compacted.error.is_none(), "{compacted:?}");
    stub.script([Reply::text("after the compaction")]);
    run.turn(&session, "LAST-MARK and now").await?;
    let last = turns_with(&stub, "LAST-MARK", &[]);
    let loaded = last.last().is_some_and(|c| {
        c.messages_contain("These deferred tools were loaded earlier")
            && c.messages_contain("TodoWrite")
    });

    let mut problems = Vec::new();
    if after["permission_mode"] != before["permission_mode"] {
        problems.push(format!(
            "permission_mode {} -> {}",
            before["permission_mode"], after["permission_mode"]
        ));
    }
    if after["todos"] != before["todos"] {
        problems.push(format!("todos {} -> {}", before["todos"], after["todos"]));
    }
    if !loaded {
        problems
            .push("the compaction after the rebuild no longer lists TodoWrite as loaded".into());
    }
    assert!(problems.is_empty(), "{problems:#?}");
    run.finish().await;
    Ok(())
}

/// `tool_progress` from a tool a program calls names a `tool_use` the host
/// was shown — the program's own, or one emitted for the sub-call.
#[tokio::test(flavor = "multi_thread")]
async fn audit_a_programs_sub_call_progress_names_a_tool_use_the_host_saw() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("program-progress", &stub)
        .settings(json!({
            "permission_rules": [
                { "tool": "Bash", "action": "allow" },
                { "tool": "RunCode", "action": "allow" }
            ]
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    let lines = "printf 'one\\ntwo\\nthree\\n'";
    stub.script([
        Reply::calls("direct-bash", "Bash", json!({ "command": lines })),
        Reply::calls(
            "program",
            "RunCode",
            json!({
                "description": "print lines",
                "code": format!("return await tools.Bash({{command: {}}});", json!(lines))
            }),
        ),
        Reply::text("done"),
    ]);
    run.turn(&session, "print some lines").await?;
    let frames = watcher.frames();
    let ids_of = |kind: &str, key: &str| -> Vec<String> {
        frames
            .iter()
            .filter(|f| f["event"]["kind"] == kind)
            .filter_map(|f| f["event"][key].as_str().map(str::to_string))
            .collect()
    };
    let used = ids_of("tool_use", "id");
    let progress = ids_of("tool_progress", "tool_use_id");
    assert!(
        progress.iter().any(|id| id == "direct-bash"),
        "even the direct Bash call reported no progress: {progress:?}"
    );
    let orphans: Vec<&String> = progress.iter().filter(|id| !used.contains(id)).collect();
    assert!(
        orphans.is_empty(),
        "tool_progress for ids no tool_use announced: {orphans:?} (tool_use ids {used:?})"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// A session the pool evicts at its cap comes back from its log on the next
/// turn. One the model put in plan mode is still in plan mode then: leaving
/// it is `ExitPlanMode`'s question to the user, not the pool's to answer by
/// dropping the engine.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "audit: confirmed defect, fix pending"]
async fn audit2_a_session_evicted_at_the_cap_comes_back_still_in_plan_mode() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("evicted-plan", &stub)
        .settings(json!({ "permission_mode": "acceptEdits" }))
        .options(|o| o.session_cap(1))
        .start()
        .await?;
    let planner = run.session().await?;
    stub.script([
        Reply::calls("pm-1", "EnterPlanMode", json!({})),
        Reply::text("planned"),
    ]);
    run.turn(&planner, "plan it").await?;
    let before = runtime_of(&mut run, &planner, "before").await?;
    anyhow::ensure!(
        before["permission_mode"] == "plan",
        "the session never got into plan mode: {before}"
    );

    let other = run.session().await?;
    stub.script([Reply::text("other answer")]);
    run.turn(&other, "something else").await?;

    let target = run.project.join("written-after-eviction.txt");
    stub.script([
        Reply::calls(
            "w-1",
            "Write",
            json!({ "file_path": target.to_string_lossy(), "content": "x" }),
        ),
        Reply::text("wrote it"),
    ]);
    run.turn(&planner, "BACK-MARK write the file now").await?;
    let after = runtime_of(&mut run, &planner, "after").await?;

    let mut problems = Vec::new();
    if after["permission_mode"] != "plan" {
        problems.push(format!(
            "permission_mode plan -> {} after the eviction",
            after["permission_mode"]
        ));
    }
    if target.exists() {
        problems.push("Write ran in a session that was in plan mode".to_string());
    }
    assert!(problems.is_empty(), "{problems:#?}");
    run.finish().await;
    Ok(())
}

/// `ARCHITECTURE.md` §3.4: a sub-agent's tools are narrowed by the caller's
/// lists, a skill's `allowed-tools` among them. A `context: fork` skill that
/// lists only `Read` forks a sub-agent that is offered `Read`, not `Write`,
/// `Edit` or `Bash`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "audit: confirmed defect, fix pending"]
async fn audit2_a_forked_skill_is_narrowed_to_its_allowed_tools() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("fork-allowed", &stub)
        .settings(json!({
            "permission_rules": [{ "tool": "Skill", "action": "allow" }]
        }))
        .start()
        .await?;
    run.write_file(
        ".agents/skills/reader/SKILL.md",
        "---\nname: reader\ndescription: reads a file\ncontext: fork\nallowed-tools: Read\n---\n\
         FORK-MARK read the readme\n",
    )?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls("s-1", "Skill", json!({ "skill": "reader" })),
            Reply::text("parent done"),
        ],
    );
    stub.script_when("FORK-MARK", [Reply::text("fork done")]);
    run.turn(&session, "PARENT-MARK use the reader skill")
        .await?;

    let fork = turns_with(&stub, "FORK-MARK", &["PARENT-MARK"]);
    anyhow::ensure!(!fork.is_empty(), "the skill never forked");
    let names = fork[0].tool_names();
    anyhow::ensure!(
        names.iter().any(|n| n == "Read"),
        "the fork has no Read at all: {names:?}"
    );
    let beyond: Vec<&String> = names
        .iter()
        .filter(|n| ["Write", "Edit", "Bash"].contains(&n.as_str()))
        .collect();
    assert!(
        beyond.is_empty(),
        "a fork of a skill with `allowed-tools: Read` was offered {beyond:?}"
    );
    run.finish().await;
    Ok(())
}

/// A `PreToolUse` agent hook is a guard, not a sub-agent the model asked
/// for: the model filling `max_concurrent_subagents` with background agents
/// does not stop the guard from being asked about the next call.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "audit: confirmed defect, fix pending"]
async fn audit2_an_agent_hook_still_judges_when_the_model_holds_every_sub_agent_slot(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("hook-slots", &stub)
        .settings(json!({
            "execution": { "max_concurrent_subagents": 1 },
            "permission_rules": [
                { "tool": "Agent", "action": "allow" },
                { "tool": "Bash", "action": "allow" }
            ],
            "hooks_config": {
                "PreToolUse": [{
                    "type": "agent",
                    "prompt": "GUARD-MARK: may this call run? answer {\"ok\": bool, \"reason\": str}.",
                    "timeout": 30000
                }]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "a-bg",
                "Agent",
                json!({ "prompt": "BG-MARK wait", "description": "bg", "background": true }),
            ),
            Reply::calls("b-1", "Bash", json!({ "command": "echo x > guarded.txt" })),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "GUARD-MARK",
        [
            Reply::text("{\"ok\": true, \"reason\": \"fine\"}"),
            Reply::text("{\"ok\": false, \"reason\": \"GUARD-SAYS-NO\"}"),
        ],
    );
    stub.script_when(
        "BG-MARK",
        [Reply::delayed(
            Duration::from_secs(120),
            Reply::text("bg done"),
        )],
    );
    let events = run
        .turn(&session, "PARENT-MARK start a helper, then run the command")
        .await?;

    let guard_asks = turns_with(&stub, "GUARD-MARK", &[]).len();
    let bash = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "Bash")
        .cloned();
    let mut problems = Vec::new();
    if run.read_file("guarded.txt").is_some() {
        problems.push(format!(
            "the command ran without its guard's verdict (guard asked {guard_asks} time(s)); \
             Bash result {bash:?}"
        ));
    }
    if guard_asks < 2 {
        problems.push(format!(
            "the guard was asked {guard_asks} time(s), not once per call"
        ));
    }
    assert!(problems.is_empty(), "{problems:#?}");
    run.finish().await;
    Ok(())
}
