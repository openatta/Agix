//! The instance's sensitive locations, in one table: where credentials are,
//! what configures the engine, and which parts of the state root are the
//! agent's own working space.
//!
//! [`CREDENTIALS`] and the constants here are the data; [`SensitivePaths`] is
//! that data resolved against one session's state root and project, built
//! once and shared. The permission gate, the file tools, the prompt-file
//! reader and both sandbox backends ask it instead of keeping lists of their
//! own: the gate and the tools through [`SensitivePaths::is_credential`], the
//! backends through [`SensitivePaths::credential_locations`], which each
//! translates into its own mechanism.

use std::path::{Component, Path, PathBuf};

use super::ConfigPaths;

/// The state root's subdirectories that are the agent's own working space
/// rather than configuration: the only parts of it the agent's tools and its
/// sandboxed commands may write.
pub const STATE_ROOT_WORKING_DIRS: &[&str] = &[STATE_ROOT_SCRATCHPAD, "memory"];

/// The state root's scratchpad, one of [`STATE_ROOT_WORKING_DIRS`].
pub const STATE_ROOT_SCRATCHPAD: &str = "scratchpad";

/// Under the state root, one temporary directory per session,
/// `<state root>/tmp/<session>`: the session's sandboxed commands may write
/// their own and no other.
pub const STATE_ROOT_SESSION_TMP: &str = "tmp";

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

/// The repository's own configuration: confirmed like the engine's rather
/// than refused — `git` plumbing through `Bash` is normal work, and a
/// blanket deny on any path containing a `.git` component blocks far more
/// than it protects.
pub const REPOSITORY_CONFIG_DIR: &str = ".git";

/// Files in a git directory that decide which programs git runs and where it
/// reads its configuration from.
pub const GIT_CONTROL_FILES: &[&str] = &["config", "config.worktree", "commondir"];

/// The directory in a git directory whose programs git runs.
pub const GIT_HOOKS_DIR: &str = "hooks";

/// The files in the state root and in each `scenes/<scene>/` under it that a
/// sandboxed command must not rewrite: the settings, and — for a
/// declarative scene — the files that define it.
pub const STATE_ROOT_SCENE_FILES: &[&str] = &["settings.json", "scene.json", "prompt.md"];

/// Names of files that hold secrets wherever they are, which the write tools
/// refuse as a path component, as they do the [`credential_dir_names`].
pub const SECRET_FILE_NAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    "id_rsa",
    "id_ed25519",
    ".netrc",
    ".pypirc",
    ".npmrc",
    ".pgpass",
];

/// The credential stores in a user's home directory: a sandboxed command
/// cannot read them unless the settings allow it
/// (`base::interface::exec::default_deny_read_in`).
pub const HOME_CREDENTIAL_STORES: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".docker/config.json",
    ".kube",
    ".azure",
    ".config/gh",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".gem/credentials",
    ".git-credentials",
    ".config/git/credentials",
    ".config/gcloud",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".vault-token",
    ".terraform.d/credentials.tfrc.json",
    ".config/glab-cli",
    ".config/hub",
    ".config/doctl",
    ".oci",
    ".boto",
    ".s3cfg",
    ".databrickscfg",
    ".config/rclone",
    ".huggingface/token",
    ".cache/huggingface/token",
    ".claude/.credentials.json",
    ".codex/auth.json",
];

/// Where a credential is: no tool reads or writes it and no permission mode
/// lets a call through to it; a sandboxed command cannot read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credential {
    pub scope: Scope,
    pub secret: Secret,
}

/// The directory a [`Credential`]'s secret is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `<state root>/`.
    StateRoot,
    /// `<state root>/scenes/<any>/`, including a scene created later.
    EachSceneDir,
    /// Any `<dir>/.agix/`. The sandbox applies it to the project's `.agix`.
    AnyAgixDir,
    /// Any directory. The sandbox leaves it to `deny_read`, which covers the
    /// user's home.
    Anywhere,
}

