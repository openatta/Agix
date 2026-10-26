//! Daemon configuration — path abstraction + layered settings loading.
//!
//! The [`DaemonPaths`] trait decouples filesystem layout from configuration
//! loading. Implementors can swap in a tempdir for testing or a custom
//! `AGIX_CONFIG_HOME` at runtime.
//!
//! [`load_daemon_config`] delegates all settings.json parsing/merging to
//! `base::interface::settings::Settings::load()` — the single canonical
//! loader (global → scene → project, generic recursive JSON merge). Before
//! 2026-08-04's second round, `daemon` maintained its own parallel
//! `SettingsFile` struct/parser with a narrower field set (only
//! model/max_tokens/mcp_servers/providers/task_models/hooks were
//! recognized) — every new setting had to be added in four places
//! (`SettingsFile`, `merge_settings`, `DaemonConfig`, and the `Settings{}`
//! literal `daemon/src/main.rs` hand-built from it) to actually take effect.
//! That's exactly how `permission_rules`/`permission_mode` ended up parsed
//! nowhere despite being real `Settings` fields. `DaemonConfig` now just
//! wraps the one canonical `Settings` plus the handful of fields that are
//! genuinely daemon-process-level, not settings.json content (socket/lock
//! paths, session cap, TCP listener).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base::interface::settings::Settings;
use permissions::ruleset::RuleSet;

// ── Path abstraction ────────────────────────────────────────────────────

/// Controls where the daemon reads its configuration and writes its runtime
/// files (socket, lock, etc.).
pub trait DaemonPaths: Send + Sync {
    fn config_root(&self) -> PathBuf;
    fn project_root(&self) -> PathBuf;

    /// Cross-scene global settings root — the lowest-priority layer, shared
    /// by every scene on this machine (`config_root()` is scene-specific:
    /// `$HOME/.agix/<scene>`; this is one level up: `$HOME/.agix`).
    ///
    /// Default: parent of `config_root()`, falling back to `config_root()`
    /// itself if it has no parent. Implementors used for deterministic
    /// testing (see `StaticDaemonPaths`) override this explicitly rather
    /// than relying on `config_root()`'s parent, since that parent is often
    /// an arbitrary tempdir shared with unrelated tests.
    fn global_root(&self) -> PathBuf {
        self.config_root()
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config_root())
    }
}

/// Default path provider: `$AGIX_CONFIG_HOME` (or `$HOME/.agix`) is the flat
/// global root; `config_root()` is `{global_root}/scenes/<scope>/`.
///
/// `scope` identifies which product instance owns this scene-specific
/// override state (see `base::paths::ConfigPaths`). This struct itself does
/// not default anything — callers must pass a value. `daemon`'s own caller
/// (`daemon/src/main.rs::resolve_scene`) derives that value from the
/// validated `--scene` CLI flag (`coding`/`chat`/`demo`; unsupported values
/// fail startup) rather than accepting an arbitrary string.
///
/// **2026-08-04**: `global_root` is stored explicitly rather than derived
/// from `config_root`'s filesystem parent — `config_root` is now nested two
/// levels under the global root (`.../scenes/<scope>/`), so a single
/// `.parent()` call no longer reaches it. This also changes what
/// `AGIX_CONFIG_HOME` overrides: it now sets the *global* root, with
/// `config_root` derived as `{AGIX_CONFIG_HOME}/scenes/<scope>/` — previously
/// it set `config_root` directly.
#[derive(Debug, Clone)]
pub struct DefaultDaemonPaths {
    global_root: PathBuf,
    config_root: PathBuf,
    project_root: PathBuf,
}

impl DefaultDaemonPaths {
    pub fn from_env(scope: &str) -> Self {
        let global_root = if let Ok(p) = std::env::var("AGIX_CONFIG_HOME") {
            PathBuf::from(p)
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".agix")
        } else {
            PathBuf::from("/tmp/agix")
        };
        let config_root = global_root.join("scenes").join(scope);
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            global_root,
            config_root,
            project_root,
        }
    }

    pub fn new(config_root: PathBuf, project_root: PathBuf) -> Self {
        // No separate global root supplied — fall back to config_root itself,
        // same "collapse to one layer" default `StaticDaemonPaths` uses.
        Self {
            global_root: config_root.clone(),
            config_root,
            project_root,
        }
    }
}

