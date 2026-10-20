//! The execution layer — how a tool touches the machine.
//!
//! Four contracts, designed as one because they are entangled: a sandbox
//! constrains a process, a process needs files and a network, and the network
//! policy has to reach inside the sandbox. `docs/ARCHITECTURE.md` §5 is
//! the blueprint; this module is its vocabulary.
//!
//! # What is deliberately not here
//!
//! An execution layer is the easiest place in an engine for a second kernel to
//! grow, so the boundary is written down rather than assumed. Retries,
//! timeouts, caching and metrics belong to [`tool_middleware`]; whether a
//! provider is healthy belongs to [`health`]; kept time belongs to
//! [`environment`]; path safety is a policy and stays with `permissions`; and
//! the sandbox *policy* — what to constrain and whether to constrain at all —
//! is the kernel's. A provider decides *how* to constrain, never *whether*.
//!
//! [`tool_middleware`]: crate::interface::tool_middleware
//! [`health`]: crate::interface::health
//! [`environment`]: crate::interface::environment

pub mod filesystem;
pub mod in_process;
pub mod local;
pub mod network;
pub mod process;
pub mod sandbox;

pub use filesystem::{DirEntry, FileSystem, Metadata};
pub use network::{HttpRequest, HttpResponse, HttpStream, Network, Origin, DEFAULT_MAX_REDIRECTS};
pub use process::{
    ExitStatus, OutputChunk, OutputStream, Process, ProcessHandle, ProcessSpec,
    OUTPUT_GRACE_AFTER_EXIT,
};
pub use sandbox::{
    default_deny_read, Confined, Enforcement, NetworkMode, Sandbox, SandboxMode, SandboxPolicy,
};

/// The four providers a tool reaches the machine through.
///
/// One field on a [`ToolContext`](crate::interface::tool::ToolContext) rather
/// than four, because they are swapped as a set: a deployment executing
/// elsewhere that kept this machine's filesystem would be running commands
/// against files that are not there.
#[derive(Clone)]
pub struct ExecProviders {
    pub process: std::sync::Arc<dyn Process>,
    pub filesystem: std::sync::Arc<dyn FileSystem>,
    pub network: std::sync::Arc<dyn Network>,
    pub sandbox: std::sync::Arc<dyn Sandbox>,
}

impl ExecProviders {
    /// This machine.
    pub fn local() -> Self {
        Self::local_with(local::SubprocessEnv::default())
    }

    /// This machine, with child processes started from `env`.
    pub fn local_with(env: local::SubprocessEnv) -> Self {
        Self {
            process: std::sync::Arc::new(local::LocalProcess::new(env)),
            filesystem: std::sync::Arc::new(local::LocalFileSystem),
            network: std::sync::Arc::new(local::LocalNetwork::default()),
            sandbox: std::sync::Arc::new(local::PlatformSandbox),
        }
    }

    /// Nothing on the machine: a memory tree, commands decided in advance, no
    /// network, and a sandbox honest enough to say it constrains nothing.
    ///
    /// The second provider. Switching is this call.
    pub fn in_process() -> Self {
        Self {
            process: std::sync::Arc::new(in_process::ScriptedProcess::new()),
            filesystem: std::sync::Arc::new(in_process::InMemoryFileSystem::new()),
            network: std::sync::Arc::new(in_process::OfflineNetwork::new()),
            sandbox: std::sync::Arc::new(in_process::NoSandbox),
        }
    }

    /// Replace the network without touching the rest — the egress policy is
    /// per-deployment while the other three are per-machine.
    pub fn with_network(mut self, n: std::sync::Arc<dyn Network>) -> Self {
        self.network = n;
        self
    }
}

impl Default for ExecProviders {
    fn default() -> Self {
        Self::local()
    }
}

impl std::fmt::Debug for ExecProviders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExecProviders")
    }
}

/// Why an execution-layer call did not do what was asked.
///
/// Three variants because a tool has three different things to say about
/// them, and collapsing any two would make one of those sentences
/// unavailable. "The execution environment is unreachable" is the host's
/// problem; "the policy refused" is something the model should work around;
/// "the file does not exist" is the model's to interpret.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ExecError {
    /// The provider itself is not usable — a remote that cannot be reached, a
    /// backend that was never mounted.
    ///
    /// A provider that reports this must never have fallen back to executing
    /// locally instead. A deployment configured to run elsewhere, quietly
    /// running here when the elsewhere is down, has an isolation promise that
    /// fails exactly when it is needed.
    #[error("execution environment unavailable: {0}")]
    Unavailable(String),

    /// A policy refused: the sandbox, the network egress rules, a path
    /// outside what may be written.
    #[error("refused by policy: {0}")]
    Denied(String),

    /// The target's own failure — no such file, a non-zero exit, an HTTP 500.
    #[error("{0}")]
    Failed(String),
}

impl ExecError {
    pub fn unavailable(m: impl std::fmt::Display) -> Self {
        Self::Unavailable(m.to_string())
    }
    pub fn denied(m: impl std::fmt::Display) -> Self {
        Self::Denied(m.to_string())
    }
    pub fn failed(m: impl std::fmt::Display) -> Self {
        Self::Failed(m.to_string())
    }
}

/// The three sentences of [`ExecError`], said in the vocabulary a tool result
/// speaks.
///
/// Collapsing them would cost the host the one case it can act on: a
/// `Denied` is the model's to work around, a `Failed` is the target's own
/// words, and an `Unavailable` is neither — it keeps its own prefix so that a
/// deployment whose execution environment is down reads as a broken
/// environment rather than as a tool the model used wrongly.
impl From<ExecError> for crate::error::ToolError {
    fn from(e: ExecError) -> Self {
        match e {
            ExecError::Unavailable(_) => Self::exec(e.to_string()),
            ExecError::Denied(m) => Self::Denied(m),
            ExecError::Failed(m) => Self::exec(m),
        }
    }
}
