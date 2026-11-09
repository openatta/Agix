//! The engine's own seams, against a scripted model: a recording replayed
//! with no model behind it (a sub-agent's calls included), the telemetry a
//! turn writes, a concurrency-safe tool started while the stream that asked
//! for it is still arriving, a tool the policy refuses never reaching the
//! outside world, the recovery policy on failures a provider reports
//! inside an open stream or as a refusal for size, and an OpenAI-compatible
//! stream that closes without saying it is done.
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
        "user_prompt_submit",
        "permission_decision",
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
async fn cost_reports_what_the_provider_counted_and_telemetry_names_the_command(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("telemetry-cost", &stub).start().await?;
    let output = run.world.telemetry_file("cost.jsonl");
    let session = run
        .session_with(json!({ "options": { "telemetry": { "output": output.to_string_lossy() } } }))
        .await?;
    stub.script([Reply::text("hello")]);
    run.turn(&session, "say hello").await?;
    let cost = run.turn(&session, "/cost").await?;
    assert!(
        cost.text
            .starts_with("Since this session was loaded: 1 model calls, 0 tool calls;"),
        "{}",
        cost.text
    );
    let events = telemetry_until(&output, |e| kinds(e).contains(&"slash_command_used")).await;
    let used = events
        .iter()
        .find(|e| e["type"] == "slash_command_used")
        .cloned()
        .unwrap_or_default();
    assert_eq!(used["command"], "cost", "{used}");
    assert_eq!(used["kind"], "local", "{used}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
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

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_cut_off_by_max_turns_says_so_to_its_parent() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("max-turns-note", &stub).start().await?;
    run.write_file(
        ".agix/agents/brief.md",
        "---\ndescription: Short on calls\nmax_turns: 1\n---\nBRIEF-ROLE.\n",
    )?;
    let session = run.session().await?;
    stub.script([
        Reply::calls(
            "a1",
            "Agent",
            json!({ "description": "brief", "prompt": "Go.", "subagent_type": "brief" }),
        ),
        Reply::calls("g1", "Glob", json!({ "pattern": "*.nothing" })),
        Reply::text("parent done"),
    ]);
    let events = run.turn(&session, "delegate it").await?;
    let agent = result_of(&events, "Agent").expect("an Agent result");
    assert!(
        agent.1.contains("reached its limit of model calls") && agent.1.contains("no answer"),
        "{agent:?}"
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
async fn a_concurrency_safe_tool_starts_while_its_stream_is_still_arriving() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route("/early", Page::text("EARLY-PAGE"));
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("streaming-exec", &stub)
        .settings(json!({ "sandbox": PageServer::reachable() }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
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
            "sandbox": PageServer::reachable(),
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
            "sandbox": PageServer::reachable(),
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

fn chat_text(text: &str, finish_reason: Option<&str>) -> Value {
    json!({ "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": finish_reason }] })
}

/// A chat stream that closes with neither `[DONE]` nor a `finish_reason`
/// still ends the turn with what arrived — many gateways send neither — but
/// a dropped connection looks the same, so the turn says it may be cut
/// short. A `finish_reason` alone is a normal end.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_stream_closed_without_an_end_marker_marks_the_turn_truncated() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("chat-truncated", &stub)
        .settings(json!({
            "providers": { "chat": {
                "api_type": "openai_compatible",
                "base_url": stub.base_url().as_str(),
                "api_key": "stub-key",
                "default_model": "chat-stub-model"
            }},
            "default_provider": "chat"
        }))
        .start()
        .await?;
    stub.script_when(
        "标题生成器",
        (0..4).map(|_| Reply::chat(vec![chat_text("title", Some("stop"))], true)),
    );
    let session = run.session().await?;
    stub.script([
        Reply::chat(vec![chat_text("HALF-AN-ANSWER", None)], false),
        Reply::chat(vec![chat_text("WHOLE-ANSWER", Some("stop"))], false),
    ]);

    let cut = run.turn(&session, "first").await?;
    assert!(cut.text.contains("HALF-AN-ANSWER"), "{}", cut.text);
    let done = cut.turn_complete_event.expect("turn_complete");
    assert_eq!(done["truncated"], true, "{done}");
    assert_eq!(done["stop_reason"], "end_turn", "{done}");

    let whole = run.turn(&session, "second").await?;
    assert!(whole.text.contains("WHOLE-ANSWER"), "{}", whole.text);
    let done = whole.turn_complete_event.expect("turn_complete");
    assert!(done.get("truncated").is_none(), "{done}");

    assert!(
        stub.seen()
            .iter()
            .any(|r| r.path.ends_with("/chat/completions")),
        "the turns never reached the chat endpoint"
    );
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

const SESSION_CONTEXT: &str = "you can use the following context";

/// The compaction a size refusal sets off is the same one the budget sets
/// off: `PreCompact` runs, and the opening context it dropped (the project's
/// AGENTS.md among it) is back on the next turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_for_size_compacts_and_retries() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let hooks = tempfile::tempdir()?;
    let ledger = hooks.path().join("pre-compact.jsonl");
    let script = hooks.path().join("pre-compact.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat >> '{}'\necho >> '{}'\necho '{{}}'\n",
            ledger.display(),
            ledger.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }
    let mut run = stubbed("too-long", &stub)
        .settings(json!({ "hooks_config": { "PreCompact": [
            { "type": "command", "command": script, "timeout": 30_000 }
        ] } }))
        .before_start(|_, project| {
            std::fs::write(project.join("AGENTS.md"), "# Rules\nPROJECT-RULE-MARK\n")?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session).await?;
    let watcher = run.watch(&session, false).await?;
    stub.script_when(
        "You are compacting an AI coding agent's conversation",
        [Reply::text("1. User intent — read four files.")],
    );
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
    let pre_compact: Vec<Value> = std::fs::read_to_string(&ledger)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(pre_compact.len(), 1, "{pre_compact:?}");
    assert_eq!(pre_compact[0]["tool_input"]["trigger"], "auto");

    let retried = turn_calls(&stub)
        .into_iter()
        .rfind(|c| c.purpose() == Purpose::Turn)
        .expect("the retry");
    assert!(
        !retried.messages_contain(SESSION_CONTEXT),
        "this case needs a compaction that drops the opening context"
    );
    stub.script([Reply::text("NEXT-TURN")]);
    run.turn(&session, "one more").await?;
    let next = turn_calls(&stub)
        .into_iter()
        .rfind(|c| c.purpose() == Purpose::Turn)
        .expect("the next turn's call");
    assert!(
        next.messages_contain(SESSION_CONTEXT) && next.messages_contain("PROJECT-RULE-MARK"),
        "the opening context did not come back after the compaction dropped it"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
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

const FILES_REMINDER: &str = "The following files were read before context compaction";

/// A threshold below what every request carries anyway, and a keep floor
/// that leaves nothing old enough to drop: there is nothing a compaction can
/// do, so none happens — no `compact` frame, and the file read is not
/// re-attached step after step.
#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_with_nothing_to_drop_does_not_happen() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("noop-compact", &stub)
        .settings(json!({ "compaction": { "threshold_tokens": 2000, "keep_recent": 50 } }))
        .start()
        .await?;
    let notes = run.write_file("notes.txt", "build: green\nowner: ops\n")?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([
        Reply::calls(
            "n1",
            "Read",
            json!({ "file_path": notes.to_string_lossy() }),
        ),
        Reply::calls("n2", "Glob", json!({ "pattern": "*.txt" })),
        Reply::calls("n3", "Glob", json!({ "pattern": "*.md" })),
        Reply::calls("n4", "Glob", json!({ "pattern": "*.rs" })),
        Reply::text("DONE"),
    ]);
    run.turn(&session, "read the notes, then look around")
        .await?;
    assert_eq!(watcher.events("compact"), Vec::<Value>::new());
    let last = turn_calls(&stub).pop().expect("a call");
    assert_eq!(last.messages_text().matches(FILES_REMINDER).count(), 0);
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// The predictive pass blanks old tool results in the middle of a turn; the
/// host is told, on the one frame that says why results vanished.
#[tokio::test(flavor = "multi_thread")]
async fn a_reactive_compaction_says_so_on_the_wire() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("reactive-frame", &stub)
        .settings(json!({
            "compaction": { "reactive": true, "threshold_tokens": 60000, "keep_recent": 2 }
        }))
        .start()
        .await?;
    let body: String = (0..80)
        .map(|l| format!("line {l}: all values nominal\n"))
        .collect();
    let mut script = Vec::new();
    for i in 0..8 {
        let path = run.write_file(&format!("part{i}.log"), &body)?;
        script.push(Reply::calls(
            &format!("p{i}"),
            "Read",
            json!({ "file_path": path.to_string_lossy() }),
        ));
    }
    script.push(Reply::text("read them all"));
    stub.script(script);
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    run.turn(&session, "read the parts").await?;
    let frames = watcher.events("compact");
    let frame = &frames
        .iter()
        .find(|f| f["event"]["strategy"] == "MicroCompact")
        .unwrap_or_else(|| panic!("no MicroCompact frame: {frames:?}"))["event"];
    assert_eq!(frame["messages_before"], frame["messages_after"], "{frame}");
    assert!(
        frame["tokens_after"].as_u64() < frame["tokens_before"].as_u64(),
        "{frame}"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}
