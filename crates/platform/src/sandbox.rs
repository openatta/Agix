//! The platform sandbox backends, macOS `sandbox-exec` and Linux `bubblewrap`: each
//! confines a command for a [`SandboxPolicy`] and reports what it delivered as `Enforcement`.
//! The policy follows the call's `ExecProfile` (`tools::bash::sandbox`): Session is the
//! session's; Unattended adds `deny_ipc_escapes`; Strict (`SandboxPolicy::strict`) cuts the
//! network, writes only `TMPDIR`, reads only its roots (`ReadScope::Only`), env `Minimal`.
//!
//! - **macOS**: `sandbox-exec -p <profile> <program> <args…>`. The profile is
//!   TinyScheme denying writes outside the allowed subtrees; exec is not
//!   restricted; under `ReadScope::Only` reads are denied but under each root.
//!   Network defaults to open; `DenyAll` / `Allowlist` both use
//!   `(deny network*)`. A profile names a remote host only as `*` or
//!   `localhost`, so the allowlist re-allows localhost when it names it and
//!   no domain at all; the domains it names are reported unmet through
//!   `Enforcement`, as on Linux. DNS goes through `mDNSResponder` IPC, which
//!   `network*` does not cover, so name resolution keeps working under both
//!   restricted modes.
//! - **Linux**: `bwrap --ro-bind / / --bind <writable> <writable> … <program>`
//!   (`ReadScope::Only`: a read-only bind per root). No bwrap: no constraint. `DenyAll` uses
//!   `--unshare-net`; bwrap has no domain-level filter, so `Allowlist`
//!   degrades to the same whole-network cut rather than silently opening up —
//!   failure goes toward the safer side, and says so through `Enforcement`.
//!   Each command gets a PID namespace of its own, so the pids it sees (`$!`
//!   among them) are not the host's, and a later command does not see what an
//!   earlier one left running. What it left running outlives it, as on macOS.
//! - **Windows**: no backend.
//!
//! # What a confined command may write
//!
//! The working directory, the policy's `additional_writable` (the caller puts
//! the session's own `TMPDIR` there), `/dev`, and on macOS the user's
//! `DARWIN_USER_TEMP_DIR`, which `mktemp(1)` uses whatever `TMPDIR` says. On
//! Linux `/tmp` is a private tmpfs that goes away with the command. Every path
//! is resolved through its symlinks before a rule is written for it.
//!
//! Inside those, what the engine or git later acts on outside the sandbox is
//! read-only: the project's `.git`, `.agix` and `.claude` entries themselves,
//! `.git/{config,config.worktree,commondir}` and `.git/hooks/`, the same in
//! the git directory of every submodule under `.git/modules/` (nested ones
//! included, and the directories leading down to them), the
//! configuration under `.agix/` and `.claude/` (`base::paths::ENGINE_CONFIG_*`:
//! settings, MCP, hooks, agents, skills, plugins, rules, scenes, policy,
//! scripts, output styles), the project's skills (`.agents/skills/`,
//! `base::paths::PROJECT_ENGINE_CONFIG_DIRS`, and the `.agents` entry above
//! it), and the `.git` links of the engine's worktrees.
//! Of the state root only `scratchpad/`, `memory/` and those of
//! `additional_writable` under `tmp/` (a session's own `TMPDIR`) stay
//! writable. A project that lives in the state root lifts that fence; the
//! state root is then guarded as the `.agix` it is — its configuration files
//! and directories are read-only, but for one the project itself is in —
//! and the settings and definitions of the root and every scene stay
//! read-only either way. On Linux `.agix`, and `.claude` where it exists, is read-only as
//! a whole and its existing working entries are opened again, so a new entry
//! directly in it cannot be created either; `.agents/skills/` is read-only
//! where `.agents` exists.
//!
//! A policy with `only_additional_writable` leaves out the working directory,
//! the state root's working entries and `DARWIN_USER_TEMP_DIR`: only
//! `additional_writable` and `/dev` are writable, the read-only entries above
//! still apply inside them. The engine's own git runs under such a policy in
//! every session whose commands are sandboxed (`base::engine_exec`), so a
//! filter driver or other program a repository's configuration names runs
//! confined, writing at most the git directory and `.agix/worktrees/` when it
//! creates a worktree, and only the session's `TMPDIR` otherwise.
//!
//! # What a confined command may not read
//!
//! The policy's `deny_read` (the credential stores in the user's home, by
//! default) less its `allow_read`, and whatever the policy says, the
//! credentials of `base::paths` (`CREDENTIALS`, placed on disk by
//! `SensitivePaths::credential_locations`): the state root's settings files
//! and MCP OAuth tokens, its own and every scene's, the daemons' socket
//! tokens under `daemon/auth/`, and the project's `.agix/settings.local.json`
//! and `.agix/mcp.local.json`. macOS writes a pattern for each; Linux covers
//! the state root and the project's `.agix` with the mounts of
//! `mask::mask_plan`, which a credential written later does not climb out
//! of, except where escape path 6 says.
//!
//! A policy whose `read` is `ReadScope::Only` narrows that further
//! (`read_scope`).
//!
//! # What a confined command may not connect to
//!
//! The daemon it runs under: a command connected to the daemon's socket would
//! be its host, free to add an MCP server that runs outside the sandbox, trust
//! its own project or answer its own permission prompts. So under every
//! policy, network open or not, a command cannot connect to a unix socket in
//! the state root (unless the project lives there) or to one this process
//! serves ([`fence_socket`], wherever `--socket` put it). macOS refuses the
//! connect, by resolved path; Linux mounts `/dev/null` over the socket — over
//! each one in the state root and its scene directories, not the directories
//! themselves, which stay readable. macOS also refuses this process's TCP
//! and WebSocket ports ([`fence_port`]); on Linux only `--unshare-net` cuts
//! them off. The daemon refuses its own descendants on the unix socket as
//! well, which is what holds where no backend runs.
//!
//! **Behind the `sandbox` feature: off here, on in assembly and the daemon.** What it
//! constrains is the commands the *model* runs, which is defence in depth on a
//! machine whose owner is sitting at it and a boundary only where the agent
//! acts for someone else. Without the feature the backends are not compiled,
//! [`PlatformSandbox`] says so, and `sandbox.require_enforcement` refuses every
//! command rather than running it unconstrained — which is the same promise the
//! enforcement report always made, kept one step earlier.
//!
//! # Known escape paths
//!
//! This is a lightweight write restriction, not a security boundary. Known
//! ways out:
//!
//! 1. **Writable `/dev`**: the macOS profile allows `file-write*` to `/dev`,
//!    so a process can `mknod` a raw disk device and bypass file ACLs.
//! 2. **Inherited environment**: `LD_PRELOAD` / `DYLD_INSERT_LIBRARIES` are
//!    not cleared, so a dynamic library can be injected.
//! 3. **Absent git files on Linux, for a git outside the sandbox**: a mount
//!    cannot stand in for a file that does not exist without leaving an empty
//!    one behind, which git refuses to read. So a project with no `.git` yet
//!    can be given one, and a repository with no `commondir` can be given one,
//!    with whatever configuration and filter drivers the command likes. The
//!    engine's own git runs inside the sandbox, so it carries none of that out
//!    unconfined; a git the user runs in the project afterwards does. macOS
//!    denies both.
//! 4. **Another process acting for the command**: the sandbox follows the
//!    command's own process tree, not what it asks a system service to do.
//!    On macOS, measured against `sandbox-exec`: `open` has LaunchServices
//!    start an app outside the sandbox, `defaults write` has `cfprefsd` write
//!    any preference domain, and an Apple event reaches an app that runs
//!    unconfined (Finder itself refuses events from a sandboxed sender, other
//!    apps need not). launchd refuses `launchctl submit` / `bootstrap` from a
//!    sandboxed caller. A policy with `deny_ipc_escapes` — every unattended
//!    session's — denies `lsopen`, `user-preference-write` and
//!    `appleevent-send`, the keychain through `com.apple.SecurityServer`
//!    (what `security find-generic-password` asks), and the sockets of the
//!    user's tmux and screen sessions. On Linux the same policy adds
//!    `--unshare-ipc` and hides the session bus and the systemd user
//!    manager's socket under `$XDG_RUNTIME_DIR` (and the bus path
//!    `DBUS_SESSION_BUS_ADDRESS` names), so `systemd-run --user` has nothing
//!    to talk to, and screen's socket directory (tmux's is in `/tmp`, private
//!    already); an abstract-socket bus is reachable unless the network is cut
//!    too. A multiplexer on a socket of its own choosing (`tmux -S`) is not
//!    found. With the network open, any local
//!    service listening on a socket the command can reach (sshd, a container
//!    daemon) is a way out under every policy.
//! 5. **A nested repository planted in the project**: the guard protects the
//!    project's own `.git`, but a command can `git init` a repository in a
//!    subdirectory, write its `.git/config` (`core.fsmonitor` and the like)
//!    and `git add` it into the project's index as a gitlink. The user's own
//!    `git` in the project afterwards, outside the sandbox, enters it and runs
//!    what its configuration names. Neither backend stops this: refusing to
//!    create a `.git` entry would break `git init`, `git clone` and every
//!    package manager that checks out a git dependency, and a repository built
//!    elsewhere and moved in would evade it anyway. The engine's own `git`
//!    carries `-c core.fsmonitor=false` and is unaffected. Above the backends,
//!    the runtime looks for one after every command not held to the strict
//!    profile (`tools::nested_repos`; a background command when its task
//!    ends): an undeclared gitlink whose configuration names a program has its
//!    `.git` moved aside unless the person attending keeps it. What it does
//!    not catch is what a command starts that outlives it and plants one
//!    later.
//! 6. **A credential written later directly into a writable directory, on
//!    Linux**: a `/dev/null` bind is pinned to the file there at wrap time, so
//!    a key file the engine creates afterwards, or rename-writes over the old
//!    one, is readable by what a command left running. A tmpfs over the
//!    directory would close this, and swallow every new entry the project
//!    makes in it. That is the state root's own key files, and `daemon/auth`
//!    while it does not exist, when the project is the state root; a scene's
//!    key files, and its `mcp/oauth` while it does not exist, when the
//!    project is that scene directory; and the key files of a project whose
//!    `.agix` is a symbolic link. What lies outside the project, the
//!    configuration directories a command cannot write anyway and a real
//!    `.agix` directory are masked whole. macOS matches its denials by path
//!    and has no such gap.
//!
//! Not among them, measured on bubblewrap 0.8: `/proc` is mounted afresh for
//! the command's PID namespace, so it shows neither the host's processes nor
//! their `fd` and `root`; a command inherits no descriptor of the daemon's but
//! its stdio, the daemon opening every one close-on-exec; and it has no
//! controlling terminal to push input into through `/dev/tty`, since
//! `LocalProcess` starts it in a session of its own.

pub use base::interface::exec::{
    default_deny_read, default_deny_read_in, Confined, Enforcement, EnvScope, NetworkMode,
    ProcessSpec, ReadScope, Sandbox, SandboxMode, SandboxPolicy,
};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

#[cfg(feature = "sandbox")]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod mask;
#[cfg(all(feature = "sandbox", target_os = "macos"))]
mod patterns;
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
mod read_scope;

/// What this process serves its clients on, one entry per live [`Fence`].
static SERVED: RwLock<Vec<(u64, Served)>> = RwLock::new(Vec::new());

static NEXT_FENCE: AtomicU64 = AtomicU64::new(0);

#[cfg_attr(not(feature = "sandbox"), allow(dead_code))]
enum Served {
    Socket(PathBuf),
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Port(SocketAddr),
    Secret(PathBuf),
}

/// One listener's or one file's place in what no confined command may reach,
/// until it is dropped. Held as long as what it fences: a port or a path
/// outlives the listener or file that had it, and whatever is there next is
/// not this process's to fence.
#[must_use = "the fence comes down when this is dropped"]
pub struct Fence(u64);

impl Drop for Fence {
    fn drop(&mut self) {
        let mut served = SERVED.write().unwrap_or_else(|e| e.into_inner());
        served.retain(|(id, _)| *id != self.0);
    }
}

fn fence(entry: Served) -> Fence {
    let id = NEXT_FENCE.fetch_add(1, Ordering::Relaxed);
    let mut served = SERVED.write().unwrap_or_else(|e| e.into_inner());
    served.push((id, entry));
    Fence(id)
}

/// No command this process confines may connect to the unix socket at
/// `path` while the returned fence is held. For a daemon's own listener,
/// wherever `--socket` put it: a command connected to it would be its host.
pub fn fence_socket(path: &Path) -> Fence {
    fence(Served::Socket(
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
    ))
}

/// [`fence_socket`] for a TCP listener. Only the macOS backend can refuse a
/// port; bwrap can cut the network only as a whole.
pub fn fence_port(addr: SocketAddr) -> Fence {
    fence(Served::Port(addr))
}

/// No command this process confines may read the file at `path` while the
/// returned fence is held, whatever `sandbox.allow_read` says. For a
/// credential the daemon writes for its own clients, such as the unix
/// socket's token.
pub fn fence_secret(path: &Path) -> Fence {
    fence(Served::Secret(
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
    ))
}

#[cfg(feature = "sandbox")]
fn fenced<T: Ord>(pick: impl Fn(&Served) -> Option<T>) -> Vec<T> {
    let served = SERVED.read().unwrap_or_else(|e| e.into_inner());
    let mut found: Vec<T> = served.iter().filter_map(|(_, s)| pick(s)).collect();
    found.sort();
    found.dedup();
    found
}

/// The fenced secrets that are still there.
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
fn fenced_secrets() -> Vec<PathBuf> {
    fenced(|s| match s {
        Served::Secret(p) if p.exists() => Some(p.clone()),
        _ => None,
    })
}

/// The fenced sockets that are still there.
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
fn fenced_sockets() -> Vec<PathBuf> {
    fenced(|s| match s {
        Served::Socket(p) if is_socket(p) => Some(p.clone()),
        _ => None,
    })
}

#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn fenced_ports() -> Vec<SocketAddr> {
    fenced(|s| match s {
        Served::Port(addr) => Some(*addr),
        _ => None,
    })
}

#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    path.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_socket())
}

/// The state root to protect: the one the caller named, else the
/// conventional `$HOME/.agix`.
#[cfg(feature = "sandbox")]
fn state_root_of(policy: &SandboxPolicy) -> Option<PathBuf> {
    policy
        .state_root
        .clone()
        .or_else(|| home_dir().map(|h| h.join(".agix")))
}

#[cfg(feature = "sandbox")]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The project's `.agix`, as given and as the kernel sees it.
#[cfg(feature = "sandbox")]
fn project_agix_dirs(cwd: &Path) -> Vec<PathBuf> {
    let given = cwd.join(".agix");
    let real = resolve_path(&given);
    if real == given {
        vec![given]
    } else {
        vec![given, real]
    }
}

#[derive(Debug, Clone)]
pub struct SandboxOptions<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub additional_writable: &'a [PathBuf],
    pub disable: bool,
    /// Extended policy (deny-read, network mode, etc).
    /// Falls back to safe defaults via `SandboxPolicy::default()`.
    pub policy: SandboxPolicy,
}

/// Pick the platform backend and wrap the command.
///
/// Never fails: no available backend means no constraint, reported honestly
/// through [`Enforcement`] rather than by refusing here. Whether an
/// unconstrained run is acceptable is the caller's decision, not this
/// function's.
pub fn wrap(opts: SandboxOptions<'_>) -> Confined {
    let inner = bash_spec(opts.command);
    if opts.disable {
        return plain(inner, SandboxMode::Disabled);
    }
    let env = CommandEnv {
        set: &[],
        scope: opts.policy.env,
    };
    wrap_program(inner, opts.cwd, opts.additional_writable, &opts.policy, env)
}

/// The environment a confined program starts with, as far as the sandbox can
/// tell: what its spec sets, over what `scope` lets it inherit from the
/// daemon's.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(
    not(all(feature = "sandbox", any(target_os = "macos", target_os = "linux"))),
    allow(dead_code)
)]
struct CommandEnv<'a> {
    set: &'a [(String, String)],
    scope: EnvScope,
}

/// [`wrap`] for any program, not only a shell command.
#[cfg_attr(
    not(all(feature = "sandbox", any(target_os = "macos", target_os = "linux"))),
    allow(unused_variables)
)]
fn wrap_program(
    inner: ProcessSpec,
    cwd: &Path,
    additional_writable: &[PathBuf],
    policy: &SandboxPolicy,
    env: CommandEnv<'_>,
) -> Confined {
    #[cfg(not(feature = "sandbox"))]
    {
        unavailable(
            inner,
            "this build has no sandbox backend; rebuild with `--features sandbox`",
        )
    }

    #[cfg(feature = "sandbox")]
    {
        #[cfg(target_os = "macos")]
        {
            mac_wrap(inner, cwd, additional_writable, policy, env)
        }

        #[cfg(target_os = "linux")]
        {
            linux_wrap(inner, cwd, additional_writable, policy, env)
        }

        // Windows / other unsupported platforms
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            plain(inner, SandboxMode::Unavailable)
        }
    }
}

/// A command nothing wrapped, and the reason in words the operator can act on.
#[cfg(not(feature = "sandbox"))]
fn unavailable(inner: ProcessSpec, why: &str) -> Confined {
    Confined {
        spec: inner,
        unmet: vec![why.to_string()],
        mode: SandboxMode::Unavailable,
        enforcement: Enforcement::None,
    }
}

fn plain(inner: ProcessSpec, mode: SandboxMode) -> Confined {
    Confined {
        spec: inner,
        unmet: vec!["nothing is constrained".into()],
        mode,
        // Nothing wraps this command, so nothing constrains it — true whether
        // the user asked for no sandbox or the platform could not provide one.
        // Which of those it was is `mode`'s job to say.
        enforcement: Enforcement::None,
    }
}

#[cfg(feature = "sandbox")]
use base::paths::{
    ENGINE_CONFIG_DIRS, ENGINE_CONFIG_FILES, ENGINE_CONFIG_ROOTS, GIT_CONTROL_FILES, GIT_HOOKS_DIR,
    PROJECT_ENGINE_CONFIG_DIRS, PROJECT_WORKTREES_DIR, REPOSITORY_CONFIG_DIR,
    STATE_ROOT_SESSION_TMP, STATE_ROOT_WORKING_DIRS,
};

/// The paths a policy is written against, each with its symlinks resolved.
///
/// The kernel checks a write against the path it resolves to: on macOS a rule
/// for `/tmp/x` never matches, because the write lands on `/private/tmp/x`.
#[cfg(feature = "sandbox")]
struct Roots {
    cwd: PathBuf,
    only_additional: bool,
    writable: Vec<PathBuf>,
    state_root: Option<PathBuf>,
    paths: base::paths::SensitivePaths,
}

#[cfg(feature = "sandbox")]
impl Roots {
    fn resolve(cwd: &Path, additional: &[PathBuf], policy: &SandboxPolicy) -> Self {
        let cwd = resolve_path(cwd);
        let state_root = state_root_of(policy).map(|p| resolve_path(&p));
        Self {
            paths: base::paths::SensitivePaths::new(state_root.as_deref(), Some(&cwd)),
            cwd,
            only_additional: policy.only_additional_writable,
            writable: additional.iter().map(|p| resolve_path(p)).collect(),
            state_root,
        }
    }

