//! The modules this build assembles into sessions — the one place the
//! assembly names them, and with the MCP plumbing in `session_pool` the only
//! place their features appear.
//!
//! Per project and scene, a [`ProjectModules`] holds what the modules share
//! across that pair's sessions (the skill catalog, the agent-type catalog,
//! and what installed packages contribute to both);
//! per session, [`ProjectModules::session_modules`] hands the builder the
//! modules in the order they depend on each other: MCP (added by the pool,
//! which owns the connections) first, then agents, then skills.

use std::sync::Arc;

use base::agent_type::AgentTypeDefinition;
use base::interface::settings::Settings;
use runtime::commands::CommandRegistry;
use runtime::module::Module;

/// The modules compiled into this build, in assembly order.
pub fn compiled() -> Vec<&'static str> {
    let mut out = Vec::new();
    if cfg!(feature = "mcp") {
        out.push("mcp");
    }
    if cfg!(feature = "agents") {
        out.push("agents");
    }
    if cfg!(feature = "skills") {
        out.push("skills");
    }
    if cfg!(feature = "scripts") {
        out.push("scripts");
    }
    if cfg!(feature = "plugin-packages") {
        out.push("plugin-packages");
    }
    out
}

/// The settings that configure a module this build does not carry, named
/// so a section that will be ignored says so instead of doing nothing.
pub fn unassembled_settings(settings: &Settings) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    if !cfg!(feature = "mcp") && !settings.mcp_servers.is_empty() {
        out.push(("mcp_servers", "mcp"));
    }
    if !cfg!(feature = "agents") && settings.execution.team_enabled {
        out.push(("execution.team_enabled", "agents"));
    }
    out
}

pub(crate) struct ProjectModules {
    /// Live-reloaded: a skill file added or removed after this was built is
    /// picked up without a rebuild, and every session on this pair sees it at
    /// once because they share this instance.
    #[cfg(feature = "skills")]
    skills: Arc<skills::manager::SkillManager>,
    /// Kept so a rescan of the skill directories rebuilds the catalog with
    /// the packages' skills rather than without them.
    #[cfg(feature = "skills")]
    plugin_skills: std::sync::RwLock<Vec<crate::plugins::PackageSkills>>,
    #[cfg(feature = "agents")]
    agent_types: agents::agent_tool::SharedAgentTypeCatalog,
}

impl ProjectModules {
    /// Neither catalog starts a watcher of its own: the pool watches every
    /// tier through one `WatchHub` and calls [`Self::reload_skills`] /
    /// [`Self::refresh_agent_types`], so a second mechanism would be both
    /// redundant and — at one `notify` stream per catalog per project — the
    /// thing that runs a process out of them.
    pub(crate) fn build(settings: &Settings, plugins: &crate::plugins::Contributions) -> Self {
        let _ = (settings, plugins);
        Self {
            #[cfg(feature = "skills")]
            skills: Arc::new(skill_catalog(settings, &plugins.skills)),
            #[cfg(feature = "skills")]
            plugin_skills: std::sync::RwLock::new(plugins.skills.clone()),
            #[cfg(feature = "agents")]
            agent_types: agents::agent_tool::SharedAgentTypeCatalog::unwatched(
                &agents::agent_tool::agent_type_dirs(settings),
                &plugins.agent_types,
            ),
        }
    }

    /// The command catalog shared by every session on the pair: the built-in
    /// local commands plus a live view of the skill catalog, so a skill file
    /// added or deleted later is reflected — in `commands.list` too — without
    /// a rebuild.
    pub(crate) fn commands(&self) -> Arc<CommandRegistry> {
        #[cfg(feature = "skills")]
        return Arc::new(CommandRegistry::from_skill_catalog(self.skills.clone()));
        #[cfg(not(feature = "skills"))]
        Arc::new(CommandRegistry::default())
    }

    pub(crate) fn skill_count(&self) -> usize {
        #[cfg(feature = "skills")]
        return self.skills.list().len();
        #[cfg(not(feature = "skills"))]
        0
    }

