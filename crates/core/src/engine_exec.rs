//! The subprocesses the engine starts on a session's behalf — its own git and
//! helpers such as `pdftotext` — and the one way it starts them: through that
//! session's [`ExecProviders`].
//!
//! Going through the session's `exec.process` is what puts them where the
//! session's commands are: on the machine (or remote) the host chose, under
//! the same `sandbox.subprocess_env`, in a session of their own with no
//! controlling terminal, and stopped as a whole group when they time out. In a
//! session whose commands the OS sandbox confines they are confined too, so
//! whatever a repository a confined command configured makes git run is held
//! to the same roots.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::interface::exec::{
    capture, Captured, Enforcement, ExecError, ExecProviders, NetworkMode, ProcessSpec,
    SandboxPolicy,
};
use crate::project_trust::ProjectTrust;

/// Options every git the engine runs itself puts before its subcommand.
///
/// They turn off the fsmonitor and hooks a repository names: one a sandboxed
/// command may have written must not start a program through them, whether
/// or not the git reading it is confined. Filter drivers cannot be turned off
/// this way; [`Confinement::Sandboxed`] is what confines them.
pub const ENGINE_GIT_OPTIONS: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

/// The engine's subprocesses for one session.
#[derive(Clone)]
pub struct EngineExec {
    exec: ExecProviders,
    confinement: Confinement,
    trust: ProjectTrust,
}

/// Whether the engine's subprocesses run in the session's sandbox.
#[derive(Debug, Clone, Default)]
#[allow(clippy::large_enum_variant)]
pub enum Confinement {
    /// The session's commands are not confined, so a confined command cannot
    /// have prepared the repository.
    #[default]
    Unconfined,
    /// Inside `exec.sandbox`, under the session's own `policy` narrowed per
    /// call: no network, and nothing writable but `tmpdir` and what the call
    /// names. `tmpdir` becomes the subprocess's `TMPDIR`.
    Sandboxed {
        policy: SandboxPolicy,
        require_enforcement: bool,
        tmpdir: Option<PathBuf>,
    },
}

/// What a git the engine runs may write.
#[derive(Debug, Clone, Copy)]
pub enum GitAccess<'a> {
    ReadOnly,
    Writes(&'a [PathBuf]),
}

impl<'a> GitAccess<'a> {
    fn writable(&self) -> &'a [PathBuf] {
        match self {
            Self::ReadOnly => &[],
            Self::Writes(paths) => paths,
        }
    }
}

