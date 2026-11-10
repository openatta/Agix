//! What the file tools and `Bash` decide about paths outside the project:
//! read freely only under the readable roots, and in an unattended session run
//! a shell command only when the OS sandbox holds it.

use base::context::SessionState;
use base::interface::tool::{PermissionDecision, Tool, ToolContext};
use std::path::Path;
use std::sync::Arc;

fn ctx_in(cwd: &Path, state: impl FnOnce(&mut SessionState)) -> ToolContext {
    let mut ctx = platform::tool_context_for_test(cwd.to_path_buf());
    ctx.permission_mode = base::permission::PermissionMode::Default;
    let mut session = SessionState::new(cwd.to_path_buf());
    state(&mut session);
    ctx.additional_writable_dirs = session.additional_writable_dirs.clone();
    ctx.session = Arc::new(session);
    ctx
}

fn allowed(d: &PermissionDecision) -> bool {
    matches!(d, PermissionDecision::Allow { .. })
}

async fn reads(ctx: &ToolContext, dir: &Path) -> [bool; 3] {
    let file = dir.join("notes.txt").display().to_string();
    let dir = dir.display().to_string();
    let read = tools::file_read::FileReadTool
        .check_permissions(&serde_json::json!({ "file_path": file }), ctx)
        .await;
    let glob = tools::glob::GlobTool
        .check_permissions(&serde_json::json!({ "pattern": "*", "path": dir }), ctx)
        .await;
    let grep = tools::grep::GrepTool
        .check_permissions(&serde_json::json!({ "pattern": "x", "path": dir }), ctx)
        .await;
    [allowed(&read), allowed(&glob), allowed(&grep)]
}

#[tokio::test]
async fn reading_outside_the_project_needs_a_readable_root() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();

    let plain = ctx_in(project.path(), |_| {});
    assert_eq!(reads(&plain, project.path()).await, [true; 3]);
    assert_eq!(reads(&plain, elsewhere.path()).await, [false; 3]);

    let listed = ctx_in(project.path(), |s| {
        s.read_directories = vec![elsewhere.path().to_path_buf()]
    });
    assert_eq!(reads(&listed, elsewhere.path()).await, [true; 3]);

    let writable = ctx_in(project.path(), |s| {
        s.additional_writable_dirs = vec![elsewhere.path().to_path_buf()]
    });
    assert_eq!(reads(&writable, elsewhere.path()).await, [true; 3]);

    let anywhere = ctx_in(project.path(), |s| s.read_anywhere = true);
    assert_eq!(reads(&anywhere, elsewhere.path()).await, [true; 3]);
}

#[tokio::test]
async fn glob_and_grep_without_a_path_search_the_project() {
    let project = tempfile::tempdir().unwrap();
    let ctx = ctx_in(project.path(), |_| {});
    let glob = tools::glob::GlobTool
        .check_permissions(&serde_json::json!({ "pattern": "*" }), &ctx)
        .await;
    assert!(allowed(&glob));
}

async fn bash(ctx: &ToolContext) -> PermissionDecision {
    tools::bash::BashTool
        .check_permissions(&serde_json::json!({ "command": "python3 build.py" }), ctx)
        .await
}

#[tokio::test]
async fn an_unattended_command_runs_only_inside_the_sandbox() {
    let project = tempfile::tempdir().unwrap();

    let attended = ctx_in(project.path(), |_| {});
    assert!(matches!(
        bash(&attended).await,
        PermissionDecision::Ask { .. }
    ));

    let mut unconfined = ctx_in(project.path(), |s| s.unattended = true);
    unconfined.dangerously_disable_sandbox = true;
    assert!(matches!(
        bash(&unconfined).await,
        PermissionDecision::Ask { .. }
    ));

    let mut unattended = ctx_in(project.path(), |s| s.unattended = true);
    unattended.dangerously_disable_sandbox = false;
    let holds = tools::bash::sandbox_status().starts_with("available");
    assert_eq!(allowed(&bash(&unattended).await), holds);

    // Asked to, the network has to be closed as well: the sandbox holds
    // writes, not the network.
    unattended.sandbox.unattended_requires_network_policy = true;
    assert!(!allowed(&bash(&unattended).await));
    unattended.sandbox.network_mode = base::context::config::NetworkModeConfig::DenyAll;
    assert_eq!(allowed(&bash(&unattended).await), holds);
}

