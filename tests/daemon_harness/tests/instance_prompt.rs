//! `config.setPrompt` changes this instance's system prompt: new sessions
//! start with it, a running session picks it up before its next turn, and
//! clearing it gives the settings files' value back.

use daemon_harness::{Daemon, DaemonOptions, ProviderStub, Reply, World};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn the_instance_prompt_reaches_new_and_running_sessions() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("prompted")?;
    world.write_project_settings(
        "prompted",
        &json!({ "memory_enabled": false, "prompt_append": "FROM-SETTINGS" }),
    )?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;

    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    provider.script([Reply::text("one")]);
    client
        .session_run_turn(&session, "first", "t1", None)
        .await?;
    assert!(provider.calls()[0].system_text().contains("FROM-SETTINGS"));

    let set = client
        .call("config.setPrompt", json!({ "append": "FROM-RPC" }))
        .await?;
    assert_eq!(
        set.result,
        Some(json!({ "append": "FROM-RPC", "override": null })),
        "{set:?}"
    );

    provider.script([Reply::text("two")]);
    client
        .session_run_turn(&session, "second", "t2", None)
        .await?;
    let running = provider.calls()[1].system_text();
    assert!(
        running.contains("FROM-RPC"),
        "the running session kept its old prompt"
    );
    assert!(
        !running.contains("FROM-SETTINGS"),
        "the instance append replaces the file's"
    );

    client
        .call("config.setPrompt", json!({ "override": "ONLY-THIS" }))
        .await?;
    let fresh = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    provider.script([Reply::text("three")]);
    client.session_run_turn(&fresh, "third", "t3", None).await?;
    let replaced = provider.calls()[2].system_text();
    assert!(replaced.starts_with("ONLY-THIS"), "{replaced}");

    let cleared = client
        .call(
            "config.setPrompt",
            json!({ "append": null, "override": null }),
        )
        .await?;
    assert_eq!(
        cleared.result,
        Some(json!({ "append": null, "override": null }))
    );
    let get = client
        .call("config.getPrompt", serde_json::Value::Null)
        .await?;
    assert_eq!(get.result, cleared.result);
    provider.script([Reply::text("four")]);
    client
        .session_run_turn(&fresh, "fourth", "t4", None)
        .await?;
    assert!(provider.calls()[3].system_text().contains("FROM-SETTINGS"));

    let bad = client
        .call("config.setPrompt", json!({ "append": 3 }))
        .await?;
    assert_eq!(bad.error.map(|e| e.code), Some(-32602));

    daemon.stop().await;
    Ok(())
}