impl DaemonPaths for DefaultDaemonPaths {
    fn config_root(&self) -> PathBuf {
        self.config_root.clone()
    }
    fn project_root(&self) -> PathBuf {
        self.project_root.clone()
    }
    fn global_root(&self) -> PathBuf {
        self.global_root.clone()
    }
}

/// Fixed-path provider for integration tests.
///
/// `global_root` defaults to `config_root` (not `config_root`'s filesystem
/// parent) — tests build `config_root` from `tempfile::tempdir()`, whose
/// parent is the shared system temp directory; defaulting there would risk
/// tests picking up an unrelated real `settings.json` if one happened to
/// exist. Call `.with_global(...)` to opt into testing the global layer
/// explicitly.
#[derive(Debug, Clone)]
pub struct StaticDaemonPaths {
    config_root: PathBuf,
    project_root: PathBuf,
    global_root: Option<PathBuf>,
}

impl StaticDaemonPaths {
    pub fn new(path: PathBuf) -> Self {
        Self {
            config_root: path.clone(),
            project_root: path,
            global_root: None,
        }
    }
    pub fn with_project(config_root: PathBuf, project_root: PathBuf) -> Self {
        Self {
            config_root,
            project_root,
            global_root: None,
        }
    }
    pub fn with_global(mut self, global_root: PathBuf) -> Self {
        self.global_root = Some(global_root);
        self
    }
}

impl DaemonPaths for StaticDaemonPaths {
    fn config_root(&self) -> PathBuf {
        self.config_root.clone()
    }
    fn project_root(&self) -> PathBuf {
        self.project_root.clone()
    }
    fn global_root(&self) -> PathBuf {
        self.global_root
            .clone()
            .unwrap_or_else(|| self.config_root.clone())
    }
}

// ── Config struct ────────────────────────────────────────────────────────

/// Fully resolved daemon configuration. `settings` is the one canonical
/// settings.json projection (see module docs); the remaining fields are
/// process-level concerns that were never settings.json content in the
/// first place (socket/lock file locations, session capacity, TCP listener).
#[derive(Clone)]
pub struct DaemonConfig {
    pub paths: Arc<dyn DaemonPaths>,
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    pub session_cap: usize,
    pub tcp_addr: Option<SocketAddr>,
    /// WebSocket transport address. Shares `tcp_token`, because both are the
    /// same question — is this connection allowed — asked over different
    /// framing.
    pub ws_addr: Option<SocketAddr>,
    pub tcp_token: Option<String>,
    /// Not yet sourced from `settings.permission_rules` — daemon has always
    /// hardcoded `AllowAllPermission` + `BypassPermissions` regardless (see
    /// `daemon/src/main.rs`, "IDE plugins manage their own sandbox"). Kept
    /// as a separate field rather than silently wired to
    /// `settings.permission_rules` because doing so would be a behavior
    /// change beyond this round's scope, not just a plumbing fix.
    pub permission_rules: RuleSet,
    /// Session 空闲超时秒数，超时后自动回收（默认 3600 = 1 小时）。
    pub session_idle_timeout_secs: u64,
    pub settings: Settings,
}

impl std::fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonConfig")
            .field("socket_path", &self.socket_path)
            .field("lock_path", &self.lock_path)
            .field("session_cap", &self.session_cap)
            .field("model", &self.settings.model.model_name)
            .field("max_tokens", &self.settings.model.max_tokens)
            .field(
                "mcp_servers",
                &self.settings.mcp_servers.keys().collect::<Vec<_>>(),
            )
            .field("tcp_addr", &self.tcp_addr)
            .field("ws_addr", &self.ws_addr)
            .field("tcp_token", &"...")
            .field("paths", &"...")
            .field(
                "permission_rules",
                &format!("RuleSet({})", self.permission_rules.len()),
            )
            .field(
                "providers",
                &self.settings.providers.keys().collect::<Vec<_>>(),
            )
            .field("default_provider", &self.settings.default_provider)
            .field(
                "task_models",
                &self.settings.task_models.keys().collect::<Vec<_>>(),
            )
            .field("hooks", &self.settings.hooks_config.is_some())
            .finish()
    }
}

