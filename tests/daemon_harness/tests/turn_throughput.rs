//! How long a spawned daemon spends on its own share of a turn — the part no
//! model latency hides on a fast provider.
//!
//! A benchmark, not a check: it prints timings and asserts only that the turns
//! ran. It exists to compare builds of the same code — `release` against
//! `release-small`, one `opt-level` against another — so it runs against
//! whichever `agixd` `AGIX_TEST_DAEMON_BIN` names:
//!
//! ```sh
//! cargo build --release -p daemon --bin agixd
//! AGIX_TEST_DAEMON_BIN=target/release/agixd \
//!   cargo test --release -p daemon-harness --test turn_throughput -- --ignored --nocapture
//! ```
//!
//! Each turn makes the model ask for a content Grep over a copy of this
//! workspace's `crates/`, a Glob, and a Read, so the time goes to the search
//! path (`ignore`, `regex`), the tool pipeline, and assembling and serialising
//! a request whose history grows by every result before it.

use std::time::{Duration, Instant};

use daemon_harness::handle::{Daemon, DaemonOptions, Mode};
use daemon_harness::provider::{Block, ProviderStub, Reply};
use daemon_harness::world::World;
use serde_json::json;

const TURNS: usize = 10;
const ROUNDS: usize = 3;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a benchmark; spawns a real daemon process"]
async fn turn_throughput() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("bench")?;
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    test_runner::fixture::copy_dir_recursive(&workspace.join("crates"), &project.join("crates"))?;
    world.write_project_settings(
        "bench",
        &json!({
            "memory_enabled": false,
            "permission_rules": [
                { "tool": "Grep", "action": "allow" },
                { "tool": "Glob", "action": "allow" },
                { "tool": "Read", "action": "allow" }
            ]
        }),
    )?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(Mode::Spawned),
    )
    .await?;
    let mut client = daemon.connect().await?;

    let mut rounds = Vec::new();
    for round in 0..ROUNDS {
        let session = client
            .session_create(json!({ "project_root": project.to_string_lossy() }))
            .await?;
        let mut turns = Vec::new();
        for turn in 0..TURNS {
            provider.script([
                Reply::Blocks(vec![
                    Block::tool(
                        format!("g-{turn}"),
                        "Grep",
                        json!({
                            "pattern": r"fn [a-z_]+\(&self",
                            "output_mode": "content",
                            "head_limit": 150
                        }),
                    ),
                    Block::tool(
                        format!("l-{turn}"),
                        "Glob",
                        json!({ "pattern": "**/*.rs", "max_results": 200 }),
                    ),
                    Block::tool(
                        format!("r-{turn}"),
                        "Read",
                        json!({
                            "file_path": project.join("crates/runtime/src/turn.rs").to_string_lossy(),
                            "offset": 1 + turn * 300,
                            "limit": 300
                        }),
                    ),
                ]),
                Reply::text("ok"),
            ]);
            let started = Instant::now();
            let events = client
                .session_run_turn(&session, "look around", &format!("t{round}-{turn}"), None)
                .await?;
            turns.push(started.elapsed());
            anyhow::ensure!(
                events.tool_uses.len() == 3,
                "turn {turn} ran {} tools, not 3: {events:?}",
                events.tool_uses.len()
            );
        }
        rounds.push(turns);
    }

    let per_turn = |i: usize| {
        let mut v: Vec<Duration> = rounds.iter().map(|r| r[i]).collect();
        v.sort();
        v[v.len() / 2]
    };
    let total: Duration = (0..TURNS).map(per_turn).sum();
    eprintln!(
        "binary: {}",
        std::env::var("AGIX_TEST_DAEMON_BIN").unwrap_or_else(|_| "<beside the test>".into())
    );
    for i in 0..TURNS {
        eprintln!("turn {i:>2}: {:>8.2} ms", per_turn(i).as_secs_f64() * 1e3);
    }
    eprintln!(
        "median of {ROUNDS} rounds, total over {TURNS} turns: {:.1} ms",
        total.as_secs_f64() * 1e3
    );

    daemon.stop().await;
    Ok(())
}
