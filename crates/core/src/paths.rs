//! Data directory path abstraction.
//!
//! `ConfigPaths` provides a single source of truth for all persistent
//! directories used by the AGENT: a flat cross-scene **global** root
//! (conventionally `~/.agix/`), a **scene**-specific override root nested
//! under it (`<global>/scenes/<scope>/`), and a **local**/project root
//! (`<cwd>/.agix/`).
//!
//! **The roots are given, never discovered.** Nothing in this module reads
//! the environment: the process entry point decides which directory the
//! instance owns and passes it down. `~/.agix` is what that entry point
//! usually picks, not something the library assumes — which is what lets a
//! test, a service account, or a deployment with its state elsewhere work
//! without every module having to agree separately.
//!
//! `scope` identifies which product built on this engine owns the
//! scene-specific override state (e.g. a coding-agent product might use
//! `"code"`). The engine never assumes a default — callers must pass one
//! explicitly. The local/project root is flat and has no scope segment: a
//! project is assumed to be worked on by one product instance at a time, so
//! there's no collision to avoid the way there is under a shared `$HOME`.
//!
//! The user-level tree is split by resource type:
//!
//! - **Global + scene override** (config-shaped, keyed by name): `settings.json`,
//!   `skills/`, `plugins/`, `agents/`, `rules/`, `hooks/`. Present at both
//!   `~/.agix/<resource>/` (base) and `~/.agix/scenes/<scope>/<resource>/`
//!   (override) — a scene-specific entry with the same name wins.
//! - **Global + project only, no scene tier** (history/state-shaped):
//!   `memory/`, `sessions/`, `recordings/`. Present at `~/.agix/<resource>/`
//!   (used when there's no specific project — reserved for a future
//!   no-project/desktop mode, not yet exercised by `daemon`) and
//!   `<cwd>/.agix/<resource>/` (project, used today). Deliberately **not**
//!   scene-scoped: a scene's tool/system-prompt config has no bearing on
//!   which project a session or memory file belongs to, so borrowing across
//!   scenes would be a correctness hazard (e.g. resuming a session recorded
//!   under a different scene's tool set, or a recording replaying against the
//!   wrong tool config).
//!
//! Project-level skills are **not** under `ConfigPaths` at all — they live in
//! `.agents/skills/`, a sibling of `.agix/`, because that's the path Codex
//! and other external agent tools scan. `ConfigPaths` only models Agix's
//! own private state and extensions, which no other tool reads.

use std::path::PathBuf;

/// The state root's subdirectories that are the agent's own working space
/// rather than configuration: the only parts of it the agent's tools and its
/// sandboxed commands may write.
pub const STATE_ROOT_WORKING_DIRS: &[&str] = &["scratchpad", "memory"];

/// The directory in a project's `.agix` that holds the git worktrees
/// `EnterWorktree` creates: project work checked out elsewhere, not
/// configuration, so it is outside [`ENGINE_CONFIG_DIRS`].
pub const PROJECT_WORKTREES_DIR: &str = "worktrees";

/// The directories an agent's configuration lives in: `.agix` in a project
/// and as the state root, and `.claude`, which the engine imports from.
/// What may be written in one is decided by [`ENGINE_CONFIG_FILES`] and
/// [`ENGINE_CONFIG_DIRS`]; the rest (scratchpad, memory, sessions,
/// worktrees) is working space.
pub const ENGINE_CONFIG_ROOTS: &[&str] = &[".agix", ".claude"];

/// Files in an [`ENGINE_CONFIG_ROOTS`] directory that configure the engine.
/// The write tools refuse them anywhere under such a directory, a sandboxed
/// command directly in it.
pub const ENGINE_CONFIG_FILES: &[&str] =
    &["settings.json", "settings.local.json", "mcp.local.json"];

