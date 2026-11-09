//! Skills, as a module: the session's skill catalog and the `Skill` tool.
//!
//! A session assembled with [`SkillsModule`] lists its skills to the model,
//! expands `/<skill>` commands, and can invoke skills through `Skill`.
//! Assemble it after the agents module for `context: fork` skills to fork;
//! without a spawner they run inline.

use std::sync::Arc;

use base::interface::settings::Settings;
use base::interface::skill_provider::SkillProvider;
use runtime::module::{Module, ModuleError, SessionContext};

use crate::manager::SkillManager;

pub const MODULE_ID: &str = "skills";

#[derive(Clone)]
pub struct SkillsModule {
    shared: Option<Arc<SkillManager>>,
    providers: Vec<Arc<dyn SkillProvider>>,
}

impl SkillsModule {
    /// Scan each session's own tiers, and watch them.
    pub fn new() -> Self {
        Self {
            shared: None,
            providers: Vec::new(),
        }
    }

    /// Use `catalog` — built and watched once by the host — for every session.
    pub fn shared(catalog: Arc<SkillManager>) -> Self {
        Self {
            shared: Some(catalog),
            providers: Vec::new(),
        }
    }

    /// Add a skill source of the host's own, after the tiers and built-ins: it
    /// fills names nobody claimed unless it declares
    /// [`SkillPrecedence::Override`](base::interface::skill_provider::SkillPrecedence::Override).
    pub fn provider(mut self, provider: Arc<dyn SkillProvider>) -> Self {
        self.providers.push(provider);
        self
    }
}

impl Default for SkillsModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for SkillsModule {
    fn id(&self) -> &'static str {
        MODULE_ID
    }

    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        let manager = match &self.shared {
            Some(shared) => shared.clone(),
            None => Arc::new(build_default_skill_manager(cx.settings)),
        };
        for provider in &self.providers {
            let id = provider.id().to_string();
            let count = manager.load_from(provider.clone());
            tracing::info!(source = %id, count, "host skill source registered");
        }
        let admission = crate::catalog::BundledAdmission::for_scene(cx.scene.as_ref());
        cx.provide_skill_catalog(Arc::new(crate::catalog::SceneSkills::new(
            manager.clone(),
            admission.clone(),
        )));
        crate::tool::register(
            cx.tools,
            manager,
            admission,
            cx.agent_spawner(),
            cx.permission.clone(),
        );
        Ok(())
    }
}

/// Scan the three skill-directory tiers from disk and register bundled
/// skills — what [`SkillsModule::new`] does per session. A daemon-style host
/// builds this once at the pool level instead and hands it to
/// [`SkillsModule::shared`].
///
/// Scans `~/.agix/skills/` (global default), then
/// `~/.agix/scenes/<scope>/skills/` (scene override — same name wins,
/// `load_dir_subdirs` overwrites by name), then `<project>/.agents/skills/`
/// (project — external fact standard, also scanned by Codex).
/// `load_dir_subdirs` (not the flat-only `load_dir`) so both `<name>.md` and
/// the `<name>/SKILL.md` subdirectory convention (what
/// `discover_for_paths`/`reload_skill` support) are picked up at startup.
///
/// Also starts this manager's own live-reload watcher (`notify` setup
/// failing degrades to no live reload, logged, not a hard error — every
/// other watcher in this codebase handles it the same way).
///
/// A host that already watches these directories wants
/// [`build_skill_manager_unwatched`] instead: one watcher per manager is
/// fine for one manager, and a `notify` backend has a per-process ceiling
/// that a host building one of these per project will reach.
pub fn build_default_skill_manager(settings: &Settings) -> SkillManager {
    build_skill_manager(settings, true)
}

/// [`build_default_skill_manager`] without the watcher — the same catalog,
/// reloaded by whoever is already watching rather than by a `notify` thread
/// of its own.
pub fn build_skill_manager_unwatched(settings: &Settings) -> SkillManager {
    build_skill_manager(settings, false)
}

/// The project's own skills whose body runs a command (`` !`cmd` `` or a
/// ` ```! ` block) — what waits for the project to be trusted. As
/// `skills/<name>`, for `project.status`.
pub fn project_skills_requiring_trust(settings: &Settings) -> Vec<String> {
    use crate::manager::SkillSource;
    if settings.project_layer.trust == base::project_trust::ProjectTrust::NoProject {
        return Vec::new();
    }
    let manager = SkillManager::new();
    let dir = settings.paths.project_root().join(".agents").join("skills");
    if manager
        .load_dir_subdirs(&dir, SkillSource::Project)
        .is_err()
    {
        return Vec::new();
    }
    let mut out: Vec<String> = manager
        .list()
        .into_iter()
        .filter(|skill| {
            manager
                .get_skill_content(&skill.name)
                .is_some_and(|body| crate::tool::runs_commands(&body))
        })
        .map(|skill| format!("skills/{}", skill.name))
        .collect();
    out.sort();
    out
}

fn build_skill_manager(settings: &Settings, watch: bool) -> SkillManager {
    use crate::manager::SkillSource;
    use crate::sources::BundledSkills;

    let skill_mgr = SkillManager::new();
    let project_skills_dir = settings.paths.project_root().join(".agents").join("skills");
    let tiers = [
        (
            settings.paths.global_data_dir.join("skills"),
            SkillSource::User,
        ),
        (
            settings.paths.user_data_dir.join("skills"),
            SkillSource::User,
        ),
        (project_skills_dir.clone(), SkillSource::Project),
    ];
    let loaded_count: usize = tiers
        .iter()
        .map(|(dir, source)| skill_mgr.load_dir_subdirs(dir, *source).unwrap_or(0))
        .sum();
    // Built-ins come last and defer: a project that writes its own skill of
    // the same name means to replace the one in the binary.
    skill_mgr.load_from(Arc::new(BundledSkills));
    let total_skills = skill_mgr.list().len();
    tracing::info!(loaded_count, total_skills, "skills loaded (incl. bundled)");

    if watch {
        if let Err(e) = skill_mgr.enable_watching(&[
            settings.paths.global_data_dir.join("skills"),
            settings.paths.user_data_dir.join("skills"),
            project_skills_dir,
        ]) {
            tracing::warn!(error = %e, "failed to enable skill file watching; live skill reload disabled for this session");
        }
    }
    skill_mgr
}
