//! Sub-agents, as a module: the `Agent` tool, the agent-type catalog, and the
//! `Team*` tools built on top of it.
//!
//! A session assembled with [`AgentsModule`] can delegate: it gets the `Agent`
//! tool (withheld at the depth limit), provides the session's delegator to
//! `"agent"` hooks, forked skills and the host, and — when
//! `execution.team_enabled` and the scene does teamwork — the team tools.
//! Each delegate it spawns is assembled with the same modules as its parent.

pub mod agent_tool;
mod delegate;
mod type_watcher;

use std::sync::Arc;

use base::context::EngineConfig;
use runtime::module::{Module, ModuleError, SessionContext};

use crate::agent_tool::{AgentCatalog, AgentTool, SharedAgentTypeMap};
use crate::delegate::{
    DelegateEnv, Delegator, HookAgents, Limits, PersistentMembers, SubagentSlots, ToolPool,
};

pub const MODULE_ID: &str = "agents";

#[derive(Clone, Default)]
pub struct AgentsModule {
    shared_types: Option<SharedAgentTypeMap>,
    /// Set on the copy a session's delegates are assembled with, so the
    /// whole tree draws on the root's `execution.max_concurrent_subagents`.
    slots: Option<SubagentSlots>,
    /// Set the same way: the root's hook agents and their descendants.
    hook_agents: Option<HookAgents>,
}

impl AgentsModule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve agent types from a catalog the host keeps (and watches) once
    /// for all its sessions, instead of scanning the `agents/` tiers per
    /// session — see [`agent_tool::SharedAgentTypeCatalog`].
    pub fn shared_agent_types(mut self, handle: SharedAgentTypeMap) -> Self {
        self.shared_types = Some(handle);
        self
    }
}

impl AgentsModule {
    fn for_delegates(
        &self,
        modules: &[Arc<dyn Module>],
        slots: &SubagentSlots,
        hook_agents: &HookAgents,
    ) -> Vec<Arc<dyn Module>> {
        modules
            .iter()
            .map(|m| -> Arc<dyn Module> {
                if m.id() == MODULE_ID {
                    Arc::new(Self {
                        shared_types: self.shared_types.clone(),
                        slots: Some(slots.clone()),
                        hook_agents: Some(hook_agents.clone()),
                    })
                } else {
                    m.clone()
                }
            })
            .collect()
    }
}

impl Module for AgentsModule {
    fn id(&self) -> &'static str {
        MODULE_ID
    }

    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        let settings = cx.settings;
        let template = Arc::new(base::tool::InMemoryToolRegistry::new());
        for t in cx.template_tools.list() {
            template.register(t);
        }
        let slots = self
            .slots
            .clone()
            .unwrap_or_else(|| SubagentSlots::new(settings.execution.max_concurrent_subagents));
        // The setting is the operator's switch; the scene's capability is what
        // the scene is for. A scene that does no teamwork never shows the
        // model these tools, rather than showing and refusing them.
        let team_registry = (settings.execution.team_enabled && cx.scene.supports_team)
            .then(|| Arc::new(team::registry::TeamRegistry::new()));

        let hook_agents = self.hook_agents.clone().unwrap_or_default();
        let mut env = DelegateEnv::capture(cx, &hook_agents);
        env.host.modules = self.for_delegates(&cx.host.modules, &slots, &hook_agents);
        let catalog = match &self.shared_types {
            Some(shared) => AgentCatalog::shared(shared.clone(), cx.plugin_agent_types),
            None => AgentCatalog::watched(
                &agent_tool::agent_type_dirs(settings),
                cx.plugin_agent_types,
                &env.config.prompt_files,
            ),
        };
        let limits = Limits::new(&env.config, env.depth, slots, hook_agents);
        let pool = ToolPool::new(template, cx.tools.clone());
        let members = PersistentMembers::new(team_registry.clone());
        let delegator = Delegator::new(env, catalog, pool, limits, members);
        let tool = Arc::new(AgentTool::new(delegator.clone()));

        let delegator: Arc<dyn base::interface::delegate::Delegator> = delegator;
        cx.provide_delegator(delegator.clone());

        // Withheld at the depth limit, so the deepest agents are never even
        // offered it.
        if cx.depth < EngineConfig::from_loaded(settings).max_agent_depth {
            cx.tools.register(tool.clone());
        }

        if let Some(registry) = team_registry {
            let coordinator =
                team::coordinator::DefaultCoordinator::with_delegator(delegator.clone())
                    .with_registry(registry.clone())
                    .with_scene_id(cx.scene.id.clone())
                    .with_events(cx.events.unbounded_bridge())
                    .with_telemetry(cx.host.telemetry.clone());
            cx.tools.register(Arc::new(team::tool::TeamCreateTool::new(
                cx.model.clone(),
                cx.config.clone(),
                Box::new(coordinator),
            )));
            cx.tools
                .register(Arc::new(team::tool::TeamListTool::new(registry.clone())));
            // Needs the delegator as well as the registry, to stop members
            // still alive, not just forget them.
            cx.tools
                .register(Arc::new(team::tool::TeamDeleteTool::new(Box::new(
                    team::coordinator::DefaultCoordinator::with_delegator(delegator)
                        .with_registry(registry),
                ))));
        }
        Ok(())
    }
}