/// What, in a [`Scope`]'s directory, is a [`Credential`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secret {
    /// The files whose names [`KeyFileNames::admits`].
    KeyFiles(KeyFileNames),
    /// This relative path and everything under it.
    Tree(&'static str),
    /// A directory of this name and everything under it.
    DirName(&'static str),
}

/// Which names a key file has, by the directory it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFileNames {
    /// The state root and the scene directories: the settings files, whose
    /// provider keys and MCP headers are plain text.
    StateRoot,
    /// A project's `.agix`: the files the engine writes the project's keys to.
    Project,
}

impl KeyFileNames {
    pub fn stems(self) -> &'static [&'static str] {
        match self {
            Self::StateRoot => &["settings", "mcp.local"],
            Self::Project => &["settings.local.json", "mcp.local.json"],
        }
    }

    /// Whether a file of this name is a key file: its name, less a leading
    /// `.`, starts with one of the [`Self::stems`] — which covers the
    /// temporary files the engine writes through — and, in the state root,
    /// has `.json` in it.
    pub fn admits(self, name: &str) -> bool {
        let name = name.strip_prefix('.').unwrap_or(name);
        let stem = self.stems().iter().any(|stem| name.starts_with(stem));
        match self {
            Self::StateRoot => stem && name.contains(".json"),
            Self::Project => stem,
        }
    }
}

impl Secret {
    /// Whether `rest`, the lowercased components below the scope's
    /// directory, is this secret or is under it.
    fn holds(self, rest: &[String]) -> bool {
        match self {
            Self::KeyFiles(names) => rest.first().is_some_and(|name| names.admits(name)),
            Self::Tree(path) | Self::DirName(path) => {
                let path: Vec<&str> = path.split('/').collect();
                rest.len() >= path.len() && path.iter().zip(rest).all(|(p, c)| p == c)
            }
        }
    }
}

/// Every credential location.
pub const CREDENTIALS: &[Credential] = &[
    Credential {
        scope: Scope::StateRoot,
        secret: Secret::KeyFiles(KeyFileNames::StateRoot),
    },
    Credential {
        scope: Scope::EachSceneDir,
        secret: Secret::KeyFiles(KeyFileNames::StateRoot),
    },
    Credential {
        scope: Scope::StateRoot,
        secret: Secret::Tree("mcp/oauth"),
    },
    Credential {
        scope: Scope::EachSceneDir,
        secret: Secret::Tree("mcp/oauth"),
    },
    Credential {
        scope: Scope::StateRoot,
        secret: Secret::Tree("daemon/auth"),
    },
    Credential {
        scope: Scope::AnyAgixDir,
        secret: Secret::KeyFiles(KeyFileNames::Project),
    },
    Credential {
        scope: Scope::Anywhere,
        secret: Secret::DirName(".ssh"),
    },
    Credential {
        scope: Scope::Anywhere,
        secret: Secret::DirName(".aws"),
    },
    Credential {
        scope: Scope::Anywhere,
        secret: Secret::DirName(".gnupg"),
    },
];

/// The names of the directories that hold credentials wherever they are.
pub fn credential_dir_names() -> impl Iterator<Item = &'static str> {
    CREDENTIALS.iter().filter_map(|c| match c.secret {
        Secret::DirName(name) if c.scope == Scope::Anywhere => Some(name),
        _ => None,
    })
}

/// One [`Credential`] placed on disk, for a sandbox backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The directory the secret is in, resolved through its symlinks.
    pub base: PathBuf,
    /// The secret is in each `base/scenes/<scene>/`, not in `base` itself.
    pub under_each_scene: bool,
    pub secret: Secret,
}

impl Located {
    /// The directories the secret is directly in, as they exist now, spelled
    /// under `base`.
    pub fn dirs(&self) -> Vec<PathBuf> {
        if self.under_each_scene {
            scene_dirs_in(&self.base)
                .into_iter()
                .map(|scene| scene.path)
                .collect()
        } else {
            vec![self.base.clone()]
        }
    }

