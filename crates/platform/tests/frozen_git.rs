//! The git context `FrozenContext` collects, with the engine's git run by
//! this machine's providers in real repositories.

use base::engine_exec::{Confinement, EngineExec};
use base::frozen::FrozenContext;
use base::interface::environment::SystemEnvironment;
use base::interface::exec::in_process::NoSandbox;
use base::interface::exec::ExecProviders;
use base::paths::ConfigPaths;
use base::project_trust::ProjectTrust;
use std::path::Path;
use tempfile::TempDir;

fn test_paths(root: &Path) -> ConfigPaths {
    ConfigPaths::new(root.join(".agix"), root.join(".agix"), "code")
}

fn direct() -> EngineExec {
    EngineExec::new(
        platform::local_providers(),
        Confinement::Unconfined,
        ProjectTrust::Trusted,
    )
}

#[tokio::test]
async fn detects_git_repo_in_a_real_repo() {
    // 当前 cwd 是 agix 仓库本身，已经初始化了 git
    let pwd = std::env::current_dir().unwrap();
    let ctx = FrozenContext::collect(
        pwd.clone(),
        &test_paths(&pwd),
        &SystemEnvironment,
        &direct(),
    )
    .await;
    assert!(ctx.is_git, "expected agix workspace to be inside git");
    assert!(ctx.git_branch.is_some());
}

/// Runs a `git` command and fails loudly if it did not work.
///
/// Checks the exit status, not just the spawn: a `git init` that exits
/// non-zero (a machine out of file descriptors or process slots) leaves a
/// directory that is not a repository, and the test would otherwise fail
/// several lines later on an assertion with nothing to act on.
fn run_git(cwd: &std::path::Path) -> impl Fn(&[&str]) + '_ {
    move |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("could not run `git {}`: {e}", args.join(" ")));
        assert!(
            out.status.success(),
            "`git {}` failed ({}): {}{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout),
        );
    }
}

/// The engine's own git runs outside the sandbox, in a repository a
/// sandboxed command may have configured. A program that repository
/// names as its fsmonitor must not run.
#[cfg(unix)]
#[tokio::test]
async fn the_engines_git_does_not_run_the_repositorys_fsmonitor() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let run = run_git(dir.path());
    run(&["init", "-q"]);
    let marker = dir.path().join("fsmonitor-ran");
    let script = dir.path().join("fsmonitor.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    run(&["config", "core.fsmonitor", script.to_str().unwrap()]);

    let mut ctx = FrozenContext::collect(
        dir.path().to_path_buf(),
        &test_paths(dir.path()),
        &SystemEnvironment,
        &direct(),
    )
    .await;
    assert!(ctx.is_git);
    ctx.refresh_git_status(&direct()).await;
    ctx.refresh_git(&direct()).await;
    assert!(!marker.exists(), "the engine's git ran core.fsmonitor");

    // A plain git in the same repository does run it, so the check above
    // is looking at something that would happen.
    run(&["status", "--short"]);
    assert!(marker.exists());
}

/// The session's commands are sandboxed and the host requires that, but
/// the sandbox cannot hold git: the engine goes without git context
/// rather than run git outside it. Without the requirement it runs the
/// plain git the session's commands then get too.
#[tokio::test]
async fn a_git_the_sandbox_cannot_hold_is_not_run_when_the_host_requires_one() {
    let pwd = std::env::current_dir().unwrap();
    let collect = |require_enforcement| {
        let pwd = pwd.clone();
        async move {
            let engine_git = EngineExec::new(
                ExecProviders {
                    sandbox: std::sync::Arc::new(NoSandbox),
                    ..platform::local_providers()
                },
                Confinement::Sandboxed {
                    policy: Default::default(),
                    require_enforcement,
                    tmpdir: None,
                },
                ProjectTrust::Trusted,
            );
            FrozenContext::collect(
                pwd.clone(),
                &test_paths(&pwd),
                &SystemEnvironment,
                &engine_git,
            )
            .await
        }
    };

    let refused = collect(true).await;
    assert!(!refused.is_git);
    assert!(refused.git_status.is_none());
    assert!(refused.git_branch.is_none());

    assert!(collect(false).await.is_git);
}

/// `refresh_git` exists precisely to relax the module's "not refreshed
/// during a session" characteristic for the git-derived fields — this
/// pins down that calling it after the branch actually changes on disk
/// updates `git_branch` in place, without needing a full re-`collect()`.
#[tokio::test]
async fn refresh_git_picks_up_a_branch_change() {
    let dir = TempDir::new().unwrap();
    let run = run_git(dir.path());
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["commit", "--allow-empty", "-q", "-m", "init"]);
    run(&["checkout", "-q", "-b", "original-branch"]);

    let mut ctx = FrozenContext::collect(
        dir.path().to_path_buf(),
        &test_paths(dir.path()),
        &SystemEnvironment,
        &direct(),
    )
    .await;
    assert_eq!(ctx.git_branch.as_deref(), Some("original-branch"));

    run(&["checkout", "-q", "-b", "new-branch"]);
    ctx.refresh_git(&direct()).await;
    assert_eq!(
        ctx.git_branch.as_deref(),
        Some("new-branch"),
        "refresh_git should have picked up the branch switch made after collect()"
    );
}

/// The per-turn `<system-reminder>` re-shows `gitStatus` on every
/// turn, so a session-start snapshot of it is not merely stale, it is
/// actively misleading — the model reads it as current fact and keeps
/// citing files that are no longer dirty. Same shape as
/// `refresh_git_picks_up_a_branch_change`, but for the field that
/// actually changes several times per turn, and driven by a change made
/// *outside* the agent (the user committing) rather than by a git command
/// the model ran — which is the case `refresh_git`'s Bash-sniffing caller
/// cannot see.
#[tokio::test]
async fn refresh_git_status_picks_up_a_commit_made_mid_session() {
    let dir = TempDir::new().unwrap();
    let run = run_git(dir.path());
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["commit", "--allow-empty", "-q", "-m", "init"]);
    tokio::fs::write(dir.path().join("dirty.txt"), "wip")
        .await
        .unwrap();
    run(&["add", "dirty.txt"]);

    let mut ctx = FrozenContext::collect(
        dir.path().to_path_buf(),
        &test_paths(dir.path()),
        &SystemEnvironment,
        &direct(),
    )
    .await;
    assert!(
        ctx.git_status
            .as_deref()
            .unwrap_or_default()
            .contains("dirty.txt"),
        "sanity check: the staged file should show up at collect() time"
    );

    // The user commits from another terminal — no Bash tool call, so the
    // heuristic that gates the wider `refresh_git` never fires.
    run(&["commit", "-q", "-m", "done"]);
    ctx.refresh_git_status(&direct()).await;

    assert!(
        !ctx.git_status
            .as_deref()
            .unwrap_or_default()
            .contains("dirty.txt"),
        "git status must not keep reporting a file that was committed \
         mid-session, got: {:?}",
        ctx.git_status
    );
}
