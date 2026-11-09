//! MCP with a real model, against the toy server: a slow call the host
//! interrupts, a server that dies mid-session, a result past the tool-result
//! budget, resources read by tool and by `@` reference, a prompt run as a
//! slash command, a server added and then changed at runtime, the HTTP and
//! SSE transports, the enterprise policy (with a plugin's server among the
//! denied), a plugin's server across `config.reload`, and an elicitation URL
//! reaching its hook.

use std::path::Path;
use std::time::Duration;

use live::{LiveRun, McpToy, Mode, World};
use serde_json::{json, Value};

const TOY_UNICODE: &str =
    "统一码 ✓ — emoji 🦀🎉 — RTL مرحبا بالعالم — combining e\u{301} — zero-width a\u{200b}b";

fn nonce() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 0xffff_ffff_ffff
    )
}

fn results_of<'a>(
    events: &'a rpc_client::TurnEvents,
    tool: &str,
) -> Vec<&'a (String, String, bool)> {
    events
        .tool_results
        .iter()
        .filter(|(name, _, _)| name == tool)
        .collect()
}

/// The tool results a request carried back to the model, joined — not the
/// prompt or the tool inputs, which may name the very text a case looks for.
fn tool_results_sent(request: &live::SeenRequest) -> String {
    let mut out = String::new();
    let blocks = request.body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result");
    for block in blocks {
        match &block["content"] {
            Value::String(text) => out.push_str(text),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .for_each(|t| out.push_str(t)),
            _ => {}
        }
        out.push('\n');
    }
    out
}

