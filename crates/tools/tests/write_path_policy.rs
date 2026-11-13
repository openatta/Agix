//! What a write tool refuses, asked through the tool.
//!
//! The engine carried two write-path checks with the same name. The thorough
//! one — credential filenames, system directories, Unicode normalization —
//! documented itself as the guard for `FileWrite` / `FileEdit` and was never
//! called by it; the tools used a ten-line one that only asked whether the
//! path stayed inside the project.
//!
//! It survived that long because every one of its rules had a unit test, and
//! every one of those called the function directly. Nothing asked the tool.
//! So these do.

use base::interface::permission::Reach;
use base::interface::tool::{Tool, ToolContext};
use std::path::PathBuf;

fn ctx_in(cwd: &std::path::Path) -> ToolContext {
    let mut ctx = platform::tool_context_for_test(cwd.to_path_buf());
    ctx.permission_mode = base::permission::PermissionMode::Default;
    ctx
}

async fn write_decision(ctx: &ToolContext, path: &std::path::Path) -> Reach {
    tools::file_write::FileWriteTool
        .permission_facts(
            &serde_json::json!({ "file_path": path.display().to_string(), "content": "x" }),
            ctx,
        )
        .await
        .reach
}

fn denied(d: &Reach) -> bool {
    matches!(d, Reach::Refused(_))
}

/// The credential list, reached the way a model reaches it.
#[tokio::test]
async fn a_credential_file_inside_the_project_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    for name in [".env", ".env.production", "id_rsa", ".npmrc"] {
        let d = write_decision(&ctx, &dir.path().join(name)).await;
        assert!(
            denied(&d),
            "{name} is on the credential list and must not be writable: {d:?}"
        );
    }
    // A nested one too — the rule is about any component, not just the leaf.
    assert!(denied(
        &write_decision(&ctx, &dir.path().join(".ssh/config")).await
    ));
}

/// And the ordinary case still works, which is what makes the refusals worth
/// anything.
#[tokio::test]
async fn an_ordinary_file_inside_the_project_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    for name in ["src/main.rs", "README.md", ".editorconfig"] {
        let d = write_decision(&ctx, &dir.path().join(name)).await;
        assert!(
            matches!(d, Reach::Inside),
            "{name} should be an ordinary write: {d:?}"
        );
    }
}

/// The control list is the point: a deployment says which of the defaults do
/// not apply to it. Without this the defaults are a wall, and the only
/// available answer is to stop checking — which is what the engine had been
/// doing.
#[tokio::test]
async fn a_deployment_can_permit_one_of_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ctx_in(dir.path());
    ctx.sandbox.allow_write = vec![dir.path().join(".env.example")];

    assert!(
        matches!(
            write_decision(&ctx, &dir.path().join(".env.example")).await,
            Reach::Inside
        ),
        "the exemption has to reach the tool"
    );
    assert!(
        denied(&write_decision(&ctx, &dir.path().join(".env")).await),
        "and it exempts what it names, not the whole rule"
    );
}

/// The other half of a control list: adding to it.
#[tokio::test]
async fn a_deployment_can_add_a_name_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ctx_in(dir.path());
    ctx.sandbox.deny_write = vec!["secrets".into()];
    assert!(denied(
        &write_decision(&ctx, &dir.path().join("secrets")).await
    ));
    assert!(denied(
        &write_decision(&ctx, &dir.path().join("secrets.yaml")).await
    ));
    assert!(matches!(
        write_decision(&ctx, &dir.path().join("public.yaml")).await,
        Reach::Inside
    ));
}

/// Leaving the project is a question for a person, not a refusal — that
/// distinction predates this and has to survive it.
#[tokio::test]
async fn writing_outside_the_project_still_asks_rather_than_refusing() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let d = write_decision(&ctx_in(dir.path()), &elsewhere.path().join("note.txt")).await;
    assert!(matches!(d, Reach::Outside), "{d:?}");
}

