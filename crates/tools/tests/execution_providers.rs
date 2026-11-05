//! The execution layer's acceptance: two providers, and switching is
//! configuration.
//!
//! `BashTool` is the tool that reaches furthest into the machine — it runs a
//! program under a sandbox — so it is the one worth proving this on. Nothing
//! below forks a process, writes a file or opens a socket.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use base::interface::exec::in_process::{
    InMemoryFileSystem, NoSandbox, ScriptedProcess, ScriptedRun,
};
use base::interface::exec::ExecProviders;
use base::interface::tool::{ProgressSender, Tool, ToolContext, ToolResult};
use std::sync::Arc;

fn ctx_with(providers: ExecProviders, require_enforcement: bool) -> ToolContext {
    let mut ctx = ToolContext::for_test(std::path::PathBuf::from("/work"));
    ctx.dangerously_disable_sandbox = false;
    ctx.sandbox.require_enforcement = require_enforcement;
    ctx.exec = providers;
    ctx
}

async fn run(ctx: ToolContext, command: &str) -> ToolResult {
    tools::bash::BashTool
        .call(
            serde_json::json!({ "command": command }),
            ctx,
            ProgressSender::noop("acceptance"),
        )
        .await
        .expect("the tool answers rather than erroring")
}

/// The acceptance. The same tool, the same input, a different provider set —
/// and the answer comes from the second provider rather than from this
/// machine.
#[tokio::test]
async fn a_tool_runs_against_the_second_provider_without_touching_the_machine() {
    let process = ScriptedProcess::new().with(
        "bash -c git status --short",
        ScriptedRun::ok(" M somewhere/else.rs\n"),
    );
    let providers = ExecProviders {
        process: Arc::new(process.clone()),
        filesystem: Arc::new(InMemoryFileSystem::new()),
        network: Arc::new(base::interface::exec::in_process::OfflineNetwork::new()),
        sandbox: Arc::new(NoSandbox),
    };

    let result = run(ctx_with(providers, false), "git status --short").await;

    assert!(!result.is_error, "got: {:?}", result.content);
    assert!(
        format!("{:?}", result.content).contains("somewhere/else.rs"),
        "the output has to come from the provider, not from a real git: {:?}",
        result.content
    );
    assert_eq!(
        process.seen(),
        vec!["bash -c git status --short".to_string()],
        "and the command has to have reached the provider intact — a tool that \
         built its own command line would not show up here"
    );
}

/// Switching is a call, not a rewrite: the same construction with the local
/// set runs the command for real.
#[tokio::test]
async fn the_local_set_is_the_same_construction() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ctx_with(ExecProviders::local(), false);
    ctx.cwd = dir.path().to_path_buf();
    ctx.dangerously_disable_sandbox = true;
    let result = run(ctx, "echo from-the-machine").await;
    assert!(format!("{:?}", result.content).contains("from-the-machine"));
}

/// The honesty that makes the second provider safe to have. It does not run
/// real processes, so it cannot confine one, and it says so — which means a
/// deployment demanding an absolute boundary does not get a false one from it.
#[tokio::test]
async fn the_second_provider_does_not_pretend_to_be_a_sandbox() {
    let providers = ExecProviders {
        process: Arc::new(ScriptedProcess::new().with("bash -c ls", ScriptedRun::ok("x"))),
        filesystem: Arc::new(InMemoryFileSystem::new()),
        network: Arc::new(base::interface::exec::in_process::OfflineNetwork::new()),
        sandbox: Arc::new(NoSandbox),
    };
    let result = run(ctx_with(providers, true), "ls").await;
    assert!(result.is_error);
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|v| v.get("refused"))
            .and_then(|v| v.as_str()),
        Some("sandbox_unenforceable"),
    );
}

/// A command nobody scripted fails rather than succeeding empty. An empty
/// success would be read as "the command produced no output", which is a much
/// harder thing to notice than "nothing answers here".
#[tokio::test]
async fn an_unscripted_command_is_a_failure_not_an_empty_success() {
    let providers = ExecProviders {
        process: Arc::new(ScriptedProcess::new()),
        filesystem: Arc::new(InMemoryFileSystem::new()),
        network: Arc::new(base::interface::exec::in_process::OfflineNetwork::new()),
        sandbox: Arc::new(NoSandbox),
    };
    let err = tools::bash::BashTool
        .call(
            serde_json::json!({ "command": "whoami" }),
            ctx_with(providers, false),
            ProgressSender::noop("acceptance"),
        )
        .await
        .expect_err("an unscripted command is a gap in the test, not a quiet pass");
    assert!(err.to_string().contains("nothing scripted"));
}