/// Directories directly in an [`ENGINE_CONFIG_ROOTS`] directory that
/// configure the engine, guarded with everything under them.
pub const ENGINE_CONFIG_DIRS: &[&str] = &[
    "mcp",
    "hooks",
    "agents",
    "skills",
    "plugins",
    "rules",
    "scenes",
    "policy",
    "scripts",
    "output-styles",
];

/// Directories in a project, outside the [`ENGINE_CONFIG_ROOTS`], that the
/// engine loads configuration from: the project's skills, whose
/// `allowed-tools` grant permissions. Guarded like a configuration
/// directory in those roots: a write tool asks, a sandboxed command cannot
/// write, and the directories above one cannot be moved aside.
pub const PROJECT_ENGINE_CONFIG_DIRS: &[&str] = &[".agents/skills"];

/// Directory names that hold credentials wherever they appear as a whole
/// path component.
pub const CREDENTIAL_DIR_NAMES: &[&str] = &[".ssh", ".aws", ".gnupg"];

/// In the state root and in each `scenes/<scene>/` under it, the stems of
/// the files that hold provider keys and MCP headers in plain text — see
/// [`is_state_root_key_file`].
pub const STATE_ROOT_KEY_FILE_STEMS: &[&str] = &["settings", "mcp.local"];

/// In the state root and in each scene directory under it: the MCP OAuth
/// tokens.
pub const STATE_ROOT_TOKEN_DIRS: &[&str] = &["mcp/oauth"];

/// In the state root only: the tokens a daemon writes for its unix socket.
pub const DAEMON_TOKEN_DIRS: &[&str] = &["daemon/auth"];

/// In a project's `.agix`: the files the engine writes the project's keys
/// to — see [`is_project_key_file`].
pub const PROJECT_KEY_FILES: &[&str] = &["settings.local.json", "mcp.local.json"];

/// Whether a file of this name in the state root or a scene directory holds
/// keys: its name, less a leading `.`, starts with a
/// [`STATE_ROOT_KEY_FILE_STEMS`] entry and has `.json` in it, which covers
/// `settings.local.json` and the temporary files the engine writes through.
pub fn is_state_root_key_file(name: &str) -> bool {
    let name = name.strip_prefix('.').unwrap_or(name);
    STATE_ROOT_KEY_FILE_STEMS
        .iter()
        .any(|stem| name.starts_with(stem))
        && name.contains(".json")
}

/// Whether a file of this name in a project's `.agix` holds the project's
/// keys: a [`PROJECT_KEY_FILES`] entry, or a temporary file named after one.
pub fn is_project_key_file(name: &str) -> bool {
    let name = name.strip_prefix('.').unwrap_or(name);
    PROJECT_KEY_FILES.iter().any(|file| name.starts_with(file))
}

/// Where credentials are, for one state root: what no tool reads or writes
/// and no permission mode lets through. The sandbox refuses the same
/// locations to a command, from the same constants.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    /// The state root as given and as the filesystem resolves it, lowercased
    /// component by component.
    state_roots: Vec<Vec<String>>,
}

impl Credentials {
    pub fn new(state_root: Option<&std::path::Path>) -> Self {
        let mut state_roots = Vec::new();
        if let Some(root) = state_root {
            state_roots.push(lowered_components(root));
            if let Ok(real) = std::fs::canonicalize(root) {
                let real = lowered_components(&real);
                if !state_roots.contains(&real) {
                    state_roots.push(real);
                }
            }
        }
        Self { state_roots }
    }

