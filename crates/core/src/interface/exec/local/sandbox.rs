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
//! ways out, none of them closed:
//!
//! 1. **Writable `/dev`**: the macOS profile allows `file-write*` to `/dev`,
//!    so a process can `mknod` a raw disk device and bypass file ACLs.
//! 2. **Inherited environment**: `LD_PRELOAD` / `DYLD_INSERT_LIBRARIES` are
//!    not cleared, so a dynamic library can be injected.
//! 3. **`/proc` leak**: bwrap mounts the host's `/proc`, reachable through
//!    `/proc/self/{fd,root}`.
//! 4. **Leaked fds**: `CLOEXEC` is not set, so a child can read inherited
//!    descriptors — git repositories, sockets.

pub use crate::interface::exec::{
    default_deny_read, Confined, Enforcement, NetworkMode, ProcessSpec, Sandbox, SandboxMode,
    SandboxPolicy,
};
use std::path::{Path, PathBuf};

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
/// covers the four builtins, so a deployment running a scene this list
/// doesn't know about would otherwise get no protection for that scene's
/// `settings.json` at all. Keep in sync with `daemon::main::resolve_scene`
/// and `crates/scene/src/scene/*.rs` regardless, since it's still the
/// default every caller gets today.
#[cfg(feature = "sandbox")]
const KNOWN_SCENES: &[&str] = &["coding", "chat", "demo", "research"];

/// `policy.known_scenes` if the caller supplied one, else [`KNOWN_SCENES`].
#[cfg(feature = "sandbox")]
fn effective_known_scenes(policy: &SandboxPolicy) -> Vec<&str> {
    if policy.known_scenes.is_empty() {
        KNOWN_SCENES.to_vec()
    } else {
        policy.known_scenes.iter().map(String::as_str).collect()
    }
}

#[derive(Debug, Clone)]
pub struct SandboxOptions<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub additional_writable: &'a [PathBuf],
    pub disable: bool,
    /// **Hardening **: extended policy (deny-read, network mode, etc).
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
    if opts.disable {
        return plain(opts.command, SandboxMode::Disabled);
    }

    #[cfg(not(feature = "sandbox"))]
    {
        unavailable(
            opts.command,
            "this build has no sandbox backend; rebuild with `--features sandbox`",
        )
    }

    #[cfg(feature = "sandbox")]
    {
        #[cfg(target_os = "macos")]
        {
            mac_wrap(opts)
        }

        #[cfg(target_os = "linux")]
        {
            linux_wrap(opts)
        }

        // Windows / other unsupported platforms
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            plain(opts.command, SandboxMode::Unavailable)
        }
    }
}

/// A command nothing wrapped, and the reason in words the operator can act on.
#[cfg(not(feature = "sandbox"))]
fn unavailable(command: &str, why: &str) -> Confined {
    Confined {
        spec: bash_spec(command),
        unmet: vec![why.to_string()],
        mode: SandboxMode::Unavailable,
        enforcement: Enforcement::None,
    }
}

fn plain(command: &str, mode: SandboxMode) -> Confined {
    Confined {
        spec: bash_spec(command),
        unmet: vec!["nothing is constrained".into()],
        mode,
        // Nothing wraps this command, so nothing constrains it — true whether
        // the user asked for no sandbox or the platform could not provide one.
        // Which of those it was is `mode`'s job to say.
        enforcement: Enforcement::None,
    }
}

#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn mac_wrap(opts: SandboxOptions<'_>) -> Confined {
    let profile = build_macos_profile(opts.cwd, opts.additional_writable, &opts.policy);
    let inner = bash_spec(opts.command);
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

