//! Slash commands with a real model behind them: the built-in local ones
//! (`/help`, `/skills`, `/cost`, `/clear`, `/compact`) answer without a
//! conversation call, `commands.list` reports the same catalog with kind and
//! source, an unknown command reaches the model as the text it is, and a
//! project skill runs as a command with its arguments substituted.

use live::{LiveRun, Purpose};
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

const SKILL: &str = "\
---
name: stamp
description: Stamps a line into stamp.txt
---
Write exactly the line `$ARGUMENTS {mark}` to stamp.txt with the Write tool, then reply DONE.
";

fn stop_reason(events: &rpc_client::TurnEvents) -> Value {
    events
        .turn_complete_event
        .as_ref()
        .map(|e| e["stop_reason"].clone())
        .unwrap_or_default()
}

async fn history_total(run: &mut LiveRun, session: &str) -> anyhow::Result<u64> {
    let resp = run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "offset": 0, "limit": 1 }),
        )
        .await?;
    Ok(resp.result.unwrap_or_default()["total"]
        .as_u64()
        .unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread")]
async fn local_commands_answer_without_a_conversation_call() -> anyhow::Result<()> {
    let mark = format!("SKILL-BODY-{}", nonce());
    let mut run = LiveRun::builder("local-commands").start().await?;
    run.write_file(
        ".agents/skills/stamp/SKILL.md",
        &SKILL.replace("{mark}", &mark),
    )?;
    let session = run.session().await?;
    run.write_file("notes.txt", &"a line of notes for the record\n".repeat(150))?;
    run.turn(
        &session,
        "Read notes.txt with the Read tool and reply with its line count only.",
    )
    .await?;
    let calls = run.provider.calls().len();

    let help = run.turn(&session, "/help").await?;
    for name in ["/help", "/skills", "/clear", "/compact", "/cost", "/stamp"] {
        assert!(
            help.text.contains(name),
            "/help lacks {name}: {}",
            help.text
        );
    }
    let skills = run.turn(&session, "/skills").await?;
    assert!(
        skills.text.contains("/stamp") && skills.text.contains("Stamps a line"),
        "{}",
        skills.text
    );
    let turn_calls = run
        .provider
        .calls()
        .iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .count();
    let cost = run.turn(&session, "/cost").await?;
    let reported: usize = cost
        .text
        .strip_prefix("Since this session was loaded: ")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(
        (1..=turn_calls).contains(&reported) && cost.text.contains("input tokens"),
        "{} (the provider saw {turn_calls} turn calls)",
        cost.text
    );
    for events in [&help, &skills, &cost] {
        assert_eq!(stop_reason(events), "command_executed");
        let kinds: Vec<&str> = events
            .events
            .iter()
            .filter_map(|e| e["kind"].as_str())
            .filter(|k| *k != "text_delta")
            .collect();
        assert_eq!(kinds, ["turn_start", "turn_complete"], "{kinds:?}");
    }
    assert_eq!(
        run.provider.calls().len(),
        calls,
        "a local command reached the model"
    );

    let compacted = run.turn(&session, "/compact").await?;
    assert_eq!(stop_reason(&compacted), "command_executed");
    let since: Vec<Purpose> = run.provider.calls()[calls..]
        .iter()
        .map(|c| c.purpose())
        .collect();
    assert!(since.contains(&Purpose::Compact), "{since:?}");
    assert!(
        !since.contains(&Purpose::Turn),
        "/compact ran a turn: {since:?}"
    );
    assert!(
        run.provider.calls()[calls..]
            .iter()
            .all(|c| !c.messages_contain("/compact")),
        "the command text reached the model"
    );

    let calls = run.provider.calls().len();
    let cleared = run.turn(&session, "/clear").await?;
    assert_eq!(stop_reason(&cleared), "command_executed");
    assert_eq!(history_total(&mut run, &session).await?, 0);
    assert_eq!(run.provider.calls().len(), calls);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_list_reports_built_ins_and_project_skills() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("commands-list").start().await?;
    run.write_file(
        ".agents/skills/stamp/SKILL.md",
        &SKILL.replace("{mark}", "x"),
    )?;
    run.session().await?;
    let listed = run.client.commands_list().await?;
    let commands = listed.result.unwrap_or_default()["commands"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let find = |name: &str| commands.iter().find(|c| c["name"] == name).cloned();
    for name in ["help", "skills", "clear", "compact", "cost"] {
        let command = find(name).unwrap_or_default();
        assert_eq!(command["kind"], "local", "{name}: {command}");
        assert_eq!(command["source"], "builtin", "{name}: {command}");
    }
    let stamp = find("stamp").expect("the project skill is a command");
    assert_eq!(stamp["kind"], "prompt", "{stamp}");
    assert_eq!(stamp["source"], "project");
    assert_eq!(stamp["description"], "Stamps a line into stamp.txt");
    assert!(run.provider.calls().is_empty());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_command_reaches_the_model_as_typed() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("unknown-command").start().await?;
    let session = run.session().await?;
    let typed = format!("/no-such-command-{} with an argument", nonce());
    let events = run.turn(&session, &typed).await?;
    assert_ne!(stop_reason(&events), "command_executed");
    let first = &run.provider.calls()[0];
    assert_eq!(first.purpose(), Purpose::Turn);
    assert!(
        first.messages_contain(&typed),
        "the text was not passed through"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_runs_as_a_command_with_its_arguments() -> anyhow::Result<()> {
    let mark = format!("SKILL-BODY-{}", nonce());
    let mut run = LiveRun::builder("skill-command").start().await?;
    run.write_file(
        ".agents/skills/stamp/SKILL.md",
        &SKILL.replace("{mark}", &mark),
    )?;
    let session = run.session().await?;
    let argument = format!("ARG-{}", nonce());
    run.turn(&session, &format!("/stamp {argument}")).await?;
    assert_eq!(
        run.read_file("stamp.txt").map(|s| s.trim().to_string()),
        Some(format!("{argument} {mark}"))
    );
    let first = &run.provider.calls()[0];
    assert!(
        first.messages_contain(&format!("<command-name>stamp {argument}</command-name>")),
        "no invocation header"
    );
    assert!(
        first.messages_contain(&format!("`{argument} {mark}`")),
        "the argument was not substituted into the body"
    );
    assert!(!first.messages_contain("$ARGUMENTS"));
    run.finish().await;
    Ok(())
}