    /// Whether a write to `path` falls inside one of the writable roots.
    fn opens(&self, path: &Path) -> bool {
        (!self.only_additional && path.starts_with(&self.cwd))
            || self.writable.iter().any(|root| path.starts_with(root))
    }

    /// Where the state root is not fenced off, the files and the
    /// directories of its configuration a command could otherwise write:
    /// it is an `.agix` like a project's. One the working directory is in
    /// is left out, or the project could not be written at all.
    fn unfenced_state_root_config(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let Some(root) = self
            .state_root
            .as_deref()
            .filter(|_| self.fenced_state_root().is_none())
        else {
            return Default::default();
        };
        let guarded = |names: &[&str]| -> Vec<PathBuf> {
            names
                .iter()
                .map(|name| root.join(name))
                .filter(|p| self.opens(p) && !self.cwd.starts_with(p))
                .collect()
        };
        (guarded(ENGINE_CONFIG_FILES), guarded(ENGINE_CONFIG_DIRS))
    }

    /// The state root to fence off, its working directories left open —
    /// unless the project lives inside it, in which case fencing the root
    /// would fence the project too, or nothing but the named roots is
    /// writable, in which case it is shut already.
    /// The writable roots that are a session's own directory under the
    /// state root's `tmp/`: the fence around the state root would otherwise
    /// shut them along with the rest of it.
    fn session_temp_dirs_in<'a>(&'a self, root: &Path) -> impl Iterator<Item = &'a PathBuf> {
        let tmp = root.join(STATE_ROOT_SESSION_TMP);
        self.writable
            .iter()
            .filter(move |p| p.starts_with(&tmp) && **p != tmp)
    }

    fn fenced_state_root(&self) -> Option<&Path> {
        self.state_root
            .as_deref()
            .filter(|root| !self.only_additional && !self.project_in(root))
    }

    /// Whether the working directory is a project inside `state_root`. A
    /// session without a project works in a directory of its own in the
    /// state root's scratchpad, which the fence leaves open anyway, so that
    /// one is no reason to drop the fence.
    fn project_in(&self, state_root: &Path) -> bool {
        self.cwd.starts_with(state_root)
            && !STATE_ROOT_WORKING_DIRS
                .iter()
                .any(|dir| self.cwd.starts_with(state_root.join(dir)))
    }
}

/// `path` with its existing prefix canonicalized and the rest appended as
/// given, so a path that does not exist yet still resolves through the
/// symlinks above it.
#[cfg(feature = "sandbox")]
fn resolve_path(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(real) = existing.canonicalize() {
            return missing.iter().rev().fold(real, |p, name| p.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The real directories directly under `dir`.
#[cfg(feature = "sandbox")]
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

/// How many directories deep below a git directory's `modules/` the search
/// for submodules goes, counting both the levels of a name with slashes in
/// it and the `modules/` of each nested submodule.
#[cfg(feature = "sandbox")]
const SUBMODULE_SEARCH_DEPTH: usize = 12;

/// The most submodule git directories one project gets rules for.
#[cfg(feature = "sandbox")]
const SUBMODULE_LIMIT: usize = 256;

/// The submodules of the repository whose git directory is `git_dir`, nested
/// ones included.
#[cfg(feature = "sandbox")]
#[derive(Default)]
struct Submodules {
    /// Every directory from `git_dir/modules` down to each submodule's git
    /// directory, outermost first: what would let a command move a guarded
    /// file's directory aside and put another in its place.
    dirs: Vec<PathBuf>,
    git_dirs: Vec<PathBuf>,
}

/// A submodule's git directory sits at `modules/<name>`, `<name>` possibly
/// several directories deep, and has a `modules/` of its own. It is known by
/// its `HEAD` or its `config`: the `config` is what a command cannot remove,
/// so removing `HEAD` does not take a submodule off the list. Symbolic links
/// are not followed: the rules are written against the paths found, and a
/// link would lead them somewhere the repository chose.
#[cfg(feature = "sandbox")]
fn submodules_of(git_dir: &Path) -> Submodules {
    let mut found = Submodules::default();
    let mut pending = vec![(git_dir.join("modules"), 0)];
    while let Some((dir, depth)) = pending.pop() {
        if depth >= SUBMODULE_SEARCH_DEPTH || found.git_dirs.len() >= SUBMODULE_LIMIT {
            continue;
        }
        if !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            continue;
        }
        found.dirs.push(dir.clone());
        for child in subdirs(&dir) {
            let is_git_dir = ["HEAD", "config"]
                .iter()
                .any(|f| child.join(f).symlink_metadata().is_ok());
            if is_git_dir {
                found.dirs.push(child.clone());
                pending.push((child.join("modules"), depth + 1));
                found.git_dirs.push(child);
            } else {
                pending.push((child, depth + 1));
            }
        }
    }
    found
}

/// What in the project a sandboxed command must not write, because the
/// engine, or the user's own git, acts on it later outside the sandbox.
#[cfg(feature = "sandbox")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct ProjectGuard {
    /// `.git`, the directories down to each submodule's git directory and
    /// the configuration roots themselves: renaming one away, or replacing
    /// it with a symlink, would put every rule below on a path nobody reads.
    entries: Vec<PathBuf>,
    files: Vec<PathBuf>,
    trees: Vec<PathBuf>,
}

#[cfg(feature = "sandbox")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn project_guard(cwd: &Path) -> ProjectGuard {
    let git = cwd.join(REPOSITORY_CONFIG_DIR);
    let git_dir = resolve_path(&git);
    let agix_dir = resolve_path(&cwd.join(".agix"));

    let mut entries = vec![git.clone()];
    if git_dir != git {
        entries.push(git_dir.clone());
    }
    let submodules = submodules_of(&git_dir);
    entries.extend(submodules.dirs);
    let mut files = Vec::new();
    let mut trees = Vec::new();
    for repo in std::iter::once(&git_dir).chain(&submodules.git_dirs) {
        files.extend(GIT_CONTROL_FILES.iter().map(|f| repo.join(f)));
        trees.push(repo.join(GIT_HOOKS_DIR));
        for worktree in subdirs(&repo.join("worktrees")) {
            files.extend(GIT_CONTROL_FILES.iter().map(|f| worktree.join(f)));
        }
    }
    for name in ENGINE_CONFIG_ROOTS {
        let root = cwd.join(name);
        let real = resolve_path(&root);
        files.extend(ENGINE_CONFIG_FILES.iter().map(|f| real.join(f)));
        trees.extend(ENGINE_CONFIG_DIRS.iter().map(|d| real.join(d)));
        if real != root {
            entries.push(real);
        }
        entries.push(root);
    }
    for dir in PROJECT_ENGINE_CONFIG_DIRS {
        let mut above = cwd.to_path_buf();
        for part in Path::new(dir).components() {
            above.push(part);
            let real = resolve_path(&above);
            if real != above {
                entries.push(real);
            }
            entries.push(above.clone());
        }
        trees.push(resolve_path(&above));
    }

    for worktree in subdirs(&agix_dir.join(PROJECT_WORKTREES_DIR)) {
        files.push(worktree.join(REPOSITORY_CONFIG_DIR));
    }
    ProjectGuard {
        entries,
        files,
        trees,
    }
}

#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn mac_wrap(
    inner: ProcessSpec,
    cwd: &Path,
    additional_writable: &[PathBuf],
    policy: &SandboxPolicy,
    env: CommandEnv<'_>,
) -> Confined {
    let profile = build_macos_profile(
        cwd,
        additional_writable,
        policy,
        &read_scope::UserDirs::of(env),
    );
    let unmet = domains_sandbox_exec_cannot_allow(policy);
    Confined {
        enforcement: if unmet.is_empty() {
            Enforcement::Full
        } else {
            Enforcement::Partial
        },
        unmet,
        spec: ProcessSpec::new("sandbox-exec", &inner.cwd)
            .arg("-p")
            .arg(profile)
            .arg(inner.program)
            .args(inner.args),
        mode: SandboxMode::MacOSSandboxExec,
    }
}

#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn domains_sandbox_exec_cannot_allow(policy: &SandboxPolicy) -> Vec<String> {
    if policy.network_mode != NetworkMode::Allowlist {
        return Vec::new();
    }
    let unreachable: Vec<&str> = policy
        .allowed_domains
        .iter()
        .map(String::as_str)
        .filter(|d| !base::interface::exec::names_loopback(d))
        .collect();
    if unreachable.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "domain allowlist: sandbox-exec can let through only localhost, so {} stays unreachable",
        unreachable.join(", ")
    )]
}

/// This user's `DARWIN_USER_TEMP_DIR`, resolved.
#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn darwin_user_temp_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let len = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if len == 0 || len > buf.len() {
        return None;
    }
    buf.truncate(len - 1);
    Some(resolve_path(Path::new(std::ffi::OsStr::from_bytes(&buf))))
}

#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn build_macos_profile(
    cwd: &Path,
    additional: &[PathBuf],
    policy: &SandboxPolicy,
    dirs: &read_scope::UserDirs,
) -> String {
    let roots = Roots::resolve(cwd, additional, policy);
    let path = |p: &Path| sandbox_escape(&p.display().to_string());
    let mut s = String::with_capacity(4096);
    s.push_str("(version 1)\n");
    s.push_str("(allow default)\n");

    // sandbox-exec evaluates the profile top to bottom and the last matching
    // rule wins, so every deny below overrides the allows above it.
    s.push_str("(deny file-write*)\n");
    s.push_str("(allow file-write*\n");
    if !roots.only_additional {
        s.push_str(&format!("  (subpath \"{}\")\n", path(&roots.cwd)));
        // macOS mktemp(1) ignores TMPDIR and creates its file here; without
        // this every script that calls a bare `mktemp` fails.
        if let Some(tmp) = darwin_user_temp_dir() {
            s.push_str(&format!("  (subpath \"{}\")\n", path(&tmp)));
        }
    }
    s.push_str("  (subpath \"/dev\")\n");
    for p in &roots.writable {
        s.push_str(&format!("  (subpath \"{}\")\n", path(p)));
    }
    s.push_str(")\n");

    if let Some(root) = roots.fenced_state_root() {
        s.push_str(&format!(
            "(deny file-write* (subpath \"{}\"))\n",
            path(root)
        ));
        s.push_str("(allow file-write*");
        for dir in STATE_ROOT_WORKING_DIRS {
            s.push_str(&format!(" (subpath \"{}\")", path(&root.join(dir))));
        }
        for dir in roots.session_temp_dirs_in(root) {
            s.push_str(&format!(" (subpath \"{}\")", path(dir)));
        }
        s.push_str(")\n");
    }

    let guard = project_guard(&roots.cwd);
    for p in guard.entries.iter().chain(&guard.files) {
        s.push_str(&format!("(deny file-write* (literal \"{}\"))\n", path(p)));
    }
    for p in &guard.trees {
        s.push_str(&format!("(deny file-write* (subpath \"{}\"))\n", path(p)));
    }
    let (config_files, config_dirs) = roots.unfenced_state_root_config();
    for p in &config_files {
        s.push_str(&format!("(deny file-write* (literal \"{}\"))\n", path(p)));
    }
    for p in &config_dirs {
        s.push_str(&format!("(deny file-write* (subpath \"{}\"))\n", path(p)));
    }
    s.push_str(&patterns::scene_file_write_rules(&roots));

    s.push_str(&read_scope::macos_rules(&policy.read, dirs));
    if !policy.deny_read.is_empty() {
        for p in policy.deny_read.iter().flat_map(|p| spellings(p)) {
            s.push_str(&format!("(deny file-read* (subpath \"{}\"))\n", path(&p)));
        }
        for p in policy
            .allow_read
            .iter()
            .filter(|p| policy.read.names(p))
            .flat_map(|p| spellings(p))
        {
            s.push_str(&format!("(allow file-read* (subpath \"{}\"))\n", path(&p)));
        }
    }
    // After `allow_read`, which is for the user's own credential stores and
    // must not reopen the engine's.
    s.push_str(&patterns::credential_read_rules(&roots));

    // `launchctl submit` / `bootstrap` need no rule: launchd refuses a job
    // from a sandboxed caller.
    if policy.deny_ipc_escapes {
        s.push_str("(deny appleevent-send)\n");
        s.push_str("(deny lsopen)\n");
        s.push_str("(deny user-preference-write)\n");
        s.push_str("(deny mach-lookup (global-name \"com.apple.SecurityServer\"))\n");
        for p in multiplexer_paths() {
            s.push_str(&format!(
                "(deny file-read* file-write* (subpath \"{0}\"))\n\
                 (deny network-outbound (remote unix-socket (subpath \"{0}\")))\n",
                path(&p)
            ));
        }
    }

    // DNS is IPC to mDNSResponder, which `network*` does not cover, so name
    // resolution survives both restricted modes.
    match policy.network_mode {
        NetworkMode::Unrestricted => {}
        NetworkMode::DenyAll => {
            s.push_str("(deny network*)\n");
        }
        NetworkMode::Allowlist => {
            s.push_str("(deny network*)\n");
            // A remote address names its host only as `*` or `localhost`; a
            // profile with `(remote tcp "example.com:*")` does not compile,
            // and every command under it fails. The other domains are
            // reported unmet instead.
            if policy
                .allowed_domains
                .iter()
                .any(|d| base::interface::exec::names_loopback(d))
            {
                s.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
            }
        }
    }

    // Last, so that no allowlist entry above reopens them. The kernel matches
    // a unix-socket connect against the socket's resolved path, so a symlink
    // or a relative name meets the same rule.
    if let Some(root) = roots
        .state_root
        .as_deref()
        .filter(|root| !roots.project_in(root))
    {
        s.push_str(&format!(
            "(deny network-outbound (remote unix-socket (subpath \"{}\")))\n",
            path(root)
        ));
    }
    for socket in fenced_sockets() {
        s.push_str(&format!(
            "(deny network-outbound (remote unix-socket (literal \"{}\")))\n",
            path(&socket)
        ));
    }
    for secret in fenced_secrets() {
        s.push_str(&format!(
            "(deny file-read* (literal \"{}\"))\n",
            path(&secret)
        ));
    }
    for addr in fenced_ports() {
        let host = if addr.ip().is_loopback() {
            "localhost"
        } else {
            "*"
        };
        s.push_str(&format!(
            "(deny network-outbound (remote tcp \"{host}:{}\"))\n",
            addr.port()
        ));
    }

    s
}

/// macOS sandbox-exec 的 TinyScheme 字符串里 `\` 和 `"` 要转义。
///
/// 只有 macOS 的配置生成器调用它，但函数本身和平台无关——它的测试在哪个平台上
/// 都该跑，所以这里没有跟着调用方一起 `cfg` 掉，而是只在别的平台上豁免"未使用"。
#[cfg(feature = "sandbox")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn sandbox_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `path` as given and as the kernel will see it, once each.
#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn spellings(path: &Path) -> Vec<PathBuf> {
    let resolved = resolve_path(path);
    if resolved == path {
        vec![resolved]
    } else {
        vec![path.to_path_buf(), resolved]
    }
}

/// Where the terminal multiplexers this user runs listen. Typing into one of
/// their sessions runs a command in a shell outside the sandbox.
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
fn multiplexer_paths() -> Vec<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let tmux_dir = format!("tmux-{uid}");
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let mut paths: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![
            Path::new("/private/tmp").join(&tmux_dir),
            "/private/tmp/screens".into(),
        ]
    } else {
        vec!["/run/screen".into(), "/var/run/screen".into()]
    };
    paths.extend(var("TMUX_TMPDIR").map(|d| d.join(&tmux_dir)));
    paths.extend(var("SCREENDIR"));
    // `$TMUX` is `<socket>,<server pid>,<session>`.
    if let Ok(tmux) = std::env::var("TMUX") {
        paths.extend(
            tmux.split(',')
                .next()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
    }
    let mut paths: Vec<PathBuf> = paths.iter().map(|p| resolve_path(p)).collect();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn bind(args: &mut Vec<String>, how: &str, path: &Path) {
    let p = path.display().to_string();
    args.extend([how.to_string(), p.clone(), p]);
}

/// Mount `path` read-only. One that does not exist yet gets an empty
/// read-only tmpfs instead, which is what keeps it from being created; bwrap
/// leaves the empty mount point behind, so this happens only where the user
/// could have created it anyway.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn read_only(args: &mut Vec<String>, path: &Path) {
    if path.symlink_metadata().is_ok() {
        bind(args, "--ro-bind", path);
    } else if path.parent().is_some_and(writable_by_us) {
        let p = path.display().to_string();
        args.extend(["--tmpfs".into(), p.clone(), "--remount-ro".into(), p]);
    }
}

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn writable_by_us(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// The Linux shape of [`project_guard`].
///
/// bwrap has no deny rule, only mounts, and a read-only bind of a file that
/// does not exist yet is skipped rather than refused. So `.agix` — and
/// `.claude` where it exists — is mounted read-only as a whole and its
/// working entries are opened again one by one (a real `.agix` directory is
/// held by [`mask_project_agix`] instead, to the same effect);
/// a configuration file that is not there yet cannot be created because its
/// directory is read-only. `.git` cannot be treated that way — git creates
/// its lock files directly in it — so it is bound onto itself, which makes it
/// a mount point that cannot be renamed or removed, and its control files are
/// mounted read-only where they exist. Each submodule's git directory, and
/// every directory on the way down to it, is bound onto itself the same way,
/// so that none of them can be moved aside for one with other contents.
///
/// A `.git` or `.agix` no writable root reaches is read-only already, and is
/// left alone: binding it writable would open it, and a mount point for an
/// absent entry cannot be made on a read-only parent.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn guard_project(args: &mut Vec<String>, roots: &Roots, agix_masked: bool) {
    let git = roots.cwd.join(REPOSITORY_CONFIG_DIR);
    if git.is_dir() {
        let open = roots.opens(&git);
        let submodules = submodules_of(&git);
        if open {
            bind(args, "--bind", &git);
            for dir in &submodules.dirs {
                bind(args, "--bind", dir);
            }
        }
        for repo in std::iter::once(&git).chain(&submodules.git_dirs) {
            for f in GIT_CONTROL_FILES {
                bind(args, "--ro-bind-try", &repo.join(f));
            }
            for worktree in subdirs(&repo.join("worktrees")) {
                for f in GIT_CONTROL_FILES {
                    bind(args, "--ro-bind-try", &worktree.join(f));
                }
            }
            if open {
                read_only(args, &repo.join(GIT_HOOKS_DIR));
            } else {
                bind(args, "--ro-bind-try", &repo.join(GIT_HOOKS_DIR));
            }
        }
    } else if git.exists() {
        bind(args, "--ro-bind", &git);
    }

    for name in ENGINE_CONFIG_ROOTS {
        let root = roots.cwd.join(name);
        // Holding an absent root leaves an empty mount point behind in the
        // project, which for `.agix` is the engine's own and for `.claude`
        // would be litter in every project.
        let held = *name == ".agix" || root.symlink_metadata().is_ok();
        if !held || !roots.opens(&root) || (*name == ".agix" && agix_masked) {
            continue;
        }
        read_only(args, &root);
        for entry in working_entries(&root) {
            bind(args, "--bind", &entry);
        }
    }
    for dir in PROJECT_ENGINE_CONFIG_DIRS {
        let path = roots.cwd.join(dir);
        let above: Vec<PathBuf> = Path::new(dir)
            .ancestors()
            .skip(1)
            .filter(|a| !a.as_os_str().is_empty())
            .map(|a| roots.cwd.join(a))
            .collect();
        // Held only where the directory above it exists, as `.claude` is.
        let held = above
            .iter()
            .all(|a| a.symlink_metadata().is_ok_and(|m| m.is_dir()));
        if !held || !roots.opens(&path) {
            continue;
        }
        for a in above.iter().rev() {
            bind(args, "--bind", a);
        }
        read_only(args, &path);
    }
    let agix = roots.cwd.join(".agix");
    for worktree in subdirs(&agix.join(PROJECT_WORKTREES_DIR)) {
        bind(args, "--ro-bind-try", &worktree.join(REPOSITORY_CONFIG_DIR));
    }
}

