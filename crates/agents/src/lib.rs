//! Sub-agents, as a module: the `Agent` tool, the agent-type catalog, and the
//! `Team*` tools built on top of it.
//!
//! A session assembled with [`AgentsModule`] can delegate: it gets the `Agent`
//! tool (withheld at the depth limit), provides the session's agent spawner
//! to `"agent"` hooks and forked skills, and — when `execution.team_enabled`
//! and the scene does teamwork — the team tools. Each delegate it spawns is
//! assembled with the same modules as its parent.

pub mod agent_tool;
mod delegate;
pub mod spawner;
mod type_watcher;

use std::sync::Arc;

use base::context::EngineConfig;
use runtime::module::{Module, ModuleError, SessionContext};

use crate::agent_tool::{AgentTool, Parts, SharedAgentTypeMap, SubagentSlots, TypeCatalog};
use crate::delegate::DelegateEnv;
use crate::spawner::RuntimeAgentSpawner;

pub const MODULE_ID: &str = "agents";

#[derive(Clone, Default)]
pub struct AgentsModule {
    shared_types: Option<SharedAgentTypeMap>,
    /// Set on the copy a session's delegates are assembled with, so the
    /// whole tree draws on the root's `execution.max_concurrent_subagents`.
    slots: Option<SubagentSlots>,
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
    ) -> Vec<Arc<dyn Module>> {
        modules
            .iter()
            .map(|m| -> Arc<dyn Module> {
                if m.id() == MODULE_ID {
                    Arc::new(Self {
                        shared_types: self.shared_types.clone(),
                        slots: Some(slots.clone()),
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
        let team_registry = (settings.execution.team_enabled && cx.scene.supports_team())
            .then(|| Arc::new(team::registry::TeamRegistry::new()));

        let mut env = DelegateEnv::capture(cx);
        env.host.modules = self.for_delegates(&cx.host.modules, &slots);
        let agent_dirs = agent_tool::agent_type_dirs(settings);
        let types = match &self.shared_types {
            Some(shared) => TypeCatalog::Shared {
                map: shared.clone(),
                session_types: cx.plugin_agent_types,
            },
            None => TypeCatalog::Watched {
                dirs: &agent_dirs,
                plugin_types: cx.plugin_agent_types,
            },
        };
        let tool = Arc::new(AgentTool::new(
            env,
            Parts {
                types,
                template,
                parent_tools: cx.tools.clone(),
                plugin_tools: cx.plugin_tools,
                slots,
                team_registry: team_registry.clone(),
            },
        ));

        let spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> =
            Arc::new(RuntimeAgentSpawner::new(tool.clone()));
        cx.provide_agent_spawner(spawner.clone());

        // Withheld at the depth limit, so the deepest agents are never even
        // offered it.
        if cx.depth < EngineConfig::from_loaded(settings).max_agent_depth {
            cx.tools.register(tool.clone());
        }

        if let Some(registry) = team_registry {
            let create = Arc::new(team::tool::TeamCreateTool::with_spawner(
                cx.model.clone(),
                cx.config.clone(),
                cx.tools.clone(),
                cx.tools.clone(),
                spawner.clone(),
                registry.clone(),
                cx.scene.id().to_string(),
            ));
            create.set_event_sender(cx.events.unbounded_bridge());
            create.set_telemetry_handle(cx.host.telemetry.clone());
            cx.tools.register(create);
            cx.tools
                .register(Arc::new(team::tool::TeamListTool::new(registry.clone())));
            // Needs the spawner as well as the registry, to stop members still
            // alive, not just forget them.
            cx.tools
                .register(Arc::new(team::tool::TeamDeleteTool::new(Box::new(
                    team::coordinator::DefaultCoordinator::with_agent_spawner(spawner)
                        .with_registry(registry),
                ))));
        }
        Ok(())
    }
}
