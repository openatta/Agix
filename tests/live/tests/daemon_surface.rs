//! What a client gets when it talks to a daemon.
//!
//! Each case here exists because a piece of the harness would be useless if
//! it were wrong, and asserting that piece once keeps every later scenario
//! from having to. They are also the first tests in this workspace that reach
//! the model over HTTP: everything else fakes it at the `AnthropicClient`
//! seam, which skips request serialization, the SSE decoder and the retry
//! loop — three things a daemon depends on.
//!
//! # Two modes
//!
//! Every case is written against a [`Mode`] and run twice. In this process it
//! is fast and runs by default; against a spawned `agixd` it is
//! `#[ignore]`d, like every other case here that starts a subprocess, and CI
//! runs it in the ignored step.
//!
//! The parity is the claim being tested. A case that passes in one mode and
//! not the other has found something real: the spawned daemon resolves its
//! own settings, its own paths and its own model endpoint from a command line
//! and an environment, and every one of those is a step the in-process build
//! is handed rather than takes.

use live::{Block, Daemon, DaemonOptions, Mode, ProviderStub, Reply, World};
use serde_json::json;
use std::time::Duration;

/// The events a session has written so far, waited for rather than read once.
///
/// Telemetry leaves the engine on a channel and is written by another task,
/// so a turn's last events land some time after the RPC that produced them
/// has already answered. Reading the file the moment a turn returns is a race
/// that only loses under load — which is to say, in CI.
async fn telemetry_until(
    path: &std::path::Path,
    enough: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let events: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if enough(&events) || std::time::Instant::now() > deadline {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn kinds(events: &[serde_json::Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
        .collect()
}

/// A daemon, a stub, and a project — the three every case starts with.
/// How many model calls this turn made.
///
/// Not `call_count()`. Memory is on by default, so the engine makes one more
/// call after the turn has already answered — on its own schedule, with no
/// tools and no system prompt — and a case that counts raw is racing it. That
/// race is won on a fast machine and lost on a loaded one, which is to say it
/// is a test that passes everywhere except CI. The turn's own calls are the
/// ones carrying its tool table.
fn turn_calls(provider: &ProviderStub) -> usize {
    provider
        .calls()
        .into_iter()
        .filter(|c| !c.tool_names().is_empty())
        .count()
}

async fn stage(
    name: &str,
    mode: Mode,
) -> anyhow::Result<(World, ProviderStub, Daemon, std::path::PathBuf)> {
    let world = World::new()?;
    let project = world.project(name)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;
    Ok((world, provider, daemon, project))
}

async fn a_turn_crosses_http_and_comes_back(mode: Mode) -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("plain", mode).await?;
    provider.script([Reply::text("STUB-ANSWERED-THE-TURN")]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    let turn = client
        .session_run_turn(&session, "say something", "t1", None)
        .await?;

    assert!(
        turn.text.contains("STUB-ANSWERED-THE-TURN"),
        "the scripted answer did not reach the client: {turn:?}"
    );
    assert!(turn.turn_complete, "turn never completed: {turn:?}");
    assert_eq!(turn_calls(&provider), 1, "expected exactly one model call");
    daemon.stop().await;
    Ok(())
}

async fn the_builtin_tools_reach_the_model(mode: Mode) -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("tools", mode).await?;
    provider.script([Reply::text("nothing to do")]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    client
        .session_run_turn(&session, "hello", "t1", None)
        .await?;

    let names = provider.calls()[0].tool_names();
    for expected in ["Bash", "Read", "Write", "Edit"] {
        assert!(
            names.iter().any(|n| n == expected),
            "`{expected}` never reached the model; the pool registered {names:?}"
        );
    }
    daemon.stop().await;
    Ok(())
}

/// The whole loop: the model asks for a tool, the daemon runs it in the
/// project, and the next request carries what it returned.
async fn a_tool_call_runs_in_the_project(mode: Mode) -> anyhow::Result<()> {
    let (_world, provider, daemon, project) = stage("tool-loop", mode).await?;
    let target = project.join("note.txt");
    provider.script([
        Reply::Blocks(vec![Block::tool(
            "call-1",
            "Write",
            json!({ "file_path": target.to_string_lossy(), "content": "written by the tool\n" }),
        )]),
        Reply::text("TOOL-LOOP-FINISHED"),
    ]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    let turn = client
        .session_run_turn(&session, "write the note", "t1", None)
        .await?;

    assert_eq!(
        std::fs::read_to_string(&target)?,
        "written by the tool\n",
        "the tool did not run against the project directory"
    );
    assert!(
        turn.tool_uses.iter().any(|(name, _)| name == "Write"),
        "no Write in the frames the client saw: {turn:?}"
    );
    assert_eq!(
        turn_calls(&provider),
        2,
        "a tool result should cost a second call"
    );
    assert!(
        provider.calls()[1].message_count() > provider.calls()[0].message_count(),
        "the second request did not carry the first one's exchange"
    );
    daemon.stop().await;
    Ok(())
}

/// One daemon, two projects, one of them carrying a settings file. The
/// unmarked project is the control: without it, a mark found in the prompt
/// could be one that was always there.
async fn a_projects_settings_reach_only_its_own_prompt(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let marked = world.project("marked")?;
    let plain = world.project("plain")?;
    world.write_project_settings("marked", &json!({ "prompt_append": "SETTINGS-MARK-XYZZY" }))?;

    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(plain.clone()).mode(mode),
    )
    .await?;
    provider.script([Reply::text("one"), Reply::text("two")]);

    let mut client = daemon.connect().await?;
    let marked_session = client
        .session_create(json!({ "project_root": marked.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&marked_session, "hello", "t1", None)
        .await?;
    let plain_session = client
        .session_create(json!({ "project_root": plain.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&plain_session, "hello", "t2", None)
        .await?;

    let calls = provider.calls();
    assert!(
        calls[0].system_text().contains("SETTINGS-MARK-XYZZY"),
        "the marked project's settings never reached its prompt"
    );
    assert!(
        !calls[1].system_text().contains("SETTINGS-MARK-XYZZY"),
        "one project's settings leaked into another's session"
    );
    daemon.stop().await;
    Ok(())
}

async fn a_session_writes_the_telemetry_it_was_asked_for(mode: Mode) -> anyhow::Result<()> {
    let (world, provider, daemon, _project) = stage("telemetry", mode).await?;
    provider.script([Reply::text("recorded")]);
    let output = world.telemetry_file("session.jsonl");

    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({
            "options": { "telemetry": { "output": output.to_string_lossy() } }
        }))
        .await?;
    client
        .session_run_turn(&session, "hello", "t1", None)
        .await?;

    let events = telemetry_until(&output, |events| kinds(events).contains(&"turn_complete")).await;
    let written = kinds(&events);
    for expected in [
        "session_start",
        "turn_start",
        "api_request",
        "turn_complete",
    ] {
        assert!(
            written.contains(&expected),
            "no `{expected}` in the telemetry a turn wrote: {written:?}"
        );
    }

    daemon.stop().await;
    Ok(())
}

async fn a_session_reports_itself_and_its_transcript(mode: Mode) -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("detail", mode).await?;
    provider.script([Reply::text("remembered")]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    client
        .session_run_turn(&session, "hello there", "t1", None)
        .await?;

    let detail = client.session_get(&session).await?;
    let detail = detail.result.expect("session.get returned no result");
    assert_eq!(
        detail.get("turn_state").and_then(|v| v.as_str()),
        Some("idle"),
        "a finished turn should leave the session idle: {detail}"
    );

    let history = client
        .call("session.history", json!({ "session_id": session }))
        .await?;
    let text = history.result.unwrap_or_default().to_string();
    assert!(
        text.contains("hello there") && text.contains("remembered"),
        "the transcript is missing one side of the exchange: {text}"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_crosses_http_and_comes_back_here() -> anyhow::Result<()> {
    a_turn_crosses_http_and_comes_back(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_builtin_tools_reach_the_model_here() -> anyhow::Result<()> {
    the_builtin_tools_reach_the_model(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_runs_in_the_project_here() -> anyhow::Result<()> {
    a_tool_call_runs_in_the_project(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_projects_settings_reach_only_its_own_prompt_here() -> anyhow::Result<()> {
    a_projects_settings_reach_only_its_own_prompt(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_writes_the_telemetry_it_was_asked_for_here() -> anyhow::Result<()> {
    a_session_writes_the_telemetry_it_was_asked_for(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_reports_itself_and_its_transcript_here() -> anyhow::Result<()> {
    a_session_reports_itself_and_its_transcript(Mode::InProcess).await
}

/// A named case, already started. Boxed because the six have different
/// concrete future types and this list holds them together.
type Case = (
    &'static str,
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>>>>,
);

/// One test rather than six, because each of these pays for a process start
/// and a settings load; the case that failed is named in the panic.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn every_case_holds_against_a_spawned_daemon() {
    let cases: Vec<Case> = vec![
        (
            "a turn crosses http and comes back",
            Box::pin(a_turn_crosses_http_and_comes_back(Mode::Spawned)),
        ),
        (
            "the builtin tools reach the model",
            Box::pin(the_builtin_tools_reach_the_model(Mode::Spawned)),
        ),
        (
            "a tool call runs in the project",
            Box::pin(a_tool_call_runs_in_the_project(Mode::Spawned)),
        ),
        (
            "a project's settings reach only its own prompt",
            Box::pin(a_projects_settings_reach_only_its_own_prompt(Mode::Spawned)),
        ),
        (
            "a session writes the telemetry it was asked for",
            Box::pin(a_session_writes_the_telemetry_it_was_asked_for(
                Mode::Spawned,
            )),
        ),
        (
            "a session reports itself and its transcript",
            Box::pin(a_session_reports_itself_and_its_transcript(Mode::Spawned)),
        ),
    ];

    for (name, case) in cases {
        if let Err(e) = case.await {
            panic!("`{name}` fails against a spawned daemon but passes in process: {e:?}");
        }
    }
}

/// A daemon announces itself while it runs and takes the announcement back
/// when it stops. A stale entry is worse than none: a client that finds one
/// connects to a socket nobody is listening on.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_daemon_publishes_a_discovery_entry_for_as_long_as_it_runs() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("discovery")?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project).mode(Mode::Spawned),
    )
    .await?;

    let entry = world.instances_dir().join("harness.json");
    let published = std::fs::read_to_string(&entry)
        .map_err(|e| anyhow::anyhow!("no discovery entry at {}: {e}", entry.display()))?;
    let published: serde_json::Value = serde_json::from_str(&published)?;
    assert_eq!(
        published.get("socket").and_then(|v| v.as_str()),
        Some(world.socket().to_string_lossy().as_ref()),
        "the entry names a socket this daemon is not on: {published}"
    );
    assert_eq!(
        published.get("protocol_version").and_then(|v| v.as_u64()),
        Some(u64::from(daemon::discovery::INSTANCE_PROTOCOL_VERSION)),
        "discovery entry is missing the protocol version: {published}"
    );

    daemon.stop().await;
    assert!(
        !entry.exists(),
        "the discovery entry outlived the daemon that wrote it"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn daemon_shutdown_ends_the_process() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("shutdown")?;
    let provider = ProviderStub::start().await?;
    let mut daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project).mode(Mode::Spawned),
    )
    .await?;

    let mut client = daemon.connect().await?;
    client.daemon_shutdown().await?;

    let status = daemon
        .wait_for_exit(Duration::from_secs(10))
        .await
        .ok_or_else(|| anyhow::anyhow!("daemon was still running ten seconds after shutdown"))?;
    assert!(
        status.success(),
        "a requested shutdown should be a clean exit, got {status}"
    );
    Ok(())
}

/// The daemon promises a network listener fails at startup without a token,
/// rather than binding and then refusing everyone. Only a process can be
/// observed failing to start.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_network_listener_without_a_token_refuses_to_start() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("tokenless")?;
    let provider = ProviderStub::start().await?;

    let (status, stderr) = Daemon::spawn_and_wait(
        &world,
        &provider,
        DaemonOptions::new(project)
            .mode(Mode::Spawned)
            .arg("--listen")
            .arg("127.0.0.1:0"),
        Duration::from_secs(20),
    )
    .await?;

    assert!(
        !status.success(),
        "a tokenless TCP listener started anyway ({status})"
    );
    assert!(
        stderr.to_lowercase().contains("token"),
        "the refusal does not say a token is what was missing: {stderr}"
    );
    Ok(())
}

/// A daemon told where its home is must keep everything there. The failure
/// this guards against does not look like a bug — it looks like a file
/// appearing somewhere nobody is looking, which is why `~` is checked by
/// name: a path that is written rather than expanded lands in a directory
/// literally called that.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_spawned_run_leaves_nothing_outside_the_world_it_was_given() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("contained")?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(Mode::Spawned),
    )
    .await?;
    provider.script([Reply::text("contained")]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    client
        .session_run_turn(&session, "hello", "t1", None)
        .await?;
    daemon.stop().await;

    let mut literal_tildes = Vec::new();
    let mut stack = vec![world.root().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|n| n == "~") {
                literal_tildes.push(path.clone());
            }
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    assert!(
        literal_tildes.is_empty(),
        "a `~` was written as a directory name instead of being expanded: {literal_tildes:?}"
    );
    assert!(
        world.global_root().join("projects").exists()
            || world.global_root().join("sessions").exists(),
        "the run left no state under the global root it was given; it went somewhere else"
    );
    Ok(())
}

// A build carries one extension carrier or none, decided at compile time.
// That makes the interesting question — what does a build *without* a
// carrier do with a settings file that asks for one — unanswerable from
// inside a build that has one, which is why `tests/testing_scripts.md` lists
// it as outside the script carrier's own net. A spawned daemon can be a
// different build, so here it is.

const SCRIPT_MARK: &str = "CARRIER-SCRIPT-REACHED-THE-PROMPT";

/// A project whose settings bind one script to the prompt.
fn a_project_with_a_script(world: &World) -> anyhow::Result<std::path::PathBuf> {
    let project = world.project("scripted")?;
    world.write_project_file(
        "scripted",
        ".agix/scripts/house.js",
        &format!(
            r#"function onAssemble(blocks) {{
                 var out = blocks.map(function (b) {{ return b; }});
                 out.push({{ name: "daemon.house", content: "{SCRIPT_MARK}" }});
                 return out;
               }}"#
        ),
    )?;
    world.write_project_settings(
        "scripted",
        &json!({
            "memory_enabled": false,
            "scripts": [
                {
                    "path": ".agix/scripts/house.js",
                    "point": "prompt.assemble",
                    "entry": "onAssemble",
                    // Far above the carrier's 100ms default. These cases are
                    // about a script reaching the prompt, and that budget is
                    // wall clock: on a machine running the rest of the suite
                    // beside them, even a script this small can miss it, and
                    // the engine then correctly drops the contribution. The
                    // deadline has its own case, with a script written to
                    // exceed it (`tests/runner/tests/script_boundaries.rs`).
                    "timeout_ms": 5000
                }
            ]
        }),
    )?;
    Ok(project)
}

/// What the daemon did with that project: whether the mark arrived, and what
/// the session says its scripts have been doing.
async fn run_the_scripted_project(
    world: &World,
    provider: &ProviderStub,
    daemon: &Daemon,
    project: &std::path::Path,
) -> anyhow::Result<(bool, Option<serde_json::Value>)> {
    let _ = world;
    provider.script([Reply::text("done")]);
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&session, "hello", "t1", None)
        .await?;

    let arrived = provider
        .calls()
        .last()
        .is_some_and(|call| call.system_text().contains(SCRIPT_MARK));
    let detail = client.session_get(&session).await?;
    let ledger = detail
        .result
        .and_then(|r| r.get("scripts").cloned())
        .filter(|v| !v.is_null());
    Ok((arrived, ledger))
}

async fn a_bound_script_reaches_the_prompt_and_the_ledger(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = a_project_with_a_script(&world)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;

    let (arrived, ledger) = run_the_scripted_project(&world, &provider, &daemon, &project).await?;
    assert!(
        arrived,
        "the project's script never reached the prompt the daemon sent"
    );
    let ledger = ledger.ok_or_else(|| {
        anyhow::anyhow!("session.get reports no scripts at all on a session that bound one")
    })?;
    assert!(
        ledger["applied"].as_u64().unwrap_or(0) >= 1,
        "the ledger does not record the script as having applied anything: {ledger}"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_script_reaches_the_prompt_and_the_ledger_here() -> anyhow::Result<()> {
    a_bound_script_reaches_the_prompt_and_the_ledger(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_bound_script_reaches_the_prompt_and_the_ledger_in_a_spawned_daemon() -> anyhow::Result<()>
{
    a_bound_script_reaches_the_prompt_and_the_ledger(Mode::Spawned).await
}

const PACKAGE_MARK: &str = "CARRIER-PACKAGE-SCRIPT-REACHED-THE-PROMPT";

/// A package whose whole content is one script bound to `prompt.assemble`,
/// packed and ready for `plugin.install`.
///
/// It only ever *appends* a block. A script that arrives from outside the
/// project may add to the prompt and nothing else — modifying, deleting or
/// reordering needs a capability it never declared — so an appending script
/// is the shape that is supposed to work, and the one worth proving works.
fn a_package_with_a_script(
    world: &World,
    name: &str,
    body: &str,
) -> anyhow::Result<(std::path::PathBuf, String)> {
    let src = world.root().join("packages").join(name);
    std::fs::create_dir_all(&src)?;
    std::fs::write(
        src.join("plugin.toml"),
        format!(
            "[plugin]\n\
             name = \"{name}\"\n\
             version = \"1.0.0\"\n\
             api_version = \"0.1\"\n\
             description = \"a script bound to prompt.assemble\"\n\
             \n\
             [[script]]\n\
             point = \"prompt.assemble\"\n\
             entry = \"annotate.js:onAssemble\"\n\
             timeout_ms = 5000\n"
        ),
    )?;
    std::fs::write(src.join("annotate.js"), body)?;

    test_runner::plugin_fixture::package_dir(&src, &world.root().join("packed"), name)
}

/// Appends a block and touches nothing else — what a package's script is
/// allowed to do.
fn a_script_that_appends() -> String {
    format!(
        r#"function onAssemble(blocks) {{
             var out = blocks.map(function (b) {{ return b; }});
             out.push({{ name: "package.annotator", content: "{PACKAGE_MARK}" }});
             return out;
           }}"#
    )
}

/// Rewrites the first block it is given — what a package's script is not
/// allowed to do, and the only edit whose refusal is observable.
fn a_script_that_rewrites() -> String {
    r#"function onAssemble(blocks) {
         var out = blocks.map(function (b) { return b; });
         if (out.length > 0) { out[0] = { name: out[0].name, content: "REWRITTEN" }; }
         return out;
       }"#
    .to_string()
}

/// A script that arrived in a package runs, and shares one ledger with the
/// project's own.
///
/// Installing a script-only package was already covered — it unpacks, it is
/// disclosed, `plugin.list` finds it. What nothing checked is the step after:
/// that the script in it is ever called. The daemon binds a package's
/// declarations into the same `BoundScripts` as the project's, which is the
/// arrangement this asserts from outside: both marks reach the model, both
/// calls land in the one ledger `session.get` reports, and the ledger names
/// each script by a path that says where it came from.
///
/// One ledger is not cosmetic. It is also one per-turn budget, reset by the
/// same turn — two would have left whichever set the session did not hold
/// running against a budget nothing ever reset.
async fn a_packaged_script_runs_beside_the_projects_own(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = a_project_with_a_script(&world)?;
    let (archive, checksum) =
        a_package_with_a_script(&world, "annotator", &a_script_that_appends())?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;

    // Installed before any session exists: a session takes the package
    // bindings when it is built, so one opened first would not have them.
    let mut client = daemon.connect().await?;
    let installed = client
        .plugin_install(json!({
            "name": "annotator",
            "version": "1.0.0",
            "download_url": test_runner::plugin_fixture::file_url(&archive),
            "checksum": checksum,
        }))
        .await?;
    anyhow::ensure!(
        installed.error.is_none(),
        "plugin.install failed: {:?}",
        installed.error
    );

    let (project_mark_arrived, ledger) =
        run_the_scripted_project(&world, &provider, &daemon, &project).await?;

    let package_mark_arrived = provider
        .calls()
        .last()
        .is_some_and(|call| call.system_text().contains(PACKAGE_MARK));

    assert!(
        project_mark_arrived,
        "the project's own script stopped reaching the prompt once a package was installed"
    );
    assert!(
        package_mark_arrived,
        "the installed package's script never reached the prompt the daemon sent"
    );

    let ledger = ledger.ok_or_else(|| {
        anyhow::anyhow!("session.get reports no scripts on a session with two sets bound")
    })?;
    assert!(
        ledger["applied"].as_u64().unwrap_or(0) >= 2,
        "one ledger should have counted both scripts: {ledger}"
    );

    // The path is what tells the two apart — the ledger has no separate
    // provenance field, and "why did nothing happen" is answerable only if a
    // reader can see which script it was.
    let paths: Vec<&str> = ledger["recent"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e["script"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        paths.iter().any(|p| p.ends_with("house.js")),
        "the project's script is missing from the ledger: {ledger}"
    );
    assert!(
        paths.iter().any(|p| p.ends_with("annotate.js")),
        "the package's script is missing from the ledger: {ledger}"
    );

    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_packaged_script_runs_beside_the_projects_own_here() -> anyhow::Result<()> {
    a_packaged_script_runs_beside_the_projects_own(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_packaged_script_runs_beside_the_projects_own_in_a_spawned_daemon() -> anyhow::Result<()>
{
    a_packaged_script_runs_beside_the_projects_own(Mode::Spawned).await
}

/// An edit a package's script may not make is refused, reported, and costs
/// only itself.
///
/// This is the one point where where a script came from changes what it may
/// do: inside the project it is the operator's own code and may rewrite the
/// prompt; anywhere else it may only add. The rule is worth nothing if a
/// refusal is indistinguishable from a script that ran and found nothing to
/// do — from inside the engine both are "the prompt did not change" — so the
/// ledger has to say which, and the project's own script has to survive the
/// refusal of somebody else's edit.
async fn a_package_may_add_to_the_prompt_but_not_rewrite_it(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = a_project_with_a_script(&world)?;
    let (archive, checksum) =
        a_package_with_a_script(&world, "rewriter", &a_script_that_rewrites())?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;

    let mut client = daemon.connect().await?;
    let installed = client
        .plugin_install(json!({
            "name": "rewriter",
            "version": "1.0.0",
            "download_url": test_runner::plugin_fixture::file_url(&archive),
            "checksum": checksum,
        }))
        .await?;
    anyhow::ensure!(
        installed.error.is_none(),
        "plugin.install failed: {:?}",
        installed.error
    );

    let (project_mark_arrived, ledger) =
        run_the_scripted_project(&world, &provider, &daemon, &project).await?;

    assert!(
        project_mark_arrived,
        "refusing the package's edit took the project's own script down with it"
    );
    let rewritten = provider
        .calls()
        .last()
        .is_some_and(|call| call.system_text().contains("REWRITTEN"));
    assert!(
        !rewritten,
        "the package rewrote a prompt block it may not touch"
    );

    let ledger = ledger.ok_or_else(|| anyhow::anyhow!("session.get reports no scripts at all"))?;
    assert!(
        ledger["refused"].as_u64().unwrap_or(0) >= 1,
        "the refused edit is not counted as refused: {ledger}"
    );
    assert!(
        ledger["applied"].as_u64().unwrap_or(0) >= 1,
        "the project's own script should still be recorded as applied: {ledger}"
    );

    let refusal = ledger["recent"]
        .as_array()
        .and_then(|entries| entries.iter().find(|e| e["outcome"] == "refused").cloned())
        .ok_or_else(|| anyhow::anyhow!("no refused entry in the ledger: {ledger}"))?;
    assert!(
        refusal["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "a refusal without a reason is the silence this ledger exists to break: {refusal}"
    );

    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_may_add_to_the_prompt_but_not_rewrite_it_here() -> anyhow::Result<()> {
    a_package_may_add_to_the_prompt_but_not_rewrite_it(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_package_may_add_to_the_prompt_but_not_rewrite_it_in_a_spawned_daemon(
) -> anyhow::Result<()> {
    a_package_may_add_to_the_prompt_but_not_rewrite_it(Mode::Spawned).await
}

/// The same project, against a daemon with no script engine in it.
///
/// Both halves are needed. That the mark is missing only says the script did
/// not run; that `session.get` reports no scripts at all says why — there was
/// nothing to run it, rather than a script that ran and did nothing. In a
/// build with a carrier those two are told apart by the ledger, and this is
/// the build where the ledger does not exist.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a second daemon, built without the script carrier"]
async fn a_build_with_no_script_carrier_honors_no_scripts_section() -> anyhow::Result<()> {
    let carrierless = live::alternate_daemon_binary(
        "AGIX_TEST_DAEMON_BIN_NO_CARRIER",
        "CARGO_TARGET_DIR=$PWD/target/no-carrier cargo build -p daemon --no-default-features --features full\n  \
         AGIX_TEST_DAEMON_BIN_NO_CARRIER=$PWD/target/no-carrier/debug/agixd cargo test \
         -p live -- --ignored",
    )?;

    let world = World::new()?;
    let project = a_project_with_a_script(&world)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).binary(carrierless),
    )
    .await?;

    let (arrived, ledger) = run_the_scripted_project(&world, &provider, &daemon, &project).await?;
    assert!(
        !arrived,
        "a build with no script engine ran a script anyway — \
         the `scripts` feature is not the only thing gating the carrier"
    );
    assert!(
        ledger.is_none(),
        "a build with no script engine reported a script ledger: {ledger:?}"
    );
    daemon.stop().await;
    Ok(())
}

/// A tool that ran and failed says so on the wire and in the telemetry.
///
/// `Bash` with a non-zero exit is the most common instance of a thing nine
/// tools in this workspace do: return `Ok` with `ToolResult::is_error` set —
/// the tool ran, the work did not go well. The model, `session.event` and
/// the telemetry all read that one flag, so all three are asserted here.
async fn a_failing_tool_says_so_everywhere(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("failing")?;
    world.write_project_settings(
        "failing",
        &json!({
            "memory_enabled": false,
            "permission_rules": [{ "tool": "Bash", "action": "allow" }]
        }),
    )?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;

    provider.script([
        Reply::Blocks(vec![Block::tool(
            "call-1",
            "Bash",
            json!({ "command": "echo the-work; exit 3" }),
        )]),
        Reply::text("done"),
    ]);
    let output = world.telemetry_file("failing.jsonl");

    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({
            "project_root": project.to_string_lossy(),
            "options": { "telemetry": { "output": output.to_string_lossy() } }
        }))
        .await?;
    let turn = client
        .session_run_turn(&session, "run it", "t1", None)
        .await?;

    // 1. The model.
    let result = tool_result_for(&provider.calls()[1], "call-1")
        .ok_or_else(|| anyhow::anyhow!("the failing command produced no result for the model"))?;
    assert!(
        result["is_error"] == json!(true),
        "a command that exited non-zero reached the model as an answer: {result}"
    );

    // 2. Every client watching the session.
    assert!(
        turn.tool_uses.iter().any(|(name, _)| name == "Bash"),
        "the client never saw the tool run: {turn:?}"
    );

    // 3. Whoever is counting.
    let events = telemetry_until(&output, |events| kinds(events).contains(&"turn_complete")).await;
    let execution = events
        .iter()
        .find(|e| {
            e.get("type").and_then(|t| t.as_str()) == Some("tool_execution")
                && e.get("tool_name").and_then(|t| t.as_str()) == Some("Bash")
        })
        .ok_or_else(|| anyhow::anyhow!("no tool_execution event for the command that ran"))?;
    assert_eq!(
        execution["is_error"],
        json!(true),
        "the telemetry counted a failed command as a success: {execution}"
    );

    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_tool_says_so_everywhere_here() -> anyhow::Result<()> {
    a_failing_tool_says_so_everywhere(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_failing_tool_says_so_everywhere_in_a_spawned_daemon() -> anyhow::Result<()> {
    a_failing_tool_says_so_everywhere(Mode::Spawned).await
}

/// The `tool_result` block answering `tool_use_id`, out of a request's
/// messages.
fn tool_result_for(request: &live::SeenRequest, id: &str) -> Option<serde_json::Value> {
    request
        .body
        .get("messages")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("content")?.as_array())
        .flatten()
        .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
        .cloned()
}

/// A package written for the WebAssembly carrier is refused, and leaves nothing
/// installed behind it.
async fn a_package_with_a_wasm_component_is_refused(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("wasm-refused")?;
    world.write_project_settings("wasm-refused", &json!({ "memory_enabled": false }))?;

    let src = world.root().join("packages").join("echo-plugin");
    std::fs::create_dir_all(&src)?;
    std::fs::write(src.join("echo.wasm"), b"\0asm")?;
    std::fs::write(
        src.join("plugin.toml"),
        "[plugin]\n\
         name = \"echo-plugin\"\n\
         version = \"1.0.0\"\n\
         api_version = \"0.1\"\n\
         \n\
         [[wasm]]\n\
         component = \"echo.wasm\"\n",
    )?;
    let (archive, checksum) = test_runner::plugin_fixture::package_dir(
        &src,
        &world.root().join("packed"),
        "echo-plugin",
    )?;

    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;
    let mut client = daemon.connect().await?;
    let installed = client
        .plugin_install(json!({
            "name": "echo-plugin",
            "version": "1.0.0",
            "download_url": test_runner::plugin_fixture::file_url(&archive),
            "checksum": checksum,
        }))
        .await?;
    let error = installed.error.ok_or_else(|| {
        anyhow::anyhow!("a `[[wasm]]` package was installed: {:?}", installed.result)
    })?;
    assert!(
        error.message.contains("[[wasm]]"),
        "the refusal does not name the section: {}",
        error.message
    );

    let listed = client.plugin_list().await?.result.unwrap_or_default();
    assert!(
        !listed.to_string().contains("echo-plugin"),
        "the refused package is still listed: {listed}"
    );

    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_with_a_wasm_component_is_refused_here() -> anyhow::Result<()> {
    a_package_with_a_wasm_component_is_refused(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_package_with_a_wasm_component_is_refused_in_a_spawned_daemon() -> anyhow::Result<()> {
    a_package_with_a_wasm_component_is_refused(Mode::Spawned).await
}

/// Telemetry is a report about a turn, so it is checked against an
/// independent witness of that turn rather than against itself: the stub
/// counted the calls, and the file has to agree.
async fn telemetry_counts_what_the_turn_actually_did(mode: Mode) -> anyhow::Result<()> {
    let (world, provider, daemon, project) = stage("telemetry-counts", mode).await?;
    let target = project.join("counted.txt");
    provider.script([
        Reply::Blocks(vec![Block::tool(
            "call-1",
            "Write",
            json!({ "file_path": target.to_string_lossy(), "content": "counted\n" }),
        )]),
        Reply::text("done"),
    ]);
    let output = world.telemetry_file("counts.jsonl");

    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({
            "options": { "telemetry": { "output": output.to_string_lossy() } }
        }))
        .await?;
    client
        .session_run_turn(&session, "write it", "t1", None)
        .await?;

    let events = telemetry_until(&output, |events| kinds(events).contains(&"turn_complete")).await;
    let of_type = |kind: &str| -> Vec<&serde_json::Value> {
        events
            .iter()
            .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some(kind))
            .collect()
    };

    // Both sides are scoped to the turn, and neither is a raw count. The
    // engine makes one more model call after the turn returns — memory
    // extraction, on a smaller model, with no system prompt and no tools —
    // and it lands asynchronously, so `provider.call_count()` grows on its
    // own schedule. The turn's own calls are the ones carrying its tool
    // table; that call is not one of them.
    let turn_calls = turn_calls(&provider);
    let turn_requests = of_type("api_request")
        .into_iter()
        .filter(|e| e.get("turn_id").and_then(|v| v.as_str()) == Some("t1"))
        .count();
    assert_eq!(
        turn_requests, turn_calls,
        "telemetry and the provider disagree about how many calls this turn made"
    );
    assert_eq!(
        turn_calls, 2,
        "the scripted turn should have made two calls"
    );
    let executions = of_type("tool_execution");
    assert!(
        executions.iter().any(|e| e["tool_name"] == "Write"),
        "the tool that ran is not in the telemetry: {executions:?}"
    );

    // What a call cost arrives in two halves — the input count in
    // `message_start`, the output count in `message_delta` — and the daemon
    // reports one figure per call. A report that only ever carried the
    // second half would say every call read nothing, which is most of what a
    // call actually costs.
    for request in of_type("api_request") {
        assert_eq!(
            request["input_tokens"].as_u64(),
            Some(live::provider::INPUT_TOKENS),
            "the input count the wire carried is not in the telemetry: {request}"
        );
        assert_eq!(
            request["output_tokens"].as_u64(),
            Some(live::provider::OUTPUT_TOKENS),
            "the output count the wire carried is not in the telemetry: {request}"
        );
        // Their own fields, not folded into the input count: a cache read is
        // priced at a fraction of ordinary input and a write at a premium, so
        // one number for all three cannot be turned back into a cost.
        assert_eq!(
            request["cache_creation_tokens"].as_u64(),
            Some(live::provider::CACHE_CREATION_TOKENS),
            "the cache write the wire reported is not in the telemetry: {request}"
        );
        assert_eq!(
            request["cache_read_tokens"].as_u64(),
            Some(live::provider::CACHE_READ_TOKENS),
            "the cache read the wire reported is not in the telemetry: {request}"
        );
    }

    let complete = of_type("turn_complete");
    let complete = complete
        .first()
        .ok_or_else(|| anyhow::anyhow!("no turn_complete event"))?;
    assert_eq!(
        complete["api_calls"].as_u64(),
        Some(2),
        "turn_complete miscounts its calls: {complete}"
    );
    assert_eq!(
        complete["tool_calls"].as_u64(),
        Some(1),
        "turn_complete miscounts its tools: {complete}"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn telemetry_counts_what_the_turn_actually_did_here() -> anyhow::Result<()> {
    telemetry_counts_what_the_turn_actually_did(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn telemetry_counts_what_the_turn_actually_did_in_a_spawned_daemon() -> anyhow::Result<()> {
    telemetry_counts_what_the_turn_actually_did(Mode::Spawned).await
}

/// The `turn_complete` event is the only place a client is told what a turn
/// cost — the final response carries neither `stop_reason` nor `usage`, and
/// the daemon has no telemetry method to ask instead. So the whole of
/// `Usage` has to survive the trip, not the half of it that existed when the
/// frame was first written.
///
/// This is checked at the socket rather than at the engine, which already
/// has a case of its own. The frame between them is assembled by hand, and a
/// hand-assembled frame is exactly where a type can grow a field and leave
/// nobody behind to notice: a cache-heavy turn reports most of its input
/// under `cache_read_input_tokens`, so dropping those two fields does not
/// lose a rounding error, it loses the bulk of what was read.
async fn the_turn_complete_event_carries_the_whole_of_usage(mode: Mode) -> anyhow::Result<()> {
    let (_world, provider, daemon, _project) = stage("turn-complete-usage", mode).await?;
    provider.script([Reply::text("done")]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    let turn = client
        .session_run_turn(&session, "say something", "t1", None)
        .await?;

    let event = turn
        .turn_complete_event
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no turn_complete event: {turn:?}"))?;
    let usage = &event["usage"];

    // Spelled out rather than round-tripped through `Usage`. These names are
    // the protocol, and a test that deserialized them back into the type it
    // is guarding would agree with any renaming the type ever does.
    for (field, expected) in [
        ("input_tokens", live::provider::INPUT_TOKENS),
        ("output_tokens", live::provider::OUTPUT_TOKENS),
        (
            "cache_creation_input_tokens",
            live::provider::CACHE_CREATION_TOKENS,
        ),
        ("cache_read_input_tokens", live::provider::CACHE_READ_TOKENS),
    ] {
        assert_eq!(
            usage[field].as_u64(),
            Some(expected),
            "`{field}` is not what the wire reported, in {event}"
        );
    }

    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_turn_complete_event_carries_the_whole_of_usage_here() -> anyhow::Result<()> {
    the_turn_complete_event_carries_the_whole_of_usage(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn the_turn_complete_event_carries_the_whole_of_usage_in_a_spawned_daemon(
) -> anyhow::Result<()> {
    the_turn_complete_event_carries_the_whole_of_usage(Mode::Spawned).await
}

/// A settings file edited while the daemon runs, and `config.reload` to pick
/// it up. The same session runs both turns: a reload that only reached new
/// sessions would leave every open one on the old configuration, which is
/// the state an operator is least likely to suspect.
async fn a_reload_reaches_a_session_that_is_already_open(mode: Mode) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("reloaded")?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(mode),
    )
    .await?;
    provider.script([
        Reply::text("before"),
        Reply::text("after"),
        Reply::text("spare"),
        Reply::text("spare"),
    ]);

    let mut client = daemon.connect().await?;
    let session = client.session_create(json!({})).await?;
    client
        .session_run_turn(&session, "first", "t1", None)
        .await?;

    // A watermark rather than an index: a turn is not the only thing that
    // calls the model, and a case that assumed call *n* was its own would
    // break the first time the engine did anything else in between.
    let before_reload = provider.call_count();

    world.write_project_settings(
        "reloaded",
        &json!({ "prompt_append": "RELOADED-MARK-PLUGH" }),
    )?;
    let reloaded = client.config_reload(&rpc_client::Target::own()).await?;
    anyhow::ensure!(
        reloaded.error.is_none(),
        "config.reload failed: {reloaded:?}"
    );

    client
        .session_run_turn(&session, "second", "t2", None)
        .await?;

    let calls = provider.calls();
    let (before, after) = calls.split_at(before_reload);
    assert!(
        !before
            .iter()
            .any(|c| c.system_text().contains("RELOADED-MARK-PLUGH")),
        "the mark was in the prompt before it was ever configured"
    );
    assert!(
        after
            .iter()
            .any(|c| c.system_text().contains("RELOADED-MARK-PLUGH")),
        "an open session kept its old settings across config.reload"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_reaches_a_session_that_is_already_open_here() -> anyhow::Result<()> {
    a_reload_reaches_a_session_that_is_already_open(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_reload_reaches_a_session_that_is_already_open_in_a_spawned_daemon() -> anyhow::Result<()>
{
    a_reload_reaches_a_session_that_is_already_open(Mode::Spawned).await
}

/// Installing is a write to disk, and the only way to prove it is a write to
/// disk is to ask a daemon that was not running when it happened.
///
/// The RPC surface of install/list/enable/disable is covered in
/// `daemon/tests/daemon_e2e.rs`, in one process. That cannot distinguish a
/// package registered on disk from one remembered in a `HashMap`, because
/// nothing there ever forgets.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns two real daemon processes"]
async fn an_installed_package_outlives_the_daemon_that_installed_it() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("packaged")?;
    let provider = ProviderStub::start().await?;
    let (archive, checksum) = test_runner::plugin_fixture::package_demo_plugin(world.root())?;

    let first = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).mode(Mode::Spawned),
    )
    .await?;
    let mut client = first.connect().await?;
    let installed = client
        .call(
            "plugin.install",
            json!({
                "name": "demo-plugin",
                "version": "1.0.0",
                "download_url": format!("file://{}", archive.display()),
                "checksum": checksum,
            }),
        )
        .await?;
    anyhow::ensure!(
        installed.error.is_none(),
        "plugin.install failed: {installed:?}"
    );
    first.stop().await;

    let second = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project).mode(Mode::Spawned),
    )
    .await?;
    let mut client = second.connect().await?;
    let listed = client.call("plugin.list", json!({})).await?;
    let plugins = listed
        .result
        .as_ref()
        .and_then(|r| r.get("plugins"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let demo = plugins
        .iter()
        .find(|p| p["name"] == "demo-plugin")
        .ok_or_else(|| {
            anyhow::anyhow!("a package installed by the previous daemon is gone: {plugins:?}")
        })?;
    assert_eq!(
        demo["enabled"], true,
        "the package came back disabled: {demo}"
    );
    second.stop().await;
    Ok(())
}

/// The engine calls the model for work no turn asked for — extracting
/// memories once a turn is over, summarizing for compaction, running a prompt
/// hook. Each is a real call to a real provider that a host pays for, and
/// none of them appeared anywhere in the telemetry that host reads: the only
/// place `api_request` was emitted from was inside the turn loop.
///
/// Memory extraction is the one every session does, so it is the one asserted
/// here. It also arrives after the turn has answered, which is why this waits
/// rather than reads.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_no_turn_made_is_still_accounted_for() -> anyhow::Result<()> {
    let (world, provider, daemon, _project) = stage("auxiliary", Mode::InProcess).await?;
    provider.script([
        Reply::text("said something worth remembering"),
        Reply::text("[]"),
    ]);
    let output = world.telemetry_file("auxiliary.jsonl");

    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({
            "options": { "telemetry": { "output": output.to_string_lossy() } }
        }))
        .await?;
    client
        .session_run_turn(&session, "remember this", "t1", None)
        .await?;

    let events = telemetry_until(&output, |events| {
        events
            .iter()
            .any(|e| e.get("purpose").and_then(|p| p.as_str()) == Some("memory"))
    })
    .await;

    let auxiliary: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e.get("purpose").and_then(|p| p.as_str()).is_some())
        .collect();
    assert!(
        auxiliary
            .iter()
            .any(|e| e["purpose"] == "memory" && e["type"] == "api_request"),
        "the memory extraction call cost something and said so nowhere: {:?}",
        kinds(&events)
    );

    let memory_call = auxiliary
        .iter()
        .find(|e| e["purpose"] == "memory")
        .expect("just asserted");
    assert_eq!(
        memory_call["input_tokens"].as_u64(),
        Some(live::provider::INPUT_TOKENS),
        "an auxiliary call is accounted for without what it cost: {memory_call}"
    );

    // The turn's own calls stay unlabelled: a reader summing everything gets
    // the session total, and one asking what the turns alone cost can still
    // tell them apart.
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "api_request"
                && e.get("purpose").map(|p| p.is_null()).unwrap_or(true)),
        "the turn's own call grew a purpose it should not have"
    );
    daemon.stop().await;
    Ok(())
}
