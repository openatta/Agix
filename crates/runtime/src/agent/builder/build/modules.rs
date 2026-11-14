//! Phases 9 and 10: running the modules' `on_session`, then the command registry.

use super::*;

pub(super) struct ModuleInputs<'a> {
    pub(super) resolved: &'a Resolved,
    pub(super) config: &'a Arc<base::context::EngineConfig>,
    pub(super) pending_permissions: &'a PendingPermissions,
    pub(super) hooks: &'a Arc<HookRunner>,
    pub(super) ambient: &'a hooks::payload::Ambient,
    pub(super) events: &'a crate::event_bus::EventBus,
    pub(super) session: &'a SessionManager,
    pub(super) session_state: &'a Arc<base::context::SessionState>,
    pub(super) depth: u32,
    pub(super) plugin_agent_types: &'a [base::agent_type::AgentTypeDefinition],
    pub(super) delegator: &'a crate::module::LateDelegator,
    pub(super) skill_catalog: &'a crate::module::LateSkillCatalog,
    pub(super) session_ended: &'a CancellationToken,
    pub(super) exec: &'a base::interface::exec::ExecProviders,
    pub(super) host_layers: HostLayers,
    pub(super) mcp: Option<Box<dyn base::interface::mcp_servers::McpServers>>,
}

impl Builder {
    pub(super) fn run_modules(
        &self,
        inputs: ModuleInputs<'_>,
    ) -> Result<Box<dyn base::interface::mcp_servers::McpServers>, EngineError> {
        let ModuleInputs {
            resolved,
            config,
            pending_permissions,
            hooks,
            ambient,
            events,
            session,
            session_state,
            depth,
            plugin_agent_types,
            delegator,
            skill_catalog,
            session_ended,
            exec,
            host_layers,
            mcp: mut mcp_slot,
        } = inputs;
        let Resolved {
            tools,
            template,
            settings,
            scene,
            model,
            rules,
            environment,
            permission,
            telemetry_handle: telemetry,
            memory_store,
            instruction_provider,
            modules,
            ..
        } = resolved;
        // Dispatch looks tools up in the registry, so a grant's adapters live
        // there as an mcp module's do.
        for adapter in mcp_slot.iter().flat_map(|m| m.tools()) {
            tools.register(adapter);
        }
        {
            let session_id = session.session_id_str().to_string();
            let host = HostContracts {
                exec: exec.clone(),
                token_counter: session.token_counter().clone(),
                rules: rules.clone(),
                environment: environment.clone(),
                permission: permission.clone(),
                history_store: self.host.history_store.clone(),
                telemetry: telemetry.clone(),
                task_router: self.host.task_router.clone(),
                scene_registry: self.host.scene_registry.clone(),
                memory_store: memory_store.clone(),
                memory_retriever: self.host.memory_retriever.clone(),
                retrieval_hooks: self.host.retrieval_hooks.clone(),
                instruction_provider: instruction_provider.clone(),
                plugins: self.host.plugins.clone(),
                modules: modules.to_vec(),
                host_layers,
            };
            let mut cx = crate::module::SessionContext {
                tools,
                template_tools: template,
                settings,
                config,
                scene,
                model,
                pending_permissions,
                hooks,
                hook_ambient: ambient,
                events,
                session_id: &session_id,
                session_state,
                depth,
                host: &host,
                plugin_agent_types,
                mcp: &mut mcp_slot,
                delegator,
                skill_catalog,
                session_ended,
            };
            for module in modules {
                module
                    .on_session(&mut cx)
                    .map_err(|e| EngineError::Internal(e.to_string()))?;
            }
        }
        Ok(mcp_slot.unwrap_or_else(|| Box::new(base::interface::mcp_servers::NoMcpServers)))
    }

    pub(super) fn command_registry(
        &self,
        skill_catalog: &crate::module::LateSkillCatalog,
        mcp: &dyn base::interface::mcp_servers::McpServers,
    ) -> Arc<crate::commands::CommandRegistry> {
        self.ext.commands_override.clone().unwrap_or_else(|| {
            let mut registry = crate::commands::CommandRegistry::from_skill_catalog(Arc::new(
                skill_catalog.clone(),
            ));
            // Only a registry this builder owns can be extended; a shared
            // `commands_override` is resolved off the live `McpManager` by
            // the slash-command interception in `turn/process.rs`.
            let mcp_prompts = mcp.prompt_commands();
            if !mcp_prompts.is_empty() {
                tracing::info!(
                    count = mcp_prompts.len(),
                    "registering MCP prompts as slash commands"
                );
                registry.register_mcp_prompts(&mcp_prompts);
            }
            std::sync::Arc::new(registry)
        })
    }
}
