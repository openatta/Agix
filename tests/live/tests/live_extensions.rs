//! What a project adds to the agent, used by a real model: a skill, an MCP
//! server's tool, and a hook that sees the tool calls.

use std::path::{Path, PathBuf};

use live::{LiveRun, Mode};
use serde_json::json;

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