    /// The scene directories among [`Self::dirs`] that are symbolic links,
    /// as they resolve: a backend that matches resolved paths against
    /// `base/scenes/<any>/` does not see the secret in them.
    pub fn linked_dirs_resolved(&self) -> Vec<PathBuf> {
        if !self.under_each_scene {
            return Vec::new();
        }
        scene_dirs_in(&self.base)
            .into_iter()
            .filter(|scene| scene.resolved != scene.path)
            .map(|scene| scene.resolved)
            .collect()
    }

    /// The paths that are this secret now.
    pub fn existing(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for dir in self.dirs() {
            match self.secret {
                Secret::KeyFiles(names) => {
                    let Ok(entries) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    found.extend(
                        entries
                            .flatten()
                            .filter(|e| {
                                e.file_name()
                                    .to_str()
                                    .is_some_and(|n| names.admits(&n.to_lowercase()))
                            })
                            .map(|e| e.path()),
                    );
                }
                Secret::Tree(path) | Secret::DirName(path) => found.push(dir.join(path)),
            }
        }
        found.retain(|p| p.symlink_metadata().is_ok());
        found.sort();
        found
    }
}

/// [`CREDENTIALS`] and the working space, resolved for one session: what its
/// state root and its project are.
#[derive(Debug, Clone, Default)]
pub struct SensitivePaths {
    state_root: Option<Spellings>,
    project: Option<Spellings>,
}

impl SensitivePaths {
    pub fn new(state_root: Option<&Path>, project: Option<&Path>) -> Self {
        Self {
            state_root: state_root.map(Spellings::of),
            project: project.map(Spellings::of),
        }
    }

    /// For a session with these roots. A session without a project has no
    /// project here either.
    pub fn for_paths(paths: &ConfigPaths) -> Self {
        Self::new(Some(&paths.global_data_dir), paths.project_root.as_deref())
    }

    pub fn state_root(&self) -> Option<&Path> {
        self.state_root.as_ref().map(|s| s.given.as_path())
    }

    /// Whether the absolute, normalized `abs` is, or is under, a credential
    /// location. Compared without case: on a case-insensitive filesystem
    /// `Settings.JSON` is the same file.
    pub fn is_credential(&self, abs: &Path) -> bool {
        let components = lowered_components(abs);
        CREDENTIALS
            .iter()
            .any(|credential| self.holds(credential, &components))
    }

    fn holds(&self, credential: &Credential, components: &[String]) -> bool {
        let secret = credential.secret;
        match credential.scope {
            Scope::Anywhere => (0..components.len()).any(|i| secret.holds(&components[i..])),
            Scope::AnyAgixDir => components
                .iter()
                .enumerate()
                .any(|(i, c)| c == ".agix" && secret.holds(&components[i + 1..])),
            Scope::StateRoot | Scope::EachSceneDir => self
                .state_root
                .iter()
                .flat_map(|root| &root.lowered)
                .any(|root| {
                    let Some(rest) = components.strip_prefix(root.as_slice()) else {
                        return false;
                    };
                    match (credential.scope, rest) {
                        (Scope::StateRoot, _) => secret.holds(rest),
                        (_, [scenes, _, inner @ ..]) => scenes == "scenes" && secret.holds(inner),
                        _ => false,
                    }
                }),
        }
    }

    /// Every [`CREDENTIALS`] entry a sandbox backend masks, placed on disk:
    /// those of the state root and its scenes under the state root as it
    /// resolves, those of any `.agix` in each of `agix_dirs`. The `Anywhere`
    /// ones are left out — the backends reach them through `deny_read`.
    pub fn credential_locations(&self, agix_dirs: &[PathBuf]) -> Vec<Located> {
        let mut located = Vec::new();
        for credential in CREDENTIALS {
            let at = |base: &Path| Located {
                base: base.to_path_buf(),
                under_each_scene: credential.scope == Scope::EachSceneDir,
                secret: credential.secret,
            };
            match credential.scope {
                Scope::StateRoot | Scope::EachSceneDir => {
                    located.extend(self.state_root.as_ref().map(|root| at(&root.real)));
                }
                Scope::AnyAgixDir => located.extend(agix_dirs.iter().map(|dir| at(dir))),
                Scope::Anywhere => {}
            }
        }
        located
    }