/// An exemption lifts the deny rules and not the project boundary, asked
/// through the tool because that is where the two could be confused.
#[tokio::test]
async fn an_exemption_does_not_open_a_path_outside_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let mut ctx = ctx_in(dir.path());
    ctx.sandbox.allow_write = vec![elsewhere.path().to_path_buf()];
    let d = write_decision(&ctx, &elsewhere.path().join(".env")).await;
    assert!(
        matches!(d, Reach::Outside),
        "still outside the project, so still a question for a person: {d:?}"
    );
}

/// The same policy governs the other two write tools. They had their own
/// copies of the call, so they get their own case.
#[tokio::test]
async fn edit_and_notebook_refuse_the_same_files() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let target = dir.path().join(".env");

    let edit = tools::file_edit::FileEditTool
        .permission_facts(
            &serde_json::json!({
                "file_path": target.display().to_string(),
                "old_string": "a",
                "new_string": "b"
            }),
            &ctx,
        )
        .await
        .reach;
    assert!(denied(&edit), "{edit:?}");

    let notebook = tools::notebook_edit::NotebookEditTool
        .permission_facts(
            &serde_json::json!({
                "file_path": dir.path().join(".env.ipynb").display().to_string(),
                "mode": "edit",
                "cell_index": 0,
                "new_source": "x"
            }),
            &ctx,
        )
        .await
        .reach;
    assert!(denied(&notebook), "{notebook:?}");
}

/// A project reached through a symlink is still the project.
///
/// The thorough checker resolved the target and not the roots, so on any
/// machine where the working directory goes through a symlink — `/tmp` is
/// `/private/tmp` on macOS — every write looked like it was leaving a project
/// it was inside.
#[cfg(unix)]
#[tokio::test]
async fn a_project_reached_through_a_symlink_is_still_the_project() {
    let real = tempfile::tempdir().unwrap();
    let link_parent = tempfile::tempdir().unwrap();
    let link: PathBuf = link_parent.path().join("project");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();

    let d = write_decision(&ctx_in(&link), &link.join("src/main.rs")).await;
    assert!(
        matches!(d, Reach::Inside),
        "both sides have to be resolved by the same filesystem: {d:?}"
    );
}

// Everything above hands the tool a context directly. Production judges the
// call with the context the runtime builds from the settings, so the control
// list configured there has to reach the policy's decision too.

use base::interface::permission::{
    PermissionOutcome, PermissionPolicy, PermissionRequest, ToolFacts,
};

fn settings_allowing(paths: Vec<PathBuf>) -> base::settings::LoadedSettings {
    let mut s = base::settings::Settings::defaults_for("test-model");
    s.sandbox.allow_write = paths;
    base::settings::LoadedSettings::new(s, base::settings::PathSettings::default())
}

async fn gate_decision(
    settings: &base::settings::LoadedSettings,
    cwd: &std::path::Path,
    path: &std::path::Path,
) -> PermissionOutcome {
    let write: std::sync::Arc<dyn base::interface::tool::Tool> =
        std::sync::Arc::new(tools::file_write::FileWriteTool);
    let gate = permissions::policy::Policy::from_settings(settings, []);
    let session = std::sync::Arc::new(base::context::SessionState::for_tests(cwd.to_path_buf()));
    let mut ctx = ctx_in(cwd);
    ctx.session = session.clone();
    ctx.sandbox.allow_write = settings.values.sandbox.allow_write.clone();
    ctx.sandbox.deny_write = settings.values.sandbox.deny_write.clone();
    let input = serde_json::json!({ "file_path": path.display().to_string(), "content": "x" });
    let facts = ToolFacts::gather(write.as_ref(), &input, &ctx).await;
    gate.decide(&PermissionRequest::new(
        "Write",
        Some(&write),
        &input,
        &facts,
        &ctx,
        &session,
    ))
    .await
}