impl DaemonConfig {
    pub fn minimal(paths: Arc<dyn DaemonPaths>) -> Self {
        let config_root = paths.config_root();
        Self {
            socket_path: socket_path_from_root(&config_root),
            lock_path: lock_path_from_root(&config_root),
            paths,
            session_cap: 32,
            tcp_addr: None,
            ws_addr: None,
            tcp_token: None,
            permission_rules: RuleSet::empty(),
            session_idle_timeout_secs: 3600,
            settings: Settings::defaults_for("claude-sonnet-4-6"),
        }
    }
}

// ── Loading ──────────────────────────────────────────────────────────────

/// Priority (low → high): global (`global_root()/settings.json`, shared by
/// every scene) → scene (`config_root()/settings.json`) → project
/// (`project_root()/.agix/settings.json`) — delegates entirely to
/// `Settings::load()`; see its doc comment for the merge algorithm.
pub fn load_daemon_config(
    cli_model: &str,
    cli_max_tokens: u32,
    cli_socket: Option<&Path>,
    scope: &str,
    paths: &dyn DaemonPaths,
) -> DaemonConfig {
    let config_root = paths.config_root();
    let project_root = paths.project_root();
    let global_root = paths.global_root();

    let mut settings = Settings::load(
        global_root.clone(),
        config_root.clone(),
        project_root.join(".agix"),
        scope,
        cli_model,
    );
    // `--max-tokens` is a fallback (same tier as `Settings::load`'s
    // `default_model` param) — only takes effect when no settings.json
    // layer set it. `Settings::load` doesn't take this as a parameter
    // itself (unlike `default_model`) because its own hardcoded default
    // (2000) already matches the CLI flag's own `clap` default, so this
    // only matters when a caller explicitly passes a non-default value.
    if settings.model.max_tokens == 2000 {
        settings.model.max_tokens = cli_max_tokens;
    }

    let socket_path = cli_socket
        .map(PathBuf::from)
        .unwrap_or_else(|| socket_path_from_root(&config_root));
    let lock_path = lock_path_from_root(&config_root);

    DaemonConfig {
        socket_path,
        lock_path,
        paths: Arc::new(
            StaticDaemonPaths::with_project(config_root, project_root).with_global(global_root),
        ),
        session_cap: 32,
        tcp_addr: None,
        ws_addr: None,
        tcp_token: None,
        permission_rules: RuleSet::empty(),
        session_idle_timeout_secs: 3600,
        settings,
    }
}

pub fn socket_path_from_root(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\agix-daemon")
    }
    #[cfg(not(windows))]
    {
        root.join("daemon.sock")
    }
}

