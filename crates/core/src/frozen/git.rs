//! Git helper functions for collecting repository context.
//!
//! All git subcommands have a 1.5 s timeout; failures silently return `None`
//! so that session startup is never blocked by a slow or missing git repo.

use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Git subcommand unified timeout.
const GIT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Options every git the engine runs itself puts before its subcommand.
///
/// That git runs outside the sandbox, in a repository a sandboxed command may
/// have written, so it must not start a program the repository names. Filter
/// drivers cannot be switched off this way; what keeps them out is the
/// sandbox refusing writes to `.git/config`.
pub const ENGINE_GIT_OPTIONS: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

/// Check whether `cwd` is inside a git work tree.
pub(crate) async fn run_git_check(cwd: &Path) -> bool {
    let out = timeout(
        GIT_TIMEOUT,
        Command::new("git")
            .args(ENGINE_GIT_OPTIONS)
            .arg("rev-parse")
            .arg("--is-inside-work-tree")
            .current_dir(cwd)
            .output(),
    )
    .await;
    match out {
        Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() == "true",
        _ => false,
    }
}

/// Run `git <args>` in `cwd` with a 1.5 s timeout. Returns `stdout` trimmed
/// on success; `None` on failure, timeout, empty output, or non-zero exit.
pub(crate) async fn run_git_text(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(ENGINE_GIT_OPTIONS);
    for a in args {
        cmd.arg(a);
    }
    cmd.current_dir(cwd);
    let out = timeout(GIT_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Detect whether the given directory is a git worktree.
/// A worktree has `.git` as a file (not a directory) containing a `gitdir:` line.
pub(crate) async fn is_worktree(cwd: &Path) -> bool {
    let dot_git = cwd.join(".git");
    if !dot_git.is_file() {
        return false;
    }
    match tokio::fs::read_to_string(&dot_git).await {
        Ok(content) => content.trim().starts_with("gitdir:"),
        Err(_) => false,
    }
}

/// Detect the main branch name. Tries `origin/HEAD` via symbolic-ref first,
/// then falls back to checking `main` / `master` locally.
pub(crate) async fn detect_main_branch(cwd: &Path) -> Option<String> {
    if let Some(out) = run_git_text(
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .await
    {
        if let Some(name) = out.strip_prefix("origin/") {
            return Some(name.to_string());
        }
        return Some(out);
    }
    // Check `main` / `master` existence
    for &cand in &["main", "master"] {
        if run_git_text(
            cwd,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{cand}"),
            ],
        )
        .await
        .is_some()
        {
            return Some(cand.to_string());
        }
    }
    None
}
