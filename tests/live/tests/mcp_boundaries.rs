//! MCP servers and web pages that misbehave, against a scripted model: a
//! server that never finishes its handshake, one that answers garbage, one
//! with hundreds of tools or the same tool twice, server names a tool name
//! cannot carry, a call whose server dies under it, an interrupted call, a
//! prompt run as a slash command, the MCP policy's globs end to end, and
//! `WebFetch` on empty, binary, looping, malformed and off-limits URLs.
//!
//! The live tier asks whether a model copes with these; this asks what the
//! product hands it, which needs the model's side held fixed.

use std::path::Path;
use std::time::Duration;

use live::{
    LiveConfig, LiveRun, LiveRunBuilder, McpToy, Mode, Page, PageServer, ProviderStub, Reply,
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

/// A stdio server that is `/bin/sh` running `script`, kept in `dir`.
fn shell_server(dir: &Path, name: &str, script: &str) -> anyhow::Result<Value> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.sh"));
    std::fs::write(&path, script)?;
    Ok(json!({ "type": "stdio", "command": "/bin/sh", "args": [path.to_string_lossy()] }))
}

/// A server that speaks just enough MCP over stdio to hand out the tool list
/// in `tools_file` and answer every call with `canned: <tool>`.
fn canned_server(dir: &Path, name: &str, tools: &[Value]) -> anyhow::Result<Value> {
    std::fs::create_dir_all(dir)?;
    let tools_file = dir.join(format!("{name}.tools.json"));
    std::fs::write(&tools_file, serde_json::to_string(tools)?)?;
    let script = format!(
        r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  case "$line" in
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"canned","version":"0"}}}}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":%s}}}}\n' "$id" "$(cat '{tools}')" ;;
    *'"method":"tools/call"'*)
      tool=$(printf '%s' "$line" | grep -o '"name":"[^"]*"' | head -1 | cut -d'"' -f4)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"canned: %s"}}]}}}}\n' "$id" "$tool" ;;
    *)
      if [ -n "$id" ]; then
        printf '{{"jsonrpc":"2.0","id":%s,"error":{{"code":-32601,"message":"not here"}}}}\n' "$id"
      fi ;;
  esac
done
"#,
        tools = tools_file.display()
    );
    shell_server(dir, name, &script)
}

fn tool(name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("canned tool {name}"),
        "inputSchema": { "type": "object", "properties": {} }
    })
}

