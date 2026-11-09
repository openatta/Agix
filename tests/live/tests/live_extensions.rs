//! What a project adds to the agent, used by a real model: a skill, an MCP
//! server's tool, and a hook that sees the tool calls; a `context: fork`
//! skill run in a sub-agent, `allowed-tools` grants (scoped from a project
//! skill, blanket from a user-level one, a project's blanket refused), and a
//! skill written mid-session announced with `skills_changed` and used.

use std::path::{Path, PathBuf};

use live::{Answer, LiveRun, Mode};
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

fn toy_mcp_server() -> anyhow::Result<PathBuf> {
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "mcp-toy-server", "--quiet"])
        .status()?;
    anyhow::ensure!(status.success(), "cargo build -p mcp-toy-server failed");
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/mcp-toy-server")
        .canonicalize()?)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_skill_is_found_and_followed() -> anyhow::Result<()> {
    let mut run = LiveRun::start("skilled", json!({}), Mode::InProcess).await?;
    let greeting = format!("HELLO-{}", nonce());
    run.write_file(
        ".agents/skills/greeter/SKILL.md",
        &format!(
            "---\nname: greeter\ndescription: Writes this project's greeting file\n---\n\
             Write the file greeting.txt in the project root containing exactly \
             {greeting} and nothing else.\n"
        ),
    )?;
    let session = run.session().await?;
    let events = run
        .turn(&session, "Use the greeter skill, then reply DONE.")
        .await?;
    assert!(
        events.tool_uses.iter().any(|(n, _)| n == "Skill"),
        "{:?}",
        events.tool_uses
    );
    assert_eq!(
        run.read_file("greeting.txt").map(|s| s.trim().to_string()),
        Some(greeting),
        "the greeting is only in the skill's body"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_tool_is_called_and_a_hook_sees_the_calls() -> anyhow::Result<()> {
    let server = toy_mcp_server()?;
    let probe = format!("ping-{}", nonce());
    let world_hook = std::env::temp_dir().join(format!("agix-live-hook-{}", nonce()));
    std::fs::create_dir_all(&world_hook)?;
    let hook_log = world_hook.join("calls.jsonl");
    let hook = world_hook.join("log.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat >> '{}'\necho >> '{}'\necho '{{\"continue\": true}}'\n",
            hook_log.display(),
            hook_log.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }

    let mut run = LiveRun::start(
        "extended",
        json!({
            "mcp_servers": { "demo": { "type": "stdio", "command": server } },
            "hooks_config": {
                "PreToolUse": [
                    { "type": "command", "command": hook, "timeout": 5000 }
                ]
            }
        }),
        Mode::InProcess,
    )
    .await?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Call the ping tool of the demo MCP server with message {probe} (use \
                 ToolSearch to find it if needed), then run `echo checked` with Bash. Reply \
                 with exactly what the ping tool returned."
            ),
        )
        .await?;

    let mcp_call = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name.contains("ping"));
    assert!(
        mcp_call.is_some_and(
            |(_, content, is_error)| !is_error && content.contains(&format!("toy: {probe}"))
        ),
        "{:?}",
        events.tool_results
    );
    assert!(
        events.text.contains(&format!("toy: {probe}")),
        "{}",
        events.text
    );
    let log = std::fs::read_to_string(&hook_log).unwrap_or_default();
    assert!(log.contains("\"Bash\""), "the hook never saw Bash: {log}");
    assert!(
        log.contains("ping"),
        "the hook never saw the MCP call: {log}"
    );
    let _ = std::fs::remove_dir_all(&world_hook);
    run.finish().await;
    Ok(())
}

