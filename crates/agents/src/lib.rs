//! Sub-agents, as a module: the `Agent` tool, the agent-type catalog, and the
//! `Team*` tools built on top of it.
//!
//! A session assembled with [`AgentsModule`] can delegate: it gets the `Agent`
//! tool (withheld at the depth limit), provides the session's agent spawner
//! to `"agent"` hooks and forked skills, and — when `execution.team_enabled`
//! and the scene does teamwork — the team tools. Each delegate it spawns is
//! assembled with the same modules as its parent.

pub mod agent_tool;
pub mod spawner;
mod type_watcher;

use std::sync::Arc;

use base::context::EngineConfig;
use runtime::module::{Module, ModuleError, SessionContext};

use crate::agent_tool::{AgentTool, SharedAgentTypeMap, SubagentSlots};
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
        let engine_config = Arc::new(EngineConfig::from_settings(settings));
        let max_depth = engine_config.max_agent_depth;

        let slots = self
            .slots
            .clone()
            .unwrap_or_else(|| SubagentSlots::new(settings.execution.max_concurrent_subagents));
        let mut tool = match &self.shared_types {
            Some(shared) => AgentTool::with_shared_agent_types(
                cx.model.clone(),
                engine_config,
                cx.tools.clone(),
                cx.tools.clone(),
                shared.clone(),
            )
            .with_session_agent_types(cx.plugin_agent_types),
            None => {
                let project = settings.paths.project_root().join(".agix").join("agents");
                let scene = settings.paths.user_data_dir.join("agents");
                let global = settings.paths.global_data_dir.join("agents");
                AgentTool::with_parent_tools(
                    cx.model.clone(),
                    engine_config,
                    cx.tools.clone(),
                    cx.tools.clone(),
                    &[&global, &scene, &project],
                    cx.plugin_agent_types,
                )
            }
        }
        .with_settings(settings.clone())
        .with_depth(cx.depth)
        .with_subagent_slots(slots.clone())
        .with_modules(self.for_delegates(cx.modules, &slots));
        if let Some(router) = cx.task_router {
            tool = tool.with_task_router(router.clone());
        }
        if !cx.delegate_plugins.is_empty() {
            tool = tool.with_delegate_plugins(cx.delegate_plugins.to_vec(), cx.plugin_tools);
        }
        if let Some(ledger) = cx.process_groups {
            tool = tool.with_process_groups(ledger.clone());
        }
        let tool = Arc::new(tool);

        // For `AgentTypeDefinition.mcp_servers` and `.skills`: a delegate is
        // granted a subset of what its parent already has connected/loaded.
        tool.set_mcp_tool_adapters(cx.mcp_tools());
        tool.set_skill_catalog(cx.skill_catalog());
        tool.set_event_sender(cx.events.clone());
        tool.set_hooks(cx.hooks.clone());
        tool.set_session_ended(cx.session_ended());
        tool.set_parent_permission(cx.permission.clone());
        tool.set_parent_pending_permissions(cx.pending_permissions.clone());
        tool.set_scene(cx.scene.clone());
        if let Some(registry) = cx.scene_registry {
            tool.set_scene_registry(registry.clone());
        }
        tool.set_instruction_file(cx.instruction_file.cloned());
        if let Some(store) = cx.history_store {
            tool.set_history_store(store.clone());
        }
        tool.set_parent_session_id(cx.session_id.to_string());
        tool.set_telemetry_handle(cx.telemetry.clone());

        let spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> =
            Arc::new(RuntimeAgentSpawner::new(tool.clone()));
        cx.provide_agent_spawner(spawner.clone());

        // Withheld at the depth limit, so the deepest agents are never even
        // offered it.
        if cx.depth < max_depth {
            cx.tools.register(tool.clone());
        }

        // The setting is the operator's switch; the scene's capability is what
        // the scene is for. A scene that does no teamwork never shows the
        // model these tools, rather than showing and refusing them.
        if settings.execution.team_enabled && cx.scene.supports_team() {
            // Shared by all three tools and by the `Agent` tool itself, which
            // spawns persistent members too (`team_name` + `name`).
            let registry = Arc::new(team::registry::TeamRegistry::new());
            tool.set_team_registry(registry.clone());
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
            create.set_telemetry_handle(cx.telemetry.clone());
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
