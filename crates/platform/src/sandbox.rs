//! The platform sandbox backends: macOS `sandbox-exec` and Linux `bubblewrap`.
//!
//! - **macOS**: `sandbox-exec -p <profile> <program> <args…>`. The profile is
//!   TinyScheme denying writes outside the allowed subtrees; exec is not
//!   restricted. Network defaults to open; `DenyAll` / `Allowlist` both use
//!   `(deny network*)`, and the allowlist re-allows named domains. DNS goes
//!   through `mDNSResponder` IPC, which `network*` does not cover, so name
//!   resolution keeps working under both restricted modes.
//! - **Linux**: `bwrap --ro-bind / / --bind <writable> <writable> … <program>`.
//!   No bwrap on PATH means no constraint, reported as such. `DenyAll` uses
//!   `--unshare-net`; bwrap has no domain-level filter, so `Allowlist`
//!   degrades to the same whole-network cut rather than silently opening up —
//!   failure goes toward the safer side, and says so through `Enforcement`.
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
//! `.git/{config,config.worktree,commondir}` and `.git/hooks/`, the
//! configuration under `.agix/` and `.claude/` (`base::paths::ENGINE_CONFIG_*`:
//! settings, MCP, hooks, agents, skills, plugins, rules, scenes, policy,
//! scripts, output styles), and the `.git` links of the engine's worktrees.
//! Of the state root only `scratchpad/`, `memory/` and those of
//! `additional_writable` under `tmp/` (a session's own `TMPDIR`) stay
//! writable. On Linux `.agix`, and `.claude` where it exists, is read-only as
//! a whole and its existing working entries are opened again, so a new entry
//! directly in it cannot be created either.
//!
//! A policy with `only_additional_writable` leaves out the working directory,
//! the state root's working entries and `DARWIN_USER_TEMP_DIR`: only
//! `additional_writable` and `/dev` are writable, the read-only entries above
//! still apply inside them. The engine's own git runs under such a policy in
//! every session whose commands are sandboxed (`frozen::git::EngineGit`), so a
//! filter driver or other program a repository's configuration names runs
//! confined, writing at most the git directory and `.agix/worktrees/` when it
//! creates a worktree, and only the session's `TMPDIR` otherwise.
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
//! **The whole file is behind the `sandbox` feature, off by default.** What it
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
//! 3. **`/proc` leak**: bwrap mounts the host's `/proc`, reachable through
//!    `/proc/self/{fd,root}`.
//! 4. **Leaked fds**: `CLOEXEC` is not set, so a child can read inherited
//!    descriptors — git repositories, sockets.
//! 5. **Absent git files on Linux, for a git outside the sandbox**: a mount
//!    cannot stand in for a file that does not exist without leaving an empty
//!    one behind, which git refuses to read. So a project with no `.git` yet
//!    can be given one, and a repository with no `commondir` can be given one,
//!    with whatever configuration and filter drivers the command likes. The
//!    engine's own git runs inside the sandbox, so it carries none of that out
//!    unconfined; a git the user runs in the project afterwards does. macOS
//!    denies both.
//! 6. **Another process acting for the command**: the sandbox follows the
//!    command's own process tree, not what it asks a system service to do.
//!    On macOS, measured against `sandbox-exec`: `open` has LaunchServices
//!    start an app outside the sandbox, `defaults write` has `cfprefsd` write
//!    any preference domain, and an Apple event reaches an app that runs
//!    unconfined (Finder itself refuses events from a sandboxed sender, other
//!    apps need not). launchd refuses `launchctl submit` / `bootstrap` from a
//!    sandboxed caller. A policy with `deny_ipc_escapes` — every unattended
//!    session's — denies `lsopen`, `user-preference-write` and
//!    `appleevent-send`. On Linux the same policy adds `--unshare-ipc` and
//!    hides the session bus and the systemd user manager's socket under
//!    `$XDG_RUNTIME_DIR` (and the bus path `DBUS_SESSION_BUS_ADDRESS` names),
//!    so `systemd-run --user` has nothing to talk to; an abstract-socket bus
//!    is reachable unless the network is cut too. The Linux half has not been
//!    run against a real bubblewrap. With the network open, any local
//!    service listening on a socket the command can reach (sshd, a container
//!    daemon) is a way out under every policy.