/// The entries of a configuration root `root` that are not configuration
/// and not symbolic links: what a command may write in it.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn working_entries(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            let configures = entry.file_name().to_str().is_some_and(|n| {
                ENGINE_CONFIG_FILES.contains(&n) || ENGINE_CONFIG_DIRS.contains(&n)
            });
            !configures && entry.file_type().is_ok_and(|t| !t.is_symlink())
        })
        .map(|entry| entry.path())
        .collect()
}

/// The project's `.agix`, where it is a real directory in the sandbox,
/// masked around the project's key files as the state root is — a key file
/// the engine writes or rename-writes there later lands under a read-only
/// tmpfs — with the rest bound back: its working entries and the writable
/// roots in it writable where they are writable at all, everything else
/// read-only, and nothing new creatable directly in it, as
/// [`guard_project`] holds it otherwise. Whether it was masked.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn mask_project_agix(
    args: &mut Vec<String>,
    roots: &Roots,
    secrets: &[base::paths::Located],
    read: &read_scope::LinuxReadScope<'_>,
) -> bool {
    let agix = roots.cwd.join(".agix");
    if !read.shows(&agix) || !agix.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let working = if roots.opens(&agix) {
        working_entries(&agix)
    } else {
        Vec::new()
    };
    let holes: Vec<PathBuf> = working
        .into_iter()
        .chain(
            roots
                .writable
                .iter()
                .filter(|p| p.starts_with(&agix) && **p != agix)
                .cloned(),
        )
        .collect();
    let plan = mask::mask_plan(&agix, secrets, &holes);
    apply_mask(args, &plan, true);
    !plan.is_empty()
}

/// The state root inside the sandbox: masked around its credentials, its
/// working directories (or the project in it) writable again.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn expose_state_root(
    args: &mut Vec<String>,
    roots: &Roots,
    secrets: &[base::paths::Located],
    read: &read_scope::LinuxReadScope<'_>,
) {
    match (
        roots.fenced_state_root(),
        mask::holes_in_state_root(roots, secrets, |root| read.shows(root)),
    ) {
        (Some(root), _) if root.is_dir() => {
            expose_state_root_masked(args, root, secrets, &[]);
            for dir in STATE_ROOT_WORKING_DIRS {
                bind(args, "--bind-try", &root.join(dir));
            }
            for dir in roots.session_temp_dirs_in(root) {
                bind(args, "--bind-try", dir);
            }
        }
        // The project lives in the state root, or nothing but the named
        // roots is writable: the root is masked as above, the project and
        // the writable roots inside it bound back over it. A scene directory
        // opened as a project keeps its scene files read-only.
        (_, Some((root, holes))) => {
            expose_state_root_masked(args, root, secrets, &holes);
            for file in state_root_scene_files(roots) {
                if roots.opens(&file) && !roots.paths.is_credential(&file) {
                    bind(args, "--ro-bind-try", &file);
                }
            }
        }
        // The project is the state root itself, or the read scope leaves
        // the root out: a tmpfs over the root would mask the project's new
        // entries or put the root in the sandbox. Its configuration
        // directories, read-only anyway, are masked on their own; the key
        // files directly in it and `daemon/auth` are bound shut one by one,
        // and a key file the engine rename-writes there afterwards is
        // readable (the module header's escape path 6).
        _ => expose_unfenced_state_root(args, roots, secrets, read),
    }
}

/// The per-file shape of [`expose_state_root`], for a state root that is
/// the project or outside the read scope.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn expose_unfenced_state_root(
    args: &mut Vec<String>,
    roots: &Roots,
    secrets: &[base::paths::Located],
    read: &read_scope::LinuxReadScope<'_>,
) {
    let Some(state_root) = &roots.state_root else {
        return;
    };
    // A path the read scope leaves out is not in the sandbox at all:
    // binding it read-only would put it there.
    let (config_files, config_dirs) = roots.unfenced_state_root_config();
    for file in config_files.iter().filter(|p| read.shows(p)) {
        bind(args, "--ro-bind-try", file);
    }
    let mut masked = Vec::new();
    for dir in config_dirs.iter().filter(|p| read.shows(p)) {
        let plan = mask::mask_below(state_root, secrets, dir);
        if plan.is_empty() {
            read_only(args, dir);
        } else {
            apply_mask(args, &plan, true);
            masked.push(dir);
        }
    }
    let unmasked = |p: &Path| !masked.iter().any(|dir| p.starts_with(dir));
    for file in state_root_scene_files(roots) {
        if read.shows(&file) && unmasked(&file) {
            bind(args, "--ro-bind-try", &file);
        }
    }
    for p in secrets.iter().flat_map(|l| l.existing()) {
        if unmasked(&p) {
            hide(args, &p);
        }
    }
}

/// `base::paths::STATE_ROOT_SCENE_FILES` in the state root and in each of
/// its scene directories as they are now.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn state_root_scene_files(roots: &Roots) -> Vec<PathBuf> {
    roots
        .state_root
        .iter()
        .cloned()
        .chain(roots.paths.scene_dirs())
        .flat_map(|dir| {
            base::paths::STATE_ROOT_SCENE_FILES
                .iter()
                .map(move |file| dir.join(file))
        })
        .collect()
}

/// Expose the state root read-only while keeping its credentials masked
/// against writes that land after the sandbox is set up: the mounts of
/// [`mask::mask_plan`].
///
/// A `/dev/null` bind over a key file is pinned to that file's dentry. The
/// engine rewrites `settings.local.json` through a temporary file and a
/// rename, which replaces the dentry and drops the bind (measured on bwrap
/// 0.8), and a key file not there at wrap is never bound at all — so a
/// lingering process from an earlier command could read a credential written
/// afterwards. Under the plan's tmpfs a credential created or
/// rename-rewritten afterwards lands on no path the command can read.
///
/// The readable contents enumerated from the host are bound back as they are
/// at wrap time; a readable file created later does not appear, which for a
/// confined command reading the state root is acceptable where masking a late
/// credential is not.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn expose_state_root_masked(
    args: &mut Vec<String>,
    root: &Path,
    secrets: &[base::paths::Located],
    holes: &[PathBuf],
) {
    apply_mask(args, &mask::mask_plan(root, secrets, holes), false);
}

/// `plan` as bwrap arguments; `read_only` remounts each of its tmpfs
/// read-only once everything is bound onto it, so that nothing new can be
/// created directly in a masked directory.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn apply_mask(args: &mut Vec<String>, plan: &[mask::Mount], read_only: bool) {
    for mount in plan {
        match mount {
            mask::Mount::Tmpfs(at) => args.extend(["--tmpfs".into(), at.display().to_string()]),
            mask::Mount::RoBind(at) => bind(args, "--ro-bind-try", at),
            mask::Mount::Bind(at) => bind(args, "--bind-try", at),
            mask::Mount::Hide(at) => hide(args, at),
        }
    }
    if read_only {
        for mount in plan {
            if let mask::Mount::Tmpfs(at) = mount {
                args.extend(["--remount-ro".into(), at.display().to_string()]);
            }
        }
    }
}

/// Make `path` unreadable where it exists: an empty tmpfs over a directory,
/// `/dev/null` over anything else.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn hide(args: &mut Vec<String>, path: &Path) {
    if path.is_dir() {
        args.extend(["--tmpfs".into(), path.display().to_string()]);
    } else if path.exists() {
        bind_over(args, Path::new("/dev/null"), path);
    }
}

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn bind_over(args: &mut Vec<String>, source: &Path, target: &Path) {
    args.extend([
        "--ro-bind-try".to_string(),
        source.display().to_string(),
        target.display().to_string(),
    ]);
}

/// Where this user's session D-Bus and systemd user manager listen. An
/// abstract socket (`unix:abstract=`) has no path to hide; only
/// `--unshare-net` reaches it.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn session_bus_paths() -> Vec<PathBuf> {
    bus_paths(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
    )
}

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn bus_paths(runtime_dir: Option<PathBuf>, bus_address: Option<&str>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(runtime) = runtime_dir {
        paths.push(runtime.join("bus"));
        paths.push(runtime.join("systemd"));
    }
    if let Some(address) = bus_address {
        paths.extend(
            address
                .split(';')
                .flat_map(|a| a.split(','))
                .filter_map(|kv| kv.strip_prefix("unix:path=").or(kv.strip_prefix("path=")))
                .map(PathBuf::from),
        );
    }
    paths.sort();
    paths.dedup();
    paths
}

/// The unix sockets a command must not reach: those this process serves, and
/// any in the state root or one of its scene directories, where daemons put
/// theirs. bwrap cannot refuse a connect, only hide the file.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn daemon_sockets(state_root: Option<&Path>) -> Vec<PathBuf> {
    let mut sockets = fenced_sockets();
    if let Some(root) = state_root {
        let dirs = std::iter::once(root.to_path_buf()).chain(subdirs(&root.join("scenes")));
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            sockets.extend(entries.flatten().map(|e| e.path()).filter(|p| is_socket(p)));
        }
    }
    sockets.sort();
    sockets.dedup();
    sockets
}

/// Runs bwrap (`"$@"`) with the command's stdout and stderr on fds 3 and 4,
/// bwrap's own stdout on `/dev/null` and its stderr on an unlinked scratch
/// file printed to stderr once bwrap exits; stdin (fd 0) passes through to
/// bwrap and the command untouched. bwrap's PID 1 keeps fds 0–2 for as long
/// as anything the command left in the background runs, which would hold the
/// command's output open; it closes every other fd.
///
/// The scratch file is made in the host's `/tmp`, not the session `TMPDIR`
/// that `mktemp` would otherwise pick up from the environment: the session
/// `TMPDIR` is bound writable into the sandbox, so a process an earlier
/// command left running there could swap the file for a symlink between
/// `mktemp` and the `exec` that opens it, and this wrapper runs outside the
/// sandbox. The sandbox always mounts its own tmpfs over `/tmp`, so a file in
/// the host's `/tmp` is on no path a confined command can reach. `mktemp`,
/// `rm` and `cat` are addressed absolutely so a writable directory earlier on
/// `PATH` cannot stand one of them in.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn bwrap_stdio_outside() -> String {
    let mktemp = trusted_bin("mktemp");
    let rm = trusted_bin("rm");
    let cat = trusted_bin("cat");
    format!(
        "err=$({mktemp} -p /tmp 2>/dev/null) && exec 5>\"$err\" 6<\"$err\" && {rm} -f \"$err\" || exec 5>/dev/null 6</dev/null\n\
         \"$@\" 3>&1 4>&2 >/dev/null 2>&5 5>&- 6<&-\n\
         status=$?\n\
         {cat} <&6 >&2\n\
         exit \"$status\""
    )
}

/// The absolute path of a core utility `name`, looked up in the system
/// directories rather than `PATH` (which a command could have planted a
/// writable entry on). Falls back to the bare name where none is found, so a
/// deployment with its utilities elsewhere still runs, unhardened.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn trusted_bin(name: &str) -> String {
    for dir in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        let path = Path::new(dir).join(name);
        if path.exists() {
            return path.display().to_string();
        }
    }
    name.to_string()
}

/// The inside half of [`BWRAP_STDIO_OUTSIDE`]: fds 3 and 4 back to 1 and 2,
/// then the command.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
const BWRAP_STDIO_INSIDE: &str = r#"exec 1>&3 2>&4 3>&- 4>&- && exec "$@""#;

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn linux_wrap(
    inner: ProcessSpec,
    cwd: &Path,
    additional_writable: &[PathBuf],
    policy: &SandboxPolicy,
    env: CommandEnv<'_>,
) -> Confined {
    if !bwrap_available() {
        return plain(inner, SandboxMode::Unavailable);
    }
    let roots = Roots::resolve(cwd, additional_writable, policy);
    // bwrap applies mounts in argument order, so each one below lands on top
    // of the ones before it.
    let read = read_scope::LinuxReadScope::new(&policy.read, read_scope::UserDirs::of(env));
    let mut args: Vec<String> = Vec::new();
    read.base_mounts(&mut args);
    args.extend(
        [
            "--tmpfs",
            "/tmp",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            // No `--die-with-parent`: bwrap exits as soon as the command does,
            // and the namespace would go with it, taking whatever the command
            // left in the background. That stays in the command's process
            // group, which is how it gets stopped.
            //
            // No `--new-session` either: bwrap's PID 1 would take the whole
            // namespace out of that group, out of reach of stopping it. The
            // `TIOCSTI` it guards against needs a controlling terminal, and
            // `LocalProcess` starts every command in a session without one.
            "--unshare-pid",
        ]
        .map(String::from),
    );
    // After the `/tmp` tmpfs, or a project under `/tmp` is hidden by it.
    read.scope_mounts(&mut args);
    if !roots.only_additional {
        bind(&mut args, "--bind", &roots.cwd);
    }
    for p in &roots.writable {
        bind(&mut args, "--bind-try", p);
    }
    let (state_root_secrets, project_secrets): (Vec<_>, Vec<_>) = roots
        .paths
        .credential_locations(&project_agix_dirs(&roots.cwd))
        .into_iter()
        .partition(|located| roots.state_root.as_ref() == Some(&located.base));
    expose_state_root(&mut args, &roots, &state_root_secrets, &read);
    let agix_masked = mask_project_agix(&mut args, &roots, &project_secrets, &read);
    guard_project(&mut args, &roots, agix_masked);
    if !agix_masked {
        for p in project_secrets.iter().flat_map(|l| l.existing()) {
            hide(&mut args, &p);
        }
    }
    for socket in daemon_sockets(roots.state_root.as_deref()) {
        bind_over(&mut args, Path::new("/dev/null"), &socket);
    }
    for secret in fenced_secrets() {
        bind_over(&mut args, Path::new("/dev/null"), &secret);
    }

    let allow_set: std::collections::HashSet<PathBuf> = policy.allow_read.iter().cloned().collect();
    for p in &policy.deny_read {
        if !allow_set.contains(p) {
            hide(&mut args, p);
        }
    }

    if policy.deny_ipc_escapes {
        args.push("--unshare-ipc".into());
        // `/tmp` is the command's own tmpfs already.
        let multiplexers = multiplexer_paths()
            .into_iter()
            .filter(|p| !p.starts_with("/tmp"));
        for p in session_bus_paths().into_iter().chain(multiplexers) {
            hide(&mut args, &p);
        }
    }

    // bwrap's only network lever is `--unshare-net`. An allowlist it cannot
    // honour fails closed to no network rather than open to all of it.
    match policy.network_mode {
        NetworkMode::Unrestricted => {}
        NetworkMode::DenyAll => {
            args.push("--unshare-net".into());
        }
        NetworkMode::Allowlist => {
            tracing::warn!(
                domains = ?policy.allowed_domains,
                "sandbox: NetworkMode::Allowlist has no bwrap equivalent (no domain/IP \
                 filtering primitive) — falling back to full network isolation \
                 (--unshare-net) rather than allowing unrestricted access"
            );
            args.push("--unshare-net".into());
        }
    }

    args.push("--".into());

    // bwrap has no domain-level network filter, so an allowlist arrives here
    // as the same whole-network cut `DenyAll` gets. Stricter than asked for in
    // one direction and not what was asked for in the other, which is what
    // `Partial` is for.
    let unmet = if policy.network_mode == NetworkMode::Allowlist {
        vec!["domain allowlist: bwrap can only cut the network entirely".into()]
    } else {
        Vec::new()
    };
    args.extend(["sh", "-c", BWRAP_STDIO_INSIDE, "sh"].map(String::from));
    args.push(inner.program);
    args.extend(inner.args);
    let outside = [
        "-c".into(),
        bwrap_stdio_outside(),
        "sh".into(),
        "bwrap".into(),
    ];
    Confined {
        enforcement: if unmet.is_empty() {
            Enforcement::Full
        } else {
            Enforcement::Partial
        },
        unmet,
        spec: ProcessSpec::new("sh", &inner.cwd).args(outside.into_iter().chain(args)),
        mode: SandboxMode::LinuxBwrap,
    }
}

/// A shell command as a process spec. The one place `bash -c` is spelled, so
/// that every backend wraps the same thing.
fn bash_spec(command: &str) -> ProcessSpec {
    ProcessSpec::new("bash", std::path::Path::new("."))
        .args(["-c".to_string(), command.to_string()])
}

/// The platform backends behind the contract.
///
/// Holds nothing: which backend applies is a fact about the machine, decided
/// per call by `cfg`, and the policy arrives as an argument.
pub struct PlatformSandbox;

impl Sandbox for PlatformSandbox {
    fn confine(&self, spec: ProcessSpec, policy: &SandboxPolicy) -> Confined {
        let ProcessSpec {
            program,
            args,
            cwd,
            env,
            env_scope,
            stdin,
        } = spec;
        let inner = ProcessSpec::new(program, Path::new(".")).args(args);
        let env_scope = match policy.env {
            EnvScope::Minimal => EnvScope::Minimal,
            EnvScope::Session => env_scope,
        };
        let seen = CommandEnv {
            set: &env,
            scope: env_scope,
        };
        let mut confined = wrap_program(inner, &cwd, &policy.additional_writable, policy, seen);
        #[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
        if matches!(
            confined.mode,
            SandboxMode::MacOSSandboxExec | SandboxMode::LinuxBwrap
        ) {
            let unmet = read_scope::unmet(
                &policy.read,
                &read_scope::UserDirs::of(seen),
                read_scope::var(seen, "PATH").as_deref(),
            );
            if !unmet.is_empty() {
                confined.unmet.extend(unmet);
                confined.enforcement = Enforcement::Partial;
            }
        }
        confined.spec.cwd = cwd;
        confined.spec.env = env;
        confined.spec.env_scope = env_scope;
        confined.spec.stdin = stdin;
        confined
    }
}