    /// Whether the absolute, normalized `path` is, or is under, a credential
    /// location. Compared without case: on a case-insensitive filesystem
    /// `Settings.JSON` is the same file.
    pub fn hold(&self, path: &std::path::Path) -> bool {
        let components = lowered_components(path);
        let names = |dirs: &[&str], rest: &[String]| {
            dirs.iter().any(|dir| {
                let dir: Vec<&str> = dir.split('/').collect();
                rest.len() >= dir.len() && dir.iter().zip(rest).all(|(d, c)| d == c)
            })
        };
        if components
            .iter()
            .any(|c| CREDENTIAL_DIR_NAMES.contains(&c.as_str()))
        {
            return true;
        }
        if components
            .windows(2)
            .any(|pair| pair[0] == ".agix" && is_project_key_file(&pair[1]))
        {
            return true;
        }
        self.state_roots.iter().any(|root| {
            let Some(rest) = components.strip_prefix(root.as_slice()) else {
                return false;
            };
            if names(DAEMON_TOKEN_DIRS, rest) {
                return true;
            }
            let in_config_dir = match rest {
                [scenes, _, inner @ ..] if scenes == "scenes" && !inner.is_empty() => inner,
                _ => rest,
            };
            [rest, in_config_dir].iter().any(|dir| {
                matches!(dir, [file] if is_state_root_key_file(file))
                    || names(STATE_ROOT_TOKEN_DIRS, dir)
            })
        })
    }
}

fn lowered_components(path: &std::path::Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect()
}

/// Under the state root, one temporary directory per session,
/// `<state root>/tmp/<session>`: the session's sandboxed commands may write
/// their own and no other.
pub const STATE_ROOT_SESSION_TMP: &str = "tmp";

/// What a data directory's `.gitignore` keeps out of the repository the
/// directory sits in: the files the engine writes keys into, and its
/// telemetry recordings.
pub const LOCAL_DATA_GITIGNORE: &str = "settings.local.json\nmcp.local.json\ntelemetry/\n";

/// Give `dir` a `.gitignore` of [`LOCAL_DATA_GITIGNORE`] if it has none. One
/// already there is the user's, and is left as it is.
pub fn ensure_local_data_gitignore(dir: &std::path::Path) {
    use std::io::Write;
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(".gitignore"));
    if let Ok(mut file) = created {
        let _ = file.write_all(LOCAL_DATA_GITIGNORE.as_bytes());
    }
}

/// Unified management of AGENT persistent directory paths.
///
/// Default: `global_data_dir = $HOME/.agix/`
///          `user_data_dir   = $HOME/.agix/scenes/<scope>/`
///          `local_data_dir  = <cwd>/.agix/`
///
/// `global_data_dir` (and therefore `user_data_dir`, derived from it) can be
/// overridden via `AGIX_DATA_DIR`; `local_data_dir` via `AGIX_LOCAL_DATA_DIR`.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// User-level, scene-specific override root. Default `$HOME/.agix/scenes/<scope>/`.
    pub user_data_dir: PathBuf,
    /// User-level, cross-scene global root (flat, shared by every scene).
    /// Default `$HOME/.agix/`.
    pub global_data_dir: PathBuf,
    /// Local/project data root. Default `<cwd>/.agix/`.
    pub local_data_dir: PathBuf,
}

impl ConfigPaths {
    /// Build from explicit roots.
    ///
    /// There is no constructor that consults the environment, and that is the
    /// point: a process serves one instance, and which directory that
    /// instance owns is the caller's to decide. Reading `$HOME` here would
    /// make every downstream module silently agree on the invoking user's
    /// home, which is wrong for a test, wrong for a service account, and
    /// wrong for any deployment that keeps its state somewhere else.
    ///
    /// `scope` identifies the product instance whose scene-specific override
    /// tree lives under `global_data_dir/scenes/<scope>/`.
    pub fn new(global_data_dir: PathBuf, local_data_dir: PathBuf, scope: &str) -> Self {
        Self {
            user_data_dir: global_data_dir.join("scenes").join(scope),
            global_data_dir,
            local_data_dir,
        }
    }

    /// The same roots [`crate::settings::Settings`] already
    /// carries — for the call sites that hold `Settings` and want these
    /// accessors without re-deriving anything.
    pub fn from_settings(paths: &crate::settings::PathSettings) -> Self {
        Self {
            user_data_dir: paths.user_data_dir.clone(),
            global_data_dir: paths.global_data_dir.clone(),
            local_data_dir: paths.local_data_dir.clone(),
        }
    }

