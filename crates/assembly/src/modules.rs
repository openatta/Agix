//! The modules this build assembles into sessions — the one place the
//! assembly names them, and with the MCP plumbing in `session_pool` the only
//! place their features appear.
//!
//! Per project and scene, a [`ProjectModules`] holds what the modules share
//! across that pair's sessions (the skill catalog, the agent-type catalog);
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
    if cfg!(feature = "plugins") {
        out.push("plugins");
    }
    out
}

pub(crate) struct ProjectModules {
    /// Live-reloaded: a skill file added or removed after this was built is
    /// picked up without a rebuild, and every session on this pair sees it at
    /// once because they share this instance.
    #[cfg(feature = "skills")]
    skills: Arc<skills::manager::SkillManager>,
    #[cfg(feature = "agents")]
    agent_types: agents::agent_tool::SharedAgentTypeCatalog,
}

impl ProjectModules {
    /// Neither catalog starts a watcher of its own: the pool watches every
    /// tier through one `WatchHub` and calls [`Self::reload_skills`] /
    /// [`Self::refresh_agent_types`], so a second mechanism would be both
    /// redundant and — at one `notify` stream per catalog per project — the
    /// thing that runs a process out of them.
    pub(crate) fn build(settings: &Settings, plugin_agent_types: &[AgentTypeDefinition]) -> Self {
        let _ = (settings, plugin_agent_types);
        Self {
            #[cfg(feature = "skills")]
            skills: Arc::new(skills::module::build_skill_manager_unwatched(settings)),
            #[cfg(feature = "agents")]
            agent_types: {
                let dirs = agent_type_dirs(settings);
                let dirs_ref: [&std::path::Path; 3] = [&dirs[0], &dirs[1], &dirs[2]];
                agents::agent_tool::SharedAgentTypeCatalog::unwatched(&dirs_ref, plugin_agent_types)
            },
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
    /// already share.
    pub(crate) fn reload_skills(&self, settings: &Settings) {
        #[cfg(feature = "skills")]
        self.skills
            .replace_catalog(skills::module::build_skill_manager_unwatched(settings));
        #[cfg(not(feature = "skills"))]
        let _ = settings;
    }

    /// Re-merge the agent types from the tiers and what plugins contribute.
    pub(crate) fn refresh_agent_types(
        &self,
        settings: &Settings,
        plugin_agent_types: &[AgentTypeDefinition],
    ) {
        #[cfg(feature = "agents")]
        {
            let dirs = agent_type_dirs(settings);
            let dirs_ref: [&std::path::Path; 3] = [&dirs[0], &dirs[1], &dirs[2]];
            self.agent_types.refresh(&dirs_ref, plugin_agent_types);
        }
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

/// Global → scene → project `agents/*.md` directories.
#[cfg(feature = "agents")]
fn agent_type_dirs(settings: &Settings) -> [std::path::PathBuf; 3] {
    [
        settings.paths.global_data_dir.join("agents"),
        settings.paths.user_data_dir.join("agents"),
        settings.paths.project_root().join(".agix").join("agents"),
    ]
}