/// Whether a git may start, decided before anything does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Run { options: &'static [&'static str] },
    Refuse(String),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum EngineExecError {
    /// Not started: the admission refused it, or it has to be sandboxed and
    /// the sandbox cannot hold it.
    #[error("not run: {0}")]
    Refused(String),
    #[error("timed out after {0:?}")]
    TimedOut(Duration),
    #[error(transparent)]
    Failed(#[from] ExecError),
}

/// The one decision point for every git the engine starts.
pub fn admit_git(
    _trust: ProjectTrust,
    _access: &GitAccess<'_>,
    _confinement: &Confinement,
) -> Admission {
    Admission::Run {
        options: ENGINE_GIT_OPTIONS,
    }
}

impl EngineExec {
    pub fn new(exec: ExecProviders, confinement: Confinement, trust: ProjectTrust) -> Self {
        Self {
            exec,
            confinement,
            trust,
        }
    }

    pub fn exec(&self) -> &ExecProviders {
        &self.exec
    }

    pub fn is_sandboxed(&self) -> bool {
        matches!(self.confinement, Confinement::Sandboxed { .. })
    }

    /// `git <options> <args>` in `cwd`, never asking for credentials on a
    /// terminal. The only way the engine starts git.
    pub async fn git(
        &self,
        cwd: &Path,
        access: GitAccess<'_>,
        args: &[&str],
        deadline: Duration,
    ) -> Result<Captured, EngineExecError> {
        let options = match admit_git(self.trust, &access, &self.confinement) {
            Admission::Run { options } => options,
            Admission::Refuse(why) => return Err(EngineExecError::Refused(why)),
        };
        let mut spec = ProcessSpec::new("git", cwd)
            .args(options.iter().copied())
            .args(args.iter().copied());
        spec.env.extend([
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
            ("GIT_ASKPASS".to_string(), String::new()),
        ]);
        self.run(spec, access.writable(), deadline).await
    }

    /// A helper program, confined as the session's commands are, writing
    /// nothing but `writable`.
    pub async fn helper(
        &self,
        spec: ProcessSpec,
        writable: &[PathBuf],
        deadline: Duration,
    ) -> Result<Captured, EngineExecError> {
        self.run(spec, writable, deadline).await
    }

    async fn run(
        &self,
        spec: ProcessSpec,
        writable: &[PathBuf],
        deadline: Duration,
    ) -> Result<Captured, EngineExecError> {
        let spec = self.confine(spec, writable)?;
        let captured = tokio::time::timeout(
            deadline,
            capture(&*self.exec.process, spec, CancellationToken::new()),
        )
        .await
        .map_err(|_| EngineExecError::TimedOut(deadline))??;
        Ok(captured)
    }

    /// `spec` as it has to be started. A sandbox that cannot hold it when
    /// nothing requires one gives the same unconfined program the session's
    /// own commands then get.
    fn confine(
        &self,
        spec: ProcessSpec,
        writable: &[PathBuf],
    ) -> Result<ProcessSpec, EngineExecError> {
        let Confinement::Sandboxed {
            policy,
            require_enforcement,
            tmpdir,
        } = &self.confinement
        else {
            return Ok(spec);
        };
        let mut policy = policy.clone();
        policy.only_additional_writable = true;
        policy.network_mode = NetworkMode::DenyAll;
        policy.additional_writable = tmpdir.iter().chain(writable).cloned().collect();
        let mut confined = self.exec.sandbox.confine(spec.clone(), &policy);
        if confined.enforcement == Enforcement::None {
            let unmet = confined.unmet.join("; ");
            if *require_enforcement {
                return Err(EngineExecError::Refused(format!(
                    "the session's commands are sandboxed and `sandbox.require_enforcement` \
                     is on, but {} cannot be sandboxed here ({unmet})",
                    spec.program
                )));
            }
            warn_unconfined_once(&unmet);
            return Ok(spec);
        }
        if let Some(tmp) = tmpdir {
            confined
                .spec
                .env
                .push(("TMPDIR".to_string(), tmp.display().to_string()));
        }
        Ok(confined.spec)
    }
}

impl std::fmt::Debug for EngineExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineExec")
            .field("confinement", &self.confinement)
            .field("trust", &self.trust)
            .finish_non_exhaustive()
    }
}

