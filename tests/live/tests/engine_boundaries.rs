//! The engine's own seams, against a scripted model: a recording replayed
//! with no model behind it (a sub-agent's calls included), the telemetry a
//! turn writes, a concurrency-safe tool started while the stream that asked
//! for it is still arriving, a tool the policy refuses never reaching the
//! outside world, and the recovery policy on failures a provider reports
//! inside an open stream or as a refusal for size.
//!
//! Each needs the model's side held fixed — a stall at a chosen block, an
//! error event after a 200 — which no real endpoint produces on demand.

use std::time::{Duration, Instant};

use live::{
    telemetry_kinds as kinds, telemetry_until, Block, LiveConfig, LiveRun, LiveRunBuilder, Page,
    PageServer, ProviderStub, Purpose, Reply,
};
use serde_json::{json, Value};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        live::ModelEndpoint::model(stub),
    ))
}

fn result_of<'a>(
    events: &'a rpc_client::TurnEvents,
    tool: &str,
) -> Option<&'a (String, String, bool)> {
    events.tool_results.iter().find(|(name, _, _)| name == tool)
}

fn turn_calls(stub: &ProviderStub) -> Vec<live::SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() != Purpose::Title)
        .collect()
}

/// A foreground sub-agent's turn inside the parent's: the parent asks for
/// it, the sub-agent answers, the parent finishes.
fn delegation(sub_answer: &str, parent_answer: &str) -> Vec<Reply> {
    vec![
        Reply::calls(
            "a1",
            "Agent",
            json!({ "description": "helper", "prompt": "Answer with the helper word." }),
        ),
        Reply::text(sub_answer),
        Reply::text(parent_answer),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recording_replays_a_turn_and_its_sub_agent_without_the_model() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    stub.script_when("标题生成器", [Reply::text("recorded title")]);
    let mut run = stubbed("recorder", &stub).start().await?;
    let dir = run.world.root().join("recordings");
    std::fs::create_dir_all(&dir)?;
    let recorder = |mode: &str| {
        json!({ "options": { "recorder": {
            "mode": mode, "name": "round-trip", "dir": dir.to_string_lossy(), "strict": true
        } } })
    };

    stub.script(delegation("SUB-WORD-7", "PARENT-SAW-SUB-WORD-7"));
    let recording = run.session_with(recorder("record")).await?;
    let recorded = run.turn(&recording, "delegate it").await?;
    assert!(
        recorded.text.contains("PARENT-SAW-SUB-WORD-7"),
        "{}",
        recorded.text
    );
    run.wait_tasks_settled(&recording, Duration::from_secs(10))
        .await?;
    let model_calls = turn_calls(&stub).len();
    assert_eq!(model_calls, 3, "parent, sub-agent, parent");
    let calls_file = dir.join("round-trip").join("calls.jsonl");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(&calls_file)
        .unwrap_or_default()
        .lines()
        .count()
        < 3
    {
        anyhow::ensure!(
            Instant::now() < deadline,
            "the recording never held the three calls: {calls_file:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let replaying = run.session_with(recorder("replay")).await?;
    let replayed = run.turn(&replaying, "delegate it").await?;
    assert!(
        replayed.text.contains("PARENT-SAW-SUB-WORD-7"),
        "the replay answered from somewhere else: {}",
        replayed.text
    );
    assert!(
        replayed.tool_uses.iter().any(|(n, _)| n == "Agent"),
        "{:?}",
        replayed.tool_uses
    );
    let tasks = run
        .wait_tasks_settled(&replaying, Duration::from_secs(10))
        .await?;
    assert!(
        tasks
            .iter()
            .any(|t| t["kind"] == "agent" && t["status"] == "done"),
        "the replayed sub-agent ran to the end: {tasks:?}"
    );
    assert_eq!(
        turn_calls(&stub).len(),
        model_calls,
        "a replay reaches no model, the sub-agent's calls included"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turns_telemetry_names_its_calls_and_its_tools() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("telemetry-turn", &stub).start().await?;
    let output = run.world.telemetry_file("turn.jsonl");
    let session = run
        .session_with(json!({ "options": { "telemetry": { "output": output.to_string_lossy() } } }))
        .await?;
    stub.script([
        Reply::calls(
            "w1",
            "Write",
            json!({ "file_path": run.project.join("out.txt").to_string_lossy(), "content": "x\n" }),
        ),
        Reply::text("written"),
    ]);
    run.turn(&session, "write it").await?;

    let events = telemetry_until(&output, |e| kinds(e).contains(&"turn_complete")).await;
    let written = kinds(&events);
    eprintln!("[telemetry-turn] {written:?}");
    for expected in [
        "session_start",
        "turn_start",
        "tool_start",
        "tool_execution",
        "turn_complete",
    ] {
        assert!(written.contains(&expected), "no `{expected}`: {written:?}");
    }
    assert_eq!(
        written.iter().filter(|k| **k == "api_request").count(),
        2,
        "one api_request per model call: {written:?}"
    );
    let tool = events
        .iter()
        .find(|e| e["type"] == "tool_execution")
        .cloned()
        .unwrap_or_default();
    assert!(tool.to_string().contains("Write"), "{tool}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: a sub-agent the Agent tool runs records no agent_spawned / agent_completed — only the team coordinator emits them (team/src/coordinator.rs:389; agents/src/agent_tool.rs never does)"]
async fn a_sub_agent_shows_in_its_sessions_telemetry() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("telemetry-agent", &stub).start().await?;
    let output = run.world.telemetry_file("agent.jsonl");
    let session = run
        .session_with(json!({ "options": { "telemetry": { "output": output.to_string_lossy() } } }))
        .await?;
    stub.script(delegation("SUB", "PARENT"));
    run.turn(&session, "delegate it").await?;

    let events = telemetry_until(&output, |e| {
        kinds(e).contains(&"turn_complete") && kinds(e).contains(&"agent_completed")
    })
    .await;
    let written = kinds(&events);
    eprintln!("[telemetry-agent] {written:?}");
    for expected in ["agent_spawned", "agent_completed"] {
        assert!(written.contains(&expected), "no `{expected}`: {written:?}");
    }
    assert_eq!(
        written.iter().filter(|k| **k == "api_request").count(),
        3,
        "the sub-agent's call is the session's spending too: {written:?}"
    );
    run.finish().await;
    Ok(())
}

/// The model asks for a fetch, then keeps the stream open for a while
/// before it asks for anything else.
fn fetch_then_stall(url: &str, stall: Duration) -> Reply {
    Reply::stalled(
        0,
        stall,
        Reply::Blocks(vec![
            Block::tool("f1", "WebFetch", json!({ "url": url })),
            Block::tool("g1", "Glob", json!({ "pattern": "*.nothing" })),
        ]),
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: a concurrency-safe call is pushed onto a FuturesUnordered that nothing polls until drain_batch after the stream ends (runtime/src/streaming.rs:192, :244), so it starts only once the whole response is in"]
async fn a_concurrency_safe_tool_starts_while_its_stream_is_still_arriving() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route("/early", Page::text("EARLY-PAGE"));
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("streaming-exec", &stub).start().await?;
    let session = run.session().await?;
    let stall = Duration::from_secs(4);
    stub.script([
        fetch_then_stall(&pages.url("/early"), stall),
        Reply::text("done"),
    ]);
    let handle = run.spawn_turn(&session, "fetch", json!({})).await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while turn_calls(&stub).is_empty() {
        anyhow::ensure!(Instant::now() < deadline, "the turn never called the model");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let asked = Instant::now();
    while pages.hits("/early") == 0 && asked.elapsed() < stall + Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let fetched_after = asked.elapsed();
    let events = tokio::time::timeout(Duration::from_secs(20), handle).await???;
    eprintln!("[streaming-exec] fetched {fetched_after:?} after the request, stall {stall:?}");
    assert_eq!(pages.hits("/early"), 1);
    assert!(
        fetched_after < stall - Duration::from_secs(1),
        "the fetch waited for the rest of the stream: {fetched_after:?}"
    );
    let fetched = result_of(&events, "WebFetch").expect("a fetch result");
    assert!(
        !fetched.2 && fetched.1.contains("EARLY-PAGE"),
        "{fetched:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_the_policy_refuses_never_runs() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route("/ruled", Page::text("RULED"));
    pages.route("/hooked", Page::text("HOOKED"));
    let stall = Duration::from_millis(1500);

    let stub = ProviderStub::start().await?;
    let mut run = stubbed("deny-rule", &stub)
        .settings(json!({
            "permission_mode": "default",
            "permission_rules": [{ "tool": "WebFetch", "action": "deny" }]
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        fetch_then_stall(&pages.url("/ruled"), stall),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "fetch").await?;
    assert_eq!(pages.hits("/ruled"), 0, "a deny rule let the fetch out");
    let refused = result_of(&events, "WebFetch").expect("a fetch result");
    assert!(refused.2, "the model was told: {refused:?}");
    run.finish().await;

    let scratch = tempfile::tempdir()?;
    let hook = scratch.path().join("block.sh");
    std::fs::write(
        &hook,
        "#!/bin/sh\ncat > /dev/null\necho '{\"decision\":\"block\",\"message\":\"not here\"}'\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("deny-hook", &stub)
        .settings(json!({
            "hooks_config": {
                "PreToolUse": [{ "type": "command", "command": hook, "timeout": 5000 }]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        fetch_then_stall(&pages.url("/hooked"), stall),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "fetch").await?;
    assert_eq!(
        pages.hits("/hooked"),
        0,
        "a blocking hook let the fetch out"
    );
    let blocked = result_of(&events, "WebFetch").expect("a fetch result");
    assert!(blocked.2, "the model was told: {blocked:?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: an error event inside an open stream is mapped to Api{500} whatever its type (model/src/adapter.rs:494) and execute_stream's Err leaves the turn by `?` (runtime/src/turn.rs:1338), so the fallback model is never tried"]
async fn an_overload_reported_inside_the_stream_moves_to_the_fallback_model() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("in-stream-overload", &stub)
        .settings(json!({ "model": { "fallback_model": "fallback-stub-model" } }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::errored(vec![Block::text("half an ans")], "overloaded_error", "busy"),
        Reply::text("RECOVERED-ON-FALLBACK"),
    ]);
    let events = run.turn(&session, "hello").await?;
    assert!(
        events.text.contains("RECOVERED-ON-FALLBACK"),
        "{}",
        events.text
    );
    let calls = turn_calls(&stub);
    assert_eq!(calls.len(), 2, "one failed call, one retry");
    assert_eq!(calls[1].model(), "fallback-stub-model");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: the adapter hands back Ok(stream) before the request is sent (model/src/adapter.rs:176; the request goes out on first poll, model/src/client.rs:332), so a 529 surfaces inside execute_stream and leaves by `?` (runtime/src/turn.rs:1338) — the recovery match at turn.rs:1157 never sees it and the fallback model is never tried"]
async fn an_overload_refused_before_the_stream_moves_to_the_fallback_model() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("open-overload", &stub)
        .settings(json!({ "model": { "fallback_model": "fallback-stub-model" } }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.inject_failures_when(
        "\"model\":\"claude-sonnet-4-6\"",
        8,
        Reply::status(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
        ),
    );
    stub.script([Reply::text("RECOVERED-ON-FALLBACK")]);
    let events = run.turn(&session, "hello").await?;
    assert!(
        events.text.contains("RECOVERED-ON-FALLBACK"),
        "{}",
        events.text
    );
    let last = turn_calls(&stub).pop().expect("a call");
    assert_eq!(last.model(), "fallback-stub-model");
    run.finish().await;
    Ok(())
}

/// A conversation with something in it to compact: a few rounds of reads.
async fn a_few_rounds(run: &mut LiveRun, stub: &ProviderStub, session: &str) -> anyhow::Result<()> {
    let filler: String = (0..200).map(|i| format!("line {i} of filler\n")).collect();
    for i in 0..4 {
        run.write_file(&format!("f{i}.txt"), &filler)?;
    }
    let mut script: Vec<Reply> = (0..4)
        .map(|i| {
            Reply::calls(
                &format!("r{i}"),
                "Read",
                json!({ "file_path": run.project.join(format!("f{i}.txt")).to_string_lossy() }),
            )
        })
        .collect();
    script.push(Reply::text("read them"));
    stub.script(script);
    run.turn(session, "read the files").await?;
    Ok(())
}

const TOO_LONG: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 212034 tokens > 200000 maximum"}}"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: the 400 surfaces inside execute_stream and leaves by `?` (runtime/src/turn.rs:1338, the request is only sent on first poll: model/src/client.rs:332); were it seen, it is ModelError::Api and classify_failure only takes ModelError::Internal containing `prompt too long` (runtime/src/turn.rs:3204), not the provider's `prompt is too long`"]
async fn a_refusal_for_size_compacts_and_retries() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("too-long", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session).await?;
    let watcher = run.watch(&session, false).await?;
    stub.inject_failures(1, Reply::status(400, TOO_LONG));
    stub.script([Reply::text("ANSWERED-AFTER-COMPACTING")]);
    let events = run.turn(&session, "and now?").await?;
    assert!(
        events.text.contains("ANSWERED-AFTER-COMPACTING"),
        "{}",
        events.text
    );
    assert!(
        !watcher.events("compact").is_empty(),
        "nothing was compacted"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: an error event inside an open stream is mapped to Api{500} whatever it says (model/src/adapter.rs:494) and execute_stream's Err leaves the turn by `?` (runtime/src/turn.rs:1338), so a size refusal there never compacts"]
async fn a_refusal_for_size_inside_the_stream_compacts_and_retries() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("too-long-in-stream", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session).await?;
    let watcher = run.watch(&session, false).await?;
    let message: Value = serde_json::from_str(TOO_LONG)?;
    stub.script([
        Reply::errored(
            vec![],
            "invalid_request_error",
            message["error"]["message"].as_str().unwrap_or_default(),
        ),
        Reply::text("ANSWERED-AFTER-COMPACTING"),
    ]);
    let events = run.turn(&session, "and now?").await?;
    assert!(
        events.text.contains("ANSWERED-AFTER-COMPACTING"),
        "{}",
        events.text
    );
    assert!(
        !watcher.events("compact").is_empty(),
        "nothing was compacted"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}
