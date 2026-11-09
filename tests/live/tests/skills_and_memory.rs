//! Skills and memory through the daemon, against a stub model: a `/name`
//! command expands a project skill the way the `Skill` tool does, a skill
//! added mid-session is announced on the session's next turn, and a memory
//! filed under the project is in the prompt's index next to the global ones.

use std::time::{Duration, Instant};

use live::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply, SeenRequest};
use serde_json::json;

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            live::ModelEndpoint::model(stub),
        ))
        .settings(json!({ "permission_mode": "default" }))
}

fn skill(name: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: the {name} skill\n---\n{body}\n")
}

fn turn_calls(stub: &ProviderStub) -> Vec<SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slash_command_runs_its_skills_inline_commands_without_the_frontmatter(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("slash-skill", &stub)
        .before_start(|_, project| {
            let dir = project.join(".agents/skills/stamp");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join("SKILL.md"),
                skill("stamp", "Stamp release $ARGUMENTS.\n!`echo inline-ran`"),
            )?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("stamped")]);
    run.turn(&session, "/stamp v1").await?;
    let first = turn_calls(&stub).remove(0);
    assert!(
        first.messages_contain("Stamp release v1."),
        "{}",
        first.messages_text()
    );
    assert!(
        first.messages_contain("inline-ran"),
        "{}",
        first.messages_text()
    );
    assert!(
        !first.messages_contain("!`echo"),
        "{}",
        first.messages_text()
    );
    assert!(
        !first.messages_contain("description: the stamp skill"),
        "{}",
        first.messages_text()
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_added_between_turns_is_announced_on_the_next() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("late-skill-stub", &stub)
        .before_start(|_, project| {
            let dir = project.join(".agents/skills/early");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("SKILL.md"), skill("early", "Reply EARLY."))?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([Reply::text("one")]);
    run.turn(&session, "hello").await?;

    run.write_file(
        ".agents/skills/latecomer/SKILL.md",
        &skill("latecomer", "Reply LATE."),
    )?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let listed = run
            .client
            .call("commands.list", serde_json::Value::Null)
            .await?;
        if listed
            .result
            .unwrap_or_default()
            .to_string()
            .contains("latecomer")
        {
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the daemon never saw the new skill"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stub.script([Reply::text("two")]);
    run.turn(&session, "again").await?;

    let changed = watcher.events("skills_changed");
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert_eq!(
        changed[0]["event"]["added"],
        json!(["latecomer"]),
        "{changed:?}"
    );
    let second = turn_calls(&stub).remove(1);
    assert!(second.messages_contain("\"latecomer\" was just added"));
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_memory_is_indexed_with_the_global_ones() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("memory-tiers", &stub)
        .settings(json!({ "permission_mode": "default", "memory_enabled": true }))
        .before_start(|world, project| {
            for (dir, name) in [
                (world.global_memory_dir(), "GLOBAL-TIER-MEMORY"),
                (world.memory_dir(project), "PROJECT-TIER-MEMORY"),
            ] {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(
                    dir.join("MEMORY.md"),
                    format!("- [{name}]({name}.md) — one of the tiers\n"),
                )?;
            }
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("ok"), Reply::text("[]")]);
    run.turn(&session, "hello").await?;
    let first = turn_calls(&stub).remove(0);
    assert!(first.system_contains("GLOBAL-TIER-MEMORY"));
    assert!(first.system_contains("PROJECT-TIER-MEMORY"));
    assert!(first.system_contains(&run.world.global_memory_dir().to_string_lossy()));
    run.finish().await;
    Ok(())
}