    /// Whether this path (or command argument) names configuration that
    /// must never be reached by a *silent* fast path, but which the user can
    /// legitimately approve: [`REPOSITORY_CONFIG_DIR`], an
    /// [`ENGINE_CONFIG_ROOTS`] directory outside its working space, or a
    /// [`PROJECT_ENGINE_CONFIG_DIRS`] directory.
    ///
    /// Not credentials: refusing them would make editing
    /// `.claude/settings.json` or `.agix/agents/*.md` impossible, and
    /// configuring the agent is a thing users routinely ask it to do. The
    /// risk is a tool quietly rewriting the rules it is judged by, so the
    /// gate does not let the tool's own `Allow` through for them.
    ///
    /// Matched by whole component — `.github` is not `.git`.
    pub fn names_configuration(text: &str) -> bool {
        let lowered = text.to_lowercase();
        let components: Vec<&str> = lowered.split('/').collect();
        // In a shell command the name runs on into whatever follows it.
        let name_in = |component: &str| {
            component
                .split(|c: char| !(c.is_alphanumeric() || "._-".contains(c)))
                .next()
                .unwrap_or_default()
                .to_string()
        };
        let opens_dir = |rest: &[&str], dir: &str| {
            let dir: Vec<&str> = dir.split('/').collect();
            let Some((last, above)) = dir.split_last() else {
                return false;
            };
            rest.len() >= dir.len()
                && above.iter().zip(rest).all(|(d, c)| d == c)
                && name_in(rest[above.len()]) == *last
        };
        lowered.contains('/')
            && components.iter().enumerate().any(|(i, component)| {
                let config_root = *component == REPOSITORY_CONFIG_DIR
                    || (ENGINE_CONFIG_ROOTS.contains(component)
                        && !(*component == ".agix"
                            && components.get(i + 1).is_some_and(|next| {
                                Self::is_engine_working_entry(&name_in(next))
                            })));
                config_root
                    || PROJECT_ENGINE_CONFIG_DIRS
                        .iter()
                        .any(|dir| opens_dir(&components[i..], dir))
            })
    }

    /// Whether `abs` is one of the [`ENGINE_CONFIG_FILES`] anywhere under an
    /// [`ENGINE_CONFIG_ROOTS`] directory — by name and location, not by
    /// directory: `.agix` also holds a sub-agent's worktree.
    pub fn is_engine_settings_file(abs: &Path) -> bool {
        let Some(name) = abs.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        ENGINE_CONFIG_FILES.contains(&name)
            && abs.components().any(|c| {
                matches!(c, Component::Normal(d) if ENGINE_CONFIG_ROOTS.iter().any(|root| d == *root))
            })
    }

    /// Whether this entry directly in `.agix` is the agent's working space
    /// rather than its configuration: the scratchpad its prompt sends
    /// temporary files to, the memory directory it is told to write to
    /// directly, or the worktrees its sessions work in.
    pub fn is_engine_working_entry(name: &str) -> bool {
        STATE_ROOT_WORKING_DIRS.contains(&name) || name == PROJECT_WORKTREES_DIR
    }

    /// The state root's [`STATE_ROOT_WORKING_DIRS`]: what the tools may write
    /// there.
    pub fn state_working_dirs(&self) -> Vec<PathBuf> {
        self.state_root
            .iter()
            .flat_map(|root| STATE_ROOT_WORKING_DIRS.iter().map(|d| root.given.join(d)))
            .collect()
    }

    /// The state root, all of which the tools may read but its credentials.
    pub fn readable_state_root(&self) -> Option<PathBuf> {
        self.state_root().map(Path::to_path_buf)
    }

    /// `<state root>/tmp/<session>`, the session's own temporary directory.
    pub fn session_tmp_dir(&self, session_id: &str) -> Option<PathBuf> {
        self.state_root().map(|root| {
            root.join(STATE_ROOT_SESSION_TMP)
                .join(session_dir_name(session_id))
        })
    }

    /// [`session_scratch_dir`] under this state root.
    pub fn session_scratch_dir(&self, session_id: &str) -> Option<PathBuf> {
        self.state_root()
            .map(|root| session_scratch_dir(root, session_id))
    }

