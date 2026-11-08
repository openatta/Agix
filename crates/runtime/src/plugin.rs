//! `Plugin` — a Rust plugin: one named bundle of registrations and
//! interceptions, compiled into the host and registered once per session.
//!
//! The other extension kind is a QuickJS script (`script-host`). Where both
//! contribute to the same point, the order is fixed by layer rather than by
//! who called the builder first: Rust runs first in a chain and innermost
//! around a tool call, QuickJS after and outside it, and a QuickJS
//! registration whose authority allows it takes a name a Rust one also
//! claims. See `docs/extension_points.md`, 「叠加顺序」.
//!
//! Contracts — replacing a subsystem — are not here: they stay single-slot
//! setters on [`crate::agent::Builder`].

use std::path::Path;
use std::sync::Arc;

use base::agent_type::AgentTypeDefinition;
use base::interface::health::HealthCheck;
use base::interface::memory_contracts::RetrievalHook;
use base::interface::model_interceptor::ModelInterceptor;
use base::interface::prompt_assembly::AssemblyHook;
use base::interface::prompt_middleware::PromptMiddleware;
use base::interface::prompt_registry::{
    ContextProvider, PromptContent, RegisteredBlock, VariableProvider,
};
use base::interface::tool_middleware::ToolMiddleware;
use base::interface::tool_result::ToolResultTransformer;
use base::prompt::BlockRole;
use base::tool::Tool;

pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;

    /// Called for every session the plugin is added to, sub-agents included,
    /// each time that session is built.
    fn register(&self, session: &PluginSession<'_>, r: &mut Registrar);
}

pub struct PluginSession<'a> {
    pub session_id: &'a str,
    pub project_root: &'a Path,
    pub scene: &'a str,
    /// 0 for a session a host created, parent + 1 for a delegate.
    pub depth: u32,
}

#[derive(Default)]
pub struct Registrar {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) agent_types: Vec<AgentTypeDefinition>,
    pub(crate) tool_middleware: Vec<Arc<dyn ToolMiddleware>>,
    pub(crate) tool_results: Vec<Arc<dyn ToolResultTransformer>>,
    pub(crate) model_interceptors: Vec<Arc<dyn ModelInterceptor>>,
    pub(crate) retrieval_hooks: Vec<Arc<dyn RetrievalHook>>,
    pub(crate) prompt_middleware: Vec<Arc<dyn PromptMiddleware>>,
    pub(crate) prompt_blocks: Vec<RegisteredBlock>,
    pub(crate) prompt_variables: Vec<(String, VariableProvider)>,
    pub(crate) assembly_hooks: Vec<Arc<dyn AssemblyHook>>,
    pub(crate) health_checks: Vec<Arc<dyn HealthCheck>>,
}

impl Registrar {
    pub fn tool(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.push(tool);
        self
    }

    pub fn agent_type(&mut self, definition: AgentTypeDefinition) -> &mut Self {
        self.agent_types.push(definition);
        self
    }

    /// `tool.around`.
    pub fn tool_middleware(&mut self, m: Arc<dyn ToolMiddleware>) -> &mut Self {
        self.tool_middleware.push(m);
        self
    }

    /// `tool.result`.
    pub fn tool_result(&mut self, t: Arc<dyn ToolResultTransformer>) -> &mut Self {
        self.tool_results.push(t);
        self
    }

    /// `model.request` and `model.message`.
    pub fn model_interceptor(&mut self, i: Arc<dyn ModelInterceptor>) -> &mut Self {
        self.model_interceptors.push(i);
        self
    }

    /// `memory.retrieval_hook`.
    pub fn retrieval_hook(&mut self, h: Arc<dyn RetrievalHook>) -> &mut Self {
        self.retrieval_hooks.push(h);
        self
    }

    /// `prompt.submit`.
    pub fn prompt_middleware(&mut self, m: Arc<dyn PromptMiddleware>) -> &mut Self {
        self.prompt_middleware.push(m);
        self
    }

    /// `prompt.block`.
    pub fn prompt_block(&mut self, block: RegisteredBlock) -> &mut Self {
        self.prompt_blocks.push(block);
        self
    }

    /// `prompt.context`.
    pub fn prompt_context(
        &mut self,
        name: impl Into<String>,
        order: i32,
        provider: ContextProvider,
    ) -> &mut Self {
        self.prompt_blocks.push(RegisteredBlock {
            name: name.into(),
            role: BlockRole::System,
            order,
            content: PromptContent::Provider(provider),
            cache_strategy: None,
            origin: base::prompt::BlockOrigin::Kernel,
        });
        self
    }

    /// `prompt.variable`.
    pub fn prompt_variable(
        &mut self,
        name: impl Into<String>,
        provider: VariableProvider,
    ) -> &mut Self {
        self.prompt_variables.push((name.into(), provider));
        self
    }

    /// `prompt.assemble`.
    pub fn assembly_hook(&mut self, hook: Arc<dyn AssemblyHook>) -> &mut Self {
        self.assembly_hooks.push(hook);
        self
    }

    /// `health.check`.
    pub fn health_check(&mut self, check: Arc<dyn HealthCheck>) -> &mut Self {
        self.health_checks.push(check);
        self
    }

    pub(crate) fn registers_on_prompt_registry(&self) -> bool {
        !self.prompt_blocks.is_empty()
            || !self.prompt_variables.is_empty()
            || !self.assembly_hooks.is_empty()
    }
}