    pub fn user_settings_path(&self) -> PathBuf {
        self.user_data_dir.join("settings.json")
    }

    pub fn user_skills_dir(&self) -> PathBuf {
        self.user_data_dir.join("skills")
    }

    /// Scene-specific installed plugins: `~/.agix/scenes/<scope>/plugins/`.
    pub fn user_plugins_dir(&self) -> PathBuf {
        self.user_data_dir.join("plugins")
    }

    /// Scene-specific subagent definitions: `~/.agix/scenes/<scope>/agents/`.
    /// Agix extension, no external standard. Loader not yet implemented.
    pub fn user_agents_dir(&self) -> PathBuf {
        self.user_data_dir.join("agents")
    }

    /// Scene-specific rule documents: `~/.agix/scenes/<scope>/rules/`.
    /// Agix extension, no external standard. Loader not yet implemented.
    pub fn user_rules_dir(&self) -> PathBuf {
        self.user_data_dir.join("rules")
    }

    /// Scene-specific hook scripts: `~/.agix/scenes/<scope>/hooks/`.
    /// Agix extension, no external standard. Loader not yet implemented.
    pub fn user_hooks_dir(&self) -> PathBuf {
        self.user_data_dir.join("hooks")
    }

    pub fn global_settings_path(&self) -> PathBuf {
        self.global_data_dir.join("settings.json")
    }

    pub fn global_skills_dir(&self) -> PathBuf {
        self.global_data_dir.join("skills")
    }

    pub fn global_plugins_dir(&self) -> PathBuf {
        self.global_data_dir.join("plugins")
    }

    /// Cross-scene memory root — used when there's no specific project open
    /// (reserved for future no-project/desktop use; today sessions always
    /// have a project, so `local_memory_dir()` is what's actually read).
    pub fn global_memory_dir(&self) -> PathBuf {
        self.global_data_dir.join("memory")
    }

    pub fn global_sessions_dir(&self) -> PathBuf {
        self.global_data_dir.join("sessions")
    }

    pub fn global_recordings_dir(&self) -> PathBuf {
        self.global_data_dir.join("recordings")
    }

    pub fn local_settings_path(&self) -> PathBuf {
        self.local_data_dir.join("settings.json")
    }

    pub fn local_memory_dir(&self) -> PathBuf {
        self.local_data_dir.join("memory")
    }

    pub fn local_sessions_dir(&self) -> PathBuf {
        self.local_data_dir.join("sessions")
    }