async fn wait_for_call(calls: &Path, wanted: (&str, &str), timeout: Duration) -> bool {
    let wanted = (wanted.0.to_string(), wanted.1.to_string());
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if McpToy::calls(calls).contains(&wanted) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn mcp_status_until(
    run: &mut LiveRun,
    what: &str,
    done: impl Fn(&[Value]) -> bool,
) -> anyhow::Result<Vec<Value>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let resp = run.client.mcp_status().await?;
        let servers = resp.result.unwrap_or_default()["servers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if done(&servers) {
            return Ok(servers);
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "mcp.status never showed {what}: {servers:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn state_of(servers: &[Value], name: &str) -> Option<String> {
    servers
        .iter()
        .find(|s| s["name"] == name)
        .and_then(|s| s["state"].as_str().map(str::to_string))
}

/// A package under the world's global plugin tier declaring one MCP server
/// (`<plugin>-mcp-<server>`) with this config.
fn install_plugin_server(
    world: &World,
    plugin: &str,
    server: &str,
    config: &Value,
) -> anyhow::Result<()> {
    let dir = world
        .global_root()
        .join("plugins")
        .join("cache")
        .join(plugin)
        .join("1.0.0");
    std::fs::create_dir_all(dir.join("mcp"))?;
    std::fs::write(dir.join("mcp").join("server.json"), config.to_string())?;
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "[plugin]\nname = \"{plugin}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
             description = \"declares an MCP server\"\n\n\
             [[mcp]]\nname = \"{server}\"\nkind = \"native\"\nconfig = \"mcp/server.json\"\n"
        ),
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_mcp_call_is_cancelled_on_the_server_when_the_turn_is_interrupted(
) -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let toy = McpToy::stdio_config(&["--calls", &calls.to_string_lossy()])?;
    let mut run = LiveRun::builder("mcp-slow")
        .settings(json!({ "mcp_servers": { "toy": toy } }))
        .start()
        .await?;
    let session = run.session().await?;
    let handle = run
        .spawn_turn(
            &session,
            "Call the mcp__toy__slow tool once with ms 120000 and wait for its answer, then \
             reply DONE. Do nothing else.",
            json!({}),
        )
        .await?;
    assert!(
        wait_for_call(&calls, ("toy", "slow"), Duration::from_secs(180)).await,
        "the model never called slow: {:?}",
        McpToy::calls(&calls)
    );
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(resp.result.unwrap_or_default()["interrupted"], json!(true));
    let events = tokio::time::timeout(Duration::from_secs(20), handle).await???;
    assert!(
        results_of(&events, "mcp__toy__slow")
            .iter()
            .all(|(_, text, _)| !text.contains("done after")),
        "the call ran to the end: {:?}",
        events.tool_results
    );
    run.wait_turn_state(&session, "idle", Duration::from_secs(10))
        .await?;
    assert!(
        wait_for_call(&calls, ("toy", "slow:cancelled"), Duration::from_secs(10)).await,
        "the server never heard of the interrupt: {:?}",
        McpToy::calls(&calls)
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_dies_is_reported_and_the_rest_keeps_working() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let calls_arg = calls.to_string_lossy().into_owned();
    let (steady_probe, after_probe) = (format!("S-{}", nonce()), format!("A-{}", nonce()));
    let mut run = LiveRun::builder("mcp-crash")
        .settings(json!({ "mcp_servers": {
            "fragile": McpToy::stdio_config(&["--calls", &calls_arg, "--name", "fragile"])?,
            "steady": McpToy::stdio_config(&["--calls", &calls_arg, "--name", "steady"])?,
        } }))
        .start()
        .await?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Do these steps in order, one tool call at a time:\n\
                 1. Call mcp__fragile__crash once. It is expected to fail; do not call it again.\n\
                 2. Call mcp__steady__ping with message {steady_probe}.\n\
                 3. Call mcp__fragile__ping with message {after_probe}.\n\
                 Then reply DONE."
            ),
        )
        .await?;

    let crash = results_of(&events, "mcp__fragile__crash");
    assert!(
        !crash.is_empty() && crash.iter().all(|(_, _, failed)| *failed),
        "the crash came back as a success: {:?}",
        events.tool_results
    );
    let steady = results_of(&events, "mcp__steady__ping");
    assert!(
        steady
            .iter()
            .any(|(_, text, failed)| !failed && text.contains(&format!("toy: {steady_probe}"))),
        "{:?}",
        events.tool_results
    );
    let after = results_of(&events, "mcp__fragile__ping");
    assert!(
        after
            .iter()
            .any(|(_, text, failed)| !failed && text.contains(&format!("toy: {after_probe}"))),
        "the fragile server did not come back: {:?}",
        events.tool_results
    );
    let logged = McpToy::calls(&calls);
    for (server, tool) in [("steady", "ping"), ("fragile", "ping")] {
        assert!(
            logged.contains(&(server.to_string(), tool.to_string())),
            "{logged:?}"
        );
    }
    let status = mcp_status_until(&mut run, "both servers", |s| {
        state_of(s, "fragile").is_some() && state_of(s, "steady").is_some()
    })
    .await?;
    assert_eq!(state_of(&status, "steady").as_deref(), Some("connected"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_big_mcp_result_is_cut_to_the_result_budget_before_the_model_sees_it(
) -> anyhow::Result<()> {
    let (small_tail, big_tail) = (
        format!("SMALL-TAIL-{}", nonce()),
        format!("BIG-TAIL-{}", nonce()),
    );
    let mut run = LiveRun::builder("mcp-big")
        .settings(json!({ "mcp_servers": { "toy": McpToy::stdio_config(&[])? } }))
        .start()
        .await?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Call mcp__toy__big with bytes 40000 and tail {small_tail}. After it answers, \
                 call mcp__toy__big with bytes 60000 and tail {big_tail}. Then reply with the \
                 last line of each result, or NOT-SHOWN for one whose end you cannot see."
            ),
        )
        .await?;

    let bigs = results_of(&events, "mcp__toy__big");
    assert!(
        bigs.iter().any(|(_, t, _)| t.ends_with(&small_tail))
            && bigs.iter().any(|(_, t, _)| t.ends_with(&big_tail)),
        "the host is handed both results whole"
    );
    let requests = run.provider.calls();
    assert!(
        requests
            .iter()
            .any(|r| tool_results_sent(r).contains(&small_tail)),
        "a 40 000-byte result reaches the model whole"
    );
    assert!(
        requests
            .iter()
            .all(|r| !tool_results_sent(r).contains(&big_tail)),
        "a 60 000-byte result reached the model past the 50 000-byte budget"
    );
    assert!(
        requests
            .iter()
            .any(|r| tool_results_sent(r).contains("[Tool result truncated: 60000 bytes")),
        "the cut is marked"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resources_reach_the_model_by_tool_and_by_reference() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let resources = scratch.path().join("res");
    std::fs::create_dir_all(&resources)?;
    let (token_a, token_b) = (format!("RA-{}", nonce()), format!("RB-{}", nonce()));
    std::fs::write(
        resources.join("note-a.txt"),
        format!("Notes, part one.\nTOKEN-A: {token_a}\n"),
    )?;
    std::fs::write(
        resources.join("note-b.txt"),
        format!("Notes, part two — 中文 🦀.\nTOKEN-B: {token_b}\n"),
    )?;
    let toy = McpToy::stdio_config(&["--resources", &resources.to_string_lossy()])?;
    let mut run = LiveRun::builder("mcp-resources")
        .settings(json!({ "mcp_servers": { "toy": toy } }))
        .start()
        .await?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "Use ListMcpResources to list the toy server's resources. Then use \
             ReadMcpResource (server toy) to read toy://files/note-a.txt, toy://unicode and \
             toy://big. Write the value after `TOKEN-A: ` to a.txt with the Write tool, then \
             reply DONE.",
        )
        .await?;
    assert_eq!(
        run.read_file("a.txt").map(|s| s.trim().to_string()),
        Some(token_a.clone())
    );
    let listed = results_of(&events, "ListMcpResources");
    assert!(
        listed
            .iter()
            .any(|(_, t, _)| t.contains("toy://files/note-a.txt") && t.contains("toy://big")),
        "{listed:?}"
    );
    let read = results_of(&events, "ReadMcpResource");
    assert!(
        read.iter()
            .any(|(_, t, failed)| !failed && t == TOY_UNICODE),
        "the unicode resource came back changed: {read:?}"
    );
    assert!(
        read.iter()
            .any(|(_, t, _)| t.ends_with("TOY-BIG-RESOURCE-END")),
        "the big resource came back whole to the host"
    );
    assert!(
        run.provider
            .calls()
            .iter()
            .all(|r| !r.messages_contain("TOY-BIG-RESOURCE-END")),
        "a 200 000-byte resource reached the model whole"
    );

    let before = run.provider.calls().len();
    let events = run
        .turn(
            &session,
            "@toy:toy://files/note-b.txt and @toy:toy://big are attached to this message. \
             Do not call ReadMcpResource. Write the value after `TOKEN-B: ` from the attached \
             note to b.txt with the Write tool, then reply DONE.",
        )
        .await?;
    assert_eq!(
        run.read_file("b.txt").map(|s| s.trim().to_string()),
        Some(token_b.clone())
    );
    assert!(
        events.tool_uses.iter().all(|(n, _)| n != "ReadMcpResource"),
        "{:?}",
        events.tool_uses
    );
    let first = run.provider.calls()[before].clone();
    assert!(first.messages_contain(&token_b), "the note was not inlined");
    assert!(
        first.messages_contain("中文 🦀"),
        "the note lost its unicode"
    );
    assert!(
        first.messages_contain("[... truncated: resource exceeded the inline size limit]")
            && !first.messages_contain("TOY-BIG-RESOURCE-END"),
        "the big resource was not capped inline"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_prompt_runs_as_a_slash_command() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("mcp-prompts")
        .settings(json!({ "mcp_servers": { "toy": McpToy::stdio_config(&[])? } }))
        .start()
        .await?;
    let session = run.session().await?;
    let name = format!("Zed{}", nonce());
    run.turn(
        &session,
        &format!("/mcp__toy__greet name={name} style=formal"),
    )
    .await?;
    let first = run.provider.calls()[0].clone();
    assert!(
        first.messages_contain(&format!("Greet {name} in a formal style.")),
        "the prompt's text never reached the model"
    );

    let before = run.provider.calls().len();
    run.turn(&session, "/mcp__toy__greet").await?;
    let missing = run.provider.calls()[before].clone();
    assert!(
        missing.messages_contain("name") && !missing.messages_contain("Greet ."),
        "the missing argument was not reported"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_added_at_runtime_is_used_and_an_update_takes_effect() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let calls_arg = calls.to_string_lossy().into_owned();
    let mut run = LiveRun::builder("mcp-runtime").start().await?;
    let session = run.session().await?;

    let added = run
        .client
        .mcp_add_server(
            &rpc_client::Target::own(),
            "late",
            McpToy::stdio_config(&["--calls", &calls_arg, "--name", "late-v1"])?,
        )
        .await?;
    let added = added.result.expect("mcp.addServer succeeds");
    assert!(
        added["servers"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["name"] == "late" && s["state"] == "connected"),
        "{added}"
    );
    let written: Value =
        serde_json::from_str(&run.read_file(".agix/settings.json").unwrap_or_default())?;
    assert!(written["mcp_servers"]["late"].is_object(), "{written}");

    let probe = format!("L1-{}", nonce());
    let events = run
        .turn(
            &session,
            &format!(
                "Call the mcp__late__ping tool with message {probe} (if it is not in your tool \
                 list, find it with ToolSearch). Reply with exactly what it returned."
            ),
        )
        .await?;
    assert!(
        results_of(&events, "mcp__late__ping")
            .iter()
            .any(|(_, t, failed)| !failed && t.contains(&format!("toy: {probe}"))),
        "{:?}",
        events.tool_results
    );
    assert!(McpToy::calls(&calls).contains(&("late-v1".into(), "ping".into())));

    let updated = run
        .client
        .call(
            "mcp.updateServer",
            json!({
                "name": "late",
                "config": McpToy::stdio_config(&["--calls", &calls_arg, "--name", "late-v2"])?,
            }),
        )
        .await?;
    assert!(updated.error.is_none(), "{:?}", updated.error);
    let probe = format!("L2-{}", nonce());
    let events = run
        .turn(
            &session,
            &format!(
                "Call mcp__late__ping again, now with message {probe}. Reply with exactly \
                 what it returned."
            ),
        )
        .await?;
    assert!(
        results_of(&events, "mcp__late__ping")
            .iter()
            .any(|(_, t, failed)| !failed && t.contains(&format!("toy: {probe}"))),
        "{:?}",
        events.tool_results
    );
    let logged = McpToy::calls(&calls);
    assert!(
        logged.contains(&("late-v2".into(), "ping".into())),
        "the call after the update went to the old process: {logged:?}"
    );
    assert_eq!(
        logged
            .iter()
            .filter(|(n, t)| n == "late-v1" && t == "ping")
            .count(),
        1,
        "{logged:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_http_and_sse_transports_carry_tool_calls() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let calls_arg = calls.to_string_lossy().into_owned();
    let web = McpToy::spawn_http(&["--calls", &calls_arg, "--name", "web"]).await?;
    let legacy = McpToy::spawn_http(&["--calls", &calls_arg, "--name", "legacy"]).await?;
    let mut run = LiveRun::builder("mcp-http")
        .settings(json!({ "mcp_servers": {
            "web": web.config("streamable_http"),
            "legacy": legacy.config("sse"),
        } }))
        .mode(Mode::Spawned)
        .start()
        .await?;
    let status = mcp_status_until(&mut run, "both connected", |s| {
        state_of(s, "web").as_deref() == Some("connected")
            && state_of(s, "legacy").as_deref() == Some("connected")
    })
    .await?;
    let transport = |name: &str| {
        status
            .iter()
            .find(|s| s["name"] == name)
            .and_then(|s| s["transport"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(transport("web"), "streamable_http", "{status:?}");
    assert_eq!(transport("legacy"), "sse", "{status:?}");

    let session = run.session().await?;
    let (a, b) = (format!("H-{}", nonce()), format!("S-{}", nonce()));
    let events = run
        .turn(
            &session,
            &format!(
                "Call mcp__web__ping with message {a}, then mcp__legacy__ping with message {b}. \
                 Reply with both results."
            ),
        )
        .await?;
    for (tool, probe) in [("mcp__web__ping", &a), ("mcp__legacy__ping", &b)] {
        assert!(
            results_of(&events, tool)
                .iter()
                .any(|(_, t, failed)| !failed && t.contains(&format!("toy: {probe}"))),
            "{tool}: {:?}",
            events.tool_results
        );
    }
    let logged = McpToy::calls(&calls);
    assert!(
        logged.contains(&("web".into(), "ping".into())),
        "{logged:?}"
    );
    assert!(
        logged.contains(&("legacy".into(), "ping".into())),
        "{logged:?}"
    );
    run.finish().await;
    web.stop().await;
    legacy.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mcp_policy_keeps_denied_servers_out_of_reach() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let calls_arg = calls.to_string_lossy().into_owned();
    let toy = McpToy::binary()?;
    let policy_path = scratch.path().join("mcp-policy.json");
    std::fs::write(
        &policy_path,
        json!({
            "allowed_servers": [ { "command": toy.to_string_lossy() } ],
            "denied_servers": [ { "name": "blocked" }, { "name": "plug-mcp-srv" } ]
        })
        .to_string(),
    )?;
    let toy_named = |name: &str| -> anyhow::Result<Value> {
        McpToy::stdio_config(&["--calls", &calls_arg, "--name", name])
    };
    let plugin_server = toy_named("plug")?;
    let policy = policy_path.clone();
    let mut run = LiveRun::builder("mcp-policy")
        .settings(json!({ "mcp_servers": {
            "demo": toy_named("demo")?,
            "blocked": toy_named("blocked")?,
        } }))
        .options(move |o| o.mcp_policy(policy.clone()))
        .before_start(move |world, _| install_plugin_server(world, "plug", "srv", &plugin_server))
        .start()
        .await?;
    let status = mcp_status_until(&mut run, "demo connected", |s| {
        state_of(s, "demo").as_deref() == Some("connected")
    })
    .await?;
    assert_eq!(state_of(&status, "blocked").as_deref(), Some("denied"));
    assert_eq!(
        state_of(&status, "plug-mcp-srv").as_deref(),
        Some("denied"),
        "the plugin's server is policed like the rest: {status:?}"
    );

    let session = run.session().await?;
    let probe = format!("P-{}", nonce());
    let events = run
        .turn(
            &session,
            &format!(
                "1. Call mcp__demo__ping with message {probe} and write exactly what it \
                 returned to demo.txt with the Write tool.\n\
                 2. Then look for a tool named mcp__blocked__ping with one ToolSearch. If it \
                 does not exist, reply UNAVAILABLE; do not try any other way to reach it."
            ),
        )
        .await?;
    assert!(
        run.read_file("demo.txt")
            .is_some_and(|s| s.contains(&format!("toy: {probe}"))),
        "{:?}",
        run.read_file("demo.txt")
    );
    assert!(
        events
            .tool_uses
            .iter()
            .all(|(n, _)| !n.starts_with("mcp__blocked__") && !n.starts_with("mcp__plug-mcp-srv__")),
        "{:?}",
        events.tool_uses
    );
    for request in run.provider.calls() {
        let offered = request.tool_names();
        assert!(
            offered
                .iter()
                .all(|n| !n.starts_with("mcp__blocked__") && !n.starts_with("mcp__plug-mcp-srv__")),
            "a denied server's tool was offered: {offered:?}"
        );
    }
    let started: Vec<String> = McpToy::calls(&calls).into_iter().map(|(n, _)| n).collect();
    assert!(
        started.iter().all(|n| n == "demo"),
        "a denied server answered: {started:?}"
    );

    let settings_file = run.project.join(".agix").join("settings.json");
    let before = std::fs::read(&settings_file)?;
    let refused = run
        .client
        .mcp_add_server(&rpc_client::Target::own(), "blocked", toy_named("blocked")?)
        .await?;
    assert_eq!(refused.error.map(|e| e.code), Some(-32020));
    assert_eq!(std::fs::read(&settings_file)?, before);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugins_mcp_server_survives_a_config_reload() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let server = McpToy::stdio_config(&["--calls", &calls.to_string_lossy(), "--name", "kit"])?;
    let mut run = LiveRun::builder("mcp-plugin")
        .before_start(move |world, _| install_plugin_server(world, "kit", "tools", &server))
        .start()
        .await?;
    let session = run.session().await?;
    let probe = format!("K1-{}", nonce());
    let ask = |probe: &str| {
        format!(
            "Call the mcp__kit-mcp-tools__ping tool with message {probe} (if it is not in your \
             tool list, find it with ToolSearch). Reply with exactly what it returned."
        )
    };
    let events = run.turn(&session, &ask(&probe)).await?;
    assert!(
        results_of(&events, "mcp__kit-mcp-tools__ping")
            .iter()
            .any(|(_, t, failed)| !failed && t.contains(&format!("toy: {probe}"))),
        "{:?}",
        events.tool_results
    );

    let reloaded = run.client.config_reload(&rpc_client::Target::own()).await?;
    assert!(reloaded.error.is_none(), "{:?}", reloaded.error);
    let status = mcp_status_until(&mut run, "the plugin's server after reload", |s| {
        state_of(s, "kit-mcp-tools").as_deref() == Some("connected")
    })
    .await?;
    assert!(!status.is_empty());

    let fresh = run.session().await?;
    let probe = format!("K2-{}", nonce());
    let events = run.turn(&fresh, &ask(&probe)).await?;
    assert!(
        results_of(&events, "mcp__kit-mcp-tools__ping")
            .iter()
            .any(|(_, t, failed)| !failed && t.contains(&format!("toy: {probe}"))),
        "the plugin's server is gone after the reload: {:?}",
        events.tool_results
    );
    assert_eq!(
        McpToy::calls(&calls)
            .iter()
            .filter(|(_, t)| t == "ping")
            .count(),
        2
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_elicitation_url_in_a_tool_result_reaches_the_elicitation_hook() -> anyhow::Result<()> {
    let scratch = tempfile::tempdir()?;
    let log = scratch.path().join("elicitation.jsonl");
    let hook = scratch.path().join("elicit.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat >> '{log}'\necho >> '{log}'\necho '{{\"continue\": true}}'\n",
            log = log.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    let reason = format!("R-{}", nonce());
    let mut run = LiveRun::builder("mcp-elicit")
        .settings(json!({
            "mcp_servers": { "toy": McpToy::stdio_config(&[])? },
            "hooks_config": {
                "Elicitation": [ { "type": "command", "command": hook, "timeout": 5000 } ]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Call mcp__toy__elicit once with reason {reason}. Do not open the URL it \
                 returns; reply WAITING."
            ),
        )
        .await?;
    assert!(
        results_of(&events, "mcp__toy__elicit")
            .iter()
            .any(|(_, t, _)| t.contains(&reason) && t.contains("elicitation://toy/confirm")),
        "{:?}",
        events.tool_results
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut seen = String::new();
    while std::time::Instant::now() < deadline {
        seen = std::fs::read_to_string(&log).unwrap_or_default();
        if !seen.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let payload: Value = serde_json::from_str(seen.lines().next().unwrap_or("null"))?;
    assert_eq!(payload["hook_event_name"], "Elicitation", "{seen}");
    assert_eq!(payload["tool_input"]["server_name"], "toy", "{seen}");
    assert!(
        payload["tool_input"]["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("elicitation://toy/confirm?call=")),
        "{seen}"
    );
    run.finish().await;
    Ok(())
}
