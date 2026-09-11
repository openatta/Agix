//! The directory a scenario runs in.
//!
//! Both ways of starting a daemon need the same roots — a cross-scene global
//! root, a scene root, one or more project roots, and somewhere to put the
//! socket — and they reach them differently: this process hands
//! `StaticDaemonPaths` the paths outright, a spawned one is told where they
//! are with `ATTA_CONFIG_HOME` and `HOME`. A scenario knows only the
//! `World`, and each mode translates it.
//!
//! The layout mirrors a real `~/.atta` closely enough that a daemon cannot
//! tell the difference, which is the point: the resolution code under test is
//! the production code, not a test-only shortcut around it.

use std::path::{Path, PathBuf};

pub struct World {
    dir: tempfile::TempDir,
}

impl World {
    pub fn new() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let world = Self { dir };
        std::fs::create_dir_all(world.global_root())?;
        Ok(world)
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// What a spawned daemon gets as `HOME`. Nothing under it belongs to the
    /// machine's real user, so a run that reaches for a home directory writes
    /// here and the test can prove it did.
    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// `ATTA_CONFIG_HOME` — the cross-scene layer, `~/.atta`'s stand-in.
    pub fn global_root(&self) -> PathBuf {
        self.home().join(".atta")
    }

    pub fn config_root(&self, scene: &str) -> PathBuf {
        self.global_root().join("scenes").join(scene)
    }

    /// A project root, created on first ask. Several may coexist: one daemon
    /// serves them all, because `session.create` takes a `project_root` and
    /// the pool merges settings per project.
    pub fn project(&self, name: &str) -> anyhow::Result<PathBuf> {
        let p = self.dir.path().join("projects").join(name);
        std::fs::create_dir_all(&p)?;
        Ok(p)
    }

    pub fn write_global_settings(&self, settings: &serde_json::Value) -> anyhow::Result<()> {
        write_json(&self.global_root().join("settings.json"), settings)
    }

    pub fn write_scene_settings(
        &self,
        scene: &str,
        settings: &serde_json::Value,
    ) -> anyhow::Result<()> {
        write_json(&self.config_root(scene).join("settings.json"), settings)
    }

    pub fn write_project_settings(
        &self,
        project: &str,
        settings: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let root = self.project(project)?;
        write_json(&root.join(".atta").join("settings.json"), settings)
    }

    /// Put a file inside a project — a script, a skill, whatever the case
    /// needs the daemon to find where a real one would.
    pub fn write_project_file(
        &self,
        project: &str,
        relative: &str,
        contents: &str,
    ) -> anyhow::Result<PathBuf> {
        let path = self.project(project)?.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
        Ok(path)
    }

    /// A skill only this project has. The daemon merges three skill tiers;
    /// this is the one that belongs to a project rather than to the machine.
    pub fn write_project_skill(
        &self,
        project: &str,
        name: &str,
        description: &str,
    ) -> anyhow::Result<PathBuf> {
        self.write_project_file(
            project,
            &format!(".agents/skills/{name}/SKILL.md"),
            &format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
        )
    }

    /// A sub-agent type only this project has — the `Agent` tool's catalog
    /// is merged from the same three tiers as skills.
    pub fn write_project_agent_type(
        &self,
        project: &str,
        name: &str,
        description: &str,
    ) -> anyhow::Result<PathBuf> {
        self.write_project_file(
            project,
            &format!(".atta/agents/{name}.md"),
            &format!("---\ndescription: {description}\n---\nbody\n"),
        )
    }

    /// Where this project's transcripts are, resolved the way the daemon
    /// resolves them — through the canonical path, because a temp directory
    /// is a symlink on macOS and the sanitized name is built from the
    /// resolved one.
    pub fn transcript_dir(&self, project: &Path) -> PathBuf {
        let canonical = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
        history::path::project_dir(&self.global_root().join("projects"), &canonical)
    }

    /// Where a session that belongs to no project at all writes instead.
    pub fn no_project_transcript_dir(&self) -> PathBuf {
        history::path::project_dir_for(&self.global_root().join("projects"), None)
    }

    /// Where a daemon leaves its discovery entry — one file per instance,
    /// under the global root so every instance's lands in one directory.
    pub fn instances_dir(&self) -> PathBuf {
        self.global_root().join("daemon").join("instances.d")
    }

    /// Somewhere for a scenario to point `options.telemetry.output` at.
    pub fn telemetry_file(&self, name: &str) -> PathBuf {
        self.dir.path().join("telemetry").join(name)
    }

    /// Kept directly under the root rather than under the scene root a daemon
    /// would pick: a Unix socket path has about a hundred bytes to live in,
    /// and the temp directory has already spent most of them.
    pub fn socket(&self) -> PathBuf {
        self.dir.path().join("d.sock")
    }
}

fn write_json(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