    /// The scene directories under the state root as it resolves, as they
    /// exist now. One created later is still covered wherever a rule is
    /// written for [`Scope::EachSceneDir`].
    pub fn scene_dirs(&self) -> Vec<PathBuf> {
        self.state_root
            .as_ref()
            .map(|root| scene_dirs_in(&root.real))
            .unwrap_or_default()
            .into_iter()
            .map(|scene| scene.path)
            .collect()
    }

    /// How the engine reads a file of the project for the model.
    pub fn prompt_files(&self) -> PromptFiles {
        PromptFiles {
            paths: self.clone(),
            root: self.project.clone(),
        }
    }

    /// How the engine reads a file under `dir` for the model.
    pub fn prompt_files_within(&self, dir: &Path) -> PromptFiles {
        PromptFiles {
            paths: self.clone(),
            root: Some(Spellings::of(dir)),
        }
    }
}

/// Where a session without a project works: a directory of its own in the
/// state root's scratchpad, `<state root>/scratchpad/sessions/<session id>`.
/// Its delegates share it — they inherit the session's settings.
pub fn session_scratch_dir(state_root: &Path, session_id: &str) -> PathBuf {
    state_root
        .join(STATE_ROOT_SCRATCHPAD)
        .join("sessions")
        .join(session_dir_name(session_id))
}