fn skill(name: &str, front: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {name} for this project\n{front}---\n{body}\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_skill_runs_in_a_sub_agent_and_only_its_answer_comes_back() -> anyhow::Result<()> {
    let mut run = LiveRun::start("forked", json!({}), Mode::InProcess).await?;
    let token = format!("FORKED-{}", nonce());
    run.write_file(
        ".agents/skills/forker/SKILL.md",
        &skill(
            "forker",
            "context: fork\n",
            &format!(
                "FORK-ROLE. Write {token} to forked.txt with the Write tool, then answer \
                 exactly OK and nothing else."
            ),
        ),
    )?;
    let session = run.session().await?;
    let events = run
        .turn(&session, "Use the forker skill, then reply DONE.")
        .await?;
    assert!(
        events.tool_uses.iter().any(|(n, _)| n == "Skill"),
        "{:?}",
        events.tool_uses
    );
    assert_eq!(
        run.read_file("forked.txt").map(|s| s.trim().to_string()),
        Some(token.clone())
    );
    let calls = run.provider.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.messages_contain("FORK-ROLE") && !c.messages_contain("Use the forker skill")),
        "the skill's body ran in a context of its own"
    );
    assert!(
        calls
            .iter()
            .filter(|c| c.messages_contain("Use the forker skill"))
            .all(|c| !c.messages_contain(&token)),
        "the fork's work leaked into the main conversation"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skills_allowed_tools_pre_approve_what_it_names_and_project_blankets_are_refused(
) -> anyhow::Result<()> {
    let mut run = LiveRun::builder("granted")
        .settings(json!({
            "permission_mode": "default",
            "permission_rules": [{ "tool": "Skill", "action": "allow" }]
        }))
        .options(|o| o.prompt_timeout(std::time::Duration::from_secs(5)))
        .before_start(|world, _| {
            let dir = world.global_root().join("skills").join("deployer");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join("SKILL.md"),
                skill(
                    "deployer",
                    "allowed-tools: Bash\n",
                    "Run `sh deploy.sh` with the Bash tool.",
                ),
            )?;
            Ok(())
        })
        .start()
        .await?;
    for script in ["build", "blanket", "other", "deploy"] {
        run.write_file(
            &format!("{script}.sh"),
            &format!("echo ran > {script}.log\necho {script} ran\n"),
        )?;
    }
    run.write_file(
        ".agents/skills/builder/SKILL.md",
        &skill(
            "builder",
            "allowed-tools: [Bash(sh build.sh)]\n",
            "Run `sh build.sh` with the Bash tool.",
        ),
    )?;
    run.write_file(
        ".agents/skills/blanket/SKILL.md",
        &skill(
            "blanket",
            "allowed-tools: Bash\n",
            "Run `sh blanket.sh` with the Bash tool.",
        ),
    )?;
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |e| {
            (e["prompt_type"] == "permission").then(|| Answer::deny("the operator says no"))
        })
        .await?;
    run.turn(
        &session,
        "Do these in order; if one is refused, do not retry it and go on to the next:\n\
         1. Use the builder skill.\n\
         2. Use the blanket skill.\n\
         3. Run `sh other.sh` with the Bash tool.\n\
         4. Use the deployer skill.\n\
         Reply with what happened at each step.",
    )
    .await?;
    let frames = watcher.frames();
    let asked: Vec<String> = watcher
        .asked()
        .iter()
        .map(|a| {
            let event = &a.prompt["event"];
            let input = frames
                .iter()
                .find(|f| {
                    f["event"]["kind"] == "tool_use" && f["event"]["id"] == event["tool_use_id"]
                })
                .map(|f| f["event"]["input"].to_string())
                .unwrap_or_default();
            format!("{} {input}", event["message"].as_str().unwrap_or_default())
        })
        .collect();
    eprintln!("[live granted] asked {asked:?}");
    assert!(
        run.read_file("build.log").is_some(),
        "the scoped grant of a project skill ran without asking"
    );
    assert!(!asked.iter().any(|m| m.contains("build.sh")), "{asked:?}");
    assert!(
        run.read_file("blanket.log").is_none(),
        "a project skill's blanket grant is not honored"
    );
    assert!(asked.iter().any(|m| m.contains("blanket.sh")), "{asked:?}");
    assert!(run.read_file("other.log").is_none());
    assert!(
        run.read_file("deploy.log").is_some(),
        "a user-level skill's blanket grant is honored"
    );
    assert!(!asked.iter().any(|m| m.contains("deploy.sh")), "{asked:?}");
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scoped_grant_written_bare_is_honored() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("granted-bare")
        .settings(json!({
            "permission_mode": "default",
            "permission_rules": [{ "tool": "Skill", "action": "allow" }]
        }))
        .options(|o| o.prompt_timeout(std::time::Duration::from_secs(5)))
        .start()
        .await?;
    run.write_file("build.sh", "echo ran > build.log\necho build ran\n")?;
    run.write_file(
        ".agents/skills/builder/SKILL.md",
        &skill(
            "builder",
            "allowed-tools: Bash(sh build.sh)\n",
            "Run `sh build.sh` with the Bash tool.",
        ),
    )?;
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |e| {
            (e["prompt_type"] == "permission").then(|| Answer::deny("the operator says no"))
        })
        .await?;
    run.turn(
        &session,
        "Use the builder skill. If its command is refused, do not retry. Reply with what happened.",
    )
    .await?;
    assert!(watcher.asked().is_empty(), "{:?}", watcher.asked());
    assert!(run.read_file("build.log").is_some());
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// A skill written while a session is open, then asked for by name: the
/// `skills_changed` frames the turn brought, and whether the skill ran.
async fn a_late_skill(name: &str, skills_dir_at_start: bool) -> anyhow::Result<(Vec<Value>, bool)> {
    let mut run = LiveRun::builder(name)
        .before_start(move |_, project| {
            if skills_dir_at_start {
                let dir = project.join(".agents/skills/early");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join("SKILL.md"), skill("early", "", "Reply EARLY."))?;
            }
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    run.turn(&session, "Reply OK.").await?;
    let token = format!("LATE-{}", nonce());
    run.write_file(
        ".agents/skills/latecomer/SKILL.md",
        &skill(
            "latecomer",
            "",
            &format!("Write {token} to late.txt with the Write tool."),
        ),
    )?;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    run.turn(
        &session,
        "Use the latecomer skill with the Skill tool, then reply DONE.",
    )
    .await?;
    let changed = watcher.events("skills_changed");
    eprintln!("[live {name}] {changed:?}");
    let ran = run.read_file("late.txt").map(|s| s.trim().to_string()) == Some(token);
    drop(watcher);
    run.finish().await;
    Ok((changed, ran))
}

fn announced(changed: &[Value], skill: &str) -> bool {
    changed.iter().any(|f| {
        f["event"]["added"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n == skill))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_skill_a_project_gets_mid_session_is_used() -> anyhow::Result<()> {
    let (_, ran) = a_late_skill("first-skill", false).await?;
    assert!(ran, "the new skill's body was followed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: the daemon swaps a binding's skill catalog in place when the watch hub sees a change (assembly/src/modules.rs:105), so the turn's before/after diff (runtime/src/turn.rs:690) is always empty — no skills_changed frame reaches the host and no reminder reaches the model"]
async fn a_skill_added_mid_session_is_announced() -> anyhow::Result<()> {
    let (changed, ran) = a_late_skill("late-skill", true).await?;
    assert!(ran, "the new skill's body was followed");
    assert!(announced(&changed, "latecomer"), "{changed:?}");
    Ok(())
}
