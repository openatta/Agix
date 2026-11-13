//! Sandbox policy for shell-running tools: settings to policy, confining a command, the session TMPDIR and the engine's own git.

use base::tool::{ToolContext, ToolResult};
use platform::sandbox;
use std::time::Duration;

/// `ToolContext.sandbox`（`base::tool::SandboxSettings`，跨 crate 的纯数据
/// 视图，见其 doc comment）→ 这个 crate 真正用来生成沙盒 profile 的
/// `sandbox::SandboxPolicy`。两边字段形状故意保持一一对应，这里只是把
/// `base::context::config::NetworkModeConfig` 换成本 crate 的
/// `sandbox::NetworkMode`（同名变体，逐一转换，没有默认值坍缩）。
/// `allow_read` 来自 `settings.json` 的 `sandbox.allow_read`：内置的凭据 deny
/// 默认名单（见下）里有些条目压在正当流程上——`npm install`
/// 要读 `~/.npmrc`、`docker build` 要读 `~/.docker/config.json`。这个字段就是
/// "这一条在我这儿没问题"的出口；macOS profile 里 allow 排在 deny 之后，最具体
/// 的规则胜出。
///
/// **空 `deny_read` = 用内置默认值**：空 vec 回落到 [`sandbox::default_deny_read`]，
/// 否则默认配置下 `~/.ssh`、`~/.aws`、`~/.config/gh` 这些凭据目录对 Bash 完全敞开。
/// 要放开其中某一条，用 `sandbox.allow_read` 逐条放行。
pub(super) fn to_sandbox_policy(settings: &base::tool::SandboxSettings) -> sandbox::SandboxPolicy {
    let network_mode = match settings.network_mode {
        base::context::config::NetworkModeConfig::Unrestricted => {
            sandbox::NetworkMode::Unrestricted
        }
        base::context::config::NetworkModeConfig::DenyAll => sandbox::NetworkMode::DenyAll,
        base::context::config::NetworkModeConfig::Allowlist => sandbox::NetworkMode::Allowlist,
    };
    let deny_read = if settings.deny_read.is_empty() {
        sandbox::default_deny_read()
    } else {
        settings.deny_read.clone()
    };
    sandbox::SandboxPolicy {
        allow_read: settings.allow_read.clone(),
        deny_read,
        network_mode,
        allowed_domains: settings.allowed_domains.clone(),
        state_root: settings
            .paths
            .state_root()
            .map(std::path::Path::to_path_buf),
        // Filled in by the caller, which is the only place that knows what
        // this turn opened up.
        additional_writable: Vec::new(),
        only_additional_writable: false,
        deny_ipc_escapes: false,
    }
}

pub(super) fn sandbox_policy(ctx: &ToolContext) -> sandbox::SandboxPolicy {
    let mut policy = to_sandbox_policy(&ctx.sandbox);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let caches =
        ctx.session
            .cache_directories
            .iter()
            .filter_map(|dir| match dir.strip_prefix("~") {
                Ok(rest) => home.as_ref().map(|h| h.join(rest)),
                Err(_) => Some(dir.clone()),
            });
    policy.additional_writable = ctx
        .additional_writable_dirs
        .iter()
        .cloned()
        .chain(caches)
        .chain([session_temp_dir(&ctx.sandbox, &ctx.session_id)])
        .collect();
    policy.deny_ipc_escapes = ctx.session.unattended;
    policy
}

/// The session's own temporary directory: a sandboxed command's `TMPDIR`,
/// and the only temporary directory it may write besides the one macOS
/// `mktemp` insists on.
///
/// Under the state root, `<state root>/tmp/<session>`, rather than in the
/// system temporary directory: on macOS that is the per-user
/// `DARWIN_USER_TEMP_DIR` every sandboxed command may write, so one
/// session's commands could reach into another's.
fn session_temp_dir(
    settings: &base::tool::SandboxSettings,
    session_id: &str,
) -> std::path::PathBuf {
    settings
        .paths
        .session_tmp_dir(session_id)
        .unwrap_or_else(|| {
            let id: String = session_id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            std::env::temp_dir().join(format!("agix-{id}"))
        })
}

