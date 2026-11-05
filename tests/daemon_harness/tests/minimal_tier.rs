//! A daemon built from the kernel alone still runs a session end to end, and
//! says plainly what it was built without.
//!
//! `cargo build -p daemon --no-default-features` carries no MCP, sub-agents,
//! skills, web tools, WebSocket listener, script carrier or package layer.
//! What is left has to create a session, run a turn, resume it and report its
//! context; a method that belongs to a module the build lacks answers
//! `METHOD_NOT_FOUND` naming the module, not a generic unknown method.

use daemon_harness::handle::{Daemon, DaemonOptions};
use daemon_harness::provider::{ProviderStub, Reply};
use daemon_harness::world::World;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a second daemon, built with no modules"]
async fn the_kernel_alone_runs_a_session_and_names_what_it_lacks() -> anyhow::Result<()> {
    let minimal = daemon_harness::alternate_daemon_binary(
        "AGIX_TEST_DAEMON_BIN_MINIMAL",
        "CARGO_TARGET_DIR=$PWD/target/minimal cargo build -p daemon --no-default-features\n  \
         AGIX_TEST_DAEMON_BIN_MINIMAL=$PWD/target/minimal/debug/agixd cargo test \
         -p daemon-harness --test minimal_tier -- --ignored",
    )?;

    let world = World::new()?;
    let project = world.project("minimal")?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).binary(minimal),
    )
    .await?;
    let mut client = daemon.connect().await?;

    let ping = client.call("daemon.ping", serde_json::Value::Null).await?;
    let modules = &ping.result.as_ref().expect("ping answers")["modules"];
    assert_eq!(
        modules,
        &json!([]),
        "a minimal build reported modules: {ping:?}"
    );

    let mcp = client.call("mcp.status", json!({})).await?;
    let error = mcp
        .error
        .as_ref()
        .expect("mcp.status must fail in a build without MCP");
    assert_eq!(error.code, -32601, "{error:?}");
    assert_eq!(
        error.data.as_ref().map(|d| d["module"].clone()),
        Some(json!("mcp")),
        "the error must name the module the build lacks: {error:?}"
    );

    provider.script([Reply::text("hello from the kernel")]);
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    let turn = client.session_run_turn(&session, "hi", "t1", None).await?;
    assert!(
        turn.text.contains("hello from the kernel"),
        "the turn did not come back: {turn:?}"
    );
    let offered = provider.calls()[0].tool_names();
    for absent in ["Agent", "Skill", "WebFetch", "StructuredOutput"] {
        assert!(
            !offered.iter().any(|t| t == absent),
            "a minimal build offered `{absent}`: {offered:?}"
        );
    }
    assert!(offered.iter().any(|t| t == "Bash"), "{offered:?}");

    let context = client
        .call("session.inspectContext", json!({ "session_id": session }))
        .await?;
    assert!(context.error.is_none(), "{context:?}");

    client
        .call("session.close", json!({ "session_id": session }))
        .await?;
    let resumed = client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");

    daemon.stop().await;
    Ok(())
}
