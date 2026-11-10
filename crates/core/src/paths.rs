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

use std::path::{Path, PathBuf};

/// The state root's subdirectories that are the agent's own working space
/// rather than configuration: the only parts of it the agent's tools and its
/// sandboxed commands may write.
pub const STATE_ROOT_WORKING_DIRS: &[&str] = &[STATE_ROOT_SCRATCHPAD, "memory"];

/// The state root's scratchpad, one of [`STATE_ROOT_WORKING_DIRS`].
pub const STATE_ROOT_SCRATCHPAD: &str = "scratchpad";

/// Where a session without a project works: a directory of its own in the
/// state root's scratchpad, `<state root>/scratchpad/sessions/<session id>`.
/// Its delegates share it — they inherit the session's settings.
pub fn session_scratch_dir(state_root: &Path, session_id: &str) -> PathBuf {
    let id: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    state_root
        .join(STATE_ROOT_SCRATCHPAD)
        .join("sessions")
        .join(id)
}

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

/// How the engine reads a file it puts in front of the model on its own —
/// skill listings and bodies, instruction files, rules, agent definitions,
/// output styles, memory. What the file resolves to may not be a
/// [`Credentials`] location, and a file spelled under `root` (the project,
/// for project content) may not resolve outside it: a repository can commit
/// a symbolic link to anything, and its own files are all it gets to show
/// the model. The state root's own files are the user's, not the project's,
/// even when the state root sits under `root` — a project opened at `$HOME`
/// holds `~/.agix`, and the user's own skills there may link anywhere.
#[derive(Debug, Clone, Default)]
pub struct PromptFiles {
    credentials: Credentials,
    root: Option<Spellings>,
    state_root: Option<Spellings>,
}

/// A directory as given, normalized, and as the filesystem resolves it.
#[derive(Debug, Clone)]
struct Spellings {
    given: PathBuf,
    real: PathBuf,
}

impl Spellings {
    fn of(dir: &std::path::Path) -> Self {
        let given = absolute_lexically(dir);
        let real = std::fs::canonicalize(&given).unwrap_or_else(|_| given.clone());
        Self { given, real }
    }

    fn spell(&self, path: &std::path::Path) -> bool {
        path.starts_with(&self.given) || path.starts_with(&self.real)
    }
}

impl PromptFiles {
    pub fn new(state_root: Option<&std::path::Path>, root: Option<&std::path::Path>) -> Self {
        Self {
            credentials: Credentials::new(state_root),
            root: root.map(Spellings::of),
            state_root: state_root.map(Spellings::of),
        }
    }

    /// For a session with these roots: the state root's credentials, and
    /// the project as the root. A session without a project reads nothing
    /// of a project's, so it has no root to confine.
    pub fn for_project(paths: &ConfigPaths) -> Self {
        Self::new(Some(&paths.global_data_dir), paths.project_root.as_deref())
    }

    /// Where `path` resolves, when the engine may read it for the model. A
    /// refusal is logged and comes back as `PermissionDenied`.
    pub fn resolve(&self, path: &std::path::Path) -> std::io::Result<PathBuf> {
        let resolved = std::fs::canonicalize(path)?;
        self.admit(path, &resolved)
            .map_err(|why| std::io::Error::new(std::io::ErrorKind::PermissionDenied, why))?;
        Ok(resolved)
    }

    /// [`Self::resolve`], then the text of what it resolved to.
    pub fn read_to_string(&self, path: &std::path::Path) -> std::io::Result<String> {
        std::fs::read_to_string(self.resolve(path)?)
    }

    /// The check [`Self::resolve`] makes, for a caller that resolved `path`
    /// through a filesystem of its own. A refusal is logged.
    pub fn admit(&self, path: &std::path::Path, resolved: &std::path::Path) -> Result<(), String> {
        let spelled = absolute_lexically(path);
        let refusal = if self.credentials.hold(&spelled) || self.credentials.hold(resolved) {
            Some("is a credential location".to_string())
        } else {
            self.root.as_ref().and_then(|root| {
                let user_owned = self.state_root.as_ref().is_some_and(|s| s.spell(&spelled));
                (root.spell(&spelled) && !user_owned && !resolved.starts_with(&root.real))
                    .then(|| format!("is outside {}", root.given.display()))
            })
        };
        match refusal {
            None => Ok(()),
            Some(why) => {
                let message = format!(
                    "not reading {} for the model: it resolves to {}, which {why}",
                    path.display(),
                    resolved.display()
                );
                tracing::warn!("{message}");
                Err(message)
            }
        }
    }
}

fn absolute_lexically(path: &std::path::Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
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
    /// The project, `None` for a session without one
    /// ([`crate::settings::PathSettings::project_root`]).
    pub project_root: Option<PathBuf>,
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
        let project_root = crate::settings::Workspace::project_of(&local_data_dir)
            .project_root()
            .map(PathBuf::from);
        Self {
            user_data_dir: global_data_dir.join("scenes").join(scope),
            global_data_dir,
            local_data_dir,
            project_root,
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
            project_root: paths.project_root().map(PathBuf::from),
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
    fn a_prompt_file_is_neither_a_credential_nor_a_link_out_of_the_root() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let (state, project, elsewhere) = (
            tmp.path().join("state"),
            tmp.path().join("project"),
            tmp.path().join("elsewhere"),
        );
        for dir in [
            &state,
            &project.join(".agix"),
            &project.join("docs"),
            &elsewhere,
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(state.join("settings.json"), "state key").unwrap();
        std::fs::write(project.join(".agix/settings.local.json"), "project key").unwrap();
        std::fs::write(project.join("docs/guide.md"), "guide").unwrap();
        std::fs::write(elsewhere.join("notes.md"), "notes").unwrap();
        symlink("docs/guide.md", project.join("AGENTS.md")).unwrap();
        symlink(".agix/settings.local.json", project.join("KEY.md")).unwrap();
        symlink("../elsewhere/notes.md", project.join("OUT.md")).unwrap();
        symlink(
            "../project/.agix/settings.local.json",
            elsewhere.join("key.md"),
        )
        .unwrap();
        symlink("../state/settings.json", elsewhere.join("state.md")).unwrap();

        let files = PromptFiles::new(Some(&state), Some(&project));
        let read = |p: PathBuf| files.read_to_string(&p).map_err(|e| e.kind());
        let denied = Err(std::io::ErrorKind::PermissionDenied);
        assert_eq!(read(project.join("AGENTS.md")), Ok("guide".into()));
        assert_eq!(read(project.join("KEY.md")), denied);
        assert_eq!(read(project.join(".agix/settings.local.json")), denied);
        assert_eq!(read(project.join("OUT.md")), denied);
        assert_eq!(read(elsewhere.join("notes.md")), Ok("notes".into()));
        assert_eq!(read(elsewhere.join("key.md")), denied);
        assert_eq!(read(elsewhere.join("state.md")), denied);
        assert_eq!(
            read(project.join("missing.md")),
            Err(std::io::ErrorKind::NotFound)
        );

        let nested_state = project.join("home-state");
        std::fs::create_dir_all(&nested_state).unwrap();
        symlink("../../elsewhere/notes.md", nested_state.join("skill.md")).unwrap();
        let files = PromptFiles::new(Some(&nested_state), Some(&project));
        assert_eq!(
            files
                .read_to_string(&nested_state.join("skill.md"))
                .map_err(|e| e.kind()),
            Ok("notes".into()),
            "the state root's files are the user's, wherever it sits"
        );
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
            workspace: crate::settings::Workspace::project_of(&p.local_data_dir),
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
