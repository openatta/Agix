//! The harness's own tools, each used once against a stub so a case built on
//! one can trust it: `LiveRun`'s builder and turn helpers driven through a
//! `LiveProvider` in front of a `ProviderStub`, a `Watcher` that answers as
//! told, the stub's scripted endings and injected failures, request
//! purposes, the page server, the toy MCP server over both transports, and
//! the `World` fixtures (git project, declarative scene, memory directory).

use std::time::Duration;

use live::{
    Answer, Block, LiveConfig, LiveRun, LiveRunBuilder, McpToy, Page, PageServer, ProviderStub,
    Purpose, Reply,
};
use serde_json::json;

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        live::ModelEndpoint::model(stub),
    ))
}

/// A client for this machine's servers, past the shell's proxy.
fn direct_http() -> anyhow::Result<reqwest::Client> {
    base::interface::exec::local::install_tls_provider();
    Ok(reqwest::Client::builder().no_proxy().build()?)
}

fn result_of<'a>(
    events: &'a rpc_client::TurnEvents,
    tool: &str,
) -> Option<&'a (String, String, bool)> {
    events.tool_results.iter().find(|(name, _, _)| name == tool)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_builder_writes_before_the_daemon_starts_and_applies_its_options() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("builder", &stub)
        .settings(json!({ "prompt_append": "PROJECT-LAYER" }))
        .options(|o| o.session_cap(2).prompt_timeout(Duration::from_secs(3)))
        .before_start(|world, project| {
            world.write_global_settings(&json!({ "language": "Klingon-GLOBAL-LAYER" }))?;
            std::fs::write(project.join("fixture.txt"), "from before_start")?;
            Ok(())
        })
        .start()
        .await?;
    assert_eq!(
        run.read_file("fixture.txt").as_deref(),
        Some("from before_start")
    );

    let session = run.session().await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "hello").await?;
    let system = stub.calls()[0].system_text();
    assert!(system.contains("PROJECT-LAYER"), "{system}");
    assert!(
        system.contains("Klingon-GLOBAL-LAYER"),
        "the global layer written before start is in force: {system}"
    );
    assert_eq!(run.provider.calls().len(), 1, "the proxy recorded the call");

    // `session_cap(2)` reaches the pool: a third live session evicts the
    // first rather than all three staying in memory.
    run.session().await?;
    run.session().await?;
    let listed = run.client.session_list().await?;
    let live_count = listed.result.unwrap_or_default()["sessions"]
        .as_array()
        .map_or(0, |s| s.iter().filter(|s| s["active"] == true).count());
    assert!(
        live_count <= 2,
        "session cap not applied: {live_count} live"
    );

    run.restart().await?;
    let again = run.session().await?;
    stub.script([Reply::text("after restart")]);
    run.turn(&again, "still there?").await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_with_sends_attachments_and_spawn_turn_can_be_interrupted() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("turns", &stub).start().await?;
    let session = run.session().await?;

    stub.script([Reply::text("seen")]);
    run.turn_with(
        &session,
        "read the attachment",
        json!({ "attachments": [{ "kind": "text", "path": "selection", "content": "ATTACHED-7731" }] }),
    )
    .await?;
    assert!(stub.calls()[0].messages_contain("ATTACHED-7731"));

    stub.script([Reply::delayed(
        Duration::from_secs(2),
        Reply::text("after the wait"),
    )]);
    let started = std::time::Instant::now();
    let handle = run
        .spawn_turn(&session, "take your time", json!({}))
        .await?;
    run.wait_turn_state(&session, "running", Duration::from_secs(10))
        .await?;
    let busy = run
        .spawn_turn(&session, "me too", json!({ "on_busy": "reject" }))
        .await?
        .await??;
    let error = busy.response.and_then(|r| r.error);
    assert_eq!(error.map(|e| e.code), Some(-32015), "SESSION_BUSY");
    let waited = handle.await??;
    assert!(waited.text.contains("after the wait"));
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "Reply::Delay held"
    );

    stub.script([
        Reply::calls(
            "b1",
            "Bash",
            json!({ "command": "sleep 30; echo late > late.txt" }),
        ),
        Reply::text("never reached"),
    ]);
    let handle = run
        .spawn_turn(&session, "run the slow thing", json!({}))
        .await?;
    run.wait_turn_state(&session, "running", Duration::from_secs(10))
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let interrupted = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(
        interrupted.result.unwrap_or_default()["interrupted"],
        json!(true)
    );
    let events = tokio::time::timeout(Duration::from_secs(15), handle).await???;
    assert!(!events.text.contains("never reached"), "{events:?}");
    assert!(run.read_file("late.txt").is_none());
    run.wait_turn_state(&session, "idle", Duration::from_secs(10))
        .await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_denies_permits_always_answers_questions_and_can_stay_silent(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("watcher", &stub)
        .settings(json!({ "permission_mode": "default" }))
        .options(|o| o.prompt_timeout(Duration::from_secs(1)))
        .start()
        .await?;
    let session = run
        .session_with(json!({ "options": { "prompt_types": ["permission", "question"] } }))
        .await?;
    let watcher = run
        .watch_with(&session, |event| {
            if event["prompt_type"] == "question" {
                return Some(Answer::choose("b"));
            }
            let message = event["input"]["command"].as_str().unwrap_or_default();
            if message.contains("denied.txt") {
                Some(Answer::deny("NOT-THAT-FILE"))
            } else if message.contains("always.txt") {
                Some(Answer::permit_always("local"))
            } else {
                None
            }
        })
        .await?;
    let write = |file: &str| {
        Reply::calls(
            &format!("w-{file}"),
            "Bash",
            json!({ "command": format!("echo x > {file}") }),
        )
    };
    stub.script([
        write("denied.txt"),
        write("always.txt"),
        write("silent.txt"),
        Reply::calls(
            "q1",
            "AskUserQuestion",
            json!({ "question": "Which?", "options": [
                { "key": "a", "label": "ALPHA-OPTION" },
                { "key": "b", "label": "BETA-OPTION" }
            ] }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "write three files, then ask").await?;

    assert!(run.read_file("denied.txt").is_none());
    let denied = &events.tool_results[0];
    assert!(denied.2 && denied.1.contains("NOT-THAT-FILE"), "{denied:?}");
    assert_eq!(run.read_file("always.txt").as_deref(), Some("x\n"));
    let local = run
        .read_file(".agix/settings.local.json")
        .unwrap_or_default();
    assert!(local.contains("Bash"), "permit_always(local): {local}");
    assert!(run.read_file("silent.txt").is_none(), "unanswered = denied");
    assert!(events.tool_results[2].2, "{:?}", events.tool_results[2]);
    assert_eq!(
        watcher.asked().len(),
        4,
        "every prompt is recorded, answered or not"
    );
    let last = stub.calls().last().cloned().expect("calls");
    assert!(
        last.messages_contain("BETA-OPTION"),
        "the chosen option reached the model"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn injected_failures_are_retried_and_replies_end_as_scripted() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("faults", &stub).start().await?;
    let session = run.session().await?;

    run.provider.inject_failures(
        1,
        Reply::status(
            529,
            "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}",
        ),
    );
    stub.script([Reply::text("after the proxy's failure")]);
    let events = run.turn(&session, "one").await?;
    assert!(events.text.contains("after the proxy's failure"));
    assert_eq!(
        stub.call_count(),
        1,
        "the injected failure never reached the stub"
    );
    assert_eq!(run.provider.exchanges().len(), 2);
    assert_eq!(run.provider.exchanges()[0].status, 529);

    stub.inject_failures(1, Reply::status(529, "overloaded"));
    stub.script([Reply::Blocks(vec![
        Block::thinking("THINKING-ABOUT-IT"),
        Block::text("after the stub's failure"),
    ])]);
    let events = run.turn(&session, "two").await?;
    assert!(events.text.contains("after the stub's failure"));
    assert!(!events.text.contains("THINKING-ABOUT-IT"));
    assert!(
        events.events.iter().any(|e| e["kind"] == "thinking_delta"),
        "{:?}",
        events.events
    );
    assert_eq!(stub.call_count(), 3);

    stub.script([
        Reply::ended(vec![Block::text("cut short")], "max_tokens"),
        Reply::text("continued"),
    ]);
    let events = run.turn(&session, "three").await?;
    let ended = events
        .events
        .iter()
        .find(|e| e["kind"] == "step_end")
        .cloned()
        .unwrap_or_default();
    assert_eq!(ended["stop_reason"], "max_tokens", "{:?}", events.events);

    let raw = direct_http()?;
    let other = ProviderStub::start().await?;
    run.provider
        .route("/v1/chat/completions", other.base_url().as_str());
    let proxy = live::ModelEndpoint::base_url(&run.provider);
    raw.post(format!("{proxy}v1/chat/completions"))
        .body("{\"model\":\"m\"}")
        .send()
        .await?;
    assert_eq!(other.seen().len(), 1, "the route went to its own upstream");
    assert_eq!(other.seen()[0].path, "/v1/chat/completions");
    run.finish().await;

    let url = format!("{}v1/messages", stub.base_url());
    stub.script([
        Reply::ended(vec![Block::text("x")], "pause_turn"),
        Reply::cut(300, Reply::text("never whole")),
    ]);
    let paused = raw.post(&url).body("{}").send().await?.text().await?;
    assert!(
        paused.contains("\"stop_reason\":\"pause_turn\""),
        "{paused}"
    );
    let cut = raw.post(&url).body("{}").send().await?.bytes().await;
    if let Ok(body) = cut {
        let body = String::from_utf8_lossy(&body);
        assert!(
            !body.contains("message_stop"),
            "the stream was not cut: {body}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_requests_purpose_is_told_apart() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("purpose", &stub).start().await?;
    let session = run.session().await?;
    for i in 0..3 {
        stub.script([Reply::text(format!(
            "answer {i} {}",
            "padding ".repeat(200)
        ))]);
        run.turn(
            &session,
            &format!("question {i} {}", "context ".repeat(200)),
        )
        .await?;
    }
    stub.script([Reply::text("SUMMARY: three questions were asked.")]);
    run.turn(&session, "/compact").await?;
    let purposes: Vec<Purpose> = stub.calls().iter().map(|c| c.purpose()).collect();
    assert_eq!(&purposes[..3], &[Purpose::Turn; 3]);
    assert!(
        purposes.contains(&Purpose::Compact),
        "no compaction call among {purposes:?}"
    );
    let turn = &stub.calls()[0];
    assert!(turn.has_tools() && turn.model() == "claude-sonnet-4-6");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_server_serves_what_it_was_given_and_counts_the_hits() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route(
        "/doc",
        Page::html("<html><body><p>PAGE-TOKEN-4410 &amp; 中文</p></body></html>"),
    );
    pages.route("/moved", Page::redirect("/doc"));
    pages.route("/gone", Page::status(410, "gone"));
    pages.route("/data", Page::json(&json!({ "k": "v" })));
    pages.route(
        "/big",
        Page::huge(70_000, &[(100, "EARLY-MARK"), (60_000, "LATE-MARK")]),
    );
    pages.route(
        "/slow",
        Page::slow(Duration::from_millis(300), Page::text("eventually")),
    );

    let http = direct_http()?;
    let followed = http.get(pages.url("/moved")).send().await?.text().await?;
    assert!(followed.contains("PAGE-TOKEN-4410"));
    assert_eq!(http.get(pages.url("/gone")).send().await?.status(), 410);
    assert_eq!(http.get(pages.url("/nowhere")).send().await?.status(), 404);
    let big = http.get(pages.url("/big")).send().await?.text().await?;
    assert_eq!(big.chars().count(), 70_000);
    assert_eq!(big.find("LATE-MARK"), Some(60_000));
    let started = std::time::Instant::now();
    http.get(pages.url("/slow?x=1")).send().await?;
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(pages.hits("/doc"), 1);
    assert_eq!(pages.hits("/slow"), 1);

    let stub = ProviderStub::start().await?;
    let mut run = stubbed("pages", &stub)
        .settings(json!({ "sandbox": PageServer::reachable() }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::calls("f1", "WebFetch", json!({ "url": pages.url("/data") })),
        Reply::text("fetched"),
    ]);
    let events = run.turn(&session, "fetch it").await?;
    assert_eq!(pages.hits("/data"), 1, "{:?}", events.tool_results);
    let fetched = result_of(&events, "WebFetch").expect("a WebFetch result");
    assert!(!fetched.2 && fetched.1.contains("\"k\""), "{fetched:?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_toy_mcp_server_answers_over_stdio_and_http() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let resources = scratch.path().join("res");
    std::fs::create_dir_all(&resources)?;
    std::fs::write(resources.join("note.txt"), "RESOURCE-TOKEN-913")?;
    let (calls_arg, res_arg) = (
        calls.to_string_lossy().into_owned(),
        resources.to_string_lossy().into_owned(),
    );
    let http = McpToy::spawn_http(&["--calls", &calls_arg, "--name", "web"]).await?;
    let stdio = McpToy::stdio_config(&[
        "--calls",
        &calls_arg,
        "--name",
        "local",
        "--resources",
        &res_arg,
    ])?;
    let initialize = direct_http()?
        .post(&http.url)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": { "name": "harness", "version": "0" } }
        }))
        .send()
        .await?
        .text()
        .await?;
    assert!(initialize.contains("\"prompts\""), "{initialize}");

    let servers = json!({
        "local": stdio,
        "web": http.config("streamable_http"),
        "legacy": http.config("sse"),
    });
    let mut run = stubbed("mcp", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;

    let session = run.session().await?;
    stub.script([
        Reply::calls("m1", "mcp__local__echo", json!({ "text": "统一码 🦀" })),
        Reply::calls(
            "m2",
            "ReadMcpResource",
            json!({ "server": "local", "uri": "toy://files/note.txt" }),
        ),
        Reply::calls(
            "m3",
            "mcp__local__big",
            json!({ "bytes": 5000, "tail": "BIG-TAIL" }),
        ),
        Reply::calls("m4", "mcp__local__elicit", json!({})),
        Reply::calls("m5", "mcp__web__ping", json!({ "message": "over-http" })),
        Reply::calls("m6", "mcp__legacy__ping", json!({ "message": "over-sse" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "use the toy").await?;
    let content = |i: usize| events.tool_results.get(i).cloned().unwrap_or_default();
    assert_eq!(content(0).1, "统一码 🦀", "{:?}", events.tool_results);
    assert!(
        content(1).1.contains("RESOURCE-TOKEN-913"),
        "{:?}",
        content(1)
    );
    assert!(content(2).1.contains("BIG-TAIL"), "{:?}", content(2));
    assert!(
        content(3).1.contains("elicitation://toy/"),
        "{:?}",
        content(3)
    );
    assert!(content(4).1.contains("toy: over-http"), "{:?}", content(4));
    assert!(content(5).1.contains("toy: over-sse"), "{:?}", content(5));
    let logged = McpToy::calls(&calls);
    for (server, tool) in [("local", "echo"), ("local", "big"), ("web", "ping")] {
        assert!(
            logged.contains(&(server.to_string(), tool.to_string())),
            "{server}/{tool} not in {logged:?}"
        );
    }

    run.finish().await;
    http.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn world_fixtures_are_where_the_daemon_looks() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scene_tools = [
        "Read",
        "Write",
        "EnterWorktree",
        "ExitWorktree",
        "NotebookEdit",
        "Ping",
        "Sleep",
        "PushNotification",
        "CronCreate",
        "CronDelete",
        "CronList",
        "TodoWrite",
    ];
    let mut run = stubbed("fixtures", &stub)
        .settings(json!({ "memory_enabled": true }))
        .before_start(move |world, _project| {
            world.write_declarative_scene(
                "workbench",
                &scene_tools,
                "You are on the WORKBENCH-SCENE.\n",
            )?;
            let global = world.global_memory_dir();
            std::fs::create_dir_all(&global)?;
            std::fs::write(
                global.join("MEMORY.md"),
                "- [Deploy](deploy.md) — MEMORY-INDEX-LINE\n",
            )?;
            Ok(())
        })
        .start()
        .await?;

    match run.world.git_project("repo") {
        Ok(repo) => {
            let log = std::process::Command::new("git")
                .args(["log", "--oneline"])
                .current_dir(&repo)
                .output()?;
            assert_eq!(String::from_utf8_lossy(&log.stdout).lines().count(), 1);
        }
        Err(e) if e.to_string().contains("needs `git`") => eprintln!("skipped: {e}"),
        Err(e) => return Err(e),
    }

    stub.script_when(
        "Return only a JSON array of memories",
        [Reply::text(
            json!([{ "name": "build-id", "description": "the build id", "content": "B-1",
                     "type": "project", "confidence": 0.9 }])
            .to_string(),
        )],
    );
    let session = run.scene_session("workbench", json!({})).await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "hello").await?;
    let turns = |stub: &ProviderStub| -> Vec<live::SeenRequest> {
        stub.calls()
            .into_iter()
            .filter(|c| c.purpose() == Purpose::Turn)
            .collect()
    };
    let first = turns(&stub)[0].clone();
    assert!(first.system_contains("WORKBENCH-SCENE"));
    let offered: Vec<String> = first.tool_names();
    let everything = format!("{offered:?} {}", first.system_text());
    for tool in scene_tools {
        assert!(everything.contains(tool), "{tool} not offered: {offered:?}");
    }

    let coding = run.session().await?;
    stub.script([Reply::text("ok")]);
    run.turn(&coding, "hello again").await?;
    let second = turns(&stub)[1].clone();
    assert!(
        second.system_contains("MEMORY-INDEX-LINE"),
        "World::global_memory_dir's index is the one the prompt loads"
    );
    assert!(second.system_contains(&run.world.global_memory_dir().to_string_lossy()));
    let extracted = run.world.memory_dir(&run.project).join("build-id.md");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !extracted.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        extracted.exists(),
        "a project memory the extraction returned is not in World::memory_dir: {:?}",
        stub.calls().iter().map(|c| c.purpose()).collect::<Vec<_>>()
    );
    run.finish().await;
    Ok(())
}