pub use base::interface::exec::{
    default_deny_read, Confined, Enforcement, NetworkMode, ProcessSpec, Sandbox, SandboxMode,
    SandboxPolicy,
};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// What this process serves its clients on.
static SERVED: RwLock<Served> = RwLock::new(Served {
    sockets: Vec::new(),
    ports: Vec::new(),
    secrets: Vec::new(),
});

struct Served {
    sockets: Vec<PathBuf>,
    ports: Vec<SocketAddr>,
    secrets: Vec<PathBuf>,
}

/// From now on, no command this process confines may connect to the unix
/// socket at `path`. For a daemon's own listener, wherever `--socket` put it:
/// a command connected to it would be its host.
pub fn fence_socket(path: &Path) {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut served = SERVED.write().unwrap_or_else(|e| e.into_inner());
    if !served.sockets.contains(&path) {
        served.sockets.push(path);
    }
}

/// [`fence_socket`] for a TCP listener. Only the macOS backend can refuse a
/// port; bwrap can cut the network only as a whole.
pub fn fence_port(addr: SocketAddr) {
    let mut served = SERVED.write().unwrap_or_else(|e| e.into_inner());
    if !served.ports.contains(&addr) {
        served.ports.push(addr);
    }
}

/// From now on, no command this process confines may read the file at
/// `path`, whatever `sandbox.allow_read` says. For a credential the daemon
/// writes for its own clients, such as the unix socket's token.
pub fn fence_secret(path: &Path) {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut served = SERVED.write().unwrap_or_else(|e| e.into_inner());
    if !served.secrets.contains(&path) {
        served.secrets.push(path);
    }
}

#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
fn fenced_secrets() -> Vec<PathBuf> {
    let served = SERVED.read().unwrap_or_else(|e| e.into_inner());
    served
        .secrets
        .iter()
        .filter(|p| p.exists())
        .cloned()
        .collect()
}

/// The fenced sockets that are still there.
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
fn fenced_sockets() -> Vec<PathBuf> {
    let served = SERVED.read().unwrap_or_else(|e| e.into_inner());
    served
        .sockets
        .iter()
        .filter(|p| is_socket(p))
        .cloned()
        .collect()
}

#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn fenced_ports() -> Vec<SocketAddr> {
    SERVED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .ports
        .clone()
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
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".agix")))
}

/// Fallback set of scenes to protect when a caller doesn't supply
/// `SandboxPolicy::known_scenes` — this crate deliberately doesn't depend
/// on the `scene` crate (`tools` sits below it in the dependency graph), so
/// it can't resolve a daemon's actual `--scenes` set itself. A daemon that
/// knows its real active scenes (`SessionPool::scene_registry`) should
/// populate `known_scenes` instead of relying on this; this constant only
/// covers the three builtins, so a deployment running a scene this list
/// doesn't know about would otherwise get no protection for that scene's
/// `settings.json` at all. Keep in sync with `daemon::main::resolve_scene`
/// and `crates/scene/src/*.rs` regardless, since it's still the
/// default every caller gets today.
#[cfg(feature = "sandbox")]
const KNOWN_SCENES: &[&str] = &["code", "harnix", "mini"];

/// `policy.known_scenes` if the caller supplied one, else [`KNOWN_SCENES`].
#[cfg(feature = "sandbox")]
fn effective_known_scenes(policy: &SandboxPolicy) -> Vec<&str> {
    if policy.known_scenes.is_empty() {
        KNOWN_SCENES.to_vec()
    } else {
        policy.known_scenes.iter().map(String::as_str).collect()
    }
}

/// The files under `<state root>/scenes/<scene>/` a sandboxed command must not
/// rewrite: the scene's settings, and — for a declarative scene — the files
/// that define it.
#[cfg(feature = "sandbox")]
const SCENE_FILES: &[&str] = &["settings.json", "scene.json", "prompt.md"];