fn warn_unconfined_once(unmet: &str) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            unmet,
            "the engine's own subprocesses run unsandboxed: the sandbox cannot hold them \
             on this build or platform"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::exec::in_process::{NoSandbox, ScriptedProcess, ScriptedRun};
    use crate::interface::exec::{Confined, Sandbox};
    use std::sync::{Arc, Mutex};

    const DEADLINE: Duration = Duration::from_secs(5);

    fn with_process(process: ScriptedProcess) -> ExecProviders {
        ExecProviders {
            process: Arc::new(process),
            ..ExecProviders::in_process()
        }
    }

    fn git_key(args: &str) -> String {
        format!("git {} {args}", ENGINE_GIT_OPTIONS.join(" "))
    }

    #[tokio::test]
    async fn git_runs_through_the_sessions_process_with_the_engines_options() {
        let process = ScriptedProcess::new().with(git_key("status"), ScriptedRun::ok("clean"));
        let engine = EngineExec::new(
            with_process(process.clone()),
            Confinement::Unconfined,
            ProjectTrust::Trusted,
        );
        let out = engine
            .git(
                Path::new("/repo"),
                GitAccess::ReadOnly,
                &["status"],
                DEADLINE,
            )
            .await
            .unwrap();
        assert_eq!(out.stdout_lossy(), "clean");
        assert_eq!(process.seen(), [git_key("status")]);
    }

    /// Records the policy it was asked to confine under, and confines fully
    /// by prefixing the program.
    #[derive(Default)]
    struct RecordingSandbox(Mutex<Vec<SandboxPolicy>>);

    impl Sandbox for RecordingSandbox {
        fn confine(&self, spec: ProcessSpec, policy: &SandboxPolicy) -> Confined {
            self.0.lock().unwrap().push(policy.clone());
            let mut wrapped = ProcessSpec::new("boxed", spec.cwd.clone()).arg(spec.program);
            wrapped.args.extend(spec.args);
            wrapped.env = spec.env;
            Confined {
                spec: wrapped,
                mode: crate::interface::exec::SandboxMode::LinuxBwrap,
                enforcement: Enforcement::Full,
                unmet: Vec::new(),
            }
        }
    }

    #[tokio::test]
    async fn a_sandboxed_git_writes_only_what_it_names_and_reaches_no_network() {
        let sandbox = Arc::new(RecordingSandbox::default());
        let process = ScriptedProcess::new().with(
            format!("boxed {}", git_key("worktree prune")),
            ScriptedRun::ok(""),
        );
        let exec = ExecProviders {
            sandbox: sandbox.clone(),
            ..with_process(process.clone())
        };
        let engine = EngineExec::new(
            exec,
            Confinement::Sandboxed {
                policy: SandboxPolicy {
                    additional_writable: vec![PathBuf::from("/elsewhere")],
                    ..Default::default()
                },
                require_enforcement: true,
                tmpdir: Some(PathBuf::from("/tmp/session")),
            },
            ProjectTrust::Trusted,
        );
        let writable = [PathBuf::from("/repo/.git")];
        engine
            .git(
                Path::new("/repo"),
                GitAccess::Writes(&writable),
                &["worktree", "prune"],
                DEADLINE,
            )
            .await
            .unwrap();
        let policy = sandbox.0.lock().unwrap()[0].clone();
        assert!(policy.only_additional_writable);
        assert_eq!(policy.network_mode, NetworkMode::DenyAll);
        assert_eq!(
            policy.additional_writable,
            [PathBuf::from("/tmp/session"), PathBuf::from("/repo/.git")]
        );
    }

    #[tokio::test]
    async fn a_git_the_sandbox_cannot_hold_is_refused_only_when_one_is_required() {
        let process = ScriptedProcess::new().with(git_key("status"), ScriptedRun::ok(""));
        let exec = ExecProviders {
            sandbox: Arc::new(NoSandbox),
            ..with_process(process.clone())
        };
        let engine = |require_enforcement| {
            EngineExec::new(
                exec.clone(),
                Confinement::Sandboxed {
                    policy: SandboxPolicy::default(),
                    require_enforcement,
                    tmpdir: None,
                },
                ProjectTrust::Trusted,
            )
        };
        let refused = engine(true)
            .git(
                Path::new("/repo"),
                GitAccess::ReadOnly,
                &["status"],
                DEADLINE,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(refused, EngineExecError::Refused(_)),
            "{refused:?}"
        );
        assert!(process.seen().is_empty(), "git ran anyway");

        engine(false)
            .git(
                Path::new("/repo"),
                GitAccess::ReadOnly,
                &["status"],
                DEADLINE,
            )
            .await
            .unwrap();
        assert_eq!(process.seen(), [git_key("status")]);
    }

    #[tokio::test]
    async fn a_helper_is_not_given_the_engines_git_options() {
        let process = ScriptedProcess::new().with("pdftotext a.pdf -", ScriptedRun::ok("text"));
        let engine = EngineExec::new(
            with_process(process),
            Confinement::Unconfined,
            ProjectTrust::Trusted,
        );
        let out = engine
            .helper(
                ProcessSpec::new("pdftotext", "/").args(["a.pdf", "-"]),
                &[],
                DEADLINE,
            )
            .await
            .unwrap();
        assert_eq!(out.stdout_lossy(), "text");
    }
}