#[tokio::test]
async fn agent_configuration_is_confirmed_before_a_write_not_a_read() {
    use base::interface::permission::{Permission, PermissionOutcome};
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".agix/agents")).unwrap();
    std::fs::write(project.path().join(".agix/agents/a.md"), "x").unwrap();
    let registry = Arc::new(base::interface::tool::InMemoryToolRegistry::new());
    registry.register(Arc::new(tools::file_read::FileReadTool));
    registry.register(Arc::new(tools::file_write::FileWriteTool));
    let gate = permissions::rule_set_permission::RuleSetPermission::from_settings(
        &base::settings::Settings::defaults_for("m"),
        base::permission::PermissionMode::Default,
        registry,
        [],
    );
    let config = project
        .path()
        .join(".agix/agents/a.md")
        .display()
        .to_string();
    let scratch = project
        .path()
        .join(".agix/scratchpad/x.txt")
        .display()
        .to_string();
    let check = |tool: &'static str, input: serde_json::Value| {
        let gate = &gate;
        let cwd = project.path().to_path_buf();
        async move { gate.check(tool, &input, &cwd, "s").await }
    };

    let read = check("Read", serde_json::json!({ "file_path": config })).await;
    assert!(matches!(read, PermissionOutcome::Permit), "{read:?}");
    let new_config = project
        .path()
        .join(".agix/agents/b.md")
        .display()
        .to_string();
    let write = check(
        "Write",
        serde_json::json!({ "file_path": new_config, "content": "y" }),
    )
    .await;
    assert!(
        matches!(write, PermissionOutcome::Prompt { .. }),
        "{write:?}"
    );
    let scratch = check(
        "Write",
        serde_json::json!({ "file_path": scratch, "content": "y" }),
    )
    .await;
    assert!(matches!(scratch, PermissionOutcome::Permit), "{scratch:?}");
}

async fn accept_edits_allows_reading(ctx: &ToolContext, dir: &Path) -> [bool; 5] {
    let gate = permissions::gate::PermissionGate::empty();
    let file = dir.join("notes.txt").display().to_string();
    let dir = dir.display().to_string();
    let calls: [(&dyn Tool, serde_json::Value); 5] = [
        (
            &tools::file_read::FileReadTool,
            serde_json::json!({ "file_path": file }),
        ),
        (
            &tools::glob::GlobTool,
            serde_json::json!({ "pattern": "*", "path": dir }),
        ),
        (
            &tools::grep::GrepTool,
            serde_json::json!({ "pattern": "x", "path": dir }),
        ),
        (
            &tools::bash::BashTool,
            serde_json::json!({ "command": format!("cat {file}") }),
        ),
        (
            &tools::monitor::MonitorTool,
            serde_json::json!({ "command": format!("tail -f {file}"), "description": "watch" }),
        ),
    ];
    let mut out = [false; 5];
    for (slot, (tool, input)) in out.iter_mut().zip(calls) {
        let Ok(decision) = gate.check(tool, &input, ctx).await else {
            panic!("{} refused its input: {input}", tool.name());
        };
        assert!(
            !matches!(decision, base::permission::PermissionDecision::Deny { .. }),
            "{}: {decision:?}",
            tool.name()
        );
        *slot = matches!(decision, base::permission::PermissionDecision::Allow { .. });
    }
    out
}

#[tokio::test]
async fn accept_edits_asks_before_reading_outside_the_readable_roots() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("notes.txt"), "x").unwrap();
    std::fs::write(elsewhere.path().join("notes.txt"), "x").unwrap();
    let accept_edits = |state: fn(&mut SessionState, &Path)| {
        let mut ctx = ctx_in(project.path(), |s| state(s, elsewhere.path()));
        ctx.permission_mode = base::permission::PermissionMode::AcceptEdits;
        ctx
    };

    let plain = accept_edits(|_, _| {});
    assert_eq!(
        accept_edits_allows_reading(&plain, project.path()).await,
        [true; 5]
    );
    assert_eq!(
        accept_edits_allows_reading(&plain, elsewhere.path()).await,
        [false; 5]
    );

    let listed = accept_edits(|s, dir| s.read_directories = vec![dir.to_path_buf()]);
    assert_eq!(
        accept_edits_allows_reading(&listed, elsewhere.path()).await,
        [true; 5]
    );

    let anywhere = accept_edits(|s, _| s.read_anywhere = true);
    assert_eq!(
        accept_edits_allows_reading(&anywhere, elsewhere.path()).await,
        [true; 5]
    );
}

#[tokio::test]
async fn accept_edits_asks_before_a_shell_read_of_the_home_directory() {
    let project = tempfile::tempdir().unwrap();
    let mut ctx = ctx_in(project.path(), |_| {});
    ctx.permission_mode = base::permission::PermissionMode::AcceptEdits;
    let decision = permissions::gate::PermissionGate::empty()
        .check(
            &tools::bash::BashTool,
            &serde_json::json!({ "command": "cat ~/.zshrc" }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        matches!(decision, base::permission::PermissionDecision::Ask { .. }),
        "{decision:?}"
    );
}