    pub fn local_recordings_dir(&self) -> PathBuf {
        self.local_data_dir.join("recordings")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> ConfigPaths {
        ConfigPaths::new(
            PathBuf::from("/state/.agix"),
            PathBuf::from("/tmp/test-project/.agix"),
            "code",
        )
    }

    #[test]
    fn credentials_are_the_state_roots_keys_and_tokens_and_a_projects_keys() {
        let creds = Credentials::new(Some(std::path::Path::new("/state/home")));
        for p in [
            "/state/home/settings.json",
            "/state/home/settings.local.json",
            "/state/home/.settings.json.tmp-1",
            "/state/home/mcp.local.json",
            "/state/home/Settings.JSON",
            "/state/home/mcp/oauth",
            "/state/home/mcp/oauth/x.json",
            "/state/home/scenes/code/settings.json",
            "/state/home/scenes/code/mcp/oauth/x.json",
            "/state/home/daemon/auth/desktop.token",
            "/repo/.agix/settings.local.json",
            "/repo/.agix/mcp.local.json",
            "/home/u/.ssh/id_rsa",
            "/home/u/.aws",
        ] {
            assert!(creds.hold(std::path::Path::new(p)), "{p}");
        }
        for p in [
            "/state/home",
            "/state/home/mcp",
            "/state/home/mcp.json",
            "/state/home/skills/x/SKILL.md",
            "/state/home/memory/settings.json",
            "/state/home/scenes/code/scene.json",
            "/state/home/scenes/settings.json/x",
            "/state/home/daemon/instances.d/x.json",
            "/state/homework/settings.json",
            "/repo/.agix/settings.json",
            "/repo/settings.local.json",
            "/repo/.github/x",
        ] {
            assert!(!creds.hold(std::path::Path::new(p)), "{p}");
        }
        assert!(!Credentials::new(None).hold(std::path::Path::new("/state/home/settings.json")));
    }

    #[test]
    fn roots_are_exactly_what_the_caller_passed() {
        let p = paths();
        assert_eq!(p.global_data_dir, PathBuf::from("/state/.agix"));
        assert_eq!(p.local_data_dir, PathBuf::from("/tmp/test-project/.agix"));
    }

    #[test]
    fn scene_dir_nests_under_global_dir() {
        let p = paths();
        assert_eq!(
            p.user_data_dir,
            p.global_data_dir.join("scenes").join("code")
        );
    }

    #[test]
    fn different_scopes_get_different_user_roots_but_same_global_root() {
        let global = PathBuf::from("/state/.agix");
        let local = PathBuf::from("/tmp/p/.agix");
        let code = ConfigPaths::new(global.clone(), local.clone(), "code");
        let ops = ConfigPaths::new(global, local, "ops");
        assert_ne!(code.user_data_dir, ops.user_data_dir);
        assert_eq!(code.global_data_dir, ops.global_data_dir);
        // Local root is scope-independent (project-level is flat).
        assert_eq!(code.local_data_dir, ops.local_data_dir);
    }

    /// The roots must survive the trip through `Settings` unchanged —
    /// `PathSettings` is how they actually reach most call sites.
    #[test]
    fn settings_roots_round_trip() {
        let p = paths();
        let carried = crate::settings::PathSettings {
            user_data_dir: p.user_data_dir.clone(),
            global_data_dir: p.global_data_dir.clone(),
            local_data_dir: p.local_data_dir.clone(),
            scope: "code".into(),
        };
        let back = ConfigPaths::from_settings(&carried);
        assert_eq!(back.user_data_dir, p.user_data_dir);
        assert_eq!(back.global_data_dir, p.global_data_dir);
        assert_eq!(back.local_data_dir, p.local_data_dir);
    }

    #[test]
    fn convenience_methods_derive_from_roots() {
        let p = paths();
        assert_eq!(
            p.user_settings_path(),
            p.user_data_dir.join("settings.json")
        );
        assert_eq!(p.user_skills_dir(), p.user_data_dir.join("skills"));
        assert_eq!(p.user_plugins_dir(), p.user_data_dir.join("plugins"));
        assert_eq!(p.user_agents_dir(), p.user_data_dir.join("agents"));
        assert_eq!(p.user_rules_dir(), p.user_data_dir.join("rules"));
        assert_eq!(p.user_hooks_dir(), p.user_data_dir.join("hooks"));
        assert_eq!(
            p.global_settings_path(),
            p.global_data_dir.join("settings.json")
        );
        assert_eq!(p.global_skills_dir(), p.global_data_dir.join("skills"));
        assert_eq!(p.global_memory_dir(), p.global_data_dir.join("memory"));
        assert_eq!(p.global_sessions_dir(), p.global_data_dir.join("sessions"));
        assert_eq!(
            p.global_recordings_dir(),
            p.global_data_dir.join("recordings")
        );
        assert_eq!(p.local_memory_dir(), p.local_data_dir.join("memory"));
        assert_eq!(p.local_sessions_dir(), p.local_data_dir.join("sessions"));
        assert_eq!(
            p.local_recordings_dir(),
            p.local_data_dir.join("recordings")
        );
    }
}