#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn build_macos_profile(cwd: &Path, additional: &[PathBuf], policy: &SandboxPolicy) -> String {
    // 默认放行（所有 file-read / process / network / signal / mach 等），
    // 然后单独 deny file-write*，再 allow 我们指定的几个 subpath。
    //
    // /private/tmp 与 /private/var/folders 是 macOS 临时区域；很多工具会写到那里。
    // 不放它们，rm/build/git 之类的会被杀。
    let mut s = String::with_capacity(1024);
    s.push_str("(version 1)\n");
    s.push_str("(allow default)\n");

    // ---- write policy ----
    s.push_str("(deny file-write*)\n");
    s.push_str("(allow file-write*\n");
    s.push_str(&format!(
        "  (subpath \"{}\")\n",
        sandbox_escape(&cwd.display().to_string())
    ));
    s.push_str("  (subpath \"/private/tmp\")\n");
    s.push_str("  (subpath \"/private/var/folders\")\n");
    s.push_str("  (subpath \"/private/var/tmp\")\n");
    s.push_str("  (subpath \"/dev\")\n");
    for p in additional {
        s.push_str(&format!(
            "  (subpath \"{}\")\n",
            sandbox_escape(&p.display().to_string())
        ));
    }
    s.push_str(")\n");
    // **Q4-followup **: re-deny writes to settings.json files even though
    // they sit inside cwd. Stops Bash-driven sandbox escapes via agix
    // overwriting its own permission rules. Aligns with TS sandbox-adapter.ts.
    // Project-level settings.json is cwd-relative and flat (no scope
    // segment).
    let cwd_str = cwd.display().to_string();
    s.push_str(&format!(
        "(deny file-write* (literal \"{}/.agix/settings.json\"))\n",
        sandbox_escape(&cwd_str)
    ));
    s.push_str(&format!(
        "(deny file-write* (literal \"{}/.agix/settings.local.json\"))\n",
        sandbox_escape(&cwd_str)
    ));
    if let Some(state_root) = state_root_of(policy) {
        let root_str = state_root.display().to_string();
        // Cross-scene global settings.json — flat, single file.
        s.push_str(&format!(
            "(deny file-write* (literal \"{}/settings.json\"))\n",
            sandbox_escape(&root_str)
        ));
        // Scene-specific settings.json lives at
        // `<state root>/scenes/<scene>/settings.json`, where `<scene>` is one of
        // the small, closed set of scenes this engine registers (see
        // `daemon::resolve_scene` / `KNOWN_SCENES` below) — not an arbitrary
        // string, so we can just enumerate all of them rather than needing
        // to know which one the current session actually uses.
        for scene in effective_known_scenes(policy) {
            s.push_str(&format!(
                "(deny file-write* (literal \"{}/scenes/{}/settings.json\"))\n",
                sandbox_escape(&root_str),
                scene
            ));
        }
    }

    // ---- **Hardening **: deny-read for credential paths ----
    if !policy.deny_read.is_empty() {
        for p in &policy.deny_read {
            // Use subpath so /aws/ children are also denied.
            s.push_str(&format!(
                "(deny file-read* (subpath \"{}\"))\n",
                sandbox_escape(&p.display().to_string())
            ));
        }
        // Re-allow specific entries the user explicitly opted back in via
        // sandbox.allow_read. macOS sandbox-exec evaluates rules top-to-bottom
        // so allows AFTER denies win.
        for p in &policy.allow_read {
            s.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                sandbox_escape(&p.display().to_string())
            ));
        }
    }

    // ---- **Hardening **: network policy ----
    // `(allow default)` at the top of the profile already covers unrestricted
    // networking — Unrestricted needs no extra rules. DenyAll/Allowlist add a
    // blanket `(deny network*)` (does not affect DNS: that's IPC to
    // mDNSResponder on macOS, not covered by the `network*` filter category),
    // then Allowlist re-allows outbound TCP to each configured domain —
    // sandbox-exec's `remote tcp` filter matches by hostname, not just IP.
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
fn linux_wrap(opts: SandboxOptions<'_>) -> Confined {
    if !bwrap_available() {
        return plain(opts.command, SandboxMode::Unavailable);
    }
    let mut args: Vec<String> = vec![
        // 文件系统：root 只读，cwd / tmp 可写
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--bind".into(),
        opts.cwd.display().to_string(),
        opts.cwd.display().to_string(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--unshare-pid".into(),
        "--die-with-parent".into(),
    ];
    for p in opts.additional_writable {
        args.push("--bind".into());
        let s = p.display().to_string();
        args.push(s.clone());
        args.push(s);
    }

    // **Hardening **: re-deny writes to settings.json even though it sits
    // inside `cwd`'s read-write bind — same rationale/paths as
    // `build_macos_profile`'s deny-write rules (stops a sandboxed Bash
    // command from overwriting its own permission rules, or injecting a
    // malicious multi-provider LLM config with an attacker-controlled
    // `base_url`/`api_key`). bwrap applies binds in argument order, so a
    // `--ro-bind-try` here remounts just this one path read-only on top of
    // the read-write `cwd` bind above — `--ro-bind-try` is a no-op (not an
    // error) when the source doesn't exist yet.
    let cwd_str = opts.cwd.display().to_string();
    for name in ["settings.json", "settings.local.json"] {
        let p = format!("{cwd_str}/.agix/{name}");
        args.push("--ro-bind-try".into());
        args.push(p.clone());
        args.push(p);
    }
    // Global/scene settings.json normally already sit outside any writable
    // bind (the read-only `/` bind above covers all of `$HOME` unless `cwd`
    // or an `additional_writable` entry happens to overlap it) — these are
    // defense-in-depth for that overlap case, mirroring the macOS profile's
    // unconditional deny rules for the same paths.
    if let Some(state_root) = state_root_of(&opts.policy) {
        let root_str = state_root.display().to_string();
        let global_settings = format!("{root_str}/settings.json");
        args.push("--ro-bind-try".into());
        args.push(global_settings.clone());
        args.push(global_settings);
        for scene in effective_known_scenes(&opts.policy) {
            let p = format!("{root_str}/scenes/{scene}/settings.json");
            args.push("--ro-bind-try".into());
            args.push(p.clone());
            args.push(p);
        }
    }

    // **Hardening **: deny-read via tmpfs over the path. Each entry gets
    // mounted as an empty tmpfs so reads return ENOENT-equivalent. Files are
    // overlaid by binding /dev/null. allow_read entries skip overlay.
    let allow_set: std::collections::HashSet<PathBuf> =
        opts.policy.allow_read.iter().cloned().collect();
    for p in &opts.policy.deny_read {
        if allow_set.contains(p) {
            continue;
        }
        let s = p.display().to_string();
        // Best-effort: if the path doesn't exist, skip — bwrap would error.
        if !p.exists() {
            continue;
        }
        if p.is_dir() {
            args.push("--tmpfs".into());
            args.push(s);
        } else {
            args.push("--ro-bind-try".into());
            args.push("/dev/null".into());
            args.push(s);
        }
    }

    // **Hardening **: network policy. bwrap has no domain/IP-level filter —
    // its only lever is `--unshare-net` (new network namespace with nothing
    // but loopback, i.e. total isolation). DenyAll maps onto that directly.
    // Allowlist can't be honored precisely on this platform (no local proxy
    // / iptables setup here), so it degrades to the same full isolation
    // rather than silently falling back to Unrestricted — consistent with
    // this crate's "safe by default, opt into less safety not more"
    // principle (README §Design Principles): failing closed on an
    // unsupported restriction is safer than failing open.
    match opts.policy.network_mode {
        NetworkMode::Unrestricted => {}
        NetworkMode::DenyAll => {
            args.push("--unshare-net".into());
        }
        NetworkMode::Allowlist => {
            tracing::warn!(
                domains = ?opts.policy.allowed_domains,
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
    let unmet = if opts.policy.network_mode == NetworkMode::Allowlist {
        vec!["domain allowlist: bwrap can only cut the network entirely".into()]
    } else {
        Vec::new()
    };
    let inner = bash_spec(opts.command);
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
        // Today every caller arrives as `bash -c <command>`; the backends
        // build their own wrapper around that pair. A spec that is not a
        // shell command is passed through unconstrained rather than
        // silently mis-wrapped — and says so, which is the whole point of
        // `unmet`.
        let command = match (spec.program.as_str(), spec.args.as_slice()) {
            ("bash", [flag, command]) if flag == "-c" => command.clone(),
            _ => {
                return Confined {
                    spec,
                    mode: SandboxMode::Unavailable,
                    enforcement: Enforcement::None,
                    unmet: vec!["this backend only confines shell commands (`bash -c …`)".into()],
                }
            }
        };
        let mut confined = wrap(SandboxOptions {
            command: &command,
            cwd: &spec.cwd,
            additional_writable: &policy.additional_writable,
            disable: false,
            policy: policy.clone(),
        });
        confined.spec.cwd = spec.cwd;
        confined.spec.env = spec.env;
        confined.spec.stdin = spec.stdin;
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
        assert!(cmd.spec.args[1].contains("(subpath \"/tmp/work\")"));
        assert!(cmd.spec.args[1].contains("(subpath \"/private/tmp\")"));
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wrap_re_denies_writes_to_project_settings_json() {
        let cmd = wrap(opts("ls", Path::new("/tmp/work")));
        if cmd.mode != SandboxMode::LinuxBwrap {
            return; // bwrap not installed in this environment — nothing to assert
        }
        let needle = "/tmp/work/.agix/settings.json".to_string();
        let pos = cmd
            .spec
            .args
            .iter()
            .position(|a| a == &needle)
            .expect("expected a --ro-bind-try re-mount for the project settings.json");
        assert_eq!(cmd.spec.args[pos - 1], "--ro-bind-try");
        // Source == dest for a self-remount, and settings.local.json gets the same treatment.
        assert_eq!(cmd.spec.args[pos + 1], needle);
        assert!(cmd
            .spec
            .args
            .iter()
            .any(|a| a == "/tmp/work/.agix/settings.local.json"));
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

    // ---- **Hardening **: policy tests ----

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

    // ---- network policy ----

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unrestricted_network_emits_no_network_rules() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Unrestricted;
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
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
        let profile = &cmd.spec.args[1];
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
        let profile = &cmd.spec.args[1];
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

    // ---- Phase 3-3: fault injection tests ----

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
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        // Must still deny writes to .agix/settings.json even when deny_read is empty
        assert!(
            profile.contains("(deny file-write* (literal \"/tmp/work/.agix/settings.json\"))"),
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
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.spec.args[1];
        assert!(
            !profile.contains("(deny file-read*"),
            "no deny-read rules when deny_read is empty"
        );
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
            known_scenes: vec!["coding".into()],
            ..Default::default()
        };
        let profile = build_macos_profile(Path::new("/work/project"), &[], &policy);

        assert!(profile.contains("(deny file-write* (literal \"/srv/agix-state/settings.json\"))"));
        assert!(profile.contains(
            "(deny file-write* (literal \"/srv/agix-state/scenes/coding/settings.json\"))"
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
