//! A background sub-agent tells the agent that started it when it is done,
//! and a host can reach it through the daemon while it runs. A foreground one
//! stays listed under its parent once its turn is over, and its questions
//! reach the host inside the parent's turn, its task waiting until answered.

use live::{Block, Daemon, DaemonOptions, ProviderStub, Reply, Watcher, World};
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

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_foreground_agent_stays_listed_under_its_parent() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("fg")?;
    world.write_project_settings(
        "fg",
        &json!({
            "memory_enabled": false,
            "permission_rules": [{ "tool": "Agent", "action": "allow" }]
        }),
    )?;
    let provider = ProviderStub::start().await?;
    provider.script_when(
        "PARENT-MARK",
        [
            Reply::Blocks(vec![Block::tool(
                "call-1",
                "Agent",
                json!({
                    "prompt": "count the files",
                    "description": "file count",
                    "model": "claude-haiku-4-5"
                }),
            )]),
            Reply::text("it said 42"),
        ],
    );
    provider.script([Reply::text("SUB-RESULT-42")]);

    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&session, "PARENT-MARK count", "t1", None)
        .await?;

    let tasks = client
        .call("session.tasks", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(tasks["active"], json!(true), "{tasks}");
    let agents: Vec<_> = tasks["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["kind"] == "agent")
        .collect();
    assert_eq!(agents.len(), 1, "{tasks}");
    assert_eq!(agents[0]["status"], "done", "{tasks}");
    assert_eq!(agents[0]["detail"]["background"], json!(false), "{tasks}");
    let sub_calls: Vec<_> = provider
        .calls()
        .into_iter()
        .filter(|c| c.model() == "claude-haiku-4-5")
        .collect();
    assert_eq!(sub_calls.len(), 1, "the call's `model` is the sub-agent's");
    assert_eq!(
        agents[0]["usage"]["input_tokens"],
        live::provider::INPUT_TOKENS,
        "{tasks}"
    );

    let children = client
        .call("session.list", json!({ "parent_session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    let children = children["sessions"].as_array().cloned().unwrap_or_default();
    assert_eq!(children.len(), 1, "{children:?}");
    assert_eq!(children[0]["session_kind"], "sidechain", "{children:?}");
    assert_eq!(
        children[0]["session_id"], agents[0]["detail"]["agent_session_id"],
        "the task names its sidechain"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreground_agents_question_comes_through_its_parents_turn() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("ask-fg")?;
    world.write_project_settings(
        "ask-fg",
        &json!({
            "memory_enabled": false,
            "permission_rules": [{ "tool": "Agent", "action": "allow" }]
        }),
    )?;
    let provider = ProviderStub::start().await?;
    provider.script_when(
        "PARENT-MARK",
        [
            Reply::Blocks(vec![Block::tool(
                "call-1",
                "Agent",
                json!({ "prompt": "stamp the project", "description": "stamp" }),
            )]),
            Reply::text("the sub-agent stamped it"),
        ],
    );
    provider.script([
        Reply::calls(
            "b-1",
            "Bash",
            json!({ "command": "echo stamped > stamped.txt" }),
        ),
        Reply::text("stamped"),
    ]);

    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    let watcher = Watcher::start(&daemon, &session, true).await?;
    client
        .session_run_turn(&session, "PARENT-MARK stamp it", "t1", None)
        .await?;

    let asked: Vec<_> = watcher
        .asked()
        .into_iter()
        .filter(|a| {
            a.prompt["event"]["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("[sub-agent "))
        })
        .collect();
    assert_eq!(asked.len(), 1, "{asked:?}");
    let question = &asked[0];
    assert_eq!(
        question.prompt["turn_id"], "t1",
        "asked inside the parent's turn, so it carries that turn"
    );
    let agent = question
        .tasks
        .iter()
        .find(|t| t["kind"] == "agent")
        .cloned()
        .unwrap_or_default();
    assert_eq!(agent["detail"]["background"], false, "{agent}");
    assert_eq!(agent["status"], "waiting", "{agent}");
    assert_eq!(
        agent["waiting_for"],
        json!([question.prompt["event"]["prompt_id"]])
    );
    let sidechain = agent["detail"]["agent_session_id"]
        .as_str()
        .unwrap_or_default();
    assert!(question.prompt["event"]["message"]
        .as_str()
        .is_some_and(|m| m.starts_with(&format!("[sub-agent {sidechain}]"))));
    assert_eq!(
        std::fs::read_to_string(project.join("stamped.txt"))?,
        "stamped\n",
        "the answered question let the command run"
    );

    let task_id = agent["task_id"].clone();
    watcher
        .wait_for("the agent to finish", Duration::from_secs(10), |e| {
            e["kind"] == "task_finished" && e["task"]["task_id"] == task_id
        })
        .await?;
    let updates: Vec<_> = watcher
        .events("task_updated")
        .into_iter()
        .filter(|f| f["event"]["task"]["task_id"] == task_id)
        .collect();
    let states: Vec<&str> = updates
        .iter()
        .filter_map(|f| f["event"]["task"]["status"].as_str())
        .collect();
    let waited = states.iter().position(|s| *s == "waiting");
    assert!(
        waited.is_some_and(|at| states[at + 1..].contains(&"running")),
        "waiting while asked, running once answered: {states:?}"
    );
    assert!(watcher
        .events("subagent_progress")
        .iter()
        .all(|f| f["event"]["task_id"] == task_id));
    daemon.stop().await;
    Ok(())
}
