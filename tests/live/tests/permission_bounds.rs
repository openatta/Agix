//! The permission gate's edges, driven through the daemon with a scripted
//! model: the model's tool call is written in the test, so what is asserted
//! is the gate's verdict alone — the file on disk, the prompt the host was or
//! was not sent, the result the model was handed. Plan mode in and out,
//! `dontAsk` with a compound command, rules from every settings tier, the
//! permission-mode aliases, an unanswered prompt, and the ways a call has
//! been seen to get past a rule it should have met: a deny rule spelled
//! differently in the shell, a path the read-only fast path does not
//! recognize as one, a team member, a skill's inline command.

use std::time::Duration;

use live::{Answer, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply, Watcher};
use serde_json::{json, Value};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        live::ModelEndpoint::model(stub),
    ))
}

fn prompted_tools(watcher: &Watcher) -> Vec<String> {
    watcher
        .asked()
        .iter()
        .map(|a| {
            a.prompt["event"]["tool_name"]
                .as_str()
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

async fn resumed_mode(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let resp = run
        .client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    Ok(resp.result.unwrap_or_default()["permission"]["mode"].clone())
}

fn write(id: &str, path: &str, content: &str) -> Reply {
    Reply::calls(
        id,
        "Write",
        json!({ "file_path": path, "content": content }),
    )
}

fn bash(id: &str, command: &str) -> Reply {
    Reply::calls(id, "Bash", json!({ "command": command }))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: the gate's plan-mode denial refuses ExitPlanMode itself, so plan mode can never be left"]
async fn exit_plan_mode_returns_to_the_mode_it_was_entered_from() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("plan-exit", &stub)
        .settings(json!({ "permission_mode": "acceptEdits" }))
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |e| {
            (e["tool_name"] == "ExitPlanMode").then(Answer::permit)
        })
        .await?;
    stub.script([
        Reply::calls("p1", "EnterPlanMode", json!({})),
        write("w1", "probe.txt", "x"),
        Reply::calls("p2", "ExitPlanMode", json!({})),
        write("w2", "after.txt", "y"),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "plan, then do").await?;
    assert!(
        run.read_file("probe.txt").is_none(),
        "{:?}",
        events.tool_results
    );
    assert_eq!(
        run.read_file("after.txt").as_deref(),
        Some("y"),
        "back in acceptEdits, an edit needs no one: {:?}",
        events.tool_results
    );
    assert_eq!(prompted_tools(&watcher), ["ExitPlanMode"]);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_exit_leaves_the_session_in_plan_mode() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("plan-stay", &stub)
        .settings(json!({ "permission_mode": "acceptEdits" }))
        .start()
        .await?;
    let session = run.session().await?;
    let _watcher = run
        .watch_with(&session, |_| Some(Answer::deny("NOT-YET")))
        .await?;
    stub.script([
        Reply::calls("p1", "EnterPlanMode", json!({})),
        Reply::calls("p2", "ExitPlanMode", json!({})),
        write("w1", "after.txt", "y"),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "plan").await?;
    assert!(events.tool_results[1].2, "{:?}", events.tool_results);
    assert!(
        run.read_file("after.txt").is_none(),
        "{:?}",
        events.tool_results
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_holds_across_a_compaction_and_the_model_is_reminded() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("plan-compact", &stub)
        .settings(json!({ "permission_mode": "acceptEdits" }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::calls("p1", "EnterPlanMode", json!({})),
        Reply::text("planning"),
    ]);
    run.turn(&session, "enter plan mode").await?;
    stub.script([Reply::text("SUMMARY: the session is planning.")]);
    run.turn(&session, "/compact").await?;
    let compacted_at = stub.call_count();
    stub.script([write("w1", "after.txt", "y"), Reply::text("done")]);
    let events = run.turn(&session, "write it").await?;

    assert!(
        run.read_file("after.txt").is_none(),
        "{:?}",
        events.tool_results
    );
    assert!(
        events.tool_results[0].1.contains("plan mode"),
        "{:?}",
        events.tool_results
    );
    let after = &stub.calls()[compacted_at];
    assert!(
        after.messages_contain("Plan mode is still active"),
        "the first call after the compaction says plan mode is on: {}",
        after.messages_text()
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dont_ask_runs_what_a_rule_allows_and_refuses_a_compound_that_hides_more(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("dontask", &stub)
        .settings(json!({
            "permission_mode": "dontAsk",
            "permission_rules": [{ "tool": "Bash(echo:*)", "action": "allow" }]
        }))
        .start()
        .await?;
    run.write_file("keep.txt", "keep\n")?;
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    stub.script([
        bash("b1", "echo allowed > a.txt"),
        bash("b2", "echo hidden && rm keep.txt"),
        bash("b3", "rm keep.txt"),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "go").await?;
    assert_eq!(run.read_file("a.txt").as_deref(), Some("allowed\n"));
    assert_eq!(run.read_file("keep.txt").as_deref(), Some("keep\n"));
    assert!(
        events.tool_results[1].2 && events.tool_results[2].2,
        "{:?}",
        events.tool_results
    );
    assert!(watcher.asked().is_empty(), "dontAsk never asks");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_from_every_tier_apply_together_and_a_deny_anywhere_wins() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("tiers", &stub)
        .settings(json!({
            "permission_mode": "acceptEdits",
            "permission_rules": [{ "tool": "Write", "action": "allow" }]
        }))
        .before_start(|world, project| {
            world.write_global_settings(&json!({
                "permission_rules": [{ "tool": "Write(**/global-locked/**)", "action": "deny" }]
            }))?;
            std::fs::create_dir_all(world.config_root("code"))?;
            std::fs::write(
                world.config_root("code").join("settings.json"),
                json!({
                    "permission_rules": [{ "tool": "Write(**/scene-locked/**)", "action": "deny" }]
                })
                .to_string(),
            )?;
            std::fs::write(
                project.join(".agix/settings.local.json"),
                json!({
                    "permission_rules": [{ "tool": "Write(**/local-locked/**)", "action": "deny" }]
                })
                .to_string(),
            )?;
            Ok(())
        })
        .start()
        .await?;
    let session = run
        .session_with(json!({ "options": { "permission_rules": [
            { "source": "session", "behavior": "allow", "tool_name": "Write" }
        ] } }))
        .await?;
    stub.script([
        write("w1", "global-locked/a.txt", "x"),
        write("w2", "scene-locked/a.txt", "x"),
        write("w3", "local-locked/a.txt", "x"),
        write("w4", "open/a.txt", "x"),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "write four").await?;
    for locked in ["global-locked", "scene-locked", "local-locked"] {
        assert!(
            run.read_file(&format!("{locked}/a.txt")).is_none(),
            "{locked}: {:?}",
            events.tool_results
        );
    }
    assert_eq!(run.read_file("open/a.txt").as_deref(), Some("x"));
    assert!(
        events.tool_results[..3]
            .iter()
            .all(|(_, out, err)| *err && out.contains("denied by rule")),
        "{:?}",
        events.tool_results
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_and_yolo_in_settings_mean_bypass_permissions() -> anyhow::Result<()> {
    for spelling in ["auto", "yolo"] {
        let stub = ProviderStub::start().await?;
        let mut run = stubbed(&format!("alias-{spelling}"), &stub)
            .settings(json!({ "permission_mode": spelling }))
            .start()
            .await?;
        let session = run.session().await?;
        let watcher = run.watch(&session, false).await?;
        stub.script([
            bash("b1", "echo aliased > aliased.txt"),
            Reply::text("done"),
        ]);
        run.turn(&session, "go").await?;
        assert_eq!(
            run.read_file("aliased.txt").as_deref(),
            Some("aliased\n"),
            "{spelling}"
        );
        assert!(watcher.asked().is_empty(), "{spelling} asked");
        assert_eq!(
            resumed_mode(&mut run, &session).await?,
            json!("bypassPermissions"),
            "{spelling} is reported under its own name"
        );
        run.finish().await;
    }
    Ok(())
}

async fn permit_always_local_then_a_new_session(reload: bool) -> anyhow::Result<usize> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("always-local", &stub)
        .settings(json!({ "permission_mode": "default" }))
        .start()
        .await?;
    let session = run.session().await?;
    let _first = run
        .watch_with(&session, |_| Some(Answer::permit_always("local")))
        .await?;
    stub.script([bash("b1", "echo one > one.txt"), Reply::text("done")]);
    run.turn(&session, "once").await?;
    let local = run
        .read_file(".agix/settings.local.json")
        .unwrap_or_default();
    assert!(local.contains("Bash(echo one > one.txt)"), "{local}");
    if reload {
        run.client.call("config.reload", json!({})).await?;
    } else {
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let other = run.session().await?;
    let second = run
        .watch_with(&other, |_| Some(Answer::deny("ASKED-AGAIN")))
        .await?;
    stub.script([bash("b2", "echo one > one.txt"), Reply::text("done")]);
    run.turn(&other, "again").await?;
    let asked = second.asked().len();
    run.finish().await;
    Ok(asked)
}

#[tokio::test(flavor = "multi_thread")]
async fn permit_always_local_holds_for_a_session_opened_after_a_reload() -> anyhow::Result<()> {
    assert_eq!(permit_always_local_then_a_new_session(true).await?, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: a rule permit_always writes to settings.local.json does not reach the next session until config.reload"]
async fn permit_always_local_holds_for_the_next_session() -> anyhow::Result<()> {
    assert_eq!(permit_always_local_then_a_new_session(false).await?, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_prompt_is_a_denial_and_the_turn_goes_on() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("timeout", &stub)
        .settings(json!({ "permission_mode": "default" }))
        .options(|o| o.prompt_timeout(Duration::from_secs(1)))
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([
        bash("b1", "echo late > late.txt"),
        Reply::text("carried on"),
    ]);
    let started = std::time::Instant::now();
    let events = run.turn(&session, "go").await?;
    assert!(run.read_file("late.txt").is_none());
    let (_, out, is_error) = &events.tool_results[0];
    assert!(*is_error && out.contains("within 1s"), "{out}");
    assert!(events.text.contains("carried on") && events.turn_complete);
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(watcher.asked().len(), 1);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn permit_always_for_the_session_is_not_written_down() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("always-session", &stub)
        .settings(json!({ "permission_mode": "default" }))
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |_| Some(Answer::permit_always("session")))
        .await?;
    stub.script([
        bash("b1", "echo one > one.txt"),
        bash("b2", "echo one > one.txt"),
        Reply::text("done"),
    ]);
    run.turn(&session, "twice").await?;
    assert_eq!(
        watcher.asked().len(),
        1,
        "the second identical call is not asked"
    );
    assert!(run.read_file(".agix/settings.local.json").is_none());

    let other = run.session().await?;
    let again = run
        .watch_with(&other, |_| Some(Answer::deny("FRESH-SESSION")))
        .await?;
    stub.script([bash("b3", "echo one > one.txt"), Reply::text("done")]);
    run.turn(&other, "again").await?;
    assert_eq!(again.asked().len(), 1, "a new session asks again");
    run.finish().await;
    Ok(())
}

/// A project that allows `Bash` and denies `rm`, under `acceptEdits`: what a
/// user who wants "anything but deleting" writes.
async fn denied_rm_spelled(spelling: &str) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("deny-rm", &stub)
        .settings(json!({
            "permission_mode": "acceptEdits",
            "permission_rules": [
                { "tool": "Bash", "action": "allow" },
                { "tool": "Bash(rm:*)", "action": "deny" }
            ]
        }))
        .start()
        .await?;
    run.write_file("victim.txt", "still here\n")?;
    let session = run.session().await?;
    stub.script([bash("b1", spelling), Reply::text("done")]);
    let events = run.turn(&session, "delete it").await?;
    assert_eq!(
        run.read_file("victim.txt").as_deref(),
        Some("still here\n"),
        "`{spelling}` got past `Bash(rm:*)`: {:?}",
        events.tool_results
    );
    assert!(events.tool_results[0].2, "{:?}", events.tool_results);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bash_deny_rule_refuses_the_plain_command_and_one_inside_a_compound() -> anyhow::Result<()>
{
    denied_rm_spelled("rm victim.txt").await?;
    denied_rm_spelled("true && rm victim.txt").await?;
    denied_rm_spelled("echo $(rm victim.txt)").await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: Bash deny rules are prefix-matched on the raw segment, so a quoted command name escapes them"]
async fn a_bash_deny_rule_refuses_a_quoted_command_name() -> anyhow::Result<()> {
    denied_rm_spelled("\"rm\" victim.txt").await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: Bash deny rules are prefix-matched on the raw segment, so a backslash-escaped command name escapes them"]
async fn a_bash_deny_rule_refuses_a_backslashed_command_name() -> anyhow::Result<()> {
    denied_rm_spelled("\\rm victim.txt").await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: Bash deny rules do not see the command a brace expansion produces"]
async fn a_bash_deny_rule_refuses_a_brace_expansion() -> anyhow::Result<()> {
    denied_rm_spelled("{rm,victim.txt}").await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: the `command` wrapper is stripped but its `-p` is not, so `command -p rm` escapes a deny rule"]
async fn a_bash_deny_rule_refuses_a_command_dash_p_wrapper() -> anyhow::Result<()> {
    denied_rm_spelled("command -p rm victim.txt").await
}

/// Under `default`, a read-only command is run without asking only when
/// every path it names is inside the project. `command` reads a file outside
/// it; the host denies anything it is asked.
async fn outside_read(command_for: impl Fn(&std::path::Path) -> String) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("outside-read", &stub)
        .settings(json!({ "permission_mode": "default" }))
        .start()
        .await?;
    let outside = run.world.root().join("outside");
    std::fs::create_dir_all(&outside)?;
    std::fs::write(outside.join("secret.txt"), "OUTSIDE-SECRET-5521\n")?;
    let command = command_for(&outside);
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |_| Some(Answer::deny("NO-OUTSIDE")))
        .await?;
    stub.script([bash("b1", &command), Reply::text("done")]);
    let events = run.turn(&session, "read it").await?;
    let (_, out, _) = &events.tool_results[0];
    assert!(
        !out.contains("OUTSIDE-SECRET-5521"),
        "`{command}` read outside the project without asking: {out}"
    );
    assert_eq!(prompted_tools(&watcher), ["Bash"], "`{command}`");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_outside_the_project_is_asked_about() -> anyhow::Result<()> {
    outside_read(|dir| format!("cat {}/secret.txt", dir.display())).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: the read-only fast path takes `$VAR/...` for a path inside the project"]
async fn a_read_through_a_variable_is_asked_about() -> anyhow::Result<()> {
    outside_read(|dir| format!("HOME={}; cat $HOME/secret.txt", dir.display())).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: the read-only fast path does not follow `cd $VAR`, so a relative read after it counts as inside the project"]
async fn a_read_after_cd_to_a_variable_is_asked_about() -> anyhow::Result<()> {
    outside_read(|dir| format!("D={}; cd $D && cat secret.txt", dir.display())).await
}

fn team_of_one(prompt: &str, mode: &str) -> Reply {
    Reply::calls(
        "t1",
        "TeamCreate",
        json!({
            "name": "crew",
            "permission_mode": mode,
            "agents": [{ "label": "solo", "prompt": prompt }]
        }),
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: team members are given an empty rule set, so the parent's deny rules do not reach them"]
async fn a_team_member_is_held_to_the_parents_deny_rules() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("team-rules", &stub)
        .settings(json!({
            "permission_mode": "acceptEdits",
            "execution": { "team_enabled": true },
            "permission_rules": [{ "tool": "Write(**/locked/**)", "action": "deny" }]
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            team_of_one("MEMBER-MARK write the file", "acceptEdits"),
            Reply::text("team done"),
        ],
    );
    stub.script_when(
        "MEMBER-MARK",
        [write("m1", "locked/member.txt", "x"), Reply::text("wrote")],
    );
    run.turn(&session, "PARENT-MARK run the team").await?;
    assert!(run.read_file("locked/member.txt").is_none());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_member_is_no_freer_than_its_parent() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("team-mode", &stub)
        .settings(json!({
            "permission_mode": "default",
            "execution": { "team_enabled": true }
        }))
        .options(|o| o.prompt_timeout(Duration::from_secs(2)))
        .start()
        .await?;
    let session = run.session().await?;
    let _watcher = run
        .watch_with(&session, |_| Some(Answer::deny("NOT-THE-TEAM")))
        .await?;
    stub.script_when(
        "PARENT-MARK",
        [
            team_of_one("MEMBER-MARK run the command", "bypassPermissions"),
            Reply::text("team done"),
        ],
    );
    stub.script_when(
        "MEMBER-MARK",
        [
            bash("m1", "echo escalated > escalated.txt"),
            Reply::text("ran"),
        ],
    );
    run.turn(&session, "PARENT-MARK run the team").await?;
    assert!(
        run.read_file("escalated.txt").is_none(),
        "a member of a `default` session wrote with a shell command nobody approved"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: a skill's inline `!cmd` runs through BashTool::call directly, past the permission gate"]
async fn a_skills_inline_command_is_held_to_the_bash_rules() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-cmd", &stub)
        .settings(json!({
            "permission_mode": "acceptEdits",
            "permission_rules": [
                { "tool": "Skill", "action": "allow" },
                { "tool": "Bash", "action": "deny" }
            ]
        }))
        .start()
        .await?;
    run.write_file(
        ".agents/skills/stamp/SKILL.md",
        "---\nname: stamp\ndescription: stamps the project\n---\n\
         Context: !`echo stamped > skill-ran.txt`\n",
    )?;
    let session = run.session().await?;
    stub.script([
        Reply::calls("s1", "Skill", json!({ "skill": "stamp" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "use the skill").await?;
    assert!(
        run.read_file("skill-ran.txt").is_none(),
        "the skill's command ran although every Bash call is denied: {:?}",
        events.tool_results
    );
    run.finish().await;
    Ok(())
}

/// Whether a sub-agent, of `agent_type` or none, under an `acceptEdits`
/// parent that denies `locked/`, manages to write `path`.
async fn sub_agent_writes(agent_type: Option<&str>, path: &str) -> anyhow::Result<bool> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("sub-agent", &stub)
        .settings(json!({
            "permission_mode": "acceptEdits",
            "permission_rules": [
                { "tool": "Agent", "action": "allow" },
                { "tool": "Write(**/locked/**)", "action": "deny" }
            ]
        }))
        .start()
        .await?;
    run.write_file(
        ".agix/agents/planner.md",
        "---\ndescription: Plans only\npermission_mode: plan\n---\nPlan.\n",
    )?;
    let session = run.session().await?;
    let _watcher = run
        .watch_with(&session, |_| Some(Answer::deny("NOBODY")))
        .await?;
    let mut input = json!({ "prompt": "write the file", "description": "writer" });
    if let Some(t) = agent_type {
        input["subagent_type"] = json!(t);
    }
    stub.script_when(
        "PARENT-MARK",
        [Reply::calls("a1", "Agent", input), Reply::text("done")],
    );
    stub.script([
        write("s1", path, "from the sub-agent"),
        Reply::text("wrote"),
    ]);
    run.turn(&session, "PARENT-MARK delegate").await?;
    let written = run.read_file(path).is_some();
    run.finish().await;
    Ok(written)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_is_held_to_the_parents_deny_rules() -> anyhow::Result<()> {
    assert!(!sub_agent_writes(None, "locked/sub.txt").await?);
    assert!(
        sub_agent_writes(None, "open/sub.txt").await?,
        "and may write elsewhere"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: an agent type's `permission_mode: plan` reaches the sub-agent's settings but not the permission it is checked with"]
async fn a_plan_mode_agent_type_cannot_write() -> anyhow::Result<()> {
    assert!(!sub_agent_writes(Some("planner"), "planned.txt").await?);
    Ok(())
}
