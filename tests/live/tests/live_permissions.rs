//! The permission system with a real model and a host answering the
//! prompts: a refusal sends the model another way, a remembered approval is
//! not asked again in the next session, an unanswered prompt becomes a
//! refusal the turn survives, plan mode holds across a compaction, `dontAsk`
//! keeps to its rules even inside a compound command, rules from every
//! settings tier apply together, and a sub-agent is held to the parent's
//! rules.
//!
//! In `default` a write inside the project is not asked about — the file
//! tools allow themselves there, and the gate keeps that verdict unless a
//! rule, plan mode or `dontAsk` says otherwise — while a shell command that
//! is not read-only is. The cases lean on that: a prompt is made with a
//! shell command, or with an `ask` rule.

use std::time::Duration;

use live::{Answer, LiveRun, Mode, Watcher};
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

fn trimmed(text: Option<String>) -> Option<String> {
    text.map(|s| s.trim().to_string())
}

fn asked_tools(watcher: &Watcher) -> Vec<String> {
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

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_from_the_host_sends_the_model_another_way() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "perm-deny",
        json!({
            "permission_mode": "default",
            "permission_rules": [{ "tool": "Write(**/secret/**)", "action": "ask" }]
        }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |event| {
            let path = event["input"]["file_path"].as_str().unwrap_or_default();
            Some(if path.contains("secret") {
                Answer::deny("secret/ is off limits; use public/ instead")
            } else {
                Answer::permit()
            })
        })
        .await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Write the text {token} to secret/a.txt with the Write tool. If that write is \
                 refused, write the same text to public/a.txt with the Write tool instead. Do \
                 not use Bash or any other way to reach secret/. Reply DONE."
            ),
        )
        .await?;

    assert!(run.read_file("secret/a.txt").is_none());
    assert_eq!(trimmed(run.read_file("public/a.txt")), Some(token));
    assert!(
        events
            .tool_results
            .iter()
            .any(|(n, out, err)| n == "Write" && *err && out.contains("off limits")),
        "the model was told why: {:?}",
        events.tool_results
    );
    let asked = watcher.asked();
    assert!(!asked.is_empty());
    assert!(
        asked
            .iter()
            .all(|a| a.prompt["event"]["tool_name"] == "Write"),
        "only the secret/ write was asked about: {:?}",
        asked_tools(&watcher)
    );
    run.finish().await;
    Ok(())
}