/// The default list reaches the real decision.
#[tokio::test]
async fn the_gate_refuses_a_credential_file() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = gate_decision(
        &settings_allowing(vec![]),
        dir.path(),
        &dir.path().join(".env"),
    )
    .await;
    assert!(
        matches!(outcome, PermissionOutcome::Deny { .. }),
        "{outcome:?}"
    );
}

/// And so does the exemption. Without this the control list is configurable
/// everywhere except where it is consulted.
#[tokio::test]
async fn the_gate_honours_a_configured_exemption() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join(".env.example");
    let outcome = gate_decision(
        &settings_allowing(vec![target.clone()]),
        dir.path(),
        &target,
    )
    .await;
    assert!(
        !matches!(outcome, PermissionOutcome::Deny { .. }),
        "a setting that never reaches the gate is a setting that does nothing: {outcome:?}"
    );
}

/// A sub-agent's worktree lives at `<repo>/.agix/worktrees/<slug>` and is its
/// cwd. Guarding the `.agix` directory by name denied every write the
/// sub-agent was created to make — while the thing actually worth guarding is
/// the settings file, which is one name in that directory rather than all of
/// them.
#[tokio::test]
async fn a_sub_agent_can_write_inside_its_worktree() {
    let repo = tempfile::tempdir().unwrap();
    let worktree = repo.path().join(".agix/worktrees/feature");
    std::fs::create_dir_all(&worktree).unwrap();

    let d = write_decision(&ctx_in(&worktree), &worktree.join("src/main.rs")).await;
    assert!(
        matches!(d, Reach::Inside),
        "the worktree is the sub-agent's project: {d:?}"
    );
}

/// And the thing that guarding `.agix` was for still holds.
#[tokio::test]
async fn the_engines_own_settings_stay_guarded() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".agix")).unwrap();
    for name in [
        ".agix/settings.json",
        ".agix/settings.local.json",
        ".agix/mcp.local.json",
        ".claude/settings.json",
        ".claude/settings.local.json",
    ] {
        let d = write_decision(&ctx_in(repo.path()), &repo.path().join(name)).await;
        assert!(denied(&d), "{name}: {d:?}");
    }
    // A file that merely lives beside them is not the settings.
    assert!(matches!(
        write_decision(&ctx_in(repo.path()), &repo.path().join(".agix/notes.md")).await,
        Reach::Inside
    ));
}

/// direnv runs `.envrc` in the user's own shell, outside any sandbox, once
/// they allow it; asked to allow a changed one again, few read it first.
#[tokio::test]
async fn a_direnv_file_is_not_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    for name in [".envrc", "sub/.envrc"] {
        let d = write_decision(&ctx_in(dir.path()), &dir.path().join(name)).await;
        assert!(denied(&d), "{name}: {d:?}");
    }
}

/// macOS hands back the name as the filesystem stores it, and anything
/// created through Cocoa is stored decomposed. Running the normalization
/// check on that answer refuses an ordinary file for being an attack; it
/// belongs on the path the model asked for.
#[tokio::test]
async fn a_file_whose_name_is_decomposed_on_disk_is_still_editable() {
    let dir = tempfile::tempdir().unwrap();
    // On disk decomposed, which is how anything created through Cocoa is
    // stored; asked for composed, which is how it displays and therefore how
    // the model refers to it. macOS resolves one to the other, so the check
    // sees a composed request and a decomposed answer.
    std::fs::write(dir.path().join("cafe\u{301}.txt"), b"x").unwrap();
    let composed = dir.path().join("caf\u{e9}.txt");

    let d = write_decision(&ctx_in(dir.path()), &composed).await;
    assert!(
        matches!(d, Reach::Inside),
        "an ordinary file the user asked to edit: {d:?}"
    );
}

/// The protection it was there for is on the request, where it belongs: a
/// path the model supplies whose normalization differs from what it displays
/// as is still refused.
#[tokio::test]
async fn a_decomposed_path_the_model_supplies_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let d = write_decision(&ctx_in(dir.path()), &dir.path().join("nue\u{301}vo.txt")).await;
    assert!(denied(&d), "{d:?}");
}
