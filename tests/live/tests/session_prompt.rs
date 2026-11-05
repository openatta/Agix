//! A session's own system prompt: given at creation, changed mid-session,
//! kept across a rebuild, and never written into a settings file.

use live::{Daemon, DaemonOptions, ProviderStub, Reply, World};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_prompt_is_its_own_and_changes_from_its_next_turn() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("prompted")?;
    world.write_project_settings(
        "prompted",
        &json!({ "memory_enabled": false, "prompt_append": "FROM-SETTINGS" }),
    )?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let root = project.to_string_lossy().to_string();

    let session = client
        .session_create(json!({
            "project_root": root,
            "options": { "prompt": { "append": "AT-CREATION" } }
        }))
        .await?;
    let other = client
        .session_create(json!({ "project_root": root }))
        .await?;

    provider.script([Reply::text("one")]);
    client
        .session_run_turn(&session, "first", "t1", None)
        .await?;
    let first = provider.calls()[0].system_text();
    assert!(first.contains("AT-CREATION"), "{first}");
    assert!(
        !first.contains("FROM-SETTINGS"),
        "the session's append replaces the file's"
    );

    provider.script([Reply::text("untouched")]);
    client.session_run_turn(&other, "other", "o1", None).await?;
    assert!(
        provider.calls()[1].system_text().contains("FROM-SETTINGS"),
        "another session must keep the settings' prompt"
    );

    let set = client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "override": "ONLY-THIS" }),
        )
        .await?;
    let result = set.result.clone().unwrap_or_default();
    assert_eq!(result["override"], json!("ONLY-THIS"), "{set:?}");
    assert_eq!(result["append"], json!("AT-CREATION"), "{set:?}");

    provider.script([Reply::text("two")]);
    client
        .session_run_turn(&session, "second", "t2", None)
        .await?;
    assert!(provider.calls()[2].system_text().starts_with("ONLY-THIS"));

    client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "append": null, "override": null }),
        )
        .await?;
    client
        .call("session.close", json!({ "session_id": session }))
        .await?;
    client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    provider.script([Reply::text("three")]);
    client
        .session_run_turn(&session, "third", "t3", None)
        .await?;
    let back = provider.calls()[3].system_text();
    assert!(
        back.contains("FROM-SETTINGS"),
        "clearing gives the settings' prompt back"
    );
    assert!(
        !back.contains("AT-CREATION"),
        "a cleared field stays cleared after a resume"
    );

    let settings_file = project.join(".agix").join("settings.json");
    let on_disk = std::fs::read_to_string(&settings_file)?;
    assert!(
        !on_disk.contains("ONLY-THIS"),
        "the settings file was rewritten"
    );

    let bad = client
        .call("session.setPrompt", json!({ "session_id": session }))
        .await?;
    assert_eq!(bad.error.map(|e| e.code), Some(-32602));

    daemon.stop().await;
    Ok(())
}