/// Every scene directory to protect: the known scenes, and every directory
/// that exists under `<state root>/scenes/` — a declarative scene is whatever
/// a directory there defines, so no list of names can be complete.
#[cfg(feature = "sandbox")]
fn protected_scene_files(policy: &SandboxPolicy, state_root: &Path) -> Vec<PathBuf> {
    let mut scenes: std::collections::BTreeSet<String> = effective_known_scenes(policy)
        .into_iter()
        .map(str::to_string)
        .collect();
    if let Ok(entries) = std::fs::read_dir(state_root.join("scenes")) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    scenes.insert(name.to_string());
                }
            }
        }
    }
    scenes
        .iter()
        .flat_map(|scene| {
            SCENE_FILES
                .iter()
                .map(move |file| state_root.join("scenes").join(scene).join(file))
        })
        .collect()
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
    wrap_program(inner, opts.cwd, opts.additional_writable, &opts.policy)
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
            mac_wrap(inner, cwd, additional_writable, policy)
        }

        #[cfg(target_os = "linux")]
        {
            linux_wrap(inner, cwd, additional_writable, policy)
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

/// Files in a git directory that decide which programs git runs and where it
/// reads its configuration from.
#[cfg(feature = "sandbox")]
const GIT_CONTROL_FILES: &[&str] = &["config", "config.worktree", "commondir"];

#[cfg(feature = "sandbox")]
use base::paths::{
    ENGINE_CONFIG_DIRS, ENGINE_CONFIG_FILES, ENGINE_CONFIG_ROOTS, PROJECT_WORKTREES_DIR,
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
}

#[cfg(feature = "sandbox")]
impl Roots {
    fn resolve(cwd: &Path, additional: &[PathBuf], policy: &SandboxPolicy) -> Self {
        Self {
            cwd: resolve_path(cwd),
            only_additional: policy.only_additional_writable,
            writable: additional.iter().map(|p| resolve_path(p)).collect(),
            state_root: state_root_of(policy).map(|p| resolve_path(&p)),
        }
    }

    /// Whether a write to `path` falls inside one of the writable roots.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn opens(&self, path: &Path) -> bool {
        (!self.only_additional && path.starts_with(&self.cwd))
            || self.writable.iter().any(|root| path.starts_with(root))
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
            .filter(|root| !self.only_additional && !self.cwd.starts_with(root))
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

/// What in the project a sandboxed command must not write, because the
/// engine, or the user's own git, acts on it later outside the sandbox.
#[cfg(feature = "sandbox")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct ProjectGuard {
    /// `.git` and the configuration roots themselves: renaming one away, or
    /// replacing it with a symlink, would put every rule below on a path
    /// nobody reads.
    entries: Vec<PathBuf>,
    files: Vec<PathBuf>,
    trees: Vec<PathBuf>,
}

#[cfg(feature = "sandbox")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn project_guard(cwd: &Path) -> ProjectGuard {
    let git = cwd.join(".git");
    let git_dir = resolve_path(&git);
    let agix_dir = resolve_path(&cwd.join(".agix"));

    let mut entries = vec![git.clone()];
    if git_dir != git {
        entries.push(git_dir.clone());
    }
    let mut files: Vec<PathBuf> = GIT_CONTROL_FILES.iter().map(|f| git_dir.join(f)).collect();
    let mut trees = vec![git_dir.join("hooks")];
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

    for worktree in subdirs(&git_dir.join("worktrees")) {
        files.extend(GIT_CONTROL_FILES.iter().map(|f| worktree.join(f)));
    }
    for worktree in subdirs(&agix_dir.join(PROJECT_WORKTREES_DIR)) {
        files.push(worktree.join(".git"));
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
) -> Confined {
    let profile = build_macos_profile(cwd, additional_writable, policy);
    Confined {
        enforcement: Enforcement::Full,
        unmet: Vec::new(),
        spec: ProcessSpec::new("sandbox-exec", &inner.cwd)
            .arg("-p")
            .arg(profile)
            .arg(inner.program)
            .args(inner.args),
        mode: SandboxMode::MacOSSandboxExec,
    }
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
fn build_macos_profile(cwd: &Path, additional: &[PathBuf], policy: &SandboxPolicy) -> String {
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
    if let Some(state_root) = &roots.state_root {
        s.push_str(&format!(
            "(deny file-write* (literal \"{}\"))\n",
            path(&state_root.join("settings.json"))
        ));
        for p in protected_scene_files(policy, state_root) {
            s.push_str(&format!("(deny file-write* (literal \"{}\"))\n", path(&p)));
        }
    }

    if !policy.deny_read.is_empty() {
        for p in &policy.deny_read {
            s.push_str(&format!(
                "(deny file-read* (subpath \"{}\"))\n",
                sandbox_escape(&p.display().to_string())
            ));
        }
        for p in &policy.allow_read {
            s.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                sandbox_escape(&p.display().to_string())
            ));
        }
    }

    // `launchctl submit` / `bootstrap` need no rule: launchd refuses a job
    // from a sandboxed caller.
    if policy.deny_ipc_escapes {
        s.push_str("(deny appleevent-send)\n");
        s.push_str("(deny lsopen)\n");
        s.push_str("(deny user-preference-write)\n");
    }

    // DNS is IPC to mDNSResponder, which `network*` does not cover, so name
    // resolution survives both restricted modes; `remote tcp` matches by
    // hostname.
    match policy.network_mode {
        NetworkMode::Unrestricted => {}
        NetworkMode::DenyAll => {
            s.push_str("(deny network*)\n");
        }
        NetworkMode::Allowlist => {
            s.push_str("(deny network*)\n");
            for domain in &policy.allowed_domains {
                s.push_str(&format!(
                    "(allow network-outbound (remote tcp \"{}:*\"))\n",
                    sandbox_escape(domain)
                ));
            }
        }
    }

    // Last, so that no allowlist entry above reopens them. The kernel matches
    // a unix-socket connect against the socket's resolved path, so a symlink
    // or a relative name meets the same rule.
    if let Some(root) = roots
        .state_root
        .as_deref()
        .filter(|root| !roots.cwd.starts_with(root))
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
/// working entries are opened again one by one;
/// a configuration file that is not there yet cannot be created because its
/// directory is read-only. `.git` cannot be treated that way — git creates
/// its lock files directly in it — so it is bound onto itself, which makes it
/// a mount point that cannot be renamed or removed, and its control files are
/// mounted read-only where they exist.
///
/// A `.git` or `.agix` no writable root reaches is read-only already, and is
/// left alone: binding it writable would open it, and a mount point for an
/// absent entry cannot be made on a read-only parent.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn guard_project(args: &mut Vec<String>, roots: &Roots) {
    let git = roots.cwd.join(".git");
    if git.is_dir() {
        let open = roots.opens(&git);
        if open {
            bind(args, "--bind", &git);
        }
        for f in GIT_CONTROL_FILES {
            bind(args, "--ro-bind-try", &git.join(f));
        }
        for worktree in subdirs(&git.join("worktrees")) {
            for f in GIT_CONTROL_FILES {
                bind(args, "--ro-bind-try", &worktree.join(f));
            }
        }
        if open {
            read_only(args, &git.join("hooks"));
        } else {
            bind(args, "--ro-bind-try", &git.join("hooks"));
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
        if !held || !roots.opens(&root) {
            continue;
        }
        read_only(args, &root);
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let configures = name.to_str().is_some_and(|n| {
                    ENGINE_CONFIG_FILES.contains(&n) || ENGINE_CONFIG_DIRS.contains(&n)
                });
                if !configures && entry.file_type().is_ok_and(|t| !t.is_symlink()) {
                    bind(args, "--bind", &entry.path());
                }
            }
        }
    }
    let agix = roots.cwd.join(".agix");
    for worktree in subdirs(&agix.join(PROJECT_WORKTREES_DIR)) {
        bind(args, "--ro-bind-try", &worktree.join(".git"));
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

#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn linux_wrap(
    inner: ProcessSpec,
    cwd: &Path,
    additional_writable: &[PathBuf],
    policy: &SandboxPolicy,
) -> Confined {
    if !bwrap_available() {
        return plain(inner, SandboxMode::Unavailable);
    }
    let roots = Roots::resolve(cwd, additional_writable, policy);
    // bwrap applies mounts in argument order, so each one below lands on top
    // of the ones before it.
    let mut args: Vec<String> = vec!["--ro-bind".into(), "/".into(), "/".into()];
    if !roots.only_additional {
        bind(&mut args, "--bind", &roots.cwd);
    }
    args.extend(
        [
            "--tmpfs",
            "/tmp",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--unshare-pid",
            "--die-with-parent",
        ]
        .map(String::from),
    );
    for p in &roots.writable {
        bind(&mut args, "--bind-try", p);
    }
    if let Some(root) = roots.fenced_state_root() {
        if root.is_dir() {
            bind(&mut args, "--ro-bind", root);
            for dir in STATE_ROOT_WORKING_DIRS {
                bind(&mut args, "--bind-try", &root.join(dir));
            }
            for dir in roots.session_temp_dirs_in(root) {
                bind(&mut args, "--bind-try", dir);
            }
        }
    }
    guard_project(&mut args, &roots);
    if let Some(state_root) = &roots.state_root {
        bind(
            &mut args,
            "--ro-bind-try",
            &state_root.join("settings.json"),
        );
        for p in protected_scene_files(policy, state_root) {
            bind(&mut args, "--ro-bind-try", &p);
        }
    }
    for socket in daemon_sockets(roots.state_root.as_deref()) {
        bind_over(&mut args, Path::new("/dev/null"), &socket);
    }
    for secret in fenced_secrets() {
        bind_over(&mut args, Path::new("/dev/null"), &secret);
    }

    // Deny-read: an empty tmpfs over a directory, /dev/null over a file.
    let allow_set: std::collections::HashSet<PathBuf> = policy.allow_read.iter().cloned().collect();
    for p in &policy.deny_read {
        if allow_set.contains(p) || !p.exists() {
            continue;
        }
        let s = p.display().to_string();
        if p.is_dir() {
            args.push("--tmpfs".into());
            args.push(s);
        } else {
            args.push("--ro-bind-try".into());
            args.push("/dev/null".into());
            args.push(s);
        }
    }

    if policy.deny_ipc_escapes {
        args.push("--unshare-ipc".into());
        for bus in session_bus_paths() {
            if bus.is_dir() {
                args.extend(["--tmpfs".into(), bus.display().to_string()]);
            } else if bus.exists() {
                bind_over(&mut args, Path::new("/dev/null"), &bus);
            }
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
    args.push(inner.program);
    args.extend(inner.args);
    Confined {
        enforcement: if unmet.is_empty() {
            Enforcement::Full
        } else {
            Enforcement::Partial
        },
        unmet,
        spec: ProcessSpec::new("bwrap", &inner.cwd).args(args),
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
            stdin,
        } = spec;
        let inner = ProcessSpec::new(program, Path::new(".")).args(args);
        let mut confined = wrap_program(inner, &cwd, &policy.additional_writable, policy);
        confined.spec.cwd = cwd;
        confined.spec.env = env;
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
            c.spec.args.iter().filter(|a| a.as_str() == "-c").count(),
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
                assert_eq!(cmd.spec.program, "bwrap");
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
        let args = &cmd.spec.args;
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == agix && w[2] == agix),
            "{args:?}"
        );
        let scratchpad = format!("{agix}/scratchpad");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--bind" && w[1] == scratchpad),
            "{args:?}"
        );
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wrap_re_denies_writes_to_global_and_scene_settings_json() {
        let cmd = wrap(opts("ls", Path::new("/tmp/work")));
        if cmd.mode != SandboxMode::LinuxBwrap {
            return;
        }
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home_str = std::path::Path::new(&home).display().to_string();
        assert!(cmd
            .spec
            .args
            .iter()
            .any(|a| a == &format!("{home_str}/.agix/settings.json")));
        for scene in KNOWN_SCENES {
            let needle = format!("{home_str}/.agix/scenes/{scene}/settings.json");
            assert!(
                cmd.spec.args.iter().any(|a| a == &needle),
                "expected a --ro-bind-try re-mount for scene `{scene}`'s settings.json"
            );
        }
    }

    #[test]
    fn effective_known_scenes_falls_back_to_the_builtin_const_when_unset() {
        let policy = SandboxPolicy::default();
        assert_eq!(effective_known_scenes(&policy), KNOWN_SCENES.to_vec());
    }

    #[test]
    fn effective_known_scenes_prefers_the_policys_own_list_when_set() {
        let policy = SandboxPolicy {
            known_scenes: vec!["custom-scene".to_string()],
            ..Default::default()
        };
        assert_eq!(effective_known_scenes(&policy), vec!["custom-scene"]);
    }

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
            known_scenes: Vec::new(),
            state_root: None,
            additional_writable: Vec::new(),
            only_additional_writable: false,
            deny_ipc_escapes: false,
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
            known_scenes: Vec::new(),
            state_root: None,
            additional_writable: Vec::new(),
            only_additional_writable: false,
            deny_ipc_escapes: false,
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
    fn macos_allowlist_network_denies_then_allows_each_domain() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["api.example.com".into(), "registry.npmjs.org".into()];
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
        assert!(
            profile.contains("(allow network-outbound (remote tcp \"api.example.com:*\"))"),
            "profile:\n{profile}"
        );
        assert!(
            profile.contains("(allow network-outbound (remote tcp \"registry.npmjs.org:*\"))"),
            "profile:\n{profile}"
        );
        // Both allow rules must come after the deny for sandbox-exec's
        // top-to-bottom evaluation order to actually re-permit them.
        let deny_idx = profile.find("(deny network*)").unwrap();
        let allow_idx = profile.find("(allow network-outbound").unwrap();
        assert!(allow_idx > deny_idx);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allowlist_with_no_domains_still_denies_network() {
        // Empty allowed_domains under Allowlist == DenyAll in effect (no
        // allow rules to re-permit anything).
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec![];
        let cmd = wrap(o);
        let profile = network_mode_rules(&cmd.spec.args[1]);
        assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
        assert!(!profile.contains("network-outbound"), "profile:\n{profile}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_network_domain_is_escaped() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["evil\".com".into()];
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        assert!(profile.contains("evil\\\".com"), "profile:\n{profile}");
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
    /// directories and over every socket this process serves, after the
    /// state root's own read-only mount so it lands on top.
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
        fence_socket(&served);

        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.state_root = Some(state.path().to_path_buf());
        let args = wrap(o).spec.args;
        if !args.iter().any(|a| a == "--") {
            return;
        }
        let state_mount = args
            .windows(2)
            .position(|w| w[0] == "--ro-bind" && Path::new(&w[1]) == resolve_path(state.path()))
            .expect("the state root is mounted read-only");
        for socket in [&in_state, &served] {
            let socket = resolve_path(socket).display().to_string();
            let hidden = args
                .windows(3)
                .position(|w| w[0] == "--ro-bind-try" && w[1] == "/dev/null" && w[2] == socket)
                .unwrap_or_else(|| panic!("{socket} is not hidden: {args:?}"));
            assert!(
                hidden > state_mount,
                "{socket} is hidden under the state root's mount"
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
                known_scenes: Vec::new(),
                state_root: None,
                additional_writable: Vec::new(),
                only_additional_writable: false,
                deny_ipc_escapes: false,
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
                known_scenes: Vec::new(),
                state_root: None,
                additional_writable: Vec::new(),
                only_additional_writable: false,
                deny_ipc_escapes: false,
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

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_protects_user_level_settings_json_for_every_known_scene() {
        // The sandbox doesn't know which scene the current session uses (no
        // plumbing for that in `ToolContext`), so it must protect all of
        // them — not just a single hardcoded one.
        let o = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        assert!(
            !KNOWN_SCENES.is_empty(),
            "sanity: scene list must not be empty"
        );
        for scene in KNOWN_SCENES {
            let needle = format!(".agix/scenes/{scene}/settings.json\"))");
            assert!(
                profile.contains(&needle),
                "expected a deny-write rule for scene `{scene}`'s settings.json, profile:\n{profile}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_protects_global_settings_json() {
        let o = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home_str = std::path::Path::new(&home).display().to_string();
        let needle = format!("(deny file-write* (literal \"{home_str}/.agix/settings.json\"))");
        assert!(
            profile.contains(&needle),
            "expected a deny-write rule for the cross-scene global settings.json, profile:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_empty_deny_read_skips_read_rules_entirely() {
        // When deny_read is empty, no file-read* deny or allow rules should
        // appear in the profile (structural optimization).
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
                known_scenes: Vec::new(),
                state_root: None,
                additional_writable: Vec::new(),
                only_additional_writable: false,
                deny_ipc_escapes: false,
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        assert!(
            !profile.contains("(deny file-read*"),
            "no deny-read rules when deny_read is empty"
        );
    }

    /// A declarative scene is defined by the files in its directory, so every
    /// scene directory on disk is protected — not just the names the engine
    /// knows — and its definition along with its settings.
    #[test]
    #[cfg(target_os = "macos")]
    fn every_scene_directory_on_disk_is_protected() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("scenes").join("triage")).unwrap();
        let policy = SandboxPolicy {
            state_root: Some(root.path().to_path_buf()),
            ..Default::default()
        };
        let profile = build_macos_profile(Path::new("/work/project"), &[], &policy);
        let scene_dir = root
            .path()
            .canonicalize()
            .unwrap()
            .join("scenes")
            .join("triage");
        for file in ["settings.json", "scene.json", "prompt.md"] {
            let rule = format!(
                "(deny file-write* (literal \"{}\"))",
                sandbox_escape(&scene_dir.join(file).display().to_string())
            );
            assert!(profile.contains(&rule), "missing {rule}");
        }
        assert!(profile.contains("/scenes/code/scene.json"));
    }

    /// The profile has to protect the settings.json this instance actually
    /// reads. When the state root is redirected, denying writes under the
    /// invoking user's home guards a file nobody uses and leaves the real one
    /// writable — which is a sandbox escape, since settings.json is where the
    /// permission rules live.
    #[test]
    #[cfg(target_os = "macos")]
    fn the_profile_protects_the_instance_root_it_was_given() {
        let policy = SandboxPolicy {
            state_root: Some(PathBuf::from("/srv/agix-state")),
            known_scenes: vec!["code".into()],
            ..Default::default()
        };
        let profile = build_macos_profile(Path::new("/work/project"), &[], &policy);

        assert!(profile.contains("(deny file-write* (literal \"/srv/agix-state/settings.json\"))"));
        assert!(profile.contains(
            "(deny file-write* (literal \"/srv/agix-state/scenes/code/settings.json\"))"
        ));
        // The project-level rules under cwd stay; what must be absent is any
        // rule derived from the invoking user's home.
        if let Some(home) = std::env::var_os("HOME") {
            let home_rule = format!(
                "(deny file-write* (literal \"{}/.agix/settings.json\"))",
                PathBuf::from(home).display()
            );
            assert!(
                !profile.contains(&home_rule),
                "a redirected instance is still protecting the home-relative path"
            );
        }
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

        fn run(&self, command: &str, deny_ipc_escapes: bool) -> bool {
            let additional = vec![self.tmp.clone()];
            let confined = wrap(SandboxOptions {
                command,
                cwd: &self.project,
                additional_writable: &additional,
                disable: false,
                policy: SandboxPolicy {
                    state_root: Some(self.state_root.clone()),
                    deny_ipc_escapes,
                    ..Default::default()
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
        fence_socket(&served);

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
        fence_secret(&secret);
        assert!(s.runs(&format!("cat '{}'", other.display())));
        assert!(!s.runs(&format!("cat '{}'", secret.display())));
    }

    /// The loopback port a daemon listens on is refused the same way: the
    /// token is not in a command's environment, but `--token` is in `ps`.
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
        fence_port(served.local_addr().unwrap());
        let served = accepting(served);
        let other = accepting(std::net::TcpListener::bind("127.0.0.1:0").unwrap());

        assert!(s.runs(&format!("nc -z 127.0.0.1 {other}")));
        for host in ["127.0.0.1", "localhost"] {
            assert!(
                !s.runs(&format!("nc -z {host} {served}")),
                "{host}:{served} was reached"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_profile_refuses_unix_sockets_in_the_state_root_and_the_fenced_ones() {
        let socket = std::env::temp_dir().join(format!("fenced-{}.sock", std::process::id()));
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        fence_socket(&socket);
        let policy = SandboxPolicy {
            state_root: Some(PathBuf::from("/srv/agix-state")),
            network_mode: NetworkMode::Allowlist,
            allowed_domains: vec!["localhost".into()],
            ..Default::default()
        };
        let profile = build_macos_profile(Path::new("/work/project"), &[], &policy);
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
        fence_secret(&secret);
        let policy = SandboxPolicy {
            deny_read: vec![PathBuf::from("/nowhere")],
            allow_read: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let profile = build_macos_profile(Path::new("/work/project"), &[], &policy);
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

    /// No root given: fall back to the conventional location rather than
    /// protecting nothing.
    #[test]
    #[cfg(target_os = "macos")]
    fn without_a_root_it_falls_back_to_the_conventional_home_path() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home = PathBuf::from(home);
        let profile =
            build_macos_profile(Path::new("/work/project"), &[], &SandboxPolicy::default());
        assert!(profile.contains(&format!(
            "(deny file-write* (literal \"{}/.agix/settings.json\"))",
            home.display()
        )));
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
                assert_eq!(w.spec.program, "bwrap");
            }
        }
    }
}
