//! A daemon killed mid-command leaves that command running; the next start of
//! the same instance stops it, and the session the command belonged to comes
//! back.
//!
//! Commands lead process groups of their own, so nothing the kernel does on
//! the parent's death reaches them. The panic hook covers a panic; this covers
//! what no hook sees — `SIGKILL`, the OOM killer, a power cut — through the
//! list each run keeps on disk.

use std::path::Path;
use std::time::Duration;

use daemon_harness::handle::{Daemon, DaemonOptions, Mode};
use daemon_harness::provider::{Block, ProviderStub, Reply};
use daemon_harness::world::World;
use serde_json::json;

fn group_alive(group: i64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{group}")])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn recorded_groups(path: &Path) -> Vec<i64> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["groups"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|g| g.as_i64())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn the_next_start_stops_what_a_killed_daemon_left_running() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("crash")?;
    world.write_project_settings(
        "crash",
        &json!({
            "memory_enabled": false,
            "permission_rules": [{ "tool": "Bash", "action": "allow" }]
        }),
    )?;
    let provider = ProviderStub::start().await?;
    provider.script([Reply::Blocks(vec![Block::tool(
        "call-1",
        "Bash",
        json!({ "command": "tail -f /dev/null", "timeout_ms": 600000 }),
    )])]);
    let opts = || DaemonOptions::new(project.clone()).mode(Mode::Spawned);
    let mut daemon = Daemon::start(&world, &provider, opts()).await?;

    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    let turn = {
        let mut client = daemon.connect().await?;
        let session = session.clone();
        tokio::spawn(async move {
            let _ = client
                .session_run_turn(&session, "wait a while", "t1", None)
                .await;
        })
    };

    let groups_file = world.global_root().join("daemon/groups/harness.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let group = loop {
        if let Some(g) = recorded_groups(&groups_file).first() {
            break *g;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the command never showed up in {}\n{}",
            groups_file.display(),
            daemon.stderr()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    daemon.kill().await;
    turn.abort();
    assert!(
        group_alive(group),
        "the command died with the daemon; this case no longer tests anything"
    );

    let daemon = Daemon::start(&world, &provider, opts()).await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while group_alive(group) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !group_alive(group),
        "the restarted daemon left the previous run's command running\n{}",
        daemon.stderr()
    );

    let mut client = daemon.connect().await?;
    let resumed = client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    assert!(
        resumed.error.is_none(),
        "the session did not come back after the restart: {resumed:?}"
    );

    daemon.stop().await;
    Ok(())
}