/// 一行可读的沙盒可用性描述，给 `/doctor` 用。
/// macOS：恒"available: sandbox-exec"（系统自带）。
/// Linux：bwrap 在 PATH 时 available；否则报 unavailable + 提示装。
/// 其它平台：unavailable + 平台名。
pub fn sandbox_status() -> String {
    #[cfg(not(feature = "sandbox"))]
    {
        "unavailable: this build has no sandbox backend (`--features sandbox`)".to_string()
    }
    #[cfg(all(feature = "sandbox", target_os = "macos"))]
    {
        "available: sandbox-exec".to_string()
    }
    #[cfg(all(feature = "sandbox", target_os = "linux"))]
    {
        if bwrap_available() {
            "available: bwrap".to_string()
        } else {
            "unavailable: bwrap not in PATH (install bubblewrap)".to_string()
        }
    }
    #[cfg(all(
        feature = "sandbox",
        not(any(target_os = "macos", target_os = "linux"))
    ))]
    {
        format!("unavailable on {}", std::env::consts::OS)
    }
}

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn bwrap_available() -> bool {
    // 一次性探测；用 sync std::process::Command 走 PATH lookup
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// What a build without the backend promises.
///
/// The gated module below tests the backends; this tests their absence, which
/// is the state most builds are in. A build that quietly reported `Full` here
/// would make `require_enforcement` a lie in exactly the configuration where
/// somebody is relying on it.
#[cfg(all(test, not(feature = "sandbox")))]
mod without_the_feature {
    use super::*;

    fn opts(command: &'static str) -> SandboxOptions<'static> {
        SandboxOptions {
            command,
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            policy: SandboxPolicy::default(),
            disable: false,
        }
    }

    #[test]
    fn a_build_without_the_backend_says_so_and_claims_nothing() {
        let c = wrap(opts("echo hi"));
        assert_eq!(c.mode, SandboxMode::Unavailable);
        assert_eq!(c.enforcement, Enforcement::None);
        assert!(
            c.unmet.iter().any(|u| u.contains("--features sandbox")),
            "the shortfall must name what is missing, not just that something is: {:?}",
            c.unmet
        );
    }

    #[test]
    fn the_command_still_runs_unchanged() {
        let c = wrap(opts("echo hi"));
        assert_eq!(c.spec.program, "bash");
        assert_eq!(c.spec.args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    #[test]
    fn status_names_the_feature_rather_than_the_platform() {
        assert!(
            sandbox_status().contains("--features sandbox"),
            "{}",
            sandbox_status()
        );
    }
}

#[cfg(all(test, feature = "sandbox"))]
mod tests {
    use super::*;

    #[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
    const INHERITED: CommandEnv<'static> = CommandEnv {
        set: &[],
        scope: EnvScope::Session,
    };

    fn opts<'a>(cmd: &'a str, cwd: &'a Path) -> SandboxOptions<'a> {
        SandboxOptions {
            command: cmd,
            cwd,
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        }
    }

    /// The command reaches the shell once, on whichever backend runs.
    ///
    /// It reached it twice on Linux for a while: generalizing the backends to
    /// take a process spec added a generic tail without removing the hardcoded
    /// one, so the argv ended `-- bash -c CMD bash -c CMD`. That runs, and
    /// looks right, and quietly sets `$0`/`$1`/`$2` to `bash`, `-c` and the
    /// command itself — so anything reading a positional parameter is wrong.
    /// Asserted as a count rather than a position because a second copy is
    /// exactly what a position-based assertion cannot see.
    #[test]
    fn the_command_reaches_the_shell_exactly_once() {
        let c = wrap(opts("echo hi", Path::new("/tmp/work")));
        assert_eq!(
            c.spec
                .args
                .iter()
                .filter(|a| a.as_str() == "echo hi")
                .count(),
            1,
            "argv: {:?}",
            c.spec.args
        );
        assert_eq!(
            c.spec
                .args
                .windows(2)
                .filter(|w| w[0] == "bash" && w[1] == "-c")
                .count(),
            1,
            "argv: {:?}",
            c.spec.args
        );
    }

    #[test]
    fn disable_yields_plain_bash() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.disable = true;
        let cmd = wrap(o);
        assert_eq!(cmd.spec.program, "bash");
        assert_eq!(cmd.spec.args[0], "-c");
        assert_eq!(cmd.spec.args[1], "ls");
        assert_eq!(cmd.mode, SandboxMode::Disabled);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_wraps_with_sandbox_exec_and_profile_includes_cwd() {
        let cmd = wrap(opts("ls", Path::new("/tmp/work")));
        assert_eq!(cmd.spec.program, "sandbox-exec");
        assert_eq!(cmd.spec.args[0], "-p");
        assert!(cmd.spec.args[1].contains("(deny file-write*)"));
        assert!(cmd.spec.args[1].contains("(subpath \"/private/tmp/work\")"));
        assert!(!cmd.spec.args[1].contains("(subpath \"/private/tmp\")"));
        assert!(!cmd.spec.args[1].contains("(subpath \"/private/var/folders\")"));
        assert_eq!(cmd.spec.args[2], "bash");
        assert_eq!(cmd.spec.args[3], "-c");
        assert_eq!(cmd.spec.args[4], "ls");
        assert_eq!(cmd.mode, SandboxMode::MacOSSandboxExec);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_additional_writable_dirs_added_to_profile() {
        let extras = vec![
            PathBuf::from("/Users/me/scratch"),
            PathBuf::from("/Users/me/another"),
        ];
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.additional_writable = &extras;
        let cmd = wrap(o);
        assert!(cmd.spec.args[1].contains("(subpath \"/Users/me/scratch\")"));
        assert!(cmd.spec.args[1].contains("(subpath \"/Users/me/another\")"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_path_with_quotes_is_escaped() {
        let cmd = wrap(opts("ls", Path::new("/tmp/with\"quote")));
        // 反斜杠转义后 sandbox-exec 仍然能 parse
        assert!(cmd.spec.args[1].contains("with\\\"quote"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wrap_either_uses_bwrap_or_falls_back() {
        let cmd = wrap(opts("ls", Path::new("/tmp/work")));
        match cmd.mode {
            SandboxMode::LinuxBwrap => {
                assert_eq!(cmd.spec.program, "sh");
                assert_eq!(cmd.spec.args[3], "bwrap");
                assert!(cmd.spec.args.iter().any(|a| a == "--ro-bind"));
                assert!(cmd.spec.args.iter().any(|a| a == "/tmp/work"));
                let bash_pos = cmd
                    .spec
                    .args
                    .iter()
                    .position(|a| a == "bash")
                    .expect("bwrap args must end with `-- bash -c <cmd>`");
                assert_eq!(cmd.spec.args[bash_pos + 1], "-c");
                assert_eq!(cmd.spec.args[bash_pos + 2], "ls");
            }
            SandboxMode::Unavailable => {
                // bwrap 没装 —— 平台合理 fallback
                assert_eq!(cmd.spec.program, "bash");
                assert_eq!(cmd.spec.args, vec!["-c", "ls"]);
            }
            other => panic!("unexpected mode on linux: {other:?}"),
        }
    }

    /// bwrap's own messages — a mount it could not make, namespaces it may
    /// not create — still reach the command's stderr, after what the command
    /// itself printed, and its exit status is the command's.
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn what_bwrap_itself_says_reaches_stderr() {
        let fake_bwrap = "echo printed >&3; echo complained >&4; echo 'bwrap: refused' >&2; exit 1";
        let out = std::process::Command::new("sh")
            .args(["-c", &bwrap_stdio_outside(), "sh", "sh", "-c", fake_bwrap])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "printed\n");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "complained\nbwrap: refused\n"
        );
        assert_eq!(out.status.code(), Some(1));
    }

    /// The outer wrapper, which runs outside the sandbox, keeps its scratch
    /// file off every writable root. It used to let `mktemp` land the file in
    /// the session `TMPDIR` — a writable root a confined process shares — where
    /// a process an earlier command left running could swap it for a symlink;
    /// the wrapper then wrote through it outside the sandbox. It now pins the
    /// file to the host's `/tmp`, which the sandbox always shadows with a
    /// private tmpfs, so no confined command reaches it. The helpers are
    /// addressed absolutely so `PATH` cannot redirect them.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_outer_wrapper_keeps_its_scratch_file_off_every_writable_root() {
        let project = Path::new("/tmp/work/project");
        let session_tmp = PathBuf::from("/home/u/.agix/tmp/session-1");
        let mut o = opts("ls", project);
        let extras = [session_tmp.clone()];
        o.additional_writable = &extras;
        let cmd = wrap(o);
        if cmd.mode != SandboxMode::LinuxBwrap {
            return;
        }
        // The outer shell script is the one the wrapping `sh -c` runs.
        assert_eq!(cmd.spec.args[0], "-c");
        let script = &cmd.spec.args[1];
        assert_eq!(cmd.spec.args[2], "sh");
        assert_eq!(cmd.spec.args[3], "bwrap");

        assert!(
            script.contains("-p /tmp"),
            "the scratch file must be pinned to the host /tmp: {script}"
        );
        for root in [
            project.display().to_string(),
            session_tmp.display().to_string(),
        ] {
            assert!(
                !script.contains(&root),
                "the wrapper names a writable root ({root}): {script}"
            );
        }
        for util in ["mktemp", "rm", "cat"] {
            let absolute = trusted_bin(util);
            if absolute.starts_with('/') {
                assert!(
                    script.contains(&format!("{absolute} "))
                        || script.contains(&format!("{absolute}\n")),
                    "{util} is not addressed by its absolute path {absolute}: {script}"
                );
            }
        }
    }

    /// A configuration file that does not exist yet is what a read-only bind
    /// of the file itself cannot cover: bwrap skips that bind, and the file
    /// can then be created. The directory around it is what is read-only.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_absent_project_settings_file_cannot_be_created() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".agix").join("scratchpad")).unwrap();
        let cmd = wrap(opts(
            "echo '{}' > .agix/settings.local.json",
            project.path(),
        ));
        if cmd.mode != SandboxMode::LinuxBwrap {
            return;
        }
        let agix = project
            .path()
            .canonicalize()
            .unwrap()
            .join(".agix")
            .display()
            .to_string();
        let end = cmd.spec.args.iter().position(|a| a == "--").unwrap();
        let args = &cmd.spec.args[..end];
        let at = |pair: [&str; 2]| args.windows(2).position(|w| w == pair);
        let scratchpad = format!("{agix}/scratchpad");
        let masked = at(["--tmpfs", &agix]).unwrap_or_else(|| panic!("{args:?}"));
        let opened = at(["--bind-try", &scratchpad]).unwrap_or_else(|| panic!("{args:?}"));
        let shut = at(["--remount-ro", &agix]).unwrap_or_else(|| panic!("{args:?}"));
        assert!(masked < opened && opened < shut, "{args:?}");
        assert!(
            !args.iter().any(|a| a.ends_with("settings.local.json")),
            "{args:?}"
        );

        let namespaces_work = std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "true"])
            .status()
            .is_ok_and(|s| s.success());
        if !namespaces_work {
            return;
        }
        let out = std::process::Command::new(&cmd.spec.program)
            .args(&cmd.spec.args)
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!project
            .path()
            .join(".agix")
            .join("settings.local.json")
            .exists());
    }

    /// With the project outside the state root, the state root and every scene
    /// directory are masked with a tmpfs and their readable entries bound back,
    /// leaving the key files under the tmpfs — which a credential written after
    /// wrap cannot climb out of, where a `/dev/null` bind over the file could.
    #[test]
    fn sandbox_escape_handles_quotes_and_backslash() {
        assert_eq!(sandbox_escape("/tmp/normal"), "/tmp/normal");
        assert_eq!(sandbox_escape("/tmp/q\"quote"), "/tmp/q\\\"quote");
        assert_eq!(sandbox_escape("/tmp/back\\slash"), "/tmp/back\\\\slash");
    }

    #[test]
    fn default_deny_read_includes_credential_paths() {
        // Only meaningful when HOME is set; in CI it usually is. Skip if not.
        let Some(_home) = std::env::var_os("HOME") else {
            return;
        };
        let list = default_deny_read();
        let joined: String = list
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains(".ssh"));
        assert!(joined.contains(".aws"));
        assert!(joined.contains(".gnupg"));
        assert!(joined.contains(".kube"));
        assert!(joined.contains("gh"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_emits_deny_read_for_default_policy() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy = SandboxPolicy {
            allow_read: Vec::new(),
            deny_read: default_deny_read(),
            network_mode: NetworkMode::Unrestricted,
            allowed_domains: Vec::new(),
            state_root: None,
            additional_writable: Vec::new(),
            only_additional_writable: false,
            deny_ipc_escapes: false,
            ..Default::default()
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        // Should at least mention .ssh in the deny-read list
        assert!(profile.contains("(deny file-read*"));
        assert!(profile.contains(".ssh"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allow_read_overrides_deny_read() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy = SandboxPolicy {
            allow_read: vec![PathBuf::from("/tmp/some-secret")],
            deny_read: vec![PathBuf::from("/tmp/some-secret")],
            network_mode: NetworkMode::Unrestricted,
            allowed_domains: vec![],
            state_root: None,
            additional_writable: Vec::new(),
            only_additional_writable: false,
            deny_ipc_escapes: false,
            ..Default::default()
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        // Both rules emitted; allow comes after deny so it wins per
        // sandbox-exec evaluation order.
        let deny_idx = profile
            .find("(deny file-read* (subpath \"/tmp/some-secret\"))")
            .unwrap();
        let allow_idx = profile
            .find("(allow file-read* (subpath \"/tmp/some-secret\"))")
            .unwrap();
        assert!(allow_idx > deny_idx);
    }

    /// The profile without the rules that fence off the daemon itself, which
    /// every network mode has.
    #[cfg(target_os = "macos")]
    fn network_mode_rules(profile: &str) -> String {
        profile
            .lines()
            .filter(|l| !l.starts_with("(deny network-outbound"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unrestricted_network_emits_no_network_rules() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Unrestricted;
        let cmd = wrap(o);
        let profile = network_mode_rules(&cmd.spec.args[1]);
        assert!(
            !profile.contains("network"),
            "Unrestricted must not add any network rule, profile:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_deny_all_network_emits_deny_network_rule() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::DenyAll;
        let cmd = wrap(o);
        let profile = network_mode_rules(&cmd.spec.args[1]);
        assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
        assert!(
            !profile.contains("network-outbound"),
            "DenyAll must not also emit an allowlist rule, profile:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allowlist_domains_stay_out_of_the_profile_and_are_reported_unmet() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["api.example.com".into(), "evil\".com".into()];
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
        assert!(
            !network_mode_rules(profile).contains("network-outbound"),
            "profile:\n{profile}"
        );
        assert!(!profile.contains("example.com"), "profile:\n{profile}");
        assert!(!profile.contains("evil"), "profile:\n{profile}");
        assert_eq!(cmd.enforcement, Enforcement::Partial);
        let unmet = cmd.unmet.join("; ");
        assert!(
            unmet.contains("api.example.com") && unmet.contains("evil\".com"),
            "{unmet}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allowlist_naming_localhost_lets_the_loopback_through() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["127.0.0.1".into()];
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        let deny_idx = profile.find("(deny network*)").unwrap();
        let allow_idx = profile
            .find("(allow network-outbound (remote ip \"localhost:*\"))")
            .unwrap_or_else(|| panic!("profile:\n{profile}"));
        assert!(allow_idx > deny_idx);
        assert_eq!(cmd.enforcement, Enforcement::Full, "{:?}", cmd.unmet);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allowlist_with_no_domains_still_denies_network() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec![];
        let cmd = wrap(o);
        let profile = network_mode_rules(&cmd.spec.args[1]);
        assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
        assert!(!profile.contains("network-outbound"), "profile:\n{profile}");
        assert_eq!(cmd.enforcement, Enforcement::Full, "{:?}", cmd.unmet);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_network_rules_keep_profile_parens_balanced() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["a.example.com".into(), "b.example.com".into()];
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        assert_eq!(profile.matches('(').count(), profile.matches(')').count());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_deny_all_network_uses_unshare_net() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::DenyAll;
        let cmd = wrap(o);
        if cmd.mode != SandboxMode::LinuxBwrap {
            return; // bwrap not installed in this environment
        }
        assert!(cmd.spec.args.iter().any(|a| a == "--unshare-net"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_a_read_scope_binds_its_roots_one_by_one() {
        let project = PathBuf::from("/tmp/work");
        let policy = SandboxPolicy::default().strict([project.clone()], None);
        let cmd = wrap(SandboxOptions {
            command: "ls",
            cwd: &project,
            additional_writable: &[],
            disable: false,
            policy,
        });
        if cmd.mode != SandboxMode::LinuxBwrap {
            return;
        }
        let args = &cmd.spec.args;
        let at = |triple: [&str; 3]| args.windows(3).position(|w| w == triple);
        assert_eq!(at(["--ro-bind", "/", "/"]), None, "{args:?}");
        assert!(at(["--ro-bind-try", "/usr", "/usr"]).is_some(), "{args:?}");
        let tmpfs = args
            .windows(2)
            .position(|w| w == ["--tmpfs", "/tmp"])
            .unwrap();
        let bound = at(["--ro-bind-try", "/tmp/work", "/tmp/work"]).unwrap();
        assert!(
            tmpfs < bound,
            "the project is hidden under /tmp's tmpfs: {args:?}"
        );
        assert!(args.iter().any(|a| a == "--unshare-net"));
        assert!(!args
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == "/tmp/work"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_unattended_unshares_ipc() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        let attended = wrap(o.clone());
        if attended.mode != SandboxMode::LinuxBwrap {
            return;
        }
        o.policy.deny_ipc_escapes = true;
        let unattended = wrap(o);
        assert!(!attended.spec.args.iter().any(|a| a == "--unshare-ipc"));
        assert!(unattended.spec.args.iter().any(|a| a == "--unshare-ipc"));
    }

    /// `/dev/null` is mounted over every socket in the state root's scene
    /// directories and over every socket this process serves, after the state
    /// root's tmpfs mask and the re-exposure of its contents so it lands on
    /// top: a socket re-bound as a readable sibling must still end up hidden.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_hides_the_daemons_sockets() {
        let state = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let scene = state.path().join("scenes").join("code");
        std::fs::create_dir_all(&scene).unwrap();
        let in_state = scene.join("daemon.sock");
        let served = elsewhere.path().join("served.sock");
        let _listeners =
            [&in_state, &served].map(|p| std::os::unix::net::UnixListener::bind(p).unwrap());
        let _fence = fence_socket(&served);

        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.state_root = Some(state.path().to_path_buf());
        let args = wrap(o).spec.args;
        if !args.iter().any(|a| a == "--") {
            return;
        }
        let state_mask = args
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && Path::new(&w[1]) == resolve_path(state.path()))
            .expect("the state root is masked with a tmpfs");
        for socket in [&in_state, &served] {
            let socket = resolve_path(socket).display().to_string();
            let hidden = args
                .windows(3)
                .position(|w| w[0] == "--ro-bind-try" && w[1] == "/dev/null" && w[2] == socket)
                .unwrap_or_else(|| panic!("{socket} is not hidden: {args:?}"));
            assert!(
                hidden > state_mask,
                "{socket} is hidden before the state root's mask lands"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_session_bus_is_found_where_the_environment_says() {
        assert_eq!(
            bus_paths(
                Some(PathBuf::from("/run/user/1000")),
                Some("unix:path=/run/user/1000/bus,guid=x;unix:guid=y,path=/tmp/dbus-z"),
            ),
            [
                PathBuf::from("/run/user/1000/bus"),
                PathBuf::from("/run/user/1000/systemd"),
                PathBuf::from("/tmp/dbus-z"),
            ]
        );
        assert!(bus_paths(None, Some("unix:abstract=/tmp/dbus-x")).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_unrestricted_network_omits_unshare_net() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Unrestricted;
        let cmd = wrap(o);
        if cmd.mode != SandboxMode::LinuxBwrap {
            return;
        }
        assert!(!cmd.spec.args.iter().any(|a| a == "--unshare-net"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_allowlist_network_falls_back_to_unshare_net() {
        // bwrap has no domain-level filter — Allowlist must fail closed
        // (full isolation), not silently degrade to Unrestricted.
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["api.example.com".into()];
        let cmd = wrap(o);
        if cmd.mode != SandboxMode::LinuxBwrap {
            return;
        }
        assert!(cmd.spec.args.iter().any(|a| a == "--unshare-net"));
    }

    #[test]
    fn sandbox_escape_handles_special_whitespace() {
        // sandbox_escape only escapes `\` and `"`. Newlines/tabs pass through
        // unmodified — the test just verifies no panic or corruption.
        let with_nl = sandbox_escape("/path/with\nnewline");
        assert!(with_nl.contains('\n'), "newline passes through unescaped");
        assert!(with_nl.contains("newline"));

        let with_tab = sandbox_escape("/path/with\t tab");
        assert!(with_tab.contains('\t'), "tab passes through unescaped");

        // Double-quote still gets escaped even when surrounded by whitespace
        assert_eq!(sandbox_escape("\" start"), "\\\" start");
    }

    #[test]
    fn sandbox_escape_handles_unicode_and_long_paths() {
        // Unicode and deeply nested paths that could trigger buffer issues.
        let unicode = "/tmp/日本語/パス";
        let escaped = sandbox_escape(unicode);
        assert_eq!(escaped, unicode); // no escaping needed
        let long = "/".repeat(1000) + "a";
        let escaped = sandbox_escape(&long);
        assert!(escaped.len() > 500);
        assert_eq!(escaped, long);
    }

    #[test]
    fn wrap_never_panics_on_edge_case_inputs() {
        // wrap() must always return a valid SandboxedCommand regardless
        // of unusual inputs — the contract is "never fails, never panics".
        let empty_opts = SandboxOptions {
            command: "",
            cwd: Path::new(""),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        };
        let cmd = wrap(empty_opts);
        assert!(!cmd.spec.program.is_empty());
        assert!(!cmd.spec.args.is_empty());

        // Unicode command with no cwd
        let unicode_opts = SandboxOptions {
            command: "echo 🦀",
            cwd: Path::new("/"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        };
        let cmd2 = wrap(unicode_opts);
        assert!(cmd2.spec.args.iter().any(|a| a.contains("🦀")));

        // Explicit disable must always yield plain bash
        let disabled_opts = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp"),
            additional_writable: &[],
            disable: true,
            policy: SandboxPolicy::default(),
        };
        let cmd3 = wrap(disabled_opts);
        assert_eq!(cmd3.spec.program, "bash");
        assert_eq!(cmd3.mode, SandboxMode::Disabled);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_has_balanced_parentheses() {
        // Structural sanity check: the TinyScheme profile must have matching
        // open/close paren counts so sandbox-exec(1) can parse it.
        let o = SandboxOptions {
            command: "echo hi",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[PathBuf::from("/Users/me/scratch")],
            disable: false,
            policy: SandboxPolicy {
                allow_read: Vec::new(),
                deny_read: default_deny_read(),
                network_mode: NetworkMode::Unrestricted,
                allowed_domains: Vec::new(),
                state_root: None,
                additional_writable: Vec::new(),
                only_additional_writable: false,
                deny_ipc_escapes: false,
                ..Default::default()
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        let opens = profile.matches('(').count();
        let closes = profile.matches(')').count();
        assert_eq!(
            opens, closes,
            "macOS sandbox profile must have balanced parens"
        );
        // Must start with (version 1) per TinyScheme sandbox-exec convention
        assert!(
            profile.starts_with("(version 1)"),
            "profile must start with (version 1)"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_denies_settings_json_even_without_deny_read() {
        // Settings.json write denial must be emitted unconditionally,
        // regardless of whether credential deny-read paths are configured.
        let o = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy {
                allow_read: vec![],
                deny_read: vec![],
                network_mode: NetworkMode::Unrestricted,
                allowed_domains: vec![],
                state_root: None,
                additional_writable: Vec::new(),
                only_additional_writable: false,
                deny_ipc_escapes: false,
                ..Default::default()
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        // Must still deny writes to .agix/settings.json even when deny_read is empty
        assert!(
            profile
                .contains("(deny file-write* (literal \"/private/tmp/work/.agix/settings.json\"))"),
            "settings.json denial must appear unconditionally"
        );
    }

    /// The regular expressions of the profile's `(deny <operation> (regex …))`
    /// rules.
    #[cfg(target_os = "macos")]
    #[allow(clippy::unwrap_used)]
    fn denied_patterns(profile: &str, operation: &str) -> Vec<regex::Regex> {
        let prefix = format!("(deny {operation} (regex \"");
        profile
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix)?.strip_suffix("\"))"))
            .map(|escaped| {
                let mut pattern = String::new();
                let mut chars = escaped.chars();
                while let Some(c) = chars.next() {
                    pattern.push(if c == '\\' { chars.next().unwrap() } else { c });
                }
                regex::Regex::new(&pattern).unwrap()
            })
            .collect()
    }

    #[cfg(target_os = "macos")]
    fn denies(patterns: &[regex::Regex], path: &Path) -> bool {
        let path = path.display().to_string();
        patterns.iter().any(|p| p.is_match(&path))
    }

    /// A declarative scene is defined by the files in its directory, so every
    /// scene directory is protected — not just the names the engine knows,
    /// and one created after the profile was written as well — and its
    /// definition along with its settings.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_protects_the_scene_files_of_the_root_and_every_scene() {
        let policy = SandboxPolicy {
            state_root: Some(PathBuf::from("/srv/agix-state")),
            ..Default::default()
        };
        let profile = build_macos_profile(
            Path::new("/work/project"),
            &[],
            &policy,
            &read_scope::UserDirs::of(INHERITED),
        );
        let writes = denied_patterns(&profile, "file-write*");
        for p in [
            "/srv/agix-state/settings.json",
            "/srv/agix-state/scenes/code/settings.json",
            "/srv/agix-state/scenes/triage/scene.json",
            "/srv/agix-state/scenes/added-later/prompt.md",
        ] {
            assert!(denies(&writes, Path::new(p)), "{p}\n{profile}");
        }
        for p in [
            "/srv/agix-state/skills/x/settings.json",
            "/srv/agix-state/scenes/code/skills/x/prompt.md",
            "/srv/agix-state/memory/settings.json",
        ] {
            assert!(!denies(&writes, Path::new(p)), "{p}");
        }
    }

    /// A scene directory linked in from elsewhere is written through its
    /// real path, which the pattern over the state root does not match.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_protects_a_linked_scenes_files_under_their_real_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("state");
        let elsewhere = dir.path().canonicalize().unwrap().join("dotfiles/scene");
        std::fs::create_dir_all(root.join("scenes")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("scenes/linked")).unwrap();
        let policy = SandboxPolicy {
            state_root: Some(root.clone()),
            ..Default::default()
        };
        let profile = build_macos_profile(
            Path::new("/work/project"),
            &[],
            &policy,
            &read_scope::UserDirs::of(INHERITED),
        );
        let writes = denied_patterns(&profile, "file-write*");
        for name in ["settings.json", "scene.json", "prompt.md"] {
            let path = elsewhere.join(name);
            assert!(denies(&writes, &path), "{}\n{profile}", path.display());
        }
        assert!(!denies(&writes, &elsewhere.join("skills/x/prompt.md")));
        assert!(!denies(
            &writes,
            &elsewhere.parent().unwrap().join("settings.json")
        ));
    }

    /// Without a root it protects the conventional one, and with one only
    /// that: a redirected instance whose profile protected `$HOME/.agix`
    /// would be guarding a file nobody uses while leaving its real
    /// settings.json — where the permission rules live — writable.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_protects_the_instance_root_it_was_given() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home_settings = resolve_path(&PathBuf::from(home).join(".agix/settings.json"));
        let fallback = build_macos_profile(
            Path::new("/work/project"),
            &[],
            &Default::default(),
            &read_scope::UserDirs::of(INHERITED),
        );
        assert!(denies(
            &denied_patterns(&fallback, "file-write*"),
            &home_settings
        ));

        let policy = SandboxPolicy {
            state_root: Some(PathBuf::from("/srv/agix-state")),
            ..Default::default()
        };
        let profile = build_macos_profile(
            Path::new("/work/project"),
            &[],
            &policy,
            &read_scope::UserDirs::of(INHERITED),
        );
        let writes = denied_patterns(&profile, "file-write*");
        assert!(denies(&writes, Path::new("/srv/agix-state/settings.json")));
        assert!(
            !denies(&writes, &home_settings),
            "a redirected instance is still protecting the home-relative path"
        );
    }

    /// Every credential of `base::paths` but those `deny_read` covers has a
    /// read denial in the profile, a scene created after it was written
    /// included.
    #[cfg(target_os = "macos")]
    #[test]
    fn every_credential_is_unreadable_under_the_macos_profile() {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = state.path().canonicalize().unwrap();
        let cwd = project.path().canonicalize().unwrap();
        let policy = SandboxPolicy {
            state_root: Some(root.clone()),
            ..Default::default()
        };
        let profile = build_macos_profile(&cwd, &[], &policy, &read_scope::UserDirs::of(INHERITED));
        let reads = denied_patterns(&profile, "file-read*");
        let examples = mask::credential_examples(&root, &cwd.join(".agix"));
        assert!(!examples.is_empty());
        for (credential, path) in &examples {
            assert!(
                denies(&reads, path),
                "{credential:?}: {} is readable\n{profile}",
                path.display()
            );
        }
        for p in [
            "skills/x/SKILL.md",
            "scenes/code/scene.json",
            "mcp/servers.json",
        ] {
            assert!(!denies(&reads, &root.join(p)), "{p}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_empty_deny_read_denies_only_the_engines_own_secrets() {
        let o = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy {
                allow_read: vec![],
                deny_read: vec![],
                network_mode: NetworkMode::Unrestricted,
                allowed_domains: vec![],
                state_root: None,
                additional_writable: Vec::new(),
                only_additional_writable: false,
                deny_ipc_escapes: false,
                ..Default::default()
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        // Other cases in this process may hold a fenced secret meanwhile.
        let reads: Vec<&str> = profile
            .lines()
            .filter(|l| l.contains("file-read*") && !l.contains("(literal "))
            .collect();
        assert_eq!(reads.len(), 6, "{reads:?}");
        assert!(
            reads
                .iter()
                .all(|l| l.starts_with("(deny file-read* (regex \"^") && l.contains("/\\\\.agix/")),
            "{reads:?}"
        );
    }

    /// A project and state root reached through `/tmp`, which macOS resolves
    /// to `/private/tmp` — the alias a profile written against the given path
    /// would never match — and a temp dir standing in for the session's.
    #[cfg(target_os = "macos")]
    struct UnderSandboxExec {
        _dirs: Vec<tempfile::TempDir>,
        project: PathBuf,
        state_root: PathBuf,
        tmp: PathBuf,
        elsewhere: PathBuf,
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::unwrap_used)]
    impl UnderSandboxExec {
        fn new() -> Option<Self> {
            let works = std::process::Command::new("sandbox-exec")
                .args(["-p", "(version 1)(allow default)", "true"])
                .status()
                .is_ok_and(|s| s.success());
            if !works {
                return None;
            }
            let dirs: Vec<tempfile::TempDir> = (0..4)
                .map(|_| tempfile::Builder::new().tempdir_in("/tmp").unwrap())
                .collect();
            let [project, state_root, tmp, elsewhere] =
                [0, 1, 2, 3].map(|i| dirs[i].path().to_path_buf());
            assert!(project.starts_with("/tmp"));
            std::fs::create_dir_all(project.join(".agix")).unwrap();
            std::fs::create_dir_all(project.join(".claude")).unwrap();
            for dir in STATE_ROOT_WORKING_DIRS {
                std::fs::create_dir_all(state_root.join(dir)).unwrap();
            }
            std::fs::write(project.join("a"), "a\n").unwrap();
            let git = |args: &[&str]| {
                let out = std::process::Command::new("git")
                    .args(args)
                    .current_dir(&project)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "git {args:?}: {out:?}");
            };
            git(&["init", "-q"]);
            git(&["config", "user.email", "t@example.com"]);
            git(&["config", "user.name", "T"]);
            Some(Self {
                _dirs: dirs,
                project,
                state_root,
                tmp,
                elsewhere,
            })
        }

        fn runs(&self, command: &str) -> bool {
            self.run(command, false)
        }

        fn runs_unattended(&self, command: &str) -> bool {
            self.run(command, true)
        }

        fn runs_allowing(&self, domains: &[&str], command: &str) -> bool {
            self.run_under(
                command,
                SandboxPolicy {
                    network_mode: NetworkMode::Allowlist,
                    allowed_domains: domains.iter().map(|d| d.to_string()).collect(),
                    ..Default::default()
                },
            )
        }

        fn run(&self, command: &str, deny_ipc_escapes: bool) -> bool {
            self.run_under(
                command,
                SandboxPolicy {
                    deny_ipc_escapes,
                    ..Default::default()
                },
            )
        }

        fn run_under(&self, command: &str, policy: SandboxPolicy) -> bool {
            let additional = vec![self.tmp.clone()];
            let confined = wrap(SandboxOptions {
                command,
                cwd: &self.project,
                additional_writable: &additional,
                disable: false,
                policy: SandboxPolicy {
                    state_root: Some(self.state_root.clone()),
                    ..policy
                },
            });
            std::process::Command::new(&confined.spec.program)
                .args(&confined.spec.args)
                .current_dir(&self.project)
                .env("TMPDIR", &self.tmp)
                .output()
                .unwrap()
                .status
                .success()
        }
    }

    #[cfg(target_os = "macos")]
    fn outside(program: &str, args: &[&str]) -> bool {
        std::process::Command::new(program)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Unattended, a command cannot have another process do outside the
    /// sandbox what the sandbox refuses it: launchd, LaunchServices, an
    /// Apple event, `cfprefsd`.
    #[cfg(target_os = "macos")]
    #[test]
    fn unattended_a_command_cannot_act_through_another_process() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let elsewhere = s.elsewhere.display().to_string();
        let label = format!("com.agix.sandbox-test.{}", std::process::id());

        for runs in [UnderSandboxExec::runs, UnderSandboxExec::runs_unattended] {
            let submitted = runs(
                &s,
                &format!("launchctl submit -l {label} -- /usr/bin/touch '{elsewhere}/launchd'"),
            );
            outside("launchctl", &["remove", &label]);
            assert!(!submitted, "launchd took a job from a sandboxed command");
        }

        let wrote = s.runs_unattended(&format!("defaults write {label} k v"));
        let stored = outside("defaults", &["read", &label, "k"]);
        outside("defaults", &["delete", &label]);
        assert!(!wrote && !stored, "cfprefsd wrote a preference for it");

        let applet = s.elsewhere.join("Applet.app");
        let applet_built = outside(
            "osacompile",
            &[
                "-o",
                &applet.display().to_string(),
                "-e",
                &format!("do shell script \"touch '{elsewhere}/opened'\""),
            ],
        );
        if applet_built {
            let opened = s.runs_unattended(&format!("open -g '{}'", applet.display()));
            if opened {
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            assert!(!opened, "LaunchServices opened an app for it");
        }

        assert!(!s.runs_unattended("osascript -e 'tell application \"Finder\" to count windows'"));

        for marker in ["launchd", "opened"] {
            assert!(!s.elsewhere.join(marker).exists(), "{marker}");
        }
    }

    /// Those refusals leave ordinary work alone, reading preferences included.
    #[cfg(target_os = "macos")]
    #[test]
    fn unattended_ordinary_work_still_runs() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        for command in [
            "git add a && git commit -qm first",
            "echo x > notes.md && mktemp && echo x > \"$TMPDIR/f\"",
            "defaults read -g > /dev/null",
        ] {
            assert!(s.runs_unattended(command), "`{command}` was refused");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_an_unattended_profile_refuses_the_ipc_routes() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        let attended = wrap(o.clone()).spec.args[1].clone();
        o.policy.deny_ipc_escapes = true;
        let unattended = wrap(o).spec.args[1].clone();
        for rule in [
            "(deny appleevent-send)",
            "(deny lsopen)",
            "(deny user-preference-write)",
        ] {
            assert!(!attended.contains(rule), "{rule}");
            assert!(unattended.contains(rule), "{rule}");
        }
    }

    /// What a sandboxed command could leave for git or the engine to act on
    /// outside the sandbox is refused, against the real `sandbox-exec`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandboxed_command_cannot_plant_what_runs_outside_the_sandbox() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let state = s.state_root.display().to_string();
        let elsewhere = s.elsewhere.display().to_string();
        for command in [
            "git config x.y z".to_string(),
            "echo /tmp > .git/commondir".to_string(),
            "echo 'touch pwned' > .git/hooks/post-commit".to_string(),
            "mv .agix .agix.old".to_string(),
            "echo '{}' > .agix/mcp.local.json".to_string(),
            "echo '{}' > .agix/settings.json".to_string(),
            "mkdir .agix/mcp".to_string(),
            "echo '{}' > .claude/settings.json".to_string(),
            "mkdir .claude/hooks".to_string(),
            "mv .claude .claude.old".to_string(),
            format!("echo '{{}}' > '{state}/mcp.json'"),
            format!("mkdir '{state}/hooks'"),
            format!("echo x > '{elsewhere}/x'"),
        ] {
            assert!(!s.runs(&command), "`{command}` was allowed");
        }
        s.runs(&format!("rm -rf .agix; ln -s '{elsewhere}' .agix"));
        let agix = s.project.join(".agix").symlink_metadata().unwrap();
        assert!(agix.is_dir(), ".agix was replaced");

        let config = std::fs::read_to_string(s.project.join(".git").join("config")).unwrap();
        assert!(!config.contains("[x]"), "{config}");
        assert!(!s.project.join(".git").join("commondir").exists());
        assert!(!s.project.join(".agix").join("settings.json").exists());
        assert!(!s.state_root.join("mcp.json").exists());
    }

    /// The filesystem here compares names without case, so a protected file
    /// spelled in another case is the same file. Seatbelt matches the path as
    /// the filesystem names it, not as the command spelled it, which is why
    /// the profile's rules can be written in one case.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_protected_path_spelled_in_another_case_is_still_protected() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        std::fs::write(s.state_root.join("settings.json"), "agix-test-secret").unwrap();
        std::fs::write(s.elsewhere.join(".netrc"), "agix-test-secret").unwrap();
        let state = s.state_root.display().to_string();
        let elsewhere = s.elsewhere.display().to_string();
        std::fs::write(s.state_root.join("notes.txt"), "fine").unwrap();
        if !s.state_root.join("SETTINGS.JSON").exists() {
            return;
        }
        assert!(s.runs(&format!(
            "cat A '{}/NOTES.TXT' && echo b > A && echo x > .GIT/description",
            s.state_root.display()
        )));
        let denied_netrc = SandboxPolicy {
            deny_read: vec![s.elsewhere.join(".netrc")],
            ..Default::default()
        };
        for policy in [SandboxPolicy::default(), denied_netrc.clone()] {
            for command in [
                format!("cat '{state}/SETTINGS.JSON'"),
                format!("cat '{state}/Settings.json'"),
            ] {
                assert!(
                    !s.run_under(&command, policy.clone()),
                    "`{command}` read it"
                );
            }
        }
        assert!(!s.run_under(&format!("cat '{elsewhere}/.NETRC'"), denied_netrc));
        for command in [
            "printf '[x]\\n\\ty = z\\n' >> .GIT/config",
            "printf '[x]\\n\\ty = z\\n' >> .git/CONFIG",
            "echo 'touch pwned' > .GIT/hooks/post-commit",
            "echo '{}' > .AGIX/settings.json",
            "echo '{}' > .agix/SETTINGS.JSON",
            "echo '{}' > .Claude/settings.json",
        ] {
            assert!(!s.runs(command), "`{command}` was allowed");
            assert!(!s.runs_unattended(command), "`{command}` was allowed");
        }
        let config = std::fs::read_to_string(s.project.join(".git").join("config")).unwrap();
        assert!(!config.contains("[x]"), "{config}");
        assert!(!s.project.join(".agix").join("settings.json").exists());
    }

    /// `.agents/skills/` is where the engine loads the project's skills
    /// from, so a sandboxed command may not plant one there, nor move the
    /// directories above it aside for others — whether they exist yet or
    /// not. The rest of `.agents/` is the project's.
    #[cfg(target_os = "macos")]
    #[test]
    fn audit_f1_a_sandboxed_command_cannot_plant_a_project_skill() {
        let s = UnderSandboxExec::new().expect("sandbox-exec runs here");
        assert!(
            !s.runs("echo x > .agix/settings.json"),
            "precondition: engine configuration is protected"
        );
        let planted =
            s.runs("mkdir -p .agents/skills/x && echo 'name: x' > .agents/skills/x/SKILL.md");
        assert!(
            !planted && !s.project.join(".agents/skills/x/SKILL.md").exists(),
            "a sandboxed command wrote .agents/skills/x/SKILL.md"
        );

        std::fs::create_dir_all(s.project.join(".agents/skills/kept")).unwrap();
        let elsewhere = s.elsewhere.display().to_string();
        for command in [
            "mkdir .agents/skills/x".to_string(),
            "echo x > .agents/skills/kept/SKILL.md".to_string(),
            "mv .agents/skills .agents/skills.old".to_string(),
            "mv .agents .agents.old && mkdir -p .agents/skills/x".to_string(),
            "rm -rf .agents/skills".to_string(),
            format!("rm -rf .agents && ln -s '{elsewhere}' .agents"),
        ] {
            assert!(!s.runs(&command), "`{command}` was allowed");
        }
        assert!(s.project.join(".agents/skills/kept").is_dir());
        assert!(!s.project.join(".agents/skills/x").exists());
        assert!(
            s.runs("echo x > .agents/notes.md"),
            "the rest of .agents was shut too"
        );
    }

    /// A repository embedded in the project (`sub/.git`, a directory, not a
    /// `.git/modules/` entry) is a submodule once the index holds it as a
    /// gitlink, and the user's `git status` in the project then runs git in
    /// it, under its own configuration. A sandboxed command may not leave
    /// one whose configuration names a program for that git to run.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "known escape: the backends cannot stop a nested repository; tools::nested_repos catches it after the command"]
    fn audit2_a_sandboxed_command_cannot_plant_an_embedded_repositorys_configuration() {
        let s = UnderSandboxExec::new().expect("sandbox-exec runs here");
        assert!(
            !s.runs("git config core.fsmonitor 'touch x'"),
            "precondition: the project's own configuration is protected"
        );
        let pwned = s.elsewhere.join("pwned");
        let planted = s.runs(&format!(
            "mkdir sub && cd sub && git init -q && touch f && git add f && \
             git -c user.email=t@example.com -c user.name=T commit -q -m s && \
             git config core.fsmonitor \"touch '{}'; false\" && cd .. && git add sub",
            pwned.display()
        ));
        let status = std::process::Command::new("git")
            .arg("status")
            .current_dir(&s.project)
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
        assert!(
            !pwned.exists(),
            "the user's `git status` ran a program the sandboxed command named \
             (planting it succeeded: {planted})"
        );
    }

    /// The same profile leaves ordinary work alone: git commits, files in the
    /// project and under `.agix/`, the state root's scratchpad and memory,
    /// and the session's TMPDIR.
    #[cfg(target_os = "macos")]
    #[test]
    fn ordinary_work_still_runs_under_the_hardened_profile() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let state = s.state_root.display().to_string();
        for command in [
            "git add a && git commit -qm first".to_string(),
            "git init -q nested && cd nested && echo b > b && git add b \
             && git -c user.name=T -c user.email=t@example.com commit -qm nested"
                .to_string(),
            "echo x > notes.md && echo x > .agix/notes.txt && echo x > .claude/notes.txt"
                .to_string(),
            format!("echo x > '{state}/scratchpad/a' && echo x > '{state}/memory/a'"),
            "test -n \"$TMPDIR\" && echo x > \"$TMPDIR/f\" && mktemp".to_string(),
        ] {
            assert!(s.runs(&command), "`{command}` was refused");
        }
        assert!(s.project.join(".agix").join("notes.txt").is_file());
        assert!(s.tmp.join("f").is_file());
    }

    /// A session's temporary directory sits in the state root, which is
    /// otherwise shut: its own is writable, a sibling session's is not.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_session_writes_its_own_temporary_directory_and_no_other() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let tmp = s.state_root.join(STATE_ROOT_SESSION_TMP);
        let (mine, theirs) = (tmp.join("mine"), tmp.join("theirs"));
        for d in [&mine, &theirs] {
            std::fs::create_dir_all(d).unwrap();
        }
        let policy = SandboxPolicy {
            state_root: Some(s.state_root.clone()),
            additional_writable: vec![mine.clone()],
            ..Default::default()
        };
        let runs = |command: String| {
            let confined = PlatformSandbox.confine(
                ProcessSpec::new("bash", &s.project).args(["-c".to_string(), command]),
                &policy,
            );
            std::process::Command::new(&confined.spec.program)
                .args(&confined.spec.args)
                .current_dir(&s.project)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(runs(format!("echo x > '{}/f'", mine.display())));
        for target in [theirs.join("f"), tmp.join("f")] {
            assert!(
                !runs(format!("echo x > '{}'", target.display())),
                "{}",
                target.display()
            );
        }
    }

    /// A session without a project works in a directory of its own in the
    /// state root's scratchpad. Working there does not open the rest of the
    /// state root, nor its sockets.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_session_without_a_project_leaves_the_state_root_fenced() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let work = s.state_root.join("scratchpad/sessions/s1");
        std::fs::create_dir_all(&work).unwrap();
        let socket = listening(s.state_root.join("daemon.sock"));
        assert!(outside("nc", &["-U", &socket.display().to_string()]));
        let policy = SandboxPolicy {
            state_root: Some(s.state_root.clone()),
            additional_writable: vec![s.tmp.clone()],
            ..Default::default()
        };
        let runs = |command: String| {
            let confined = PlatformSandbox.confine(
                ProcessSpec::new("bash", &work).args(["-c".to_string(), command]),
                &policy,
            );
            std::process::Command::new(&confined.spec.program)
                .args(&confined.spec.args)
                .current_dir(&work)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(runs("echo x > notes.md".into()));
        assert!(work.join("notes.md").is_file());
        for command in [
            format!("echo x > '{}/AGENTS.md'", s.state_root.display()),
            format!("mkdir -p '{}/skills/x'", s.state_root.display()),
            format!("nc -U '{}' < /dev/null", socket.display()),
        ] {
            assert!(!runs(command.clone()), "`{command}` was allowed");
        }
    }

    /// A policy that names its writable roots leaves nothing else writable:
    /// not the project, the state root's working directories, or the
    /// per-user temporary directory `mktemp` uses. And the backend confines
    /// a program that is not a shell the same way.
    #[cfg(target_os = "macos")]
    #[test]
    fn only_the_named_roots_are_writable_when_the_policy_says_so() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let policy = SandboxPolicy {
            state_root: Some(s.state_root.clone()),
            additional_writable: vec![s.tmp.clone()],
            only_additional_writable: true,
            ..Default::default()
        };
        let runs = |program: &str, args: &[&str]| {
            let confined = PlatformSandbox.confine(
                ProcessSpec::new(program, &s.project).args(args.iter().copied()),
                &policy,
            );
            assert_eq!(confined.enforcement, Enforcement::Full);
            std::process::Command::new(&confined.spec.program)
                .args(&confined.spec.args)
                .current_dir(&s.project)
                .output()
                .unwrap()
                .status
                .success()
        };
        let state = s.state_root.display().to_string();
        let tmp = s.tmp.display().to_string();

        assert!(runs("bash", &["-c", &format!("echo x > '{tmp}/f'")]));
        assert!(runs("git", &["status", "--short"]));
        for command in [
            "echo x > a".to_string(),
            "echo x > .agix/notes.txt".to_string(),
            format!("echo x > '{state}/scratchpad/a'"),
            "mktemp".to_string(),
        ] {
            assert!(!runs("bash", &["-c", &command]), "`{command}` was allowed");
        }
        assert!(!runs("git", &["add", "a"]));
        assert!(!s.project.join(".git").join("index").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_a_read_scope_denies_everything_else_before_the_deny_list() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/Users/me".into()));
        let project = PathBuf::from("/tmp/work");
        let policy = SandboxPolicy {
            deny_read: vec![home.join(".ssh")],
            allow_read: vec![home.join(".npmrc"), project.join("vendored")],
            ..Default::default()
        }
        .strict([project.clone()], Some(PathBuf::from("/tmp/session-tmp")));
        let cmd = wrap(SandboxOptions {
            command: "true",
            cwd: &project,
            additional_writable: &policy.additional_writable.clone(),
            disable: false,
            policy,
        });
        let profile = &cmd.spec.args[1];
        let scoped = profile
            .find("(deny file-read*)\n(allow file-read-metadata)\n")
            .unwrap_or_else(|| panic!("no read scope:\n{profile}"));
        let ssh = profile
            .find(&format!(
                "(deny file-read* (subpath \"{}",
                home.join(".ssh").display()
            ))
            .unwrap_or_else(|| panic!("no deny_read rule:\n{profile}"));
        assert!(scoped < ssh, "the deny list would be reopened:\n{profile}");
        for root in [
            "/usr",
            "/System",
            "/private/tmp/work",
            "/private/tmp/session-tmp",
        ] {
            assert!(
                profile.contains(&format!("(subpath \"{root}\")")),
                "{root} is not readable:\n{profile}"
            );
        }
        assert!(
            !profile.contains(".npmrc"),
            "an allow_read outside the scope reopened it:\n{profile}"
        );
        assert!(profile.contains("vendored"), "{profile}");
        assert_eq!(profile.matches('(').count(), profile.matches(')').count());
    }

    /// The strict profile on this machine: the commands the read-only lane
    /// lets through — `ls`, `cat`, git through the xcrun shim and directly —
    /// run; reading outside the project, writing in it and starting from the
    /// daemon's environment do not.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_strict_command_reads_the_project_and_the_system_and_nothing_else() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let commit = std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null", "add", "a"])
            .current_dir(&s.project)
            .status()
            .unwrap()
            .success()
            && std::process::Command::new("git")
                .args(["-c", "core.hooksPath=/dev/null", "commit", "-qm", "a"])
                .current_dir(&s.project)
                .status()
                .unwrap()
                .success();
        assert!(commit);
        std::fs::write(s.elsewhere.join("secret"), "s3cret").unwrap();
        let policy = SandboxPolicy {
            state_root: Some(s.state_root.clone()),
            ..Default::default()
        }
        .strict([s.project.clone()], Some(s.tmp.clone()));
        let unmet = read_scope::unmet(
            &policy.read,
            &read_scope::UserDirs::of(INHERITED),
            std::env::var_os("PATH").as_deref(),
        );
        if !unmet.is_empty() {
            eprintln!("skipped: the strict profile cannot run this machine's git: {unmet:?}");
            return;
        }
        let run = |command: &str| {
            let confined = PlatformSandbox.confine(
                ProcessSpec::new("bash", &s.project).args(["-c", command]),
                &policy,
            );
            assert_eq!(
                confined.enforcement,
                Enforcement::Full,
                "{:?}",
                confined.unmet
            );
            assert_eq!(confined.spec.env_scope, EnvScope::Minimal);
            std::process::Command::new(&confined.spec.program)
                .args(&confined.spec.args)
                .current_dir(&s.project)
                .output()
                .unwrap()
        };
        let tmp = s.tmp.display().to_string();
        for command in [
            "ls".to_string(),
            "ls -la /".to_string(),
            "cat a".to_string(),
            "head -1 a && wc -l a && grep -r a . >/dev/null".to_string(),
            "date".to_string(),
            "git status --short".to_string(),
            "git log --oneline -1".to_string(),
            "git diff HEAD".to_string(),
            "/usr/bin/git rev-parse HEAD".to_string(),
            "/usr/bin/xcrun --find git".to_string(),
            format!("echo x > '{tmp}/f' && cat '{tmp}/f'"),
        ] {
            let out = run(&command);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "`{command}` failed: {stderr}");
            assert!(
                !stderr.contains("unable to access") && !stderr.contains("xcrun_db"),
                "`{command}`: {stderr}"
            );
        }
        let developer = std::process::Command::new("xcode-select")
            .arg("-p")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if let Some(dir) = developer {
            let out = run(&format!("'{dir}/usr/bin/git' status --short"));
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        for command in [
            format!("cat '{}/secret'", s.elsewhere.display()),
            format!("ls '{}'", s.state_root.display()),
            "echo x > a".to_string(),
            "git add a".to_string(),
        ] {
            assert!(!run(&command).status.success(), "`{command}` was allowed");
        }
    }

    /// A unix socket that answers every connection and hangs up.
    #[cfg(target_os = "macos")]
    #[allow(clippy::unwrap_used)]
    fn listening(path: PathBuf) -> PathBuf {
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                drop(conn);
            }
        });
        path
    }

    /// No sandboxed command connects to a unix socket in the state root or
    /// one this process serves, however it names the socket: the daemon's own
    /// socket is one or the other, and a command connected to it is the
    /// daemon's host. Other sockets stay reachable.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandboxed_command_cannot_connect_to_the_daemons_socket() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let scene = s.state_root.join("scenes").join("code");
        std::fs::create_dir_all(&scene).unwrap();
        let in_state = listening(scene.join("daemon.sock"));
        let served = listening(s.elsewhere.join("served.sock"));
        let other = listening(s.elsewhere.join("other.sock"));
        let _fence = fence_socket(&served);

        let nc = |path: &Path| format!("nc -U '{}' < /dev/null", path.display());
        for socket in [&in_state, &served, &other] {
            assert!(
                outside("nc", &["-U", &socket.display().to_string()]),
                "{} is not reachable at all",
                socket.display()
            );
        }
        assert!(s.runs(&nc(&other)), "an unrelated socket was refused");
        for socket in [&in_state, &served] {
            assert!(!s.runs(&nc(socket)), "{} was reached", socket.display());
        }
        let link = format!(
            "ln -s '{}' link.sock && nc -U link.sock < /dev/null",
            in_state.display()
        );
        assert!(!s.runs(&link), "reached through a symlink");
        let relative = format!("cd '{}' && nc -U daemon.sock < /dev/null", scene.display());
        assert!(!s.runs(&relative), "reached by a relative name");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandboxed_command_cannot_read_a_fenced_secret() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let secret = s.elsewhere.join("unix.token");
        let other = s.elsewhere.join("other.txt");
        std::fs::write(&secret, "t").unwrap();
        std::fs::write(&other, "o").unwrap();
        let fence = fence_secret(&secret);
        assert!(s.runs(&format!("cat '{}'", other.display())));
        assert!(!s.runs(&format!("cat '{}'", secret.display())));
        drop(fence);
        assert!(
            s.runs(&format!("cat '{}'", secret.display())),
            "the file stayed fenced after its fence was dropped"
        );
    }

    /// The loopback port a daemon listens on is refused the same way: the
    /// token is not in a command's environment, but `--token` is in `ps`.
    /// Once the fence is dropped with its listener, the port is anybody's
    /// again.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandboxed_command_cannot_connect_to_the_daemons_port() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let accepting = |listener: std::net::TcpListener| {
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    drop(conn);
                }
            });
            port
        };
        let served = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let fence = fence_port(served.local_addr().unwrap());
        let served = accepting(served);
        let other = accepting(std::net::TcpListener::bind("127.0.0.1:0").unwrap());

        assert!(s.runs(&format!("nc -z 127.0.0.1 {other}")));
        for host in ["127.0.0.1", "localhost"] {
            assert!(
                !s.runs(&format!("nc -z {host} {served}")),
                "{host}:{served} was reached"
            );
        }
        drop(fence);
        assert!(
            s.runs(&format!("nc -z 127.0.0.1 {served}")),
            "the port stayed fenced after its fence was dropped"
        );
    }

    /// sandbox-exec names a remote host only as `*` or `localhost`, and a
    /// profile naming any other does not compile: an allowlist of real
    /// domains must still run the command, with the network shut but for
    /// what the allowlist names and the profile can say.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_allowlist_of_domains_runs_the_command_with_the_rest_of_the_network_shut() {
        let Some(s) = UnderSandboxExec::new() else {
            return;
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                drop(conn);
            }
        });
        let reach = format!("nc -z 127.0.0.1 {port}");
        let domains = ["example.com", "registry.npmjs.org"];

        assert!(s.runs_allowing(&domains, "true"), "the command did not run");
        assert!(
            !s.runs_allowing(&domains, &reach),
            "127.0.0.1:{port}, which the allowlist does not name, was reached"
        );
        assert!(
            s.runs_allowing(&["example.com", "localhost"], &reach),
            "localhost, which the allowlist names, was refused"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_socket_is_fenced_until_its_fence_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = resolve_path(&dir.path().join("served.sock"));
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let fence = fence_socket(&path);
        let twice = fence_socket(&path);
        assert!(fenced_sockets().contains(&path));
        drop(twice);
        assert!(
            fenced_sockets().contains(&path),
            "one listener's fence came down with another's"
        );
        drop(fence);
        assert!(!fenced_sockets().contains(&path));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_profile_refuses_unix_sockets_in_the_state_root_and_the_fenced_ones() {
        let socket = std::env::temp_dir().join(format!("fenced-{}.sock", std::process::id()));
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let _fence = fence_socket(&socket);
        let policy = SandboxPolicy {
            state_root: Some(PathBuf::from("/srv/agix-state")),
            network_mode: NetworkMode::Allowlist,
            allowed_domains: vec!["localhost".into()],
            ..Default::default()
        };
        let profile = build_macos_profile(
            Path::new("/work/project"),
            &[],
            &policy,
            &read_scope::UserDirs::of(INHERITED),
        );
        let _ = std::fs::remove_file(&socket);

        let state_rule =
            "(deny network-outbound (remote unix-socket (subpath \"/srv/agix-state\")))";
        let socket_rule = format!(
            "(deny network-outbound (remote unix-socket (literal \"{}\")))",
            resolve_path(&socket).display()
        );
        let allow = profile.find("(allow network-outbound").unwrap();
        for rule in [state_rule, &socket_rule] {
            let at = profile
                .find(rule)
                .unwrap_or_else(|| panic!("{rule}\n{profile}"));
            assert!(at > allow, "an allowlist entry would reopen it:\n{profile}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_fenced_secret_stays_unreadable_whatever_allow_read_says() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("unix.token");
        std::fs::write(&secret, "t").unwrap();
        let _fence = fence_secret(&secret);
        let policy = SandboxPolicy {
            deny_read: vec![PathBuf::from("/nowhere")],
            allow_read: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let profile = build_macos_profile(
            Path::new("/work/project"),
            &[],
            &policy,
            &read_scope::UserDirs::of(INHERITED),
        );
        let rule = format!(
            "(deny file-read* (literal \"{}\"))",
            resolve_path(&secret).display()
        );
        let denied = profile
            .find(&rule)
            .unwrap_or_else(|| panic!("{rule}\n{profile}"));
        let allowed = profile.find("(allow file-read*").unwrap();
        assert!(denied > allowed, "allow_read would reopen it:\n{profile}");
    }

    /// Whether this machine has a backend that actually runs. Where it has
    /// none the real-run cases below skip, saying why, unless
    /// `AGIX_REQUIRE_SANDBOX` is set — CI sets it, so that a runner whose
    /// bubblewrap cannot make namespaces fails rather than passes empty.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn a_backend_runs() -> bool {
        #[cfg(target_os = "macos")]
        let works = std::process::Command::new("sandbox-exec")
            .args(["-p", "(version 1)(allow default)", "true"])
            .status()
            .is_ok_and(|s| s.success());
        #[cfg(target_os = "linux")]
        let works = bwrap_available()
            && std::process::Command::new("bwrap")
                .args(["--ro-bind", "/", "/", "true"])
                .status()
                .is_ok_and(|s| s.success());
        if !works {
            assert!(
                std::env::var_os("AGIX_REQUIRE_SANDBOX").is_none(),
                "AGIX_REQUIRE_SANDBOX is set and no sandbox backend runs here"
            );
            eprintln!(
                "skipped: no sandbox backend runs here (Linux: install bubblewrap and \
                 allow unprivileged user namespaces)"
            );
        }
        works
    }

    /// A `git` that is a shim under the home directory (asdf, `~/bin`) and a
    /// git configuration that includes a file from elsewhere are outside
    /// what the strict profile reads, so git does not run under it: the
    /// profile is reported as not fully held, with what is missing. The
    /// session's own policy, which reads everything, holds.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn the_strict_profile_reports_a_git_it_cannot_run_as_unmet() {
        use std::os::unix::fs::PermissionsExt;
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap().join("home");
        let shims = home.join(".asdf/shims");
        write_file(&shims.join("git"), "#!/bin/sh\necho shim-ran\n");
        std::fs::set_permissions(shims.join("git"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        write_file(
            &home.join(".gitconfig"),
            "[user]\n\tname = T\n[include]\n\tpath = ~/dotfiles/gitconfig\n",
        );
        write_file(&home.join("dotfiles/gitconfig"), "");
        let spec = || {
            let mut spec = ProcessSpec::new("bash", dir.path()).args(["-c", "git"]);
            spec.env.extend([
                (
                    "PATH".to_string(),
                    format!("{}:/usr/bin:/bin", shims.display()),
                ),
                ("HOME".to_string(), home.display().to_string()),
            ]);
            spec
        };
        let strict = SandboxPolicy::default().strict(Vec::new(), None);

        let confined = PlatformSandbox.confine(spec(), &strict);
        assert_eq!(confined.enforcement, Enforcement::Partial, "{confined:?}");
        let says = |what: &Path| {
            confined
                .unmet
                .iter()
                .any(|line| line.contains(&what.display().to_string()))
        };
        assert!(says(&shims.join("git")), "{:?}", confined.unmet);
        assert!(
            says(&home.join("dotfiles/gitconfig")),
            "{:?}",
            confined.unmet
        );
        let ran = std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&ran.stdout).contains("shim-ran"),
            "the shim ran under the strict profile: {ran:?}"
        );

        let session = PlatformSandbox.confine(spec(), &SandboxPolicy::default());
        assert_eq!(
            session.enforcement,
            Enforcement::Full,
            "{:?}",
            session.unmet
        );

        let report = base::interface::exec::ConfinementReport::probe(
            &PlatformSandbox,
            spec(),
            &SandboxPolicy::default(),
            true,
            false,
        );
        assert_eq!(report.strict, Enforcement::Partial);
        assert_eq!(report.strict_unmet, confined.unmet);
        assert_eq!(report.session, Enforcement::Full);
        assert!(report.session_unmet.is_empty(), "{report:?}");
    }

    /// git reads `$XDG_CONFIG_HOME/git/config` in place of
    /// `~/.config/git/config` when the variable is set; the strict profile
    /// reads that file but not what it includes from elsewhere, which is
    /// reported unmet.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn the_strict_profile_looks_for_the_git_configuration_under_xdg_config_home() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let xdg = root.join("xdg");
        write_file(
            &xdg.join("git/config"),
            "[include]\n\tpath = ../../dotfiles/gitconfig\n",
        );
        write_file(&root.join("dotfiles/gitconfig"), "");
        let mut spec = ProcessSpec::new("bash", &root).args(["-c", "git"]);
        spec.env.extend([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), home.display().to_string()),
            ("XDG_CONFIG_HOME".to_string(), xdg.display().to_string()),
        ]);

        let confined =
            PlatformSandbox.confine(spec, &SandboxPolicy::default().strict(Vec::new(), None));
        assert_eq!(confined.enforcement, Enforcement::Partial, "{confined:?}");
        let says = |what: &Path| {
            confined
                .unmet
                .iter()
                .any(|line| line.contains(&what.display().to_string()))
        };
        assert!(
            says(&root.join("dotfiles/gitconfig")),
            "{:?}",
            confined.unmet
        );
        assert!(!says(&xdg.join("git/config")), "{:?}", confined.unmet);
    }

    /// The command's own `HOME`, not the daemon's, is the home whose git
    /// configuration the strict profile reads: the one the probe checked.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn the_strict_profile_reads_the_git_configuration_of_the_commands_own_home() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_file(&home.join(".gitconfig"), "[user]\n\tname = SpecHome\n");
        let mut spec =
            ProcessSpec::new("bash", &project).args(["-c", "git config --global --get user.name"]);
        spec.env.extend([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), home.display().to_string()),
        ]);

        let confined = PlatformSandbox.confine(
            spec,
            &SandboxPolicy::default().strict([project.clone()], None),
        );
        assert_eq!(confined.enforcement, Enforcement::Full, "{confined:?}");
        let ran = std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .env_clear()
            .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&project)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout).trim(),
            "SpecHome",
            "{ran:?}"
        );
    }

    /// A git configuration kept under `$XDG_CONFIG_HOME/git` is read under
    /// the strict profile, file by file: the credentials beside it are not.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn the_strict_profile_reads_the_git_files_under_xdg_config_home_but_not_their_credentials() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let xdg = root.join("xdg");
        write_file(&xdg.join("git/config"), "[user]\n\tname = FromXdg\n");
        write_file(&xdg.join("git/ignore"), "ignored\n");
        write_file(&xdg.join("git/attributes"), "*.bin binary\n");
        write_file(
            &xdg.join("git/credentials"),
            "https://u:s3cret@example.com\n",
        );
        let run = |command: &str| {
            let mut spec = ProcessSpec::new("bash", &project).args(["-c", command]);
            spec.env.extend([
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("HOME".to_string(), home.display().to_string()),
                ("XDG_CONFIG_HOME".to_string(), xdg.display().to_string()),
            ]);
            let confined = PlatformSandbox.confine(
                spec,
                &SandboxPolicy::default().strict([project.clone()], None),
            );
            assert_eq!(confined.enforcement, Enforcement::Full, "{confined:?}");
            std::process::Command::new(&confined.spec.program)
                .args(&confined.spec.args)
                .env_clear()
                .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
                .current_dir(&project)
                .output()
                .unwrap()
        };

        let name = run("git config --global --get user.name");
        assert_eq!(
            String::from_utf8_lossy(&name.stdout).trim(),
            "FromXdg",
            "{name:?}"
        );
        for file in ["ignore", "attributes"] {
            let out = run(&format!("cat \"$XDG_CONFIG_HOME/git/{file}\""));
            assert!(out.status.success(), "{file}: {out:?}");
        }
        let creds = run("cat \"$XDG_CONFIG_HOME/git/credentials\"");
        assert!(
            !creds.status.success() && !String::from_utf8_lossy(&creds.stdout).contains("s3cret"),
            "{creds:?}"
        );
    }

    /// `~/.config/git` is read file by file as well: its `credentials` stay
    /// unreadable under a policy whose `deny_read` does not name them.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn the_strict_profile_reads_the_git_files_under_dot_config_but_not_their_credentials() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let git = home.join(".config/git");
        write_file(&git.join("config"), "[user]\n\tname = FromDotConfig\n");
        write_file(&git.join("ignore"), "ignored\n");
        write_file(&git.join("attributes"), "*.bin binary\n");
        write_file(&git.join("credentials"), "https://u:s3cret@example.com\n");
        let policy = SandboxPolicy {
            deny_read: Vec::new(),
            ..Default::default()
        }
        .strict([project.clone()], None);
        let run = |command: &str| {
            let mut spec = ProcessSpec::new("bash", &project).args(["-c", command]);
            spec.env.extend([
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("HOME".to_string(), home.display().to_string()),
            ]);
            let confined = PlatformSandbox.confine(spec, &policy);
            assert_eq!(confined.enforcement, Enforcement::Full, "{confined:?}");
            std::process::Command::new(&confined.spec.program)
                .args(&confined.spec.args)
                .env_clear()
                .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
                .current_dir(&project)
                .output()
                .unwrap()
        };

        let name = run("git config --global --get user.name");
        assert_eq!(
            String::from_utf8_lossy(&name.stdout).trim(),
            "FromDotConfig",
            "{name:?}"
        );
        for file in ["ignore", "attributes"] {
            let out = run(&format!("cat \"$HOME/.config/git/{file}\""));
            assert!(out.status.success(), "{file}: {out:?}");
        }
        let creds = run("cat \"$HOME/.config/git/credentials\"");
        assert!(
            !creds.status.success() && !String::from_utf8_lossy(&creds.stdout).contains("s3cret"),
            "{creds:?}"
        );
    }

    /// The strict profile's minimal environment does not pass the daemon's
    /// `XDG_CONFIG_HOME` on, so the command's git reads `~/.config/git` and
    /// the probe looks there too, not at a configuration git never opens.
    /// Run again in a child whose own environment sets the variable.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn the_strict_profile_ignores_a_daemon_xdg_config_home_its_command_does_not_inherit() {
        const ROOT: &str = "AGIX_TEST_STRICT_XDG_ROOT";
        if !a_backend_runs() {
            return;
        }
        let Some(root) = std::env::var_os(ROOT).map(PathBuf::from) else {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            write_file(
                &root.join("xdg/git/config"),
                "[include]\n\tpath = ../../dotfiles/gitconfig\n",
            );
            write_file(&root.join("dotfiles/gitconfig"), "");
            write_file(
                &root.join("home/.config/git/config"),
                "[user]\n\tname = FromDotConfig\n",
            );
            std::fs::create_dir_all(root.join("project")).unwrap();
            let test = format!(
                "{}::the_strict_profile_ignores_a_daemon_xdg_config_home_its_command_does_not_inherit",
                module_path!().split_once("::").unwrap().1
            );
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([test.as_str(), "--exact", "--test-threads=1"])
                .env(ROOT, &root)
                .env("XDG_CONFIG_HOME", root.join("xdg"))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        };
        let home = root.join("home");
        let project = root.join("project");
        let mut spec =
            ProcessSpec::new("bash", &project).args(["-c", "git config --global --get user.name"]);
        spec.env.extend([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), home.display().to_string()),
        ]);

        let confined = PlatformSandbox.confine(
            spec,
            &SandboxPolicy::default().strict([project.clone()], None),
        );
        assert_eq!(confined.enforcement, Enforcement::Full, "{confined:?}");
        let ran = std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .env_clear()
            .envs(confined.spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&project)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout).trim(),
            "FromDotConfig",
            "{ran:?}"
        );
    }

    /// `command` under the real backend, in `cwd`, with `env` added.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[allow(clippy::unwrap_used)]
    fn run_confined(
        cwd: &Path,
        policy: &SandboxPolicy,
        env: &[(&str, &Path)],
        command: &str,
    ) -> std::process::Output {
        let confined = PlatformSandbox.confine(
            ProcessSpec::new("bash", cwd).args(["-c".to_string(), command.to_string()]),
            policy,
        );
        assert_eq!(
            confined.enforcement,
            Enforcement::Full,
            "{:?}",
            confined.unmet
        );
        let mut run = std::process::Command::new(&confined.spec.program);
        run.args(&confined.spec.args).current_dir(cwd);
        for (k, v) in env {
            run.env(k, v);
        }
        run.output().unwrap()
    }

    /// `npm run dev > log &` and the like: what a confined command leaves in
    /// the background, its output sent elsewhere, neither holds the command's
    /// output open nor goes when the command exits. It is in the command's
    /// process group, where stopping the group reaches it.
    ///
    /// The background loop gives up after a while, so that a sandbox holding
    /// the output open fails this rather than hangs it. What it leaves
    /// running holds a FIFO open, so that the group emptying out of reach —
    /// bwrap's `--new-session` takes its PID 1 into a group of its own — is
    /// not taken for the group having stopped.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used, unsafe_code)]
    fn what_a_confined_command_leaves_running_outlives_it_in_its_process_group() {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::process::CommandExt;
        if !a_backend_runs() {
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let fifo =
            std::ffi::CString::new(project.path().join("held").as_os_str().as_encoded_bytes())
                .unwrap();
        // SAFETY: a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let held = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(project.path().join("held"))
            .unwrap();
        let command = "echo out; echo err >&2; \
                       (i=0; while [ ! -e exited ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done; \
                        [ -e exited ] && exec 3>held && touch outlived && exec sleep 600) >/dev/null 2>&1 & \
                       exit 3";
        let confined = PlatformSandbox.confine(
            ProcessSpec::new("bash", project.path()).args(["-c", command].map(String::from)),
            &SandboxPolicy::default(),
        );
        assert_eq!(
            confined.enforcement,
            Enforcement::Full,
            "{:?}",
            confined.unmet
        );
        let child = std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .current_dir(project.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = child.id() as libc::pid_t;
        let output = child.wait_with_output().unwrap();
        std::fs::write(project.path().join("exited"), "").unwrap();

        let until = |what: &dyn Fn() -> bool| {
            (0..1500).any(|_| {
                let done = what();
                if !done {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                done
            })
        };
        let outlived = until(&|| project.path().join("outlived").exists());
        // SAFETY: plain integers; the group is the one just started.
        unsafe { libc::killpg(group, libc::SIGKILL) };
        // SAFETY: as above; signal 0 only asks whether the group exists.
        let stopped = until(&|| unsafe { libc::killpg(group, 0) } != 0);
        let released = until(&|| matches!((&held).read(&mut [0u8; 1]), Ok(0)));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "err\n");
        assert_eq!(output.status.code(), Some(3));
        assert!(outlived, "the background loop went with the command");
        assert!(stopped, "stopping the group stops what the command left");
        assert!(released, "what the command left runs on outside its group");
    }

    /// A submodule's git directory, `modules/<name>` under the git directory
    /// of the repository that holds it, as `git submodule` lays it out.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[allow(clippy::unwrap_used)]
    fn plant_git_dir(dir: &Path) {
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join("config"), "[core]\n\tbare = false\n").unwrap();
    }

    /// A submodule's configuration and hooks are git's as much as the
    /// superproject's: `core.fsmonitor` planted in one runs the next time
    /// the user's git, outside the sandbox, looks at the submodule. Nested
    /// submodules have git directories of their own under `modules/`, and
    /// the name a submodule is filed under may itself contain a slash.
    /// Removing a submodule's `HEAD` first does not unguard it.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_sandboxed_command_cannot_rewrite_a_submodules_configuration_or_hooks() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let state = dir.path().join("state");
        let elsewhere = dir.path().join("elsewhere");
        for d in [&project, &state, &elsewhere] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(project.join(".agix")).unwrap();
        let git = project.join(".git");
        plant_git_dir(&git);
        let modules = git.join("modules");
        for module in ["sub", "a", "a/modules/b", "libs/c"] {
            plant_git_dir(&modules.join(module));
        }
        let policy = SandboxPolicy {
            state_root: Some(state.clone()),
            ..Default::default()
        };
        let runs = |command: &str| {
            run_confined(&project, &policy, &[], command)
                .status
                .success()
        };

        let fsmonitor = "printf '[core]\\n\\tfsmonitor = touch pwned\\n' >>";
        runs("rm .git/modules/sub/HEAD");
        let elsewhere = elsewhere.display();
        let mut refused = vec![
            format!("{fsmonitor} .git/modules/sub/config"),
            format!("{fsmonitor} .git/modules/a/modules/b/config"),
            format!("{fsmonitor} .git/modules/libs/c/config"),
            "echo 'touch pwned' > .git/modules/sub/hooks/post-checkout".to_string(),
            "echo 'touch pwned' > .git/modules/a/modules/b/hooks/x".to_string(),
            "mv .git/modules/sub .git/modules/gone".to_string(),
            "mv .git/modules/a/modules .git/modules/a/gone".to_string(),
            format!("mv .git/modules/libs '{elsewhere}/libs'"),
        ];
        // bwrap cannot hold a file that is not there yet (the module header's
        // escape path 3); a submodule's git directory has no `commondir`.
        let absent_files_held = cfg!(target_os = "macos");
        if absent_files_held {
            refused.push("echo /tmp > .git/modules/sub/commondir".to_string());
        }
        for command in refused {
            assert!(!runs(&command), "`{command}` was allowed");
        }
        for module in ["sub", "a/modules/b", "libs/c"] {
            let module = modules.join(module);
            assert!(
                module.symlink_metadata().unwrap().is_dir(),
                "{} was replaced",
                module.display()
            );
            let config = std::fs::read_to_string(module.join("config")).unwrap();
            assert!(!config.contains("fsmonitor"), "{config}");
            assert!(!absent_files_held || !module.join("commondir").exists());
        }
        assert!(!modules.join("a/modules/b/hooks/x").exists());

        assert!(
            runs("echo x > .git/modules/sub/index && echo x > .git/modules/a/modules/b/FETCH_HEAD"),
            "a submodule's own bookkeeping must stay writable"
        );
    }

    /// A project that lives in the state root lifts the fence around it,
    /// and the state root is then guarded as the configuration root it is:
    /// its skills, hooks and agents, and its settings files, stay
    /// read-only. The project itself stays writable.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_project_in_the_state_root_cannot_rewrite_the_state_roots_configuration() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().canonicalize().unwrap().join("state");
        write_file(&state.join("skills/s/SKILL.md"), "fine");
        write_file(&state.join("hooks/h.sh"), "fine");
        write_file(&state.join("settings.local.json"), "{}");
        let policy = SandboxPolicy {
            state_root: Some(state.clone()),
            ..Default::default()
        };
        let runs = |command: &str| run_confined(&state, &policy, &[], command).status.success();

        assert!(
            runs("echo x > notes.md"),
            "the project itself stays writable"
        );
        for command in [
            "echo x > skills/s/SKILL.md",
            "mkdir -p skills/planted && echo x > skills/planted/SKILL.md",
            "echo x >> hooks/h.sh",
            "mkdir -p agents && echo x > agents/a.md",
            "echo '{}' > settings.local.json",
        ] {
            assert!(!runs(command), "`{command}` was allowed");
        }
        assert_eq!(
            std::fs::read_to_string(state.join("skills/s/SKILL.md")).unwrap(),
            "fine"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[allow(clippy::unwrap_used)]
    fn write_file(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A command reads none of the credential stores in the user's home, nor
    /// what in the state root holds keys — the settings files, whose
    /// `api_key` and MCP headers are plain text, and the MCP OAuth tokens, of
    /// the root and of every scene. Everything else in the state root, which
    /// skills, memory and the session's own files live in, stays readable.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_sandboxed_command_reads_no_credentials_and_the_rest_of_the_state_root() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let project = dir.path().join("project");
        // In the project, which the command sees on every backend: bwrap
        // gives a command a `/tmp` of its own, and the temporary directory
        // may well be there.
        let home = project.join("home");
        std::fs::create_dir_all(project.join(".agix")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let secret = "agix-test-secret";
        let secrets = [
            home.join(".git-credentials"),
            home.join(".config/git/credentials"),
            home.join(".config/gcloud/application_default_credentials.json"),
            home.join(".cargo/credentials"),
            home.join(".cargo/credentials.toml"),
            home.join(".vault-token"),
            home.join(".docker/config.json"),
            home.join(".kube/config"),
            home.join(".npmrc"),
            home.join(".pypirc"),
            home.join(".netrc"),
            state.join("settings.json"),
            state.join("settings.local.json"),
            state.join(".settings.local.json.1234.tmp"),
            state.join("scenes/code/settings.json"),
            state.join("scenes/code/settings.local.json"),
            state.join("scenes/code/mcp/oauth/server.json"),
            state.join("scenes/triage/settings.local.json"),
            state.join("mcp/oauth/server.json"),
            state.join("daemon/auth/desktop.token"),
        ];
        for path in &secrets {
            write_file(path, secret);
        }
        let readable = [
            home.join(".gitconfig"),
            home.join(".cargo/config.toml"),
            state.join("skills/s/SKILL.md"),
            state.join("scenes/code/skills/t/run.sh"),
            state.join("scenes/code/scene.json"),
            state.join("scenes/code/scheduled_tasks.json"),
            state.join("memory/MEMORY.md"),
            state.join("scratchpad/notes.md"),
            state.join("sessions/s.jsonl"),
            state.join("daemon/instances.d/desktop.json"),
        ];
        for path in &readable {
            write_file(path, "fine");
        }
        let policy = SandboxPolicy {
            deny_read: default_deny_read_in(&home),
            state_root: Some(state.clone()),
            ..Default::default()
        };
        let cat = |command: &str| {
            let out = run_confined(&project, &policy, &[("HOME", &home)], command);
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )
        };

        for command in [
            "cat ~/.git-credentials".to_string(),
            format!("cat '{}/settings.local.json'", state.display()),
        ] {
            let (ok, shown) = cat(&command);
            assert!(!ok && !shown.contains(secret), "`{command}` read {shown:?}");
        }
        for path in &secrets {
            let (ok, shown) = cat(&format!("cat '{}'", path.display()));
            assert!(
                !ok && !shown.contains(secret),
                "{} was read: {shown:?}",
                path.display()
            );
        }
        for path in &readable {
            let (ok, shown) = cat(&format!("cat '{}'", path.display()));
            assert!(ok && shown == "fine", "{} was not read", path.display());
        }
        let (ok, listed) = cat(&format!("ls '{}'", state.join("scenes/code").display()));
        assert!(ok && listed.contains("skills"), "{listed}");
    }

    /// 场景目录是指向别处的符号链接时（把场景放在 dotfiles 仓库里再链进来），
    /// 它的 `settings.json` 仍是这个场景的凭据，沙箱里的命令照样读不到。macOS
    /// 的读拒绝匹配的是解析后的路径，链接目标不在 `<状态目录>/scenes/` 下。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn audit3_a_scene_linked_in_from_elsewhere_keeps_its_settings_unreadable() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let project = dir.path().join("project");
        let elsewhere = dir.path().join("dotfiles/scene");
        std::fs::create_dir_all(state.join("scenes")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let secret = "agix-audit3-scene-secret";
        write_file(&elsewhere.join("settings.json"), secret);
        write_file(&state.join("scenes/code/settings.json"), secret);
        std::os::unix::fs::symlink(&elsewhere, state.join("scenes/linked")).unwrap();
        let policy = SandboxPolicy {
            state_root: Some(state.clone()),
            ..Default::default()
        };
        for path in [
            state.join("scenes/code/settings.json"),
            state.join("scenes/linked/settings.json"),
        ] {
            let out = run_confined(&project, &policy, &[], &format!("cat '{}'", path.display()));
            let shown = String::from_utf8_lossy(&out.stdout);
            assert!(
                !out.status.success() && !shown.contains(secret),
                "{} was read: {shown:?}",
                path.display()
            );
        }
    }

    /// The project's own `.agix/settings.local.json` and `.agix/mcp.local.json`
    /// are where the engine writes the project's keys; a command reads neither,
    /// even with the whole project in `allow_read`. The committed
    /// `settings.json` beside them stays readable.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_sandboxed_command_reads_none_of_the_projects_keys() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let agix = project.join(".agix");
        let secret = "agix-test-secret";
        let secrets = [
            agix.join("settings.local.json"),
            agix.join("mcp.local.json"),
            agix.join(".settings.local.json.1234.tmp"),
        ];
        for path in &secrets {
            write_file(path, secret);
        }
        write_file(&agix.join("settings.json"), "fine");
        let policy = SandboxPolicy {
            deny_read: vec![dir.path().join("nowhere")],
            allow_read: vec![project.clone()],
            state_root: Some(dir.path().join("state")),
            ..Default::default()
        };
        let cat = |path: &Path| {
            let out = run_confined(&project, &policy, &[], &format!("cat '{}'", path.display()));
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )
        };
        for path in &secrets {
            let (ok, shown) = cat(path);
            assert!(
                !ok && !shown.contains(secret),
                "{} was read: {shown:?}",
                path.display()
            );
        }
        assert_eq!(cat(&agix.join("settings.json")), (true, "fine".to_string()));
    }

    /// What a process a confined command left running in `project` reads of
    /// `paths` after each of them is written — the engine's way, a
    /// temporary file renamed over it — once it is running.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[allow(clippy::unwrap_used)]
    fn read_after_the_fact(project: &Path, policy: &SandboxPolicy, paths: &[PathBuf]) -> String {
        let listed: Vec<String> = paths.iter().map(|p| format!("'{}'", p.display())).collect();
        let command = format!(
            "( until [ -e go ]; do sleep 0.02; done; \
              for f in {}; do cat \"$f\"; echo; done > out 2>/dev/null; \
              touch done ) & exit 0",
            listed.join(" ")
        );
        let confined = PlatformSandbox.confine(
            ProcessSpec::new("bash", project).args(["-c", &command].map(String::from)),
            policy,
        );
        assert_eq!(
            confined.enforcement,
            Enforcement::Full,
            "{:?}",
            confined.unmet
        );
        let mut child = std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .current_dir(project)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let _ = child.wait();
        for path in paths {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let tmp = path.with_file_name(".agix-rename.tmp");
            std::fs::write(&tmp, "agix-after-the-fact-secret").unwrap();
            std::fs::rename(&tmp, path).unwrap();
        }
        std::fs::write(project.join("go"), "").unwrap();
        let done = (0..500).any(|_| {
            if project.join("done").exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            false
        });
        assert!(done, "the background reader never finished");
        std::fs::read_to_string(project.join("out")).unwrap_or_default()
    }

    /// The layouts that keep the state root from being masked as a whole —
    /// the project is the state root, or a scene directory in it — and the
    /// project's own `.agix`: a credential the engine writes there after a
    /// command started, or rename-writes over an old one, does not reach
    /// what that command left running. On Linux the key files directly in
    /// a project that is the state root or a scene directory are the known
    /// limit (`mask`'s header) and not asserted; macOS denies them by
    /// pattern.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_credential_written_later_stays_unreadable_where_the_project_holds_credentials() {
        if !a_backend_runs() {
            return;
        }
        let pattern_held = cfg!(target_os = "macos");
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();

        let state = base.join("state-is-project");
        write_file(&state.join("scenes/code/settings.local.json"), "{}");
        write_file(&state.join("scenes/code/scene.json"), "fine");
        write_file(&state.join("mcp/servers.json"), "fine");
        write_file(&state.join("settings.local.json"), "{}");
        let policy = SandboxPolicy {
            state_root: Some(state.clone()),
            ..Default::default()
        };
        let mut later = vec![
            state.join("scenes/code/settings.local.json"),
            state.join("scenes/new/settings.json"),
            state.join("mcp/oauth/server.json"),
            state.join(".agix/settings.local.json"),
        ];
        if pattern_held {
            later.push(state.join("settings.local.json"));
        }
        let saw = read_after_the_fact(&state, &policy, &later);
        assert!(
            !saw.contains("after-the-fact"),
            "project = state root: {saw:?}"
        );

        let state = base.join("state");
        let scene = state.join("scenes/code");
        write_file(&state.join("settings.local.json"), "{}");
        write_file(&state.join("scenes/triage/settings.json"), "{}");
        write_file(&scene.join("settings.local.json"), "{}");
        write_file(&scene.join("scene.json"), "{}");
        let policy = SandboxPolicy {
            state_root: Some(state.clone()),
            ..Default::default()
        };
        let mut later = vec![
            state.join("settings.local.json"),
            state.join("settings.json"),
            state.join("scenes/triage/settings.json"),
            state.join("scenes/new/settings.json"),
            scene.join(".agix/mcp.local.json"),
        ];
        if pattern_held {
            later.push(scene.join("settings.local.json"));
        }
        let saw = read_after_the_fact(&scene, &policy, &later);
        assert!(!saw.contains("after-the-fact"), "project = scene: {saw:?}");
        assert!(
            run_confined(
                &scene,
                &policy,
                &[],
                "echo x > notes.md && ! echo x > scene.json"
            )
            .status
            .success(),
            "a scene opened as a project keeps its scene files read-only, the rest writable"
        );

        let project = base.join("project");
        write_file(&project.join(".agix/settings.json"), "fine");
        write_file(&project.join(".agix/settings.local.json"), "{}");
        std::fs::create_dir_all(project.join(".agix/scratchpad")).unwrap();
        let policy = SandboxPolicy {
            state_root: Some(base.join("elsewhere")),
            ..Default::default()
        };
        let later = [
            project.join(".agix/settings.local.json"),
            project.join(".agix/mcp.local.json"),
        ];
        let saw = read_after_the_fact(&project, &policy, &later);
        assert!(
            !saw.contains("after-the-fact"),
            "the project's .agix: {saw:?}"
        );
        assert!(
            run_confined(
                &project,
                &policy,
                &[],
                "cat .agix/settings.json && echo x > .agix/scratchpad/n"
            )
            .status
            .success(),
            "the project's .agix keeps its other entries, the working ones writable"
        );
        if cfg!(target_os = "linux") {
            assert!(
                !run_confined(&project, &policy, &[], "echo x > .agix/new")
                    .status
                    .success(),
                "a new entry directly in .agix was created"
            );
        }
    }

    /// A process an earlier command left running cannot read a credential the
    /// engine writes into the state root while a later command is confined.
    /// Credentials used to be masked by binding over each file at wrap time;
    /// a file absent then was never bound, and `/dev/null` bound over an
    /// existing one falls off when the engine rewrites it through a temporary
    /// file and a rename (both measured on bwrap 0.8). The state root's
    /// credential directories are now covered with a tmpfs instead, so neither
    /// a late create nor a rename reaches the confined command — while a
    /// readable sibling stays readable.
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_lingering_process_cannot_read_a_credential_written_after_it_started() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".agix")).unwrap();
        write_file(&state.join("skills/s/SKILL.md"), "fine");
        let existing = state.join("settings.local.json");
        write_file(&existing, "{\"old\":true}");
        let fresh = state.join("settings.json");
        let policy = SandboxPolicy {
            state_root: Some(state.clone()),
            ..Default::default()
        };
        let command = format!(
            "( until [ -e go ]; do sleep 0.02; done; \
              {{ cat '{existing}'; echo; cat '{fresh}'; echo; cat '{skill}'; }} > out 2>/dev/null; \
              touch done ) & exit 0",
            existing = existing.display(),
            fresh = fresh.display(),
            skill = state.join("skills/s/SKILL.md").display(),
        );
        let confined = PlatformSandbox.confine(
            ProcessSpec::new("bash", &project).args(["-c", &command].map(String::from)),
            &policy,
        );
        assert_eq!(
            confined.enforcement,
            Enforcement::Full,
            "{:?}",
            confined.unmet
        );
        let mut child = std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .current_dir(&project)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        let secret = "agix-after-the-fact-secret";
        std::fs::write(&fresh, secret).unwrap();
        let tmp = state.join(".settings.local.json.9999.tmp");
        std::fs::write(&tmp, secret).unwrap();
        std::fs::rename(&tmp, &existing).unwrap();
        std::fs::write(project.join("go"), "").unwrap();

        let done = (0..500).any(|_| {
            if project.join("done").exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            false
        });
        let _ = child.wait();
        assert!(done, "the background reader never finished");
        let saw = std::fs::read_to_string(project.join("out")).unwrap_or_default();
        assert!(
            !saw.contains(secret),
            "a lingering process read a credential written after it started: {saw:?}"
        );
        assert!(
            saw.contains("fine"),
            "the mask shut a readable file too: {saw:?}"
        );
    }

    /// Unattended, a command cannot have the keychain read for it through
    /// `securityd`, the way `security find-generic-password` does, nor type
    /// into a terminal multiplexer's session outside the sandbox. Attended,
    /// both stay: the user approves each such command, and `git push`
    /// through the keychain's credential helper or `codesign` need them.
    #[cfg(target_os = "macos")]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn unattended_a_command_cannot_reach_the_keychain_or_a_multiplexer() {
        if !a_backend_runs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let attended = SandboxPolicy {
            state_root: Some(dir.path().join("state")),
            ..Default::default()
        };
        let unattended = SandboxPolicy {
            deny_ipc_escapes: true,
            ..attended.clone()
        };
        let runs = |policy: &SandboxPolicy, command: &str| {
            run_confined(dir.path(), policy, &[], command)
                .status
                .success()
        };
        assert!(runs(&attended, "security list-keychains"));
        assert!(!runs(&unattended, "security list-keychains"));
        assert!(!runs(
            &unattended,
            "security find-generic-password -s agix-none"
        ));

        let uid = unsafe { libc::getuid() };
        let tmux = PathBuf::from(format!("/private/tmp/tmux-{uid}"));
        let made = !tmux.exists();
        if made {
            std::fs::create_dir(&tmux).unwrap();
        }
        let socket = listening(tmux.join(format!("agix-test-{}", std::process::id())));
        let nc = format!("nc -U '{}' < /dev/null", socket.display());
        let attended_reached = runs(&attended, &nc);
        let unattended_reached = runs(&unattended, &nc);
        let _ = std::fs::remove_file(&socket);
        if made {
            let _ = std::fs::remove_dir(&tmux);
        }
        assert!(attended_reached, "the multiplexer socket is not reachable");
        assert!(!unattended_reached, "an unattended command reached tmux");
    }
}