    /// Re-read the skill directories into the catalog the pair's sessions
    /// already share. A running session notices the difference at the start
    /// of its next turn.
    pub(crate) fn reload_skills(&self, settings: &Settings) {
        #[cfg(feature = "skills")]
        {
            let packages = self
                .plugin_skills
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            self.skills
                .replace_catalog(skill_catalog(settings, &packages));
        }
        #[cfg(not(feature = "skills"))]
        let _ = settings;
    }

    /// Take up what the active packages contribute now.
    pub(crate) fn refresh_plugins(
        &self,
        settings: &Settings,
        plugins: &crate::plugins::Contributions,
    ) {
        self.refresh_agent_types(settings, &plugins.agent_types);
        #[cfg(feature = "skills")]
        {
            *self
                .plugin_skills
                .write()
                .unwrap_or_else(|e| e.into_inner()) = plugins.skills.clone();
            self.reload_skills(settings);
        }
    }

    /// Package skills whose name another source holds in this catalog, by
    /// package: which source, and the file it was read from.
    pub(crate) fn skill_conflicts(
        &self,
    ) -> std::collections::HashMap<String, Vec<serde_json::Value>> {
        #[cfg_attr(not(feature = "skills"), allow(unused_mut))]
        let mut out: std::collections::HashMap<String, Vec<serde_json::Value>> =
            std::collections::HashMap::new();
        #[cfg(feature = "skills")]
        {
            let packages = self.plugin_skills.read().unwrap_or_else(|e| e.into_inner());
            let package_of = |path: &std::path::Path| {
                packages
                    .iter()
                    .find(|p| p.skills.iter().any(|(entry, _)| entry.path == path))
                    .map(|p| p.plugin.clone())
            };
            for package in packages.iter() {
                for (entry, _) in &package.skills {
                    let Some(holder) = self.skills.get(&entry.name) else {
                        continue;
                    };
                    if holder.path == entry.path {
                        continue;
                    }
                    let mut conflict = serde_json::json!({
                        "skill": entry.name,
                        "shadowed_by": holder.source.as_str(),
                        "path": holder.path.display().to_string(),
                    });
                    if let Some(other) = package_of(&holder.path) {
                        conflict["plugin"] = other.into();
                    }
                    out.entry(package.plugin.clone())
                        .or_default()
                        .push(conflict);
                }
            }
        }
        out
    }

    /// Re-merge the agent types from the tiers and what plugins contribute.
    pub(crate) fn refresh_agent_types(
        &self,
        settings: &Settings,
        plugin_agent_types: &[AgentTypeDefinition],
    ) {
        #[cfg(feature = "agents")]
        self.agent_types.refresh(
            &agents::agent_tool::agent_type_dirs(settings),
            plugin_agent_types,
        );
        #[cfg(not(feature = "agents"))]
        let _ = (settings, plugin_agent_types);
    }

    #[cfg(all(test, feature = "skills"))]
    pub(crate) fn skills(&self) -> &Arc<skills::manager::SkillManager> {
        &self.skills
    }

    /// Everything after MCP, in order. Pushed one by one because each push
    /// is behind its own feature.
    #[allow(clippy::vec_init_then_push)]
    pub(crate) fn session_modules(&self) -> Vec<Arc<dyn Module>> {
        #[cfg_attr(not(any(feature = "agents", feature = "skills")), allow(unused_mut))]
        let mut out: Vec<Arc<dyn Module>> = Vec::new();
        #[cfg(feature = "agents")]
        out.push(Arc::new(
            agents::AgentsModule::new().shared_agent_types(self.agent_types.handle()),
        ));
        #[cfg(feature = "skills")]
        out.push(Arc::new(skills::SkillsModule::shared(self.skills.clone())));
        out
    }
}

