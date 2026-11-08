//! The two example plugins in `examples/plugins` on one daemon, against a model
//! stub: `demo-rust` built into the daemon as a host would build it, and
//! `demo-quickjs` installed with `plugin.install`. Every registration either
//! shows up in what the model was sent or is reported back over RPC.
//!
//! In this process only: a spawned `agixd` cannot be handed Rust code.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use demo_rust_plugin::{stamp, DemoPlugin, AUDITOR, DEFAULT_MAX_TOKENS};
use live::{Block, Daemon, DaemonOptions, Host, ProviderStub, Reply, SeenRequest, World};
use serde_json::{json, Value};
use test_runner::plugin_fixture::{package_demo, DEMO_PACKAGE, DEMO_PACKAGE_VERSION};

const SECRET: &str = "SECRET-DEMO42";
const NOTICE: &str = "NOTICE-MARK: release freeze until Friday";

struct Staged {
    _world: World,
    provider: ProviderStub,
    daemon: Daemon,
    project: PathBuf,
    disclosure: Value,
}

async fn stage(name: &str) -> anyhow::Result<Staged> {
    let world = World::new()?;
    let project = world.project(name)?;
    world.write_project_settings(name, &json!({ "memory_enabled": false }))?;
    world.write_project_file(
        name,
        "notes.txt",
        &format!("the deploy key is {SECRET} for now\n"),
    )?;
    world.write_project_file(name, "keys.secret", "never read\n")?;
    world.write_project_file(name, ".demo/notice.md", NOTICE)?;
    world.write_project_file(
        name,
        "big.txt",
        &"forty characters of filler, line after.\n".repeat(200),
    )?;

    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(
        &world,
        &provider,
        DaemonOptions::new(project.clone()).host(Host {
            plugins: vec![Arc::new(DemoPlugin::new())],
            ..Default::default()
        }),
    )
    .await?;

    let (archive, checksum) = package_demo(world.root())?;
    let mut client = daemon.connect().await?;
    let installed = client
        .call(
            "plugin.install",
            json!({
                "name": DEMO_PACKAGE,
                "version": DEMO_PACKAGE_VERSION,
                "download_url": format!("file://{}", archive.display()),
                "checksum": checksum,
            }),
        )
        .await?;
    let disclosure = installed
        .result
        .as_ref()
        .and_then(|r| r.get("disclosure"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("plugin.install failed: {installed:?}"))?;
    Ok(Staged {
        _world: world,
        provider,
        daemon,
        project,
        disclosure,
    })
}

fn path(project: &Path, relative: &str) -> String {
    project.join(relative).to_string_lossy().into_owned()
}

fn turn_calls(provider: &ProviderStub) -> Vec<SeenRequest> {
    provider
        .calls()
        .into_iter()
        .filter(|c| !c.tool_names().is_empty())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_package_discloses_its_scripts_agent_and_scene() -> anyhow::Result<()> {
    let staged = stage("disclosed").await?;
    let d = &staged.disclosure;
    assert_eq!(d["plugin"], DEMO_PACKAGE, "{d}");
    assert_eq!(d["scene"], "plugin:demo-quickjs", "{d}");
    let capabilities = d["capabilities"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        capabilities.len(),
        9,
        "one capability per bound script: {capabilities:?}"
    );
    let visible = d["model_visible"].to_string();
    for text in [
        "You are the demo-quickjs reviewer",
        "You are a reviewer working inside",
    ] {
        assert!(visible.contains(text), "`{text}` is not disclosed: {d}");
    }
    staged.daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn every_contribution_reaches_the_model_or_the_host() -> anyhow::Result<()> {
    let staged = stage("demo").await?;
    let project = &staged.project;
    staged.provider.script([
        Reply::Blocks(vec![Block::tool(
            "c1",
            "DemoStamp",
            json!({ "text": "hello" }),
        )]),
        Reply::Blocks(vec![Block::tool(
            "c2",
            "Read",
            json!({ "file_path": path(project, "keys.secret") }),
        )]),
        Reply::Blocks(vec![Block::tool(
            "c3",
            "Read",
            json!({ "file_path": path(project, "notes.txt") }),
        )]),
        Reply::Blocks(vec![Block::tool(
            "c4",
            "Read",
            json!({ "file_path": path(project, "big.txt") }),
        )]),
        Reply::Blocks(vec![Block::tool(
            "c5",
            "Write",
            json!({ "file_path": path(project, "generated/out.txt"), "content": "x\n" }),
        )]),
        Reply::text("all done\n\n\n\nbye"),
        Reply::text("you're welcome"),
    ]);

    let mut client = staged.daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&session, "!stamp hello", "t1", None)
        .await?;
    client
        .session_run_turn(&session, "thanks", "t2", None)
        .await?;

    let calls = turn_calls(&staged.provider);
    let first = calls.first().expect("the turn called the model");
    let system = first.system_text();
    let asked = first.messages_text();
    assert!(
        asked.contains("Call the DemoStamp tool with exactly this text: \"hello\"")
            && !asked.contains("!stamp"),
        "prompt.submit did not expand the shortcut: {asked}"
    );
    for (mark, what) in [
        ("in project `demo`", "prompt.block with prompt.variable"),
        (
            "demo-rust: turn 1;",
            "prompt.context with prompt_variable_from",
        ),
        ("Sections of this prompt, in order:", "assembly_hook"),
        (NOTICE, "async_assembly_hook"),
        ("House rules (demo-quickjs)", "the script's prompt.block"),
        ("demo-quickjs: today is", "the script's prompt.context"),
        ("Checklist (demo-quickjs,", "the script's prompt.assemble"),
    ] {
        assert!(system.contains(mark), "{what} is missing: {system}");
    }
    assert!(
        !system.contains("{{"),
        "a variable was left unexpanded: {system}"
    );
    let tools = first.tool_names();
    assert!(tools.iter().any(|t| t == "DemoStamp"), "{tools:?}");
    assert!(
        !tools.iter().any(|t| t == "WebSearch" || t == "WebFetch"),
        "the script's model.request left a web tool in: {tools:?}"
    );
    assert!(
        calls.iter().all(|c| c.body["max_tokens"]
            .as_u64()
            .is_some_and(|m| m <= u64::from(DEFAULT_MAX_TOKENS))),
        "a request went out above the plugin's ceiling"
    );

    let last = calls.last().expect("two turns called the model");
    let conversation = last.messages_text();
    for (mark, what) in [
        (stamp("hello"), "the tool's answer"),
        (
            "are off limits in this project".to_string(),
            "tool_middleware",
        ),
        ("[redacted]".to_string(), "tool_result"),
        (
            "[demo-quickjs: trimmed".to_string(),
            "the script's tool.result",
        ),
        (
            "generated/ is produced by the build".to_string(),
            "the script's tool.around",
        ),
        ("all done\n\nbye".to_string(), "the script's model.message"),
    ] {
        assert!(conversation.contains(&mark), "{what}: `{mark}` is missing");
    }
    assert!(
        !conversation.contains(SECRET),
        "the secret reached the model"
    );
    assert!(
        !conversation.contains("all done\n\n\n"),
        "blank lines were not folded"
    );
    assert!(!project.join("generated/out.txt").exists());
    assert!(last.system_text().contains("demo-rust: turn 2;"));

    let detail = client
        .session_get(&session)
        .await?
        .result
        .expect("session.get answered");
    let check = detail["health"]["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|c| c["name"] == "demo-rust.activity"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("the plugin's health check is not reported: {detail}"))?;
    assert_eq!(check["status"], "ok", "{check}");
    let counts = &check["details"];
    for (counter, expected) in [("turns", 2), ("stamps", 1), ("refused", 1), ("redacted", 1)] {
        assert_eq!(counts[counter], expected, "{counter}: {check}");
    }
    let scripts = &detail["scripts"];
    assert!(scripts["applied"].as_u64().unwrap_or(0) > 0, "{scripts}");
    assert_eq!(scripts["failed"], 0, "a demo script failed: {scripts}");
    assert_eq!(
        scripts["refused"], 0,
        "a demo script overreached: {scripts}"
    );

    staged.daemon.stop().await;
    Ok(())
}

/// The plugin's agent type is offered, and what its sub-agent runs under is
/// the policy, not the prompt: the refusal follows it, the guide does not. The
/// type's tool list narrows the plugin's own tools as it does every other.
#[tokio::test(flavor = "multi_thread")]
async fn the_auditor_runs_under_the_plugins_policy() -> anyhow::Result<()> {
    let staged = stage("audited").await?;
    let project = &staged.project;
    staged.provider.script_when(
        "You are the demo auditor",
        [
            Reply::Blocks(vec![Block::tool(
                "a1",
                "Read",
                json!({ "file_path": path(project, "keys.secret") }),
            )]),
            Reply::text("AUDIT: 0 findings"),
        ],
    );
    staged.provider.script([
        Reply::Blocks(vec![Block::tool(
            "c1",
            "Agent",
            json!({
                "description": "audit",
                "prompt": "Audit keys.secret",
                "subagent_type": AUDITOR,
            }),
        )]),
        Reply::text("the auditor reported back"),
    ]);

    let mut client = staged.daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&session, "audit the project", "t1", None)
        .await?;

    let calls = staged.provider.calls();
    let parent = calls.first().expect("the turn called the model");
    let agent_tool = parent.body["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|t| t["name"] == "Agent"))
        .map(Value::to_string)
        .unwrap_or_default();
    assert!(
        agent_tool.contains(AUDITOR),
        "the Agent tool does not offer the auditor"
    );

    let audits: Vec<_> = calls
        .iter()
        .filter(|c| c.messages_text().contains("You are the demo auditor"))
        .collect();
    assert_eq!(
        audits.len(),
        2,
        "the auditor should have called the model twice"
    );
    let tools = audits[0].tool_names();
    assert!(tools.iter().any(|t| t == "DemoStamp"), "{tools:?}");
    assert!(
        !tools.iter().any(|t| t == "Write" || t == "Bash"),
        "{tools:?}"
    );
    assert!(
        audits[1]
            .messages_text()
            .contains("are off limits in this project"),
        "the refusal did not follow the work into the sub-agent"
    );
    assert!(
        !audits[0]
            .system_text()
            .contains("This session runs the demo-rust plugin"),
        "the parent's prompt followed it into the sub-agent"
    );

    staged.daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_packages_scene_hosts_a_session() -> anyhow::Result<()> {
    let staged = stage("reviewed").await?;
    staged.provider.script([Reply::text("nothing to report")]);
    let mut client = staged.daemon.connect().await?;
    let session = client
        .session_create(json!({
            "project_root": staged.project.to_string_lossy(),
            "scene": "plugin:demo-quickjs",
        }))
        .await?;
    client
        .session_run_turn(&session, "review the project", "t1", None)
        .await?;

    let call = turn_calls(&staged.provider)
        .pop()
        .expect("the turn called the model");
    assert!(call
        .system_text()
        .contains("You are a reviewer working inside the demo-quickjs scene"));
    let tools = call.tool_names();
    assert!(
        !tools
            .iter()
            .any(|t| t == "Write" || t == "Edit" || t == "Bash"),
        "the scene's tool list was not applied: {tools:?}"
    );
    staged.daemon.stop().await;
    Ok(())
}