#[cfg(test)]
mod enforcement_tests {
    use super::*;

    fn opts<'a>(cmd: &'a str, cwd: &'a std::path::Path, disable: bool) -> SandboxOptions<'a> {
        SandboxOptions {
            command: cmd,
            cwd,
            additional_writable: &[],
            disable,
            policy: SandboxPolicy::default(),
        }
    }

    /// An explicit opt-out is unconstrained and says so. This is the case
    /// where `Enforcement::None` is the honest answer *and* nothing is wrong.
    #[test]
    fn disabling_the_sandbox_reports_no_enforcement() {
        let dir = std::env::temp_dir();
        let w = wrap(opts("echo hi", &dir, true));
        assert_eq!(w.mode, SandboxMode::Disabled);
        assert_eq!(w.enforcement, Enforcement::None);
    }

    /// The invariant the field exists for: `enforcement` and `mode` never
    /// disagree about whether anything is actually wrapping the command.
    /// A backend that reported `Full` while running bare `bash` would make
    /// every consumer's refusal check useless.
    #[test]
    fn enforcement_agrees_with_the_selected_backend() {
        let dir = std::env::temp_dir();
        let w = wrap(opts("echo hi", &dir, false));
        match w.mode {
            SandboxMode::Disabled | SandboxMode::Unavailable => {
                assert_eq!(w.enforcement, Enforcement::None);
                assert_eq!(
                    w.spec.program, "bash",
                    "an unenforced command must be plain bash"
                );
            }
            SandboxMode::MacOSSandboxExec => {
                assert_eq!(w.enforcement, Enforcement::Full);
                assert_eq!(w.spec.program, "sandbox-exec");
            }
            SandboxMode::LinuxBwrap => {
                assert_eq!(w.enforcement, Enforcement::Full);
                assert_eq!(w.spec.program, "sh");
                assert_eq!(w.spec.args[3], "bwrap");
            }
        }
    }
}