#[cfg(feature = "skills")]
fn skill_catalog(
    settings: &Settings,
    packages: &[crate::plugins::PackageSkills],
) -> skills::manager::SkillManager {
    let providers: Vec<Arc<dyn base::interface::skill_provider::SkillProvider>> = packages
        .iter()
        .map(|p| {
            Arc::new(skills::sources::PluginSkills::new(
                &p.plugin,
                p.skills.clone(),
            )) as Arc<dyn base::interface::skill_provider::SkillProvider>
        })
        .collect();
    skills::module::build_skill_manager_with_plugins(settings, &providers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_for_a_missing_module_is_named() {
        let mut settings = Settings::defaults_for("test");
        settings
            .mcp_servers
            .insert("x".into(), serde_json::json!({}));
        settings.execution.team_enabled = true;
        let named = unassembled_settings(&settings);
        assert_eq!(
            named.contains(&("mcp_servers", "mcp")),
            !cfg!(feature = "mcp")
        );
        assert_eq!(
            named.contains(&("execution.team_enabled", "agents")),
            !cfg!(feature = "agents")
        );
    }

    #[cfg(feature = "skills")]
    fn package(plugin: &str, names: &[&str]) -> crate::plugins::PackageSkills {
        crate::plugins::PackageSkills {
            plugin: plugin.into(),
            skills: names
                .iter()
                .map(|name| {
                    let entry = base::frozen::skill::SkillEntry {
                        name: (*name).into(),
                        description: format!("{plugin} {name}"),
                        source: base::frozen::skill::SkillSource::Plugin,
                        path: format!("/cache/{plugin}/skills/{name}/SKILL.md").into(),
                        ..Default::default()
                    };
                    (entry, format!("{plugin} body"))
                })
                .collect(),
        }
    }

    /// A package skill that loses its name is still listed with its package,
    /// and the conflict says who has the name: the disk, or the package that
    /// sorts first.
    #[cfg(feature = "skills")]
    #[test]
    fn a_shadowed_package_skill_is_reported_with_what_shadows_it() {
        let home = tempfile::tempdir().unwrap();
        let mut settings = Settings::defaults_for("test");
        settings.paths.global_data_dir = home.path().join("global");
        settings.paths.user_data_dir = home.path().join("scene");
        settings.paths.local_data_dir = home.path().join("project/.agix");
        let disk = home.path().join("project/.agents/skills/tour");
        std::fs::create_dir_all(&disk).unwrap();
        std::fs::write(
            disk.join("SKILL.md"),
            "---\ndescription: the project's tour\n---\nbody\n",
        )
        .unwrap();

        let modules = ProjectModules::build(
            &settings,
            &crate::plugins::Contributions {
                skills: vec![
                    package("alpha", &["guide", "tour"]),
                    package("beta", &["guide", "solo"]),
                ],
                ..Default::default()
            },
        );
        let conflicts = modules.skill_conflicts();

        let alpha = &conflicts["alpha"];
        assert_eq!(alpha.len(), 1, "{conflicts:?}");
        assert_eq!(alpha[0]["skill"], "tour");
        assert_eq!(alpha[0]["shadowed_by"], "project");
        assert!(alpha[0]["path"]
            .as_str()
            .unwrap()
            .ends_with(".agents/skills/tour/SKILL.md"));
        assert!(alpha[0].get("plugin").is_none());

        let beta = &conflicts["beta"];
        assert_eq!(beta.len(), 1, "{conflicts:?}");
        assert_eq!(beta[0]["skill"], "guide");
        assert_eq!(beta[0]["shadowed_by"], "plugin");
        assert_eq!(beta[0]["plugin"], "alpha");

        modules.refresh_plugins(
            &settings,
            &crate::plugins::Contributions {
                skills: vec![package("beta", &["guide", "solo"])],
                ..Default::default()
            },
        );
        assert!(modules.skill_conflicts().is_empty());
        assert_eq!(
            modules.skills().get_skill_content("guide").as_deref(),
            Some("beta body"),
            "with alpha gone, beta's skill has the name"
        );
        assert!(modules.skills().get("solo").is_some());
    }
}