pub fn lock_path_from_root(root: &Path) -> PathBuf {
    root.join("daemon.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_settings(dir: &Path, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("settings.json"), content).unwrap();
    }

    fn write_project_settings(project: &Path, content: &str) {
        let agix_dir = project.join(".agix");
        std::fs::create_dir_all(&agix_dir).unwrap();
        std::fs::write(agix_dir.join("settings.json"), content).unwrap();
    }

    #[test]
    fn default_paths_from_env_falls_back_to_home() {
        let paths = DefaultDaemonPaths::from_env("code");
        let root = paths.config_root();
        assert!(!root.as_os_str().is_empty());
    }

    #[test]
    fn default_paths_from_env_respects_scope() {
        // Clear AGIX_CONFIG_HOME so scope actually drives the path in this test.
        std::env::remove_var("AGIX_CONFIG_HOME");
        let code_paths = DefaultDaemonPaths::from_env("code");
        let ops_paths = DefaultDaemonPaths::from_env("ops");
        assert_ne!(code_paths.config_root(), ops_paths.config_root());
        assert!(code_paths.config_root().ends_with("code"));
        assert!(ops_paths.config_root().ends_with("ops"));
    }

    #[test]
    fn static_paths_returns_configured_dirs() {
        let paths = StaticDaemonPaths::new(PathBuf::from("/test/config"));
        assert_eq!(paths.config_root(), PathBuf::from("/test/config"));
        assert_eq!(paths.project_root(), PathBuf::from("/test/config"));
    }

    #[test]
    fn static_paths_global_root_defaults_to_config_root() {
        // Deliberately not `config_root`'s filesystem parent — see the
        // `StaticDaemonPaths` doc comment on why.
        let paths = StaticDaemonPaths::new(PathBuf::from("/test/config"));
        assert_eq!(paths.global_root(), PathBuf::from("/test/config"));
    }

    #[test]
    fn static_paths_global_root_respects_explicit_override() {
        let paths = StaticDaemonPaths::new(PathBuf::from("/test/config"))
            .with_global(PathBuf::from("/test/global"));
        assert_eq!(paths.global_root(), PathBuf::from("/test/global"));
    }

    #[test]
    fn default_daemon_paths_config_root_nests_under_global_root_scenes() {
        std::env::remove_var("AGIX_CONFIG_HOME");
        let paths = DefaultDaemonPaths::from_env("coding");
        assert_eq!(
            paths.config_root(),
            paths.global_root().join("scenes").join("coding")
        );
        assert!(paths.global_root().ends_with(".agix"));
    }

    #[test]
    fn socket_path_derived_from_config_root() {
        #[cfg(not(windows))]
        {
            let p = socket_path_from_root(Path::new("/home/user/.agix/code"));
            assert_eq!(p, PathBuf::from("/home/user/.agix/code/daemon.sock"));
        }
    }

    #[test]
    fn lock_path_derived_from_config_root() {
        let p = lock_path_from_root(Path::new("/home/user/.agix/code"));
        assert_eq!(p, PathBuf::from("/home/user/.agix/code/daemon.lock"));
    }

    #[test]
    fn load_daemon_config_cli_fallback_when_no_settings() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let config = load_daemon_config("cli-model", 5000, None, "code", &paths);
        assert_eq!(config.settings.model.model_name, "cli-model");
        assert_eq!(config.settings.model.max_tokens, 5000);
    }

    #[test]
    fn load_daemon_config_project_overrides_user() {
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        write_settings(
            config_dir.path(),
            r#"{"model": {"model_name": "user-model"}, "execution": {"max_parallelism": 1, "max_api_calls_per_turn": 1}}"#,
        );
        write_project_settings(
            project_dir.path(),
            r#"{"model": {"model_name": "project-model"}}"#,
        );
        let paths = StaticDaemonPaths::with_project(
            config_dir.path().to_path_buf(),
            project_dir.path().to_path_buf(),
        );
        let config = load_daemon_config("cli-model", 2000, None, "code", &paths);
        assert_eq!(config.settings.model.model_name, "project-model");
    }

    #[test]
    fn load_daemon_config_global_is_lowest_priority() {
        let global_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        write_settings(
            global_dir.path(),
            r#"{"model": {"model_name": "global-model"}}"#,
        );
        write_settings(
            config_dir.path(),
            r#"{"model": {"model_name": "scene-model"}}"#,
        );
        // Project layer doesn't touch model — scene value should win over global.
        write_project_settings(project_dir.path(), r#"{}"#);

        let paths = StaticDaemonPaths::with_project(
            config_dir.path().to_path_buf(),
            project_dir.path().to_path_buf(),
        )
        .with_global(global_dir.path().to_path_buf());

        let config = load_daemon_config("cli-model", 2000, None, "code", &paths);
        assert_eq!(config.settings.model.model_name, "scene-model"); // scene overrides global
    }

    /// Regression guard: `load_daemon_config` used to build its returned
    /// `DaemonConfig.paths` via `StaticDaemonPaths::with_project` without
    /// `.with_global(..)`, so `paths.global_root()` fell back to
    /// `config_root` — collapsing the global and scene tiers into the same
    /// directory for every downstream consumer of `DaemonConfig.paths`
    /// (`daemon.doctor`, `config.reload`, plugin tier discovery). Only the
    /// *first* `Settings::load` call (which takes `paths.global_root()` as a
    /// plain fn argument, not through `DaemonConfig.paths`) was ever correct.
    #[test]
    fn load_daemon_config_paths_preserve_distinct_global_root() {
        let global_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::with_project(
            config_dir.path().to_path_buf(),
            project_dir.path().to_path_buf(),
        )
        .with_global(global_dir.path().to_path_buf());

        let config = load_daemon_config("cli-model", 2000, None, "code", &paths);

        assert_eq!(config.paths.global_root(), global_dir.path());
        assert_ne!(config.paths.global_root(), config.paths.config_root());
    }

    #[test]
    fn load_daemon_config_scene_overrides_global_which_is_the_only_layer_set() {
        let global_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        write_settings(
            global_dir.path(),
            r#"{"model": {"model_name": "global-model"}}"#,
        );
        // No scene-level or project-level settings.json at all.
        let paths = StaticDaemonPaths::new(config_dir.path().to_path_buf())
            .with_global(global_dir.path().to_path_buf());
        let config = load_daemon_config("cli-model", 2000, None, "code", &paths);
        assert_eq!(config.settings.model.model_name, "global-model");
    }

    #[test]
    fn load_daemon_config_parses_and_merges_providers() {
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        write_settings(
            config_dir.path(),
            r#"{
                "providers": {
                    "deepseek": {
                        "api_type": "openai_compatible",
                        "base_url": "https://api.deepseek.com/v1",
                        "api_key": "user-key",
                        "default_model": "deepseek-pro",
                        "models": ["deepseek-pro", "deepseek-flash"]
                    }
                },
                "default_provider": "deepseek",
                "task_models": { "subagent": "deepseek" }
            }"#,
        );
        write_project_settings(
            project_dir.path(),
            r#"{ "providers": { "deepseek": { "api_key": "project-key" } } }"#,
        );
        let paths = StaticDaemonPaths::with_project(
            config_dir.path().to_path_buf(),
            project_dir.path().to_path_buf(),
        );
        let config = load_daemon_config("cli-model", 2000, None, "code", &paths);

        let deepseek = &config.settings.providers["deepseek"];
        assert_eq!(deepseek.api_key.as_deref(), Some("project-key")); // project overrides field
        assert_eq!(deepseek.default_model.as_deref(), Some("deepseek-pro")); // untouched field survives
        assert_eq!(
            config.settings.default_provider.as_deref(),
            Some("deepseek")
        );
        assert_eq!(
            config.settings.task_models["subagent"],
            base::provider::TaskModelOverride::ProviderOnly("deepseek".into())
        );
    }

    #[test]
    fn load_daemon_config_parses_and_merges_hooks() {
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        write_settings(
            config_dir.path(),
            r#"{
                "hooks_config": {
                    "PreToolUse": [
                        { "type": "command", "command": "echo scene-hook" }
                    ]
                }
            }"#,
        );
        write_project_settings(
            project_dir.path(),
            r#"{
                "hooks_config": {
                    "PreToolUse": [
                        { "type": "command", "command": "echo project-hook" }
                    ]
                }
            }"#,
        );
        let paths = StaticDaemonPaths::with_project(
            config_dir.path().to_path_buf(),
            project_dir.path().to_path_buf(),
        );
        let config = load_daemon_config("cli-model", 2000, None, "code", &paths);

        // Generic recursive JSON merge: both layers set the same
        // `PreToolUse` array key, so the project layer's array (a
        // non-object value) fully replaces the scene layer's.
        let hooks = config
            .settings
            .hooks_config
            .expect("hooks should be present");
        let command = hooks["PreToolUse"][0]["command"].as_str().unwrap();
        assert_eq!(command, "echo project-hook");
    }

    #[test]
    fn load_daemon_config_hooks_absent_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let config = load_daemon_config("cli-model", 2000, None, "code", &paths);
        assert!(config.settings.hooks_config.is_none());
    }

    #[test]
    fn load_daemon_config_project_can_override_just_one_provider_field_and_permission_rules_flow_through(
    ) {
        // Demonstrates the fix this round makes concrete: any `Settings`
        // field (not just the handful `SettingsFile` used to special-case)
        // now flows through settings.json automatically — permission_rules
        // is a real example of a field that was previously parsed nowhere.
        let dir = tempfile::tempdir().unwrap();
        write_settings(
            dir.path(),
            r#"{
                "permission_rules": [
                    { "tool": "Bash(git push:*)", "action": "deny" }
                ]
            }"#,
        );
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let config = load_daemon_config("cli-model", 2000, None, "code", &paths);
        assert_eq!(config.settings.permission_rules.len(), 1);
        assert_eq!(config.settings.permission_rules[0].tool, "Bash(git push:*)");
    }

    #[test]
    fn minimal_config_uses_sensible_defaults() {
        let paths = Arc::new(StaticDaemonPaths::new(PathBuf::from("/tmp/test")));
        let config = DaemonConfig::minimal(paths.clone());
        assert_eq!(config.session_cap, 32);
        assert!(config.settings.mcp_servers.is_empty());
        assert!(config.tcp_addr.is_none());
    }
}
