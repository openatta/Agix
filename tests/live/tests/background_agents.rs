//! A background sub-agent tells the agent that started it when it is done,
//! and a host can reach it through the daemon while it runs.

use live::{Block, Daemon, DaemonOptions, ProviderStub, Reply, World};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_background_agent_reaches_its_parents_next_call() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("bg")?;
    world.write_project_settings(
        "bg",
        &json!({
            "memory_enabled": false,
            "permission_rules": [{ "tool": "Agent", "action": "allow" }]
        }),
    )?;
    let provider = ProviderStub::start().await?;
    // Only the parent's conversation carries the mark; the sub-agent draws
    // from the unconditional queue.
    provider.script_when(
        "PARENT-MARK",
        [
            Reply::Blocks(vec![Block::tool(
                "call-1",
                "Agent",
                json!({
                    "prompt": "count the files",
                    "description": "file count",
                    "background": true
                }),
            )]),
            Reply::text("started it"),
        ],
    );
    provider.script([Reply::text("SUB-RESULT-42")]);

    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&session, "PARENT-MARK start a count", "t1", None)
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let task_id = loop {
        let tasks = client
            .call("session.tasks", json!({ "session_id": session }))
            .await?
            .result
            .unwrap_or_default();
        let done = tasks["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["kind"] == "agent" && t["status"] == "done");
        if let Some(t) = done {
            break t["task_id"].as_str().unwrap_or_default().to_string();
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the background agent never finished: {tasks}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let late = client
        .call(
            "session.messageTask",
            json!({ "session_id": session, "task_id": task_id, "message": "too late" }),
        )
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(late["delivered"], json!(false), "{late}");

    provider.script_when("PARENT-MARK", [Reply::text("noted")]);
    client
        .session_run_turn(&session, "anything new?", "t2", None)
        .await?;
    let last = provider.calls().last().cloned().expect("a call");
    let body = last.body.to_string();
    assert!(body.contains("task-notification"), "{body}");
    assert!(body.contains(&task_id), "{body}");
    assert!(body.contains("SUB-RESULT-42"), "{body}");
    assert_eq!(
        body.matches("<task-notification>").count(),
        1,
        "told once: {body}"
    );
    daemon.stop().await;
    Ok(())
}
