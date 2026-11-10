//! `Sandbox` — how a process is constrained.
//!
//! The split this contract exists to keep: a backend decides *how* to
//! constrain, the kernel decides *what* to constrain and *whether to at all*.
//! The policy below is the kernel's; an implementation may only report how
//! much of it it managed to deliver.
//!
//! **The hard invariant: a policy that asked for constraint must never
//! silently become an unconstrained run.** A backend that cannot deliver says
//! so through [`Enforcement`], and the caller — with
//! `sandbox.require_enforcement` — decides whether to refuse. A `warn!` nobody
//! reads is not a decision.

use std::path::{Path, PathBuf};

use super::process::ProcessSpec;

/// What to constrain. The kernel's, not a backend's.
///
/// Defaults bake in a deny-read list (`~/.ssh`, `~/.aws`, …) so that a naive
/// Bash command does not dump credential files into the model's transcript by
/// accident. Not every field lands cleanly on every backend; the ones that do
/// not are reported through [`Enforcement`] rather than ignored.
#[derive(Debug, Clone, Default)]
pub struct SandboxPolicy {
    /// Absolute paths the sandbox is allowed to **read**, on top of the
    /// universal default (everything readable). When non-empty, paths in
    /// `deny_read` matching these get re-allowed (most-specific wins).
    pub allow_read: Vec<PathBuf>,
    /// Absolute paths the sandbox is **denied** read access to. Defaults
    /// include common credential stores (see [`default_deny_read`]).
    pub deny_read: Vec<PathBuf>,
    /// Network policy — see [`NetworkMode`].
    pub network_mode: NetworkMode,
    /// Domains allowed to make outbound connections when `network_mode` is
    /// [`NetworkMode::Allowlist`]. Ignored otherwise. What each backend makes
    /// of it is under [`NetworkMode::Allowlist`].
    pub allowed_domains: Vec<String>,
    /// Scene ids whose `~/.agix/scenes/<scene>/settings.json` the backend
    /// should protect from writes.
    ///
    /// Empty means the backend falls back to the builtin scenes it knows
    /// about by name, which is all it can do — this crate cannot resolve a
    /// daemon's actual `--scenes` set. A deployment running a scene outside
    /// that list gets no protection for that scene's settings unless it
    /// populates this.
    pub known_scenes: Vec<String>,
    /// Paths outside `cwd` this command may still write to.
    ///
    /// Part of the policy rather than a separate argument because it answers
    /// the same question the rest of it does — what may be written — and a
    /// backend that received it separately could be handed a policy that
    /// looks complete and is not.
    pub additional_writable: Vec<PathBuf>,
    /// Nothing is writable but `additional_writable` and `/dev`: not `cwd`,
    /// not the state root's working directories, not the per-user temporary
    /// directory. For the engine's own git, which reads the project and
    /// writes only where its bookkeeping lives.
    pub only_additional_writable: bool,
    /// This instance's global state root. `None` falls back to `$HOME/.agix`,
    /// which is where an instance usually lives but not where it must: a
    /// redirected instance whose sandbox protected `$HOME/.agix` would be
    /// guarding a file it does not use while leaving its real settings.json
    /// writable.
    pub state_root: Option<PathBuf>,
    /// Also refuse the ways a command can have another process act for it
    /// outside the sandbox: on macOS sending Apple events, opening apps and
    /// URLs through LaunchServices, and writing preferences through
    /// `cfprefsd`; on Linux the host's IPC namespace and the session bus.
    /// For an unattended session, where nobody approved the command.
    pub deny_ipc_escapes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// Unrestricted outbound (default; Bash often needs `curl` / `npm install`).
    #[default]
    Unrestricted,
    /// No outbound network access at all (DNS resolution via the OS
    /// resolver still works on macOS — it's IPC to `mDNSResponder`, not a
    /// raw socket the sandbox profile's `network*` filter covers — but no
    /// TCP/UDP connection actually reaching the network is permitted).
    DenyAll,
    /// Outbound restricted to `SandboxPolicy::allowed_domains`. Neither
    /// backend can filter by domain: macOS lets through localhost when the
    /// list names it and nothing else, Linux cuts the network entirely, and
    /// both report the domains they could not allow as unmet. DNS resolution
    /// still works on macOS (same rationale as `DenyAll`).
    Allowlist,
}

/// Default deny-read paths — credential stores that almost never want to be
/// inside an LLM tool result. User can override via `sandbox.allow_read` in
/// settings.json. Returned absolute (`HOME` resolved).
///
/// 两个生产调用点：
/// - `bash::to_sandbox_policy` —— 空 `deny_read` 回落到这里，作为真正的沙盒 profile；
/// - `bash::classify` —— 命令分类时用同一份名单判断某个参数是不是在读凭据，
///   避免 `cat ~/.ssh/id_rsa` 被当成"只读命令"静默放行。
///   两处共用一份定义，是为了不让"沙盒挡得住"和"分类器放得过"两套标准打架。
pub fn default_deny_read() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| default_deny_read_in(Path::new(&home)))
        .unwrap_or_default()
}

/// [`default_deny_read`] for the home directory `home`.
pub fn default_deny_read_in(home: &Path) -> Vec<PathBuf> {
    crate::paths::HOME_CREDENTIAL_STORES
        .iter()
        .map(|p| home.join(p))
        .collect()
}

/// Which backend ran. Advisory — for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Explicitly turned off by configuration.
    Disabled,
    /// No backend for this platform, or its tool is missing.
    Unavailable,
    /// macOS `sandbox-exec`.
    MacOSSandboxExec,
    /// Linux `bubblewrap`.
    LinuxBwrap,
}

/// How much of the policy is actually in force.
///
/// This is the field a caller acts on, as against [`SandboxMode`], which only
/// names the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// Everything the policy asked for.
    Full,
    /// Some of it. The backend has capabilities the policy needs and lacks
    /// others — neither backend has a domain-level network filter, so an
    /// allowlist of domains becomes "no network" (localhost aside, on macOS),
    /// which is stricter in one direction and not what was asked for in the
    /// other. What is missing is listed in [`Confined::unmet`].
    Partial,
    /// None of it. The command below runs unconstrained.
    None,
}

/// A command, after the sandbox has had its say.
#[derive(Debug, Clone)]
pub struct Confined {
    /// What to actually run — usually the original program wrapped in the
    /// backend's own.
    pub spec: ProcessSpec,
    pub mode: SandboxMode,
    pub enforcement: Enforcement,
    /// The parts of the policy this backend could not deliver, one per line,
    /// in words a person can act on. Empty when `enforcement` is `Full`.
    pub unmet: Vec<String>,
}

/// How a process is constrained.
///
/// A pure transformation, which is what makes it testable: it turns the
/// command someone meant to run into the command that will actually run, and
/// reports honestly how much of the policy survived. It does not execute
/// anything — that is [`Process`](super::Process), and keeping them apart is
/// what lets a caller inspect the confinement before committing to it.
pub trait Sandbox: Send + Sync {
    fn confine(&self, spec: ProcessSpec, policy: &SandboxPolicy) -> Confined;
}