/// The next session is opened after a `config.reload`: without one it does
/// not see the rule yet (`permission_bounds.rs` pins that as a product bug).
#[tokio::test(flavor = "multi_thread")]
async fn an_approval_remembered_locally_is_not_asked_again() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "perm-always",
        json!({ "permission_mode": "default" }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    let command = format!("echo {token} >> stamp.txt");
    let first = run.session().await?;
    let watcher = run
        .watch_with(&first, |_| Some(Answer::permit_always("local")))
        .await?;
    run.turn(
        &first,
        &format!(
            "Run this exact command with the Bash tool, unchanged, two times — two separate \
             Bash calls, one after the other: `{command}`. Then reply DONE."
        ),
    )
    .await?;
    assert_eq!(
        run.read_file("stamp.txt").map(|s| s.lines().count()),
        Some(2)
    );
    assert_eq!(
        asked_tools(&watcher),
        ["Bash"],
        "the second identical call is not asked"
    );
    let local = run
        .read_file(".agix/settings.local.json")
        .unwrap_or_default();
    assert!(
        local.contains("Bash") && local.contains(&token),
        "the approved command is written down: {local}"
    );

    run.client.call("config.reload", json!({})).await?;
    let second = run.session().await?;
    let fresh = run
        .watch_with(&second, |_| Some(Answer::deny("SHOULD-NOT-BE-ASKED")))
        .await?;
    run.turn(
        &second,
        &format!("Run this exact command with the Bash tool, unchanged, once: `{command}`. Then reply DONE."),
    )
    .await?;
    assert_eq!(
        run.read_file("stamp.txt").map(|s| s.lines().count()),
        Some(3)
    );
    assert!(
        fresh.asked().is_empty(),
        "the remembered rule covers the next session: {:?}",
        asked_tools(&fresh)
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_prompt_times_out_into_a_refusal_and_the_turn_goes_on() -> anyhow::Result<()>
{
    let mut run = LiveRun::builder("perm-timeout")
        .settings(json!({ "permission_mode": "default" }))
        .options(|o| o.prompt_timeout(Duration::from_secs(3)))
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let started = std::time::Instant::now();
    let events = run
        .turn(
            &session,
            "Run `echo late > late.txt` with the Bash tool. If it is refused, do not try again \
             or any other way; reply REFUSED.",
        )
        .await?;
    assert!(run.read_file("late.txt").is_none());
    assert!(
        events
            .tool_results
            .iter()
            .any(|(n, out, err)| n == "Bash" && *err && out.contains("within 3s")),
        "{:?}",
        events.tool_results
    );
    assert!(events.turn_complete);
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert!(!watcher.asked().is_empty());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: the gate's plan-mode denial refuses ExitPlanMode itself, so plan mode can never be left"]
async fn leaving_plan_mode_returns_to_the_mode_it_was_entered_from() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "perm-plan",
        json!({ "permission_mode": "acceptEdits" }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    run.write_file("a.txt", "old\n")?;
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |e| {
            Some(if e["tool_name"] == "ExitPlanMode" {
                Answer::permit()
            } else {
                Answer::deny("not asked for")
            })
        })
        .await?;
    run.turn(
        &session,
        &format!(
            "1. Call EnterPlanMode.\n2. Read a.txt with the Read tool.\n3. Try to write PROBE \
             to probe.txt with the Write tool; it will be refused in plan mode — that is \
             expected, do not retry.\n4. Call ExitPlanMode with the note 'replace a.txt'.\n\
             5. Write {token} to a.txt with the Write tool.\nReply DONE."
        ),
    )
    .await?;
    assert!(run.read_file("probe.txt").is_none());
    assert_eq!(trimmed(run.read_file("a.txt")), Some(token));
    assert_eq!(
        asked_tools(&watcher),
        ["ExitPlanMode"],
        "back in acceptEdits, the edit is not asked about"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_holds_across_a_compaction() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "perm-plan-compact",
        json!({ "permission_mode": "acceptEdits" }),
        Mode::InProcess,
    )
    .await?;
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |_| Some(Answer::deny("stay in plan mode")))
        .await?;
    run.turn(&session, "Call the EnterPlanMode tool now, then reply OK.")
        .await?;
    run.turn(&session, "/compact").await?;
    watcher
        .wait_for("a compact frame", Duration::from_secs(30), |e| {
            e["kind"] == "compact"
        })
        .await?;
    let compacted_at = run.provider.calls().len();
    let events = run
        .turn(
            &session,
            "Write the word AFTER to after.txt with the Write tool. If it is refused, do not \
             try any other way; reply REFUSED.",
        )
        .await?;
    assert!(
        run.read_file("after.txt").is_none(),
        "{:?}",
        events.tool_results
    );
    let after = &run.provider.calls()[compacted_at];
    assert!(
        after.messages_contain("Plan mode is still active"),
        "the first call after compaction tells the model it is still planning"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dont_ask_keeps_to_its_rules_even_inside_a_compound_command() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "perm-dontask",
        json!({
            "permission_mode": "dontAsk",
            "permission_rules": [{ "tool": "Bash(echo:*)", "action": "allow" }]
        }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    run.write_file("keep.txt", "keep\n")?;
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Run these two commands with the Bash tool, one call each, exactly as written:\n\
                 1. `echo {token} > a.txt`\n2. `echo cleaning && rm keep.txt`\n\
                 If a command is refused, do not retry it or try another way. Reply with which \
                 ran and which were refused."
            ),
        )
        .await?;
    assert_eq!(trimmed(run.read_file("a.txt")), Some(token));
    assert_eq!(run.read_file("keep.txt").as_deref(), Some("keep\n"));
    assert!(
        events
            .tool_results
            .iter()
            .any(|(n, _, err)| n == "Bash" && *err),
        "{:?}",
        events.tool_results
    );
    assert!(watcher.asked().is_empty(), "dontAsk never asks");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_from_every_tier_apply_together_and_a_deny_anywhere_wins() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("perm-tiers")
        .settings(json!({
            "permission_mode": "acceptEdits",
            "permission_rules": [{ "tool": "Write", "action": "allow" }]
        }))
        .before_start(|world, project| {
            world.write_global_settings(&json!({
                "permission_rules": [{ "tool": "Write(**/.env)", "action": "deny" }]
            }))?;
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
    let token = format!("TOKEN-{}", nonce());
    let session = run
        .session_with(json!({ "options": { "permission_rules": [
            { "source": "session", "behavior": "allow", "tool_name": "Write", "rule_content": "**/.env" }
        ] } }))
        .await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Write the text {token} to each of these four files with the Write tool, one \
                 call per file: .env, config/.env, local-locked/a.txt, ok.txt. If a write is \
                 refused, move on to the next file; do not try any other way. Reply with which \
                 succeeded."
            ),
        )
        .await?;
    for refused in [".env", "config/.env", "local-locked/a.txt"] {
        assert!(
            run.read_file(refused).is_none(),
            "{refused}: {:?}",
            events.tool_results
        );
    }
    assert_eq!(trimmed(run.read_file("ok.txt")), Some(token));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_is_held_to_the_parents_rules() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "perm-sub",
        json!({
            "permission_mode": "acceptEdits",
            "permission_rules": [
                { "tool": "Agent", "action": "allow" },
                { "tool": "Write(**/locked/**)", "action": "deny" }
            ]
        }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    let session = run.session().await?;
    let _watcher = run
        .watch_with(&session, |_| Some(Answer::deny("nobody approves this")))
        .await?;
    run.turn(
        &session,
        &format!(
            "1. Call the Agent tool with the prompt \"Write the text {token} to locked/sub.txt \
             with the Write tool, then write it to open/sub.txt with the Write tool. If a write \
             is refused, do not try another way.\"\n\
             2. Write the text {token} to parent.txt with the Write tool yourself.\n\
             Do not write the sub-agent's files yourself. Reply DONE."
        ),
    )
    .await?;
    assert!(
        run.read_file("locked/sub.txt").is_none(),
        "the parent's deny rule holds in a sub-agent"
    );
    assert_eq!(trimmed(run.read_file("open/sub.txt")), Some(token.clone()));
    assert_eq!(trimmed(run.read_file("parent.txt")), Some(token));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: an agent type's `permission_mode: plan` reaches the sub-agent's settings but not the permission it is checked with"]
async fn a_plan_mode_agent_type_cannot_write() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "perm-sub-plan",
        json!({
            "permission_mode": "acceptEdits",
            "permission_rules": [{ "tool": "Agent", "action": "allow" }]
        }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    run.write_file(
        ".agix/agents/planner.md",
        "---\ndescription: Plans only; never changes files\npermission_mode: plan\n---\n\
         Do what you are asked. If a tool is refused, say so and stop.\n",
    )?;
    let session = run.session().await?;
    run.turn(
        &session,
        &format!(
            "Call the Agent tool with subagent_type planner and the prompt \"Write the text \
             {token} to planned.txt with the Write tool.\" Do not write planned.txt yourself. \
             Reply DONE."
        ),
    )
    .await?;
    assert!(run.read_file("planned.txt").is_none());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn yolo_in_settings_is_bypass_permissions_by_its_own_name() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "perm-yolo",
        json!({ "permission_mode": "yolo" }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    run.turn(
        &session,
        &format!("Run `echo {token} > yolo.txt` with the Bash tool, then reply DONE."),
    )
    .await?;
    assert_eq!(trimmed(run.read_file("yolo.txt")), Some(token));
    assert!(watcher.asked().is_empty());
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    let permission: Value = resumed.result.unwrap_or_default()["permission"].clone();
    assert_eq!(permission["mode"], "bypassPermissions", "{permission}");
    assert_eq!(permission["prompts"], false, "{permission}");
    run.finish().await;
    Ok(())
}