/// How long a session's temporary directory may go untouched before another
/// session's start clears it away.
const STALE_SESSION_TEMP_DIR: Duration = Duration::from_secs(7 * 24 * 3600);

/// [`session_temp_dir`], created private to this user if it is not there yet.
fn prepared_session_temp_dir(
    settings: &base::tool::SandboxSettings,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let dir = session_temp_dir(settings, session_id);
    let private = |recursive| {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(recursive);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
    };
    if let Some(parent) = dir
        .parent()
        .filter(|_| settings.paths.state_root().is_some())
    {
        let _ = private(true).create(parent);
        clear_stale_session_temp_dirs(parent);
    }
    match private(false).create(&dir) {
        Ok(()) => Some(dir),
        Err(e)
            if e.kind() == std::io::ErrorKind::AlreadyExists
                && dir.symlink_metadata().is_ok_and(|m| m.is_dir()) =>
        {
            Some(dir)
        }
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "could not create the session's TMPDIR");
            None
        }
    }
}

/// Once per process: the session directories under `parent` nothing has
/// touched for [`STALE_SESSION_TEMP_DIR`]. Nothing else removes them — unlike
/// the system temporary directory, the state root is not swept by the OS.
fn clear_stale_session_temp_dirs(parent: &std::path::Path) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let stale = entry.metadata().is_ok_and(|m| {
                m.is_dir()
                    && m.modified().is_ok_and(|t| {
                        now.duration_since(t).unwrap_or_default() > STALE_SESSION_TEMP_DIR
                    })
            });
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    });
}

/// `command` as the process a shell-running tool spawns: wrapped by the OS
/// sandbox unless the session turned it off. `Err` is the refusal to return
/// instead, when the sandbox cannot hold the command and the host said it
/// must.
pub(crate) fn confine_shell(
    tool: &str,
    command: &str,
    ctx: &ToolContext,
) -> Result<base::interface::exec::ProcessSpec, ToolResult> {
    use base::interface::exec::{Enforcement, ProcessSpec, SandboxMode};
    let intent = ProcessSpec::new("bash", &ctx.cwd).args(["-c".to_string(), command.to_string()]);
    if ctx.dangerously_disable_sandbox {
        return Ok(intent);
    }
    let mut confined = ctx.exec.sandbox.confine(intent, &sandbox_policy(ctx));
    if confined.enforcement != Enforcement::None {
        if let Some(tmp) = prepared_session_temp_dir(&ctx.sandbox, &ctx.session_id) {
            confined
                .spec
                .env
                .push(("TMPDIR".to_string(), tmp.display().to_string()));
        }
    }

    // A policy that cannot be enforced is the one case where "log it and
    // carry on" is the wrong default *and* the wrong thing to change
    // silently: refusing breaks every Linux host without bubblewrap and
    // every Windows host, while permitting means a command the operator
    // believes is constrained runs with nothing holding it. So the engine
    // reports honestly and lets the host decide which it wants.
    //
    // `Partial` counts as unenforced here. A host that turned
    // `require_enforcement` on wants an absolute boundary, and "most of
    // your policy" is not one.
    let shortfall = match confined.enforcement {
        Enforcement::Full => None,
        Enforcement::None if confined.mode == SandboxMode::Disabled => None,
        _ => Some(confined.unmet.join("; ")),
    };
    if let Some(shortfall) = shortfall {
        if ctx.sandbox.require_enforcement {
            return Err(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "Refused: a sandbox policy is configured but this build on this \
                     platform ({}) cannot enforce all of it — {shortfall} — and \
                     `sandbox.require_enforcement` is on. The backend is behind a \
                     compile-time feature that is off by default: rebuild with \
                     `--features sandbox`, and on Linux install bubblewrap. Or set \
                     `sandbox.require_enforcement: false` to accept what this build \
                     can do, or `sandbox.dangerously_disable_sandbox: true` to stop \
                     asking for a sandbox.",
                    std::env::consts::OS
                )),
                is_error: true,
                structured_content: Some(serde_json::json!({
                    "refused": "sandbox_unenforceable",
                    "platform": std::env::consts::OS,
                    "unmet": confined.unmet,
                })),
                mcp_meta: None,
                new_messages: Some(vec![]),
            });
        }
        tracing::warn!(
            tool,
            platform = std::env::consts::OS,
            unmet = %shortfall,
            "sandbox policy is not fully enforced. Install bwrap (Linux), \
             set sandbox.require_enforcement to refuse instead, or \
             dangerously_disable_sandbox to stop asking."
        );
    }
    Ok(confined.spec)
}

