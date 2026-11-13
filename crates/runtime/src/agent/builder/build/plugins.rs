//! Phase 3: the plugins' layers, and how they fold into the chains the `Agent` runs with.

use super::*;

impl Builder {
    pub(super) fn register_plugins(
        &self,
        session: &SessionManager,
        settings: &LoadedSettings,
        scene: &SceneSpec,
    ) -> Result<PluginLayers, EngineError> {
        let agent_depth = self.spec.agent_depth;
        let mut layers = {
            let plugin_session = crate::plugin::PluginSession {
                session_id: session.session_id_str(),
                project_root: settings.paths.project_root(),
                scene: &scene.id,
                depth: agent_depth,
            };
            crate::plugin::register_all(
                &self.host.plugins,
                &plugin_session,
                self.ext.prompt_registry.as_deref(),
            )
            .map_err(EngineError::Internal)?
        };
        let plugin_agent_types: Vec<base::agent_type::AgentTypeDefinition> =
            std::mem::take(&mut layers.rust.agent_types)
                .into_iter()
                .chain(std::mem::take(&mut layers.quickjs.agent_types))
                .collect();
        Ok(PluginLayers {
            rust: layers.rust,
            quickjs: layers.quickjs,
            tool_owners: layers.tool_owners,
            agent_types: plugin_agent_types,
        })
    }
}

impl PluginLayers {
    pub(super) fn take_tools(&mut self) -> Vec<Arc<dyn base::tool::Tool>> {
        std::mem::take(&mut self.rust.tools)
            .into_iter()
            .chain(std::mem::take(&mut self.quickjs.tools))
            .collect()
    }

    pub(super) fn into_chains(self, builder: &mut Builder) -> Chains {
        let PluginLayers {
            rust: mut rust_layer,
            quickjs: mut quickjs_layer,
            ..
        } = self;
        let prompt_registry: Arc<dyn base::interface::prompt_registry::PromptRegistry> =
            if rust_layer.registers_on_prompt_registry()
                || quickjs_layer.registers_on_prompt_registry()
            {
                use base::interface::prompt_registry::PromptRegistry as _;
                let own = base::interface::prompt_registry::InMemoryPromptRegistry::new();
                for block in std::mem::take(&mut rust_layer.prompt_blocks) {
                    own.register_block(block);
                }
                for (name, provider, _) in std::mem::take(&mut rust_layer.prompt_variables) {
                    own.register_variable(&name, provider);
                }
                for (hook, authority) in std::mem::take(&mut rust_layer.assembly_hooks) {
                    own.register_assembly_hook(hook, authority);
                }
                for (hook, authority) in std::mem::take(&mut rust_layer.async_assembly_hooks) {
                    own.register_async_assembly_hook(hook, authority);
                }
                Arc::new(
                    base::interface::prompt_registry::LayeredPromptRegistry::new(
                        builder.ext.prompt_registry.take(),
                        own,
                        base::interface::prompt_registry::ScriptContributions {
                            blocks: std::mem::take(&mut quickjs_layer.prompt_blocks),
                            variables: std::mem::take(&mut quickjs_layer.prompt_variables),
                            assembly_hooks: std::mem::take(&mut quickjs_layer.assembly_hooks),
                            async_assembly_hooks: std::mem::take(
                                &mut quickjs_layer.async_assembly_hooks,
                            ),
                        },
                    ),
                )
            } else {
                builder
                    .ext
                    .prompt_registry
                    .take()
                    .unwrap_or_else(|| Arc::new(base::interface::prompt_registry::NoRegistrations))
            };
        // Rust first in a chain and innermost around a tool call; QuickJS
        // after it and outside it.
        let tool_middleware: Vec<_> = quickjs_layer
            .tool_middleware
            .into_iter()
            .chain(std::mem::take(&mut builder.host.tool_middleware))
            .chain(rust_layer.tool_middleware)
            .collect();
        let result_transformers: Vec<_> = std::mem::take(&mut builder.host.result_transformers)
            .into_iter()
            .chain(rust_layer.tool_results)
            .chain(quickjs_layer.tool_results)
            .collect();
        let model_interceptors: Vec<_> = std::mem::take(&mut builder.host.model_interceptors)
            .into_iter()
            .chain(rust_layer.model_interceptors)
            .chain(quickjs_layer.model_interceptors)
            .collect();
        let retrieval_hooks: Vec<_> = std::mem::take(&mut builder.host.retrieval_hooks)
            .into_iter()
            .chain(rust_layer.retrieval_hooks)
            .chain(quickjs_layer.retrieval_hooks)
            .collect();
        let prompt_middleware: Vec<_> = std::mem::take(&mut builder.host.prompt_middleware)
            .into_iter()
            .chain(rust_layer.prompt_middleware)
            .chain(quickjs_layer.prompt_middleware)
            .collect();
        let health_checks: Vec<_> = std::mem::take(&mut builder.ext.health_checks)
            .into_iter()
            .chain(rust_layer.health_checks)
            .chain(quickjs_layer.health_checks)
            .collect();
        let turn_starts: Vec<_> = rust_layer
            .turn_starts
            .into_iter()
            .chain(quickjs_layer.turn_starts)
            .collect();
        Chains {
            prompt_registry,
            tool_middleware,
            result_transformers,
            model_interceptors,
            retrieval_hooks,
            prompt_middleware,
            health_checks,
            turn_starts,
        }
    }
}