/// `mcp.status`, polled until `done` holds of it.
async fn mcp_status_until(
    run: &mut LiveRun,
    what: &str,
    done: impl Fn(&[Value]) -> bool,
) -> anyhow::Result<Vec<Value>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn names_in_state(servers: &[Value], state: &str) -> Vec<String> {
    let mut names: Vec<String> = servers
        .iter()
        .filter(|s| s["state"] == state)
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

async fn wait_for_line(path: &Path, wanted: (&str, &str), timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let wanted = (wanted.0.to_string(), wanted.1.to_string());
    while std::time::Instant::now() < deadline {
        if McpToy::calls(path).contains(&wanted) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_never_answers_initialize_does_not_hold_up_a_session() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let servers = json!({
        "mute": shell_server(
            scratch.path(),
            "mute",
            "while IFS= read -r line; do :; done\n",
        )?,
        "toy": McpToy::stdio_config(&["--calls", &calls.to_string_lossy()])?,
    });
    let mut run = stubbed("mute", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = tokio::time::timeout(Duration::from_secs(20), run.session())
        .await
        .map_err(|_| anyhow::anyhow!("session.create still waiting on the mute server"))??;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls(
            "p1",
            "mcp__toy__ping",
            json!({ "message": "past-the-mute" }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "ping").await?;
    let ping = result_of(&events, "mcp__toy__ping").expect("a ping result");
    assert!(!ping.2 && ping.1.contains("toy: past-the-mute"), "{ping:?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_answers_garbage_is_left_out_and_the_others_still_work() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let servers = json!({
        "garbage": shell_server(
            scratch.path(),
            "garbage",
            "while IFS= read -r line; do echo '{not json at all'; echo '[1,2'; done\n",
        )?,
        "toy": McpToy::stdio_config(&[])?,
    });
    let mut run = stubbed("garbage", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = tokio::time::timeout(Duration::from_secs(20), run.session())
        .await
        .map_err(|_| anyhow::anyhow!("session.create still waiting on the garbage server"))??;
    let _host = run.watch(&session, true).await?;
    let status = mcp_status_until(&mut run, "the toy server connected", |s| {
        names_in_state(s, "connected").contains(&"toy".to_string())
    })
    .await?;
    assert!(
        !names_in_state(&status, "connected").contains(&"garbage".to_string()),
        "{status:?}"
    );

    stub.script([
        Reply::calls(
            "p1",
            "mcp__toy__ping",
            json!({ "message": "beside-garbage" }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "ping").await?;
    let ping = result_of(&events, "mcp__toy__ping").expect("a ping result");
    assert!(
        !ping.2 && ping.1.contains("toy: beside-garbage"),
        "{ping:?}"
    );
    assert!(
        stub.calls()[0]
            .tool_names()
            .iter()
            .all(|n| !n.starts_with("mcp__garbage__")),
        "{:?}",
        stub.calls()[0].tool_names()
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_with_hundreds_of_tools_is_listed_whole_and_its_last_tool_answers(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let many: Vec<Value> = (0..400).map(|i| tool(&format!("t{i:03}"))).collect();
    let servers = json!({ "many": canned_server(scratch.path(), "many", &many)? });
    let mut run = stubbed("many", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let status = mcp_status_until(&mut run, "the many server connected", |s| {
        s.iter()
            .any(|s| s["name"] == "many" && s["state"] == "connected")
    })
    .await?;
    let listed = status.iter().find(|s| s["name"] == "many").cloned();
    assert_eq!(
        listed.as_ref().map(|s| s["tool_count"].clone()),
        Some(json!(400)),
        "{status:?}"
    );

    stub.script([
        Reply::calls("c1", "mcp__many__t399", json!({})),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "call the last one").await?;
    let last = result_of(&events, "mcp__many__t399").expect("a result");
    assert!(!last.2 && last.1.contains("canned: t399"), "{last:?}");
    let offered = stub.calls()[0].tool_names();
    let mut ours: Vec<&String> = offered
        .iter()
        .filter(|n| n.starts_with("mcp__many__"))
        .collect();
    ours.sort();
    ours.dedup();
    let searchable = stub.calls()[0].system_contains("mcp__many__t399")
        || offered.iter().any(|n| n == "ToolSearch");
    assert!(
        ours.len() == 400 || searchable,
        "{} of 400 tools offered and no way to find the rest",
        ours.len()
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_listed_twice_by_one_server_is_offered_once() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let servers = json!({
        "twin": canned_server(scratch.path(), "twin", &[tool("dup"), tool("dup"), tool("solo")])?,
    });
    let mut run = stubbed("twin", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "hello").await?;
    let offered = stub.calls()[0].tool_names();
    assert_eq!(
        offered.iter().filter(|n| *n == "mcp__twin__dup").count(),
        1,
        "{offered:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_tool_on_two_servers_reaches_the_server_it_was_called_on() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let calls_arg = calls.to_string_lossy().into_owned();
    let servers = json!({
        "left": McpToy::stdio_config(&["--calls", &calls_arg, "--name", "left"])?,
        "right": McpToy::stdio_config(&["--calls", &calls_arg, "--name", "right"])?,
    });
    let mut run = stubbed("twins", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("e1", "mcp__right__echo", json!({ "text": "to-the-right" })),
        Reply::calls("e2", "mcp__left__echo", json!({ "text": "to-the-left" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "echo on both").await?;
    assert_eq!(
        result_of(&events, "mcp__right__echo").map(|r| r.1.as_str()),
        Some("to-the-right")
    );
    assert_eq!(
        result_of(&events, "mcp__left__echo").map(|r| r.1.as_str()),
        Some("to-the-left")
    );
    assert_eq!(
        McpToy::calls(&calls),
        [
            ("right".to_string(), "echo".to_string()),
            ("left".to_string(), "echo".to_string())
        ]
    );
    run.finish().await;
    Ok(())
}

fn is_api_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[tokio::test(flavor = "multi_thread")]
async fn odd_server_names_still_make_tool_names_the_api_accepts() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let odd = ["with space", "dot.ted", "ünïcode", &"long".repeat(16)];
    let mut servers = serde_json::Map::new();
    for name in odd {
        servers.insert(name.to_string(), McpToy::stdio_config(&[])?);
    }
    let mut run = stubbed("odd", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "hello").await?;
    let offered = stub.calls()[0].tool_names();
    let bad: Vec<&String> = offered.iter().filter(|n| !is_api_tool_name(n)).collect();
    assert!(bad.is_empty(), "tool names the API would refuse: {bad:?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_whose_server_died_is_reported_not_replayed() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let servers = json!({ "toy": McpToy::stdio_config(&["--calls", &calls.to_string_lossy()])? });
    let mut run = stubbed("replay", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("c1", "mcp__toy__crash", json!({})),
        Reply::calls(
            "p1",
            "mcp__toy__ping",
            json!({ "message": "after-the-crash" }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "crash it").await?;
    let crash = result_of(&events, "mcp__toy__crash").expect("a crash result");
    assert!(crash.2, "{crash:?}");
    let crashes = McpToy::calls(&calls)
        .iter()
        .filter(|(_, t)| t == "crash")
        .count();
    assert_eq!(crashes, 1, "{:?}", McpToy::calls(&calls));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_died_comes_back_for_the_next_call() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let servers = json!({ "toy": McpToy::stdio_config(&["--calls", &calls.to_string_lossy()])? });
    let mut run = stubbed("revive", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("c1", "mcp__toy__crash", json!({})),
        Reply::calls(
            "p1",
            "mcp__toy__ping",
            json!({ "message": "after-the-crash" }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "crash it").await?;
    let crash = result_of(&events, "mcp__toy__crash").expect("a crash result");
    assert!(crash.2, "the crash surfaces as an error: {crash:?}");
    let ping = result_of(&events, "mcp__toy__ping").expect("a ping result");
    assert!(
        !ping.2 && ping.1.contains("toy: after-the-crash"),
        "{ping:?}"
    );
    assert!(McpToy::calls(&calls).contains(&("toy".into(), "ping".into())));
    let status = mcp_status_until(&mut run, "toy listed", |s| {
        s.iter().any(|s| s["name"] == "toy")
    })
    .await?;
    assert!(
        status
            .iter()
            .any(|s| s["name"] == "toy" && s["state"] == "connected"),
        "{status:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_mcp_call_is_cancelled_on_the_server() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let servers = json!({ "toy": McpToy::stdio_config(&["--calls", &calls.to_string_lossy()])? });
    let mut run = stubbed("cancel", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("s1", "mcp__toy__slow", json!({ "ms": 60_000 })),
        Reply::text("never reached"),
    ]);
    let handle = run.spawn_turn(&session, "go slow", json!({})).await?;
    assert!(
        wait_for_line(&calls, ("toy", "slow"), Duration::from_secs(20)).await,
        "the slow call never reached the server"
    );
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(resp.result.unwrap_or_default()["interrupted"], json!(true));
    tokio::time::timeout(Duration::from_secs(10), handle).await???;
    run.wait_turn_state(&session, "idle", Duration::from_secs(10))
        .await?;
    assert!(
        wait_for_line(&calls, ("toy", "slow:cancelled"), Duration::from_secs(5)).await,
        "the server was never told: {:?}",
        McpToy::calls(&calls)
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_prompt_runs_as_a_slash_command_and_a_missing_argument_is_reported(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let servers = json!({ "toy": McpToy::stdio_config(&[])? });
    let mut run = stubbed("prompts", &stub)
        .settings(json!({ "mcp_servers": servers }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("greeted"), Reply::text("explained")]);
    run.turn(&session, "/mcp__toy__greet name=Zed-4471 style=formal")
        .await?;
    let first = stub.calls()[0].messages_text();
    assert!(
        first.contains("Greet Zed-4471 in a formal style."),
        "the prompt's text is not what the model got: {first}"
    );
    run.turn(&session, "/mcp__toy__greet").await?;
    let second = stub
        .calls()
        .last()
        .cloned()
        .expect("a call")
        .messages_text();
    assert!(
        second.contains("name") && !second.contains("Greet ."),
        "the missing argument is not what the model got: {second}"
    );
    run.finish().await;
    Ok(())
}

/// The policy file's globs and URL rules, against servers whose configs are
/// written to fall just outside them.
async fn the_policys_globs_hold_end_to_end(mode: Mode) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let calls_arg = calls.to_string_lossy().into_owned();
    let toy = McpToy::binary()?;
    let bin_dir = toy
        .parent()
        .expect("toy binary has a directory")
        .to_path_buf();
    let elsewhere = scratch.path().join("sub");
    let policy_path = scratch.path().join("mcp-policy.json");
    std::fs::write(
        &policy_path,
        serde_json::to_vec_pretty(&json!({
            "allowed_servers": [
                { "command": format!("{}/*", bin_dir.display()) },
                { "command": format!("{}/*", elsewhere.display()) },
                { "command": "mcp-toy-server" },
                { "url": "https://*.corp.example/**" },
                { "name": "named" }
            ],
            "denied_servers": [ { "name": "blocked" } ]
        }))?,
    )?;
    let toy_with = |name: &str| {
        json!({
            "type": "stdio",
            "command": toy.to_string_lossy(),
            "args": ["--calls", &calls_arg, "--name", name],
        })
    };
    let servers = json!({
        "globbed": toy_with("globbed"),
        "blocked": toy_with("blocked"),
        "nested": { "type": "stdio", "command": elsewhere.join("x").join("mcp-toy-server").to_string_lossy() },
        "pathed": { "type": "stdio", "command": scratch.path().join("evil").join("mcp-toy-server").to_string_lossy() },
        "named": { "type": "stdio", "command": scratch.path().join("absent-binary").to_string_lossy() },
        "evil_url": { "type": "sse", "url": "https://evil.example?.corp.example/" },
        "port_url": { "type": "sse", "url": "https://mcp.corp.example:8443/mcp" },
    });
    let policy = policy_path.clone();
    let mut run = stubbed(&format!("policy-{mode:?}"), &stub)
        .settings(json!({ "mcp_servers": servers }))
        .mode(mode)
        .options(move |o| o.mcp_policy(policy.clone()))
        .start()
        .await?;

    let status = mcp_status_until(&mut run, "globbed connected", |s| {
        names_in_state(s, "connected").contains(&"globbed".to_string())
    })
    .await?;
    assert_eq!(
        names_in_state(&status, "denied"),
        ["blocked", "evil_url", "named", "nested", "pathed", "port_url"],
        "{status:?}"
    );
    let reason = |name: &str| {
        status
            .iter()
            .find(|s| s["name"] == name)
            .and_then(|s| s["error"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    assert!(reason("blocked").contains("denied_servers"), "{status:?}");
    assert!(
        reason("nested").contains("no allowed_servers rule"),
        "{status:?}"
    );
    assert!(
        reason("named").contains("a `name` rule there admits nothing"),
        "{status:?}"
    );

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("p1", "mcp__globbed__ping", json!({ "message": "inside" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "ping").await?;
    let ping = result_of(&events, "mcp__globbed__ping").expect("a ping result");
    assert!(!ping.2 && ping.1.contains("toy: inside"), "{ping:?}");
    let offered = stub.calls()[0].tool_names();
    for denied in [
        "blocked", "named", "nested", "pathed", "evil_url", "port_url",
    ] {
        let prefix = format!("mcp__{denied}__");
        assert!(
            offered.iter().all(|n| !n.starts_with(&prefix)),
            "{offered:?}"
        );
    }
    let started: Vec<String> = McpToy::calls(&calls).into_iter().map(|(n, _)| n).collect();
    assert!(
        started.iter().all(|n| n == "globbed"),
        "a denied server answered a call: {started:?}"
    );

    let settings_file = run.project.join(".agix").join("settings.json");
    let before = std::fs::read(&settings_file)?;
    let refused = run
        .client
        .mcp_add_server(&rpc_client::Target::own(), "blocked", toy_with("blocked"))
        .await?;
    let error = refused
        .error
        .expect("mcp.addServer of a denied server fails");
    assert_eq!(error.code, -32020, "MCP_SERVER_DENIED: {}", error.message);
    assert_eq!(
        std::fs::read(&settings_file)?,
        before,
        "settings.json was written"
    );

    let doctor = run.client.daemon_doctor().await?;
    let doctor = doctor.result.unwrap_or_default();
    let check = doctor["health"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "mcp.policy")
        .cloned()
        .unwrap_or_default();
    assert_eq!(check["status"], "degraded", "{check}");
    assert_eq!(
        check["details"]["inert_allowed_rules"],
        json!([r#"{"name": "named"}"#]),
        "{check}"
    );
    assert_eq!(check["details"]["source"], "file", "{check}");
    assert_eq!(check["details"]["allowed_servers"], 5, "{check}");
    assert_eq!(check["details"]["denied_servers"], 1, "{check}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_policys_globs_hold_end_to_end_in_process() -> anyhow::Result<()> {
    the_policys_globs_hold_end_to_end(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn the_policys_globs_hold_end_to_end_in_a_spawned_daemon() -> anyhow::Result<()> {
    the_policys_globs_hold_end_to_end(Mode::Spawned).await
}

async fn a_malformed_policy_denies_every_server(mode: Mode) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let policy_path = scratch.path().join("mcp-policy.json");
    std::fs::write(&policy_path, r#"{"allow_servers": [{"name": "toy"}]}"#)?;
    let toy = McpToy::stdio_config(&["--calls", &calls.to_string_lossy()])?;
    let policy = policy_path.clone();
    let mut run = stubbed(&format!("badpolicy-{mode:?}"), &stub)
        .settings(json!({ "mcp_servers": { "toy": toy.clone() } }))
        .mode(mode)
        .options(move |o| o.mcp_policy(policy.clone()))
        .start()
        .await?;
    let status = mcp_status_until(&mut run, "toy denied", |s| {
        names_in_state(s, "denied").contains(&"toy".to_string())
    })
    .await?;
    let reason = status[0]["error"].as_str().unwrap_or_default();
    assert!(
        reason.contains(&policy_path.display().to_string()),
        "{status:?}"
    );

    let refused = run
        .client
        .mcp_add_server(&rpc_client::Target::own(), "other", toy)
        .await?;
    assert_eq!(refused.error.map(|e| e.code), Some(-32020));

    let session = run.session().await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "hello").await?;
    assert!(
        stub.calls()[0]
            .tool_names()
            .iter()
            .all(|n| !n.starts_with("mcp__")),
        "{:?}",
        stub.calls()[0].tool_names()
    );
    assert!(McpToy::calls(&calls).is_empty());

    let doctor = run.client.daemon_doctor().await?;
    let doctor = doctor.result.unwrap_or_default();
    let check = doctor["health"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "mcp.policy")
        .cloned()
        .unwrap_or_default();
    assert_eq!(check["status"], "failing", "{check}");
    assert_eq!(check["details"]["in_force"], true, "{check}");
    assert_eq!(doctor["health"]["status"], "failing", "{doctor}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_policy_denies_every_server_in_process() -> anyhow::Result<()> {
    a_malformed_policy_denies_every_server(Mode::InProcess).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn a_malformed_policy_denies_every_server_in_a_spawned_daemon() -> anyhow::Result<()> {
    a_malformed_policy_denies_every_server(Mode::Spawned).await
}

#[tokio::test(flavor = "multi_thread")]
async fn webfetch_answers_odd_pages_and_refuses_odd_urls() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route("/empty", Page::html(""));
    pages.route(
        "/blob",
        Page::Body {
            status: 200,
            content_type: "application/octet-stream".into(),
            body: "\u{0}\u{1}\u{2}PNG\r\n\u{1a}\nBLOB-MARK".into(),
        },
    );
    pages.route("/loop", Page::redirect("/loop"));
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("odd-pages", &stub)
        .settings(json!({ "sandbox": PageServer::reachable() }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let fetch = |id: &str, url: &str| Reply::calls(id, "WebFetch", json!({ "url": url }));
    stub.script([
        fetch("f1", &pages.url("/empty")),
        fetch("f2", &pages.url("/blob")),
        fetch("f3", &pages.url("/loop")),
        fetch("f4", "not a url"),
        fetch("f5", "ftp://127.0.0.1/x"),
        fetch("f6", "file:///etc/passwd"),
        fetch("f7", "http://"),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "fetch them all").await?;
    let results: Vec<&(String, String, bool)> = events
        .tool_results
        .iter()
        .filter(|(n, _, _)| n == "WebFetch")
        .collect();
    assert_eq!(results.len(), 7, "{:?}", events.tool_results);

    let (_, empty, failed) = results[0];
    assert!(!failed && empty.starts_with("[HTTP 200"), "{empty}");
    let (_, blob, failed) = results[1];
    assert!(!failed && blob.contains("BLOB-MARK"), "{blob}");
    let (_, looped, failed) = results[2];
    assert!(
        !failed && looped.contains(&format!("Redirected to: {}", pages.url("/loop"))),
        "{looped}"
    );
    assert_eq!(pages.hits("/loop"), 1, "a redirect is never followed");
    for (i, (_, text, failed)) in results[3..].iter().enumerate() {
        assert!(*failed, "url #{} was not refused: {text}", i + 4);
    }
    assert!(
        !results[5].1.contains("root:"),
        "file:// was read: {}",
        results[5].1
    );
    assert_eq!(
        pages.requests().len(),
        3,
        "only the three http pages were fetched: {:?}",
        pages.requests()
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_network_allowlist_keeps_webfetch_off_this_machine() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route("/doc", Page::text("LOCAL-PAGE"));
    for sandbox in [
        json!({ "network_mode": "allowlist", "allowed_domains": ["example.invalid"] }),
        json!({ "network_mode": "deny_all" }),
    ] {
        let stub = ProviderStub::start().await?;
        let mut run = stubbed("allowlist", &stub)
            .settings(json!({ "sandbox": sandbox }))
            .start()
            .await?;
        let session = run.session().await?;
        let _host = run.watch(&session, true).await?;
        stub.script([
            Reply::calls("f1", "WebFetch", json!({ "url": pages.url("/doc") })),
            Reply::text("done"),
        ]);
        let events = run.turn(&session, "fetch").await?;
        let fetched = result_of(&events, "WebFetch").expect("a WebFetch result");
        assert!(
            fetched.2 && !fetched.1.contains("LOCAL-PAGE"),
            "{sandbox}: {fetched:?}"
        );
        run.finish().await;
    }
    assert_eq!(pages.hits("/doc"), 0, "a refused fetch reached the server");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_part_of_a_page_webfetch_keeps_reaches_the_model() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route(
        "/big",
        Page::huge(70_000, &[(49_000, "EDGE-MARK-1"), (51_000, "PAST-MARK-1")]),
    );
    pages.route(
        "/wide",
        Page::text(format!("{}WIDE-END-MARK", "中".repeat(20_000))),
    );
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("page-budget", &stub)
        .settings(json!({ "sandbox": PageServer::reachable() }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("f1", "WebFetch", json!({ "url": pages.url("/big") })),
        Reply::calls("f2", "WebFetch", json!({ "url": pages.url("/wide") })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "fetch both").await?;
    let fetched: Vec<&(String, String, bool)> = events
        .tool_results
        .iter()
        .filter(|(n, _, _)| n == "WebFetch")
        .collect();
    assert!(fetched[0].1.contains("EDGE-MARK-1") && !fetched[0].1.contains("PAST-MARK-1"));
    assert!(fetched[1].1.contains("WIDE-END-MARK"));
    let last = stub.calls().last().cloned().expect("a call");
    assert!(
        last.messages_contain("EDGE-MARK-1"),
        "the model got less of the page than WebFetch kept"
    );
    assert!(
        last.messages_contain("WIDE-END-MARK"),
        "a 20 000-character page was cut"
    );
    run.finish().await;
    Ok(())
}
