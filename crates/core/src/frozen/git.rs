//! The git commands the engine runs itself, and the helpers that collect
//! repository context with them.
//!
//! [`EngineGit`] decides how each one is started: in a session whose commands
//! the OS sandbox confines, inside that same sandbox, so whatever a repository
//! a confined command configured makes git run is confined too. The context
//! helpers give every subcommand a 1.5 s timeout and turn any failure into
//! `None`, so session startup is never blocked by a slow or missing repo.

use crate::interface::exec::{Enforcement, NetworkMode, ProcessSpec, Sandbox, SandboxPolicy};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Git subcommand unified timeout.
const GIT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Options every git the engine runs itself puts before its subcommand.
///
/// They turn off the fsmonitor and hooks a repository names: one a sandboxed
/// command may have written must not start a program through them, whether
/// or not the git reading it is confined. Filter drivers cannot be turned off
/// this way; [`EngineGit`] is what confines them.
pub const ENGINE_GIT_OPTIONS: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

/// How the engine starts its own git for one session.
#[derive(Clone, Default)]
pub enum EngineGit {
    /// As a plain child of the engine: the session's commands are not
    /// confined, so a confined command cannot have prepared the repository.
    #[default]
    Direct,
    /// Inside the sandbox the session's commands run in.
    Sandboxed(GitSandbox),
}

/// The sandbox an [`EngineGit::Sandboxed`] git runs in.
#[derive(Clone)]
pub struct GitSandbox {
    sandbox: Arc<dyn Sandbox>,
    policy: SandboxPolicy,
    require_enforcement: bool,
    tmpdir: Option<PathBuf>,
}

impl std::fmt::Debug for EngineGit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct => f.write_str("Direct"),
            Self::Sandboxed(s) => f
                .debug_struct("Sandboxed")
                .field("require_enforcement", &s.require_enforcement)
                .field("tmpdir", &s.tmpdir)
                .finish_non_exhaustive(),
        }
    }
}

impl EngineGit {
    /// `policy` is the session's own; what this git may write is narrowed
    /// from it per command. `tmpdir` becomes the command's `TMPDIR`, and is
    /// writable.
    pub fn sandboxed(
        sandbox: Arc<dyn Sandbox>,
        mut policy: SandboxPolicy,
        require_enforcement: bool,
        tmpdir: Option<PathBuf>,
    ) -> Self {
        policy.only_additional_writable = true;
        policy.network_mode = NetworkMode::DenyAll;
        policy.additional_writable = tmpdir.iter().cloned().collect();
        Self::Sandboxed(GitSandbox {
            sandbox,
            policy,
            require_enforcement,
            tmpdir,
        })
    }

    pub fn is_sandboxed(&self) -> bool {
        matches!(self, Self::Sandboxed(_))
    }

    /// `git <ENGINE_GIT_OPTIONS> <args>` in `cwd`, as it has to be started.
    /// Inside the sandbox it may write only `writable`.
    ///
    /// `Err` says why it must not be started at all: the sandbox cannot hold
    /// it and the host requires that it does. A sandbox that cannot hold it
    /// when nothing requires one gives the same plain git the session's own
    /// commands then get.
    pub fn command(
        &self,
        cwd: &Path,
        args: &[&str],
        writable: &[PathBuf],
    ) -> Result<ProcessSpec, String> {
        let spec = ProcessSpec::new("git", cwd)
            .args(ENGINE_GIT_OPTIONS.iter().copied())
            .args(args.iter().copied());
        let Self::Sandboxed(s) = self else {
            return Ok(spec);
        };
        let mut policy = s.policy.clone();
        policy.additional_writable.extend(writable.iter().cloned());
        let mut confined = s.sandbox.confine(spec.clone(), &policy);
        if confined.enforcement == Enforcement::None {
            let unmet = confined.unmet.join("; ");
            if s.require_enforcement {
                return Err(format!(
                    "the session's commands are sandboxed and `sandbox.require_enforcement` \
                     is on, but git cannot be sandboxed here ({unmet})"
                ));
            }
            warn_unconfined_once(&unmet);
            return Ok(spec);
        }
        if let Some(tmp) = &s.tmpdir {
            confined
                .spec
                .env
                .push(("TMPDIR".to_string(), tmp.display().to_string()));
        }
        Ok(confined.spec)
    }
}

fn warn_unconfined_once(unmet: &str) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            unmet,
            "the engine's own git runs unsandboxed: the sandbox cannot hold it on this \
             build or platform"
        );
    });
}

async fn output(git: &EngineGit, cwd: &Path, args: &[&str]) -> Option<std::process::Output> {
    let spec = git.command(cwd, args, &[]).ok()?;
    let mut cmd = Command::new(&spec.program);
    crate::interface::exec::local::SubprocessEnv::default().apply(&mut cmd);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(spec.env.iter().map(|(k, v)| (k, v)));
    timeout(GIT_TIMEOUT, cmd.output()).await.ok()?.ok()
}

/// Check whether `cwd` is inside a git work tree.
pub(crate) async fn run_git_check(git: &EngineGit, cwd: &Path) -> bool {
    match output(git, cwd, &["rev-parse", "--is-inside-work-tree"]).await {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() == "true",
        _ => false,
    }
}

/// Run `git <args>` in `cwd` with a 1.5 s timeout. Returns `stdout` trimmed
/// on success; `None` on failure, timeout, empty output, or non-zero exit.
pub(crate) async fn run_git_text(git: &EngineGit, cwd: &Path, args: &[&str]) -> Option<String> {
    let out = output(git, cwd, args).await?;
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
pub(crate) async fn detect_main_branch(git: &EngineGit, cwd: &Path) -> Option<String> {
    if let Some(out) = run_git_text(
        git,
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
            git,
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