/// What the session's sandbox holds a command to, for the permission policy
/// to read: one probe command confined the way a command nobody attends is,
/// since that is the command the answer lets run.
pub fn confinement_report(
    settings: &base::tool::SandboxSettings,
    exec: &base::interface::exec::ExecProviders,
    sandbox_disabled: bool,
    cwd: &std::path::Path,
    session_id: &str,
) -> base::interface::exec::ConfinementReport {
    use base::interface::exec::{ConfinementReport, ProcessSpec};
    let (require, network) = (
        settings.require_enforcement,
        settings.unattended_requires_network_policy,
    );
    if sandbox_disabled {
        return ConfinementReport::disabled(require, network);
    }
    let mut policy = to_sandbox_policy(settings);
    policy.additional_writable = vec![session_temp_dir(settings, session_id)];
    policy.deny_ipc_escapes = true;
    let probe = ProcessSpec::new("bash", cwd).args(["-c", "true"]);
    ConfinementReport::probe(&*exec.sandbox, probe, &policy, require, network)
}

/// How the engine starts its own git and helpers in the session `ctx`
/// belongs to.
pub fn engine_exec(ctx: &ToolContext) -> base::engine_exec::EngineExec {
    engine_exec_for(
        &ctx.sandbox,
        &ctx.exec,
        ctx.dangerously_disable_sandbox,
        &ctx.session_id,
        ctx.config.project_trust,
    )
}

/// Through the session's providers, and inside its sandbox whenever the
/// session's shell commands are, so what a repository such a command
/// configured makes git run is held to the same roots. Run by the engine,
/// outside it, git would carry that out unconfined.
pub fn engine_exec_for(
    settings: &base::tool::SandboxSettings,
    exec: &base::interface::exec::ExecProviders,
    sandbox_disabled: bool,
    session_id: &str,
    trust: base::project_trust::ProjectTrust,
) -> base::engine_exec::EngineExec {
    use base::engine_exec::{Confinement, EngineExec};
    let confinement = if sandbox_disabled {
        Confinement::Unconfined
    } else {
        Confinement::Sandboxed {
            policy: to_sandbox_policy(settings),
            require_enforcement: settings.require_enforcement,
            tmpdir: prepared_session_temp_dir(settings, session_id),
        }
    };
    EngineExec::new(exec.clone(), confinement, trust)
}

#[cfg(test)]
mod allow_read_tests {
    use super::*;
    use std::path::PathBuf;

    /// `sandbox.allow_read` must reach the generated profile. Without an
    /// upstream source for it, the newly-live `default_deny_read()` list had
    /// no escape hatch — a user whose `npm install` reads `~/.npmrc` could
    /// only turn the sandbox off entirely.
    #[test]
    fn allow_read_is_carried_through_alongside_the_deny_defaults() {
        let settings = base::tool::SandboxSettings {
            allow_read: vec![PathBuf::from("/home/u/.npmrc")],
            deny_read: Vec::new(),
            allowed_domains: Vec::new(),
            network_mode: base::context::config::NetworkModeConfig::Unrestricted,
            paths: Default::default(),
            require_enforcement: false,
            unattended_requires_network_policy: false,
            deny_write: Vec::new(),
            allow_write: Vec::new(),
        };
        let policy = to_sandbox_policy(&settings);
        assert_eq!(policy.allow_read, vec![PathBuf::from("/home/u/.npmrc")]);
        assert!(
            !policy.deny_read.is_empty(),
            "an empty deny_read still falls back to the credential defaults"
        );
    }
}