fn session_dir_name(session_id: &str) -> String {
    session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

/// How the engine reads a file it puts in front of the model on its own —
/// skill listings and bodies, instruction files, rules, agent definitions,
/// output styles, memory. What the file resolves to may not be a credential
/// location, and a file spelled under `root` (the project, for project
/// content) may not resolve outside it: a repository can commit a symbolic
/// link to anything, and its own files are all it gets to show the model.
/// The state root's own files are the user's, not the project's, even when
/// the state root sits under `root` — a project opened at `$HOME` holds
/// `~/.agix`, and the user's own skills there may link anywhere.
#[derive(Debug, Clone)]
pub struct PromptFiles {
    paths: SensitivePaths,
    root: Option<Spellings>,
}

impl PromptFiles {
    /// With no state root and no root: only the credentials a name alone
    /// gives away are refused.
    pub fn unscoped() -> Self {
        SensitivePaths::default().prompt_files()
    }

    /// For a session with these roots: the state root's credentials, and
    /// the project as the root. A session without a project reads nothing
    /// of a project's, so it has no root to confine.
    pub fn for_project(paths: &ConfigPaths) -> Self {
        SensitivePaths::for_paths(paths).prompt_files()
    }

    /// Where `path` resolves, when the engine may read it for the model. A
    /// refusal is logged and comes back as `PermissionDenied`.
    pub fn resolve(&self, path: &Path) -> std::io::Result<PathBuf> {
        let resolved = std::fs::canonicalize(path)?;
        self.admit(path, &resolved)
            .map_err(|why| std::io::Error::new(std::io::ErrorKind::PermissionDenied, why))?;
        Ok(resolved)
    }

    /// [`Self::resolve`], then the text of what it resolved to.
    pub fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(self.resolve(path)?)
    }

    /// The check [`Self::resolve`] makes, for a caller that resolved `path`
    /// through a filesystem of its own. A refusal is logged.
    pub fn admit(&self, path: &Path, resolved: &Path) -> Result<(), String> {
        let spelled = absolute_lexically(path);
        let refusal = if self.paths.is_credential(&spelled) || self.paths.is_credential(resolved) {
            Some("is a credential location".to_string())
        } else {
            self.root.as_ref().and_then(|root| {
                let user_owned = self
                    .paths
                    .state_root
                    .as_ref()
                    .is_some_and(|s| s.spell(&spelled));
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

/// A directory as given, normalized, and as the filesystem resolves it;
/// and both lowercased component by component.
#[derive(Debug, Clone)]
struct Spellings {
    given: PathBuf,
    real: PathBuf,
    lowered: Vec<Vec<String>>,
}

impl Spellings {
    fn of(dir: &Path) -> Self {
        let given = absolute_lexically(dir);
        let real = std::fs::canonicalize(&given).unwrap_or_else(|_| given.clone());
        let mut lowered = vec![lowered_components(&given)];
        let real_lowered = lowered_components(&real);
        if !lowered.contains(&real_lowered) {
            lowered.push(real_lowered);
        }
        Self {
            given,
            real,
            lowered,
        }
    }

    fn spell(&self, path: &Path) -> bool {
        path.starts_with(&self.given) || path.starts_with(&self.real)
    }
}

/// A directory in `<state root>/scenes/`, possibly a symbolic link to one
/// elsewhere.
struct SceneDir {
    path: PathBuf,
    resolved: PathBuf,
}

/// The scene directories in `base/scenes/`, symbolic links to directories
/// included, in name order.
fn scene_dirs_in(base: &Path) -> Vec<SceneDir> {
    let Ok(entries) = std::fs::read_dir(base.join("scenes")) else {
        return Vec::new();
    };
    let mut dirs: Vec<SceneDir> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|path| path.is_dir())
        .map(|path| SceneDir {
            resolved: path.canonicalize().unwrap_or_else(|_| path.clone()),
            path,
        })
        .collect();
    dirs.sort_by(|a, b| a.path.cmp(&b.path));
    dirs
}

fn absolute_lexically(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn lowered_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensitive(state_root: &str) -> SensitivePaths {
        SensitivePaths::new(Some(Path::new(state_root)), None)
    }

    #[test]
    fn credentials_are_the_state_roots_keys_and_tokens_and_a_projects_keys() {
        let paths = sensitive("/state/home");
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
            assert!(paths.is_credential(Path::new(p)), "{p}");
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
            assert!(!paths.is_credential(Path::new(p)), "{p}");
        }
        assert!(!SensitivePaths::default().is_credential(Path::new("/state/home/settings.json")));
    }

    /// Where an entry of [`CREDENTIALS`] puts its secret, and a path beside
    /// it that the entry must not catch.
    fn example(credential: &Credential) -> (String, String) {
        let name = match credential.secret {
            Secret::KeyFiles(KeyFileNames::StateRoot) => "settings.json",
            Secret::KeyFiles(KeyFileNames::Project) => "settings.local.json",
            Secret::Tree(path) | Secret::DirName(path) => path,
        };
        match credential.scope {
            Scope::StateRoot => (format!("/state/home/{name}"), format!("/elsewhere/{name}")),
            Scope::EachSceneDir => (
                format!("/state/home/scenes/triage/{name}"),
                format!("/state/home/scenes/{name}"),
            ),
            Scope::AnyAgixDir => (format!("/repo/.agix/{name}"), format!("/repo/{name}")),
            Scope::Anywhere => (format!("/home/u/{name}/x"), format!("/home/u/{name}-not/x")),
        }
    }

    #[test]
    fn every_credential_is_recognized_and_every_scope_has_a_near_miss() {
        let paths = sensitive("/state/home");
        let mut scopes = Vec::new();
        for credential in CREDENTIALS {
            let (hit, miss) = example(credential);
            assert!(
                paths.is_credential(Path::new(&hit)),
                "{credential:?}: {hit}"
            );
            assert!(
                !paths.is_credential(Path::new(&miss)),
                "{credential:?}: {miss}"
            );
            if !scopes.contains(&credential.scope) {
                scopes.push(credential.scope);
            }
        }
        for scope in [
            Scope::StateRoot,
            Scope::EachSceneDir,
            Scope::AnyAgixDir,
            Scope::Anywhere,
        ] {
            assert!(scopes.contains(&scope), "no credential in {scope:?}");
        }
    }

    #[test]
    fn a_backend_gets_every_credential_but_the_ones_it_reaches_through_deny_read() {
        let paths = sensitive("/state/home");
        let agix = [PathBuf::from("/repo/.agix")];
        let located = paths.credential_locations(&agix);
        let expected = CREDENTIALS
            .iter()
            .filter(|c| c.scope != Scope::Anywhere)
            .count();
        assert_eq!(located.len(), expected, "{located:?}");
        assert!(located
            .iter()
            .all(|l| l.base == Path::new("/state/home") || l.base == Path::new("/repo/.agix")));
        assert!(SensitivePaths::default()
            .credential_locations(&[])
            .is_empty());
    }

    /// Linux 沙箱撤掉围栏的分支（引擎 git、pdftotext、项目在状态目录内）按
    /// `existing()` 逐个遮挡凭据：场景目录是符号链接时，它的密钥文件也在里面，
    /// 解析后的目录交给按解析路径匹配的后端。
    #[cfg(unix)]
    #[test]
    fn audit3_a_scene_linked_in_from_elsewhere_has_its_key_files_located() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let elsewhere = dir.path().join("dotfiles/scene");
        std::fs::create_dir_all(root.join("scenes")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("settings.json"), "").unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("scenes/linked")).unwrap();
        let root = root.canonicalize().unwrap();
        let located = SensitivePaths::new(Some(&root), None).credential_locations(&[]);
        let keys = located
            .iter()
            .find(|l| l.under_each_scene && matches!(l.secret, Secret::KeyFiles(_)))
            .unwrap();
        assert!(
            keys.existing()
                .contains(&root.join("scenes/linked/settings.json")),
            "{:?}",
            keys.existing()
        );
        assert_eq!(
            keys.linked_dirs_resolved(),
            vec![elsewhere.canonicalize().unwrap()]
        );
    }

    #[test]
    fn the_existing_secrets_of_a_location_are_found_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for scene in ["code", "triage"] {
            std::fs::create_dir_all(root.join("scenes").join(scene).join("mcp/oauth")).unwrap();
            std::fs::write(root.join("scenes").join(scene).join("settings.json"), "").unwrap();
        }
        std::fs::write(root.join("scenes/code/scene.json"), "").unwrap();
        let located = SensitivePaths::new(Some(&root), None).credential_locations(&[]);
        let keys = located
            .iter()
            .find(|l| l.under_each_scene && matches!(l.secret, Secret::KeyFiles(_)))
            .unwrap();
        assert_eq!(
            keys.existing(),
            vec![
                root.join("scenes/code/settings.json"),
                root.join("scenes/triage/settings.json")
            ]
        );
        let daemon = located
            .iter()
            .find(|l| l.secret == Secret::Tree("daemon/auth"))
            .unwrap();
        assert!(daemon.existing().is_empty());
    }

    #[test]
    fn the_roots_derive_from_the_state_root() {
        let paths = sensitive("/state/home");
        assert_eq!(
            paths.state_working_dirs(),
            vec![
                PathBuf::from("/state/home/scratchpad"),
                PathBuf::from("/state/home/memory")
            ]
        );
        assert_eq!(
            paths.session_tmp_dir("s-1/../x"),
            Some(PathBuf::from("/state/home/tmp/s-1x"))
        );
        assert_eq!(
            paths.session_scratch_dir("s-1/../x"),
            Some(PathBuf::from("/state/home/scratchpad/sessions/s-1x"))
        );
        assert_eq!(
            paths.readable_state_root(),
            Some(PathBuf::from("/state/home"))
        );
        let none = SensitivePaths::default();
        assert!(none.state_working_dirs().is_empty());
        assert_eq!(none.session_tmp_dir("s"), None);
    }

    /// `~/.ssh` 里 `.ssh` 前面是 `~`，嵌套在别的目录下、出现在命令行里的也一样
    /// 认得出；`.sshx` 这种只是前缀相同的不算。
    #[test]
    fn credential_dirs_are_whole_components_anywhere() {
        let paths = SensitivePaths::default();
        for p in [
            "~/.ssh/id_rsa",
            "cat ~/.ssh/id_rsa",
            "/home/u/.ssh/config",
            "~/.aws/credentials",
            "/Users/me/.gnupg/secring.gpg",
            "/repo/.github/workflows/../.ssh/id_rsa",
            "/a/b/c/d/e/f/.aws/credentials",
        ] {
            assert!(paths.is_credential(Path::new(p)), "{p}");
        }
        for p in ["/home/u/.sshx/config", "/repo/.github/x", "/repo/.awsome"] {
            assert!(!paths.is_credential(Path::new(p)), "{p}");
        }
    }

    /// 配置目录（`.git` / `.claude` / `.agix`）不是凭据，走的是"要确认"而不是
    /// "一律拒绝"——否则 agent 连自己的 `.claude/settings.json` 都改不了。
    #[test]
    fn config_dirs_require_confirmation() {
        for p in [
            "repo/.git/config",
            "/Users/me/.agix/settings.json",
            "/a/b/.claude/settings.local.json",
        ] {
            assert!(SensitivePaths::names_configuration(p), "{p}");
        }
        assert!(!SensitivePaths::names_configuration(
            "/repo/.github/workflows/ci.yml"
        ));
        assert!(!SensitivePaths::names_configuration("/repo/src/main.rs"));
    }

    #[test]
    fn a_projects_skills_are_configuration() {
        for p in [
            "/repo/.agents/skills/x/SKILL.md",
            "/repo/.agents/skills",
            ".agents/skills;",
            "/repo/.Agents/Skills/x/SKILL.md",
        ] {
            assert!(SensitivePaths::names_configuration(p), "{p}");
        }
        for p in [
            "/repo/.agents/notes.md",
            "/repo/.agents/skills2/x",
            "/repo/skills/x",
            "/repo/AGENTS.md",
        ] {
            assert!(!SensitivePaths::names_configuration(p), "{p}");
        }
    }

    #[test]
    fn the_agents_working_space_under_agix_is_not_configuration() {
        for p in [
            "/repo/.agix/scratchpad/cases.txt",
            "/Users/me/.agix/memory/MEMORY.md",
            "cd /repo\nS=/repo/.agix/scratchpad\nmkdir -p \"$S\"",
            "printf x > \"$d/.agix/scratchpad\"",
            "/repo/.agix/worktrees/feat-a/feature.txt",
            "cd /repo/.agix/worktrees/feat-a && git add -A",
        ] {
            assert!(!SensitivePaths::names_configuration(p), "{p}");
        }
        for p in [
            "/repo/.agix/agents/reviewer.md",
            "/repo/.agix/scratchpad.md",
            "/repo/.agix/worktrees/feat-a/.agix/agents/reviewer.md",
            "/repo/.agix/worktrees/feat-a/.claude/settings.json",
            "/repo/.agix/worktrees/feat-a/.git",
        ] {
            assert!(SensitivePaths::names_configuration(p), "{p}");
        }
    }

    #[test]
    fn what_the_gate_treats_as_working_space_the_sandbox_leaves_writable() {
        for name in STATE_ROOT_WORKING_DIRS
            .iter()
            .chain([&PROJECT_WORKTREES_DIR])
        {
            assert!(SensitivePaths::is_engine_working_entry(name), "{name}");
            assert!(
                !ENGINE_CONFIG_DIRS.contains(name) && !ENGINE_CONFIG_FILES.contains(name),
                "{name}"
            );
        }
    }

    #[test]
    fn engine_settings_files_are_known_by_name_under_a_config_root() {
        for p in [
            "/repo/.agix/settings.json",
            "/repo/.claude/settings.local.json",
            "/repo/.agix/worktrees/a/.agix/mcp.local.json",
            "/repo/.agix/agents/settings.json",
        ] {
            assert!(SensitivePaths::is_engine_settings_file(Path::new(p)), "{p}");
        }
        for p in [
            "/repo/settings.json",
            "/repo/.agix/scene.json",
            "/repo/.agix",
        ] {
            assert!(
                !SensitivePaths::is_engine_settings_file(Path::new(p)),
                "{p}"
            );
        }
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

        let files = SensitivePaths::new(Some(&state), Some(&project)).prompt_files();
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
        let files = SensitivePaths::new(Some(&nested_state), None).prompt_files_within(&project);
        assert_eq!(
            files
                .read_to_string(&nested_state.join("skill.md"))
                .map_err(|e| e.kind()),
            Ok("notes".into()),
            "the state root's files are the user's, wherever it sits"
        );
    }
}
