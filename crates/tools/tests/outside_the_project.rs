//! What the file tools and `Bash` decide about paths outside the project:
//! read freely only under the readable roots, and in an unattended session run
//! a shell command only when the OS sandbox holds it.

use base::context::{SessionEnv, SessionState};
use base::interface::tool::{PermissionDecision, Tool, ToolContext};
use std::path::Path;
use std::sync::Arc;

fn ctx_in(cwd: &Path, state: impl FnOnce(&mut SessionEnv)) -> ToolContext {
    let mut ctx = platform::tool_context_for_test(cwd.to_path_buf());
    ctx.permission_mode = base::permission::PermissionMode::Default;
    let mut env = SessionEnv::new(cwd.to_path_buf());
    state(&mut env);
    ctx.additional_writable_dirs = env.additional_writable_dirs.clone();
    ctx.session = Arc::new(SessionState::for_tests_in(env));
    ctx
}

/// The policy's word on a call made in `ctx`, in the mode `ctx` says.
async fn judged(
    tool: Arc<dyn Tool>,
    input: &serde_json::Value,
    ctx: &ToolContext,
) -> base::interface::permission::PermissionOutcome {
    use base::interface::permission::{PermissionPolicy, PermissionRequest};
    ctx.session.set_permission_mode(ctx.permission_mode);
    permissions::policy::Policy::empty()
        .decide(
            &PermissionRequest::new(
                tool.name(),
                Some(&tool),
                input,
                &ctx.cwd,
                &ctx.session_id,
                &ctx.session,
            )
            .with_ctx(ctx),
        )
        .await
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

/// Whether the policy lets `python3 build.py` run with nobody attending.
async fn runs_unattended(ctx: &ToolContext) -> bool {
    use base::interface::permission::{Attendance, PermissionPolicy, PermissionRequest, ToolFacts};
    let mut ctx = ctx.clone();
    ctx.attendance = Attendance::Unattended;
    ctx.confinement = tools::bash::confinement_report(
        &ctx.sandbox,
        &ctx.exec,
        ctx.dangerously_disable_sandbox,
        &ctx.cwd,
        &ctx.session_id,
    );
    let tool: Arc<dyn Tool> = Arc::new(tools::bash::BashTool);
    let input = serde_json::json!({ "command": "python3 build.py" });
    let facts = ToolFacts::gather(tool.as_ref(), &input, &ctx).await;
    permissions::policy::Policy::empty()
        .decide(
            &PermissionRequest::new("Bash", Some(&tool), &input, &ctx.cwd, "s", &ctx.session)
                .with_facts(&facts)
                .with_ctx(&ctx)
                .with_attendance(Attendance::Unattended),
        )
        .await
        .is_permit()
}

#[tokio::test]
async fn an_unattended_command_runs_only_inside_the_sandbox() {
    let project = tempfile::tempdir().unwrap();

    let attended = ctx_in(project.path(), |_| {});
    let asked = judged(
        Arc::new(tools::bash::BashTool),
        &serde_json::json!({ "command": "python3 build.py" }),
        &attended,
    )
    .await;
    assert!(
        matches!(
            asked,
            base::interface::permission::PermissionOutcome::Prompt { .. }
        ),
        "{asked:?}"
    );

    let mut unconfined = ctx_in(project.path(), |s| s.unattended = true);
    unconfined.dangerously_disable_sandbox = true;
    assert!(!runs_unattended(&unconfined).await);

    let mut unattended = ctx_in(project.path(), |s| s.unattended = true);
    unattended.dangerously_disable_sandbox = false;
    let holds = tools::bash::sandbox_status().starts_with("available");
    assert_eq!(runs_unattended(&unattended).await, holds);

    // Asked to, the network has to be closed as well: the sandbox holds
    // writes, not the network.
    unattended.sandbox.unattended_requires_network_policy = true;
    assert!(!runs_unattended(&unattended).await);
    unattended.sandbox.network_mode = base::context::config::NetworkModeConfig::DenyAll;
    assert_eq!(runs_unattended(&unattended).await, holds);
}

#[tokio::test]
async fn agent_configuration_is_confirmed_before_a_write_not_a_read() {
    use base::interface::permission::{PermissionOutcome, PermissionPolicy, PermissionRequest};
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".agix/agents")).unwrap();
    std::fs::write(project.path().join(".agix/agents/a.md"), "x").unwrap();
    let registry = Arc::new(base::interface::tool::InMemoryToolRegistry::new());
    registry.register(Arc::new(tools::file_read::FileReadTool));
    registry.register(Arc::new(tools::file_write::FileWriteTool));
    let gate = permissions::policy::Policy::from_settings(
        &base::settings::LoadedSettings::new(
            base::settings::Settings::defaults_for("m"),
            base::settings::PathSettings::default(),
        ),
        [],
    );
    let session = Arc::new(base::context::SessionState::for_tests(
        project.path().to_path_buf(),
    ));
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
        let (gate, session, found) = (&gate, &session, registry.get(tool));
        let cwd = project.path().to_path_buf();
        async move {
            gate.decide(&PermissionRequest::new(
                tool,
                found.as_ref(),
                &input,
                &cwd,
                "s",
                session,
            ))
            .await
        }
    };

    let read = check("Read", serde_json::json!({ "file_path": config })).await;
    assert!(matches!(read, PermissionOutcome::Permit { .. }), "{read:?}");
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
    assert!(
        matches!(scratch, PermissionOutcome::Permit { .. }),
        "{scratch:?}"
    );
}

async fn accept_edits_allows_reading(ctx: &ToolContext, dir: &Path) -> [bool; 5] {
    let file = dir.join("notes.txt").display().to_string();
    let dir = dir.display().to_string();
    let calls: [(Arc<dyn Tool>, serde_json::Value); 5] = [
        (
            Arc::new(tools::file_read::FileReadTool),
            serde_json::json!({ "file_path": file }),
        ),
        (
            Arc::new(tools::glob::GlobTool),
            serde_json::json!({ "pattern": "*", "path": dir }),
        ),
        (
            Arc::new(tools::grep::GrepTool),
            serde_json::json!({ "pattern": "x", "path": dir }),
        ),
        (
            Arc::new(tools::bash::BashTool),
            serde_json::json!({ "command": format!("cat {file}") }),
        ),
        (
            Arc::new(tools::monitor::MonitorTool),
            serde_json::json!({ "command": format!("tail -f {file}"), "description": "watch" }),
        ),
    ];
    let mut out = [false; 5];
    for (slot, (tool, input)) in out.iter_mut().zip(calls) {
        let name = tool.name().to_string();
        let decision = judged(tool, &input, ctx).await;
        assert!(
            !matches!(
                decision,
                base::interface::permission::PermissionOutcome::Deny { .. }
            ),
            "{name}: {decision:?}"
        );
        *slot = decision.is_permit();
    }
    out
}

#[tokio::test]
async fn accept_edits_asks_before_reading_outside_the_readable_roots() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("notes.txt"), "x").unwrap();
    std::fs::write(elsewhere.path().join("notes.txt"), "x").unwrap();
    let accept_edits = |state: fn(&mut SessionEnv, &Path)| {
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
    let decision = judged(
        Arc::new(tools::bash::BashTool),
        &serde_json::json!({ "command": "cat ~/.zshrc" }),
        &ctx,
    )
    .await;
    assert!(
        matches!(
            decision,
            base::interface::permission::PermissionOutcome::Prompt { .. }
        ),
        "{decision:?}"
    );
}
