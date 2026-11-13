//! The values the phases of `Builder::build` hand to one another.

use super::*;

pub(super) struct Resolved {
    pub(super) scene: Arc<SceneSpec>,
    pub(super) modules: Vec<Arc<dyn crate::module::Module>>,
    pub(super) model: Arc<dyn Model>,
    pub(super) template: Arc<dyn base::tool::ToolRegistry>,
    pub(super) tools: Arc<InMemoryToolRegistry>,
    pub(super) settings: Arc<LoadedSettings>,
    pub(super) telemetry_handle: TelemetryHandle,
    pub(super) permission: PermissionSet,
    pub(super) instruction_file: Option<PathBuf>,
    pub(super) instruction_provider:
        Arc<dyn base::interface::instruction_provider::InstructionProvider>,
    pub(super) claude_md_content: Option<String>,
    pub(super) environment: Arc<dyn base::interface::environment::Environment>,
    pub(super) rules: Arc<dyn base::interface::instruction_provider::RuleProvider>,
    pub(super) memory_store: Arc<MemoryStore>,
}

pub(super) struct SessionWiring {
    pub(super) state: Arc<base::context::SessionState>,
    pub(super) generation: u64,
    pub(super) injected: bool,
    pub(super) env: base::context::SessionEnv,
    pub(super) exec: base::interface::exec::ExecProviders,
    pub(super) ambient: hooks::payload::Ambient,
}

pub(super) struct CronWiring {
    pub(super) store: Option<Arc<tools::cron::CronStore>>,
    pub(super) fire_here: bool,
}

pub(super) struct PluginLayers {
    pub(super) rust: crate::plugin::Registrar,
    pub(super) quickjs: crate::plugin::Registrar,
    pub(super) tool_owners: std::collections::HashMap<String, crate::plugin::ToolOwner>,
    pub(super) agent_types: Vec<base::agent_type::AgentTypeDefinition>,
}

pub(super) struct HookWiring {
    pub(super) delegator: crate::module::LateDelegator,
    pub(super) session_ended: CancellationToken,
    pub(super) hooks: Arc<HookRunner>,
    pub(super) task_wake: Option<mpsc::UnboundedSender<()>>,
}

pub(super) struct EventWiring {
    pub(super) tx: crate::event_bus::EventBus,
    pub(super) rx: EventReceiver,
    pub(super) pending_permissions: PendingPermissions,
    pub(super) pending_questions: crate::elicitation::PendingQuestions,
}

/// Not `ToolSurface`: that name is the scene's allow/deny lists in `base`.
pub(super) struct SessionTools {
    pub(super) tools: Arc<InMemoryToolRegistry>,
    pub(super) policy: Arc<crate::tool_policy::ToolPolicy>,
}

pub(super) struct Chains {
    pub(super) prompt_registry: Arc<dyn base::interface::prompt_registry::PromptRegistry>,
    pub(super) tool_middleware: Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>,
    pub(super) result_transformers:
        Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>,
    pub(super) model_interceptors:
        Vec<Arc<dyn base::interface::model_interceptor::ModelInterceptor>>,
    pub(super) retrieval_hooks: Vec<Arc<dyn base::interface::memory_contracts::RetrievalHook>>,
    pub(super) prompt_middleware:
        Vec<Arc<dyn base::interface::prompt_middleware::PromptMiddleware>>,
    pub(super) health_checks: Vec<Arc<dyn base::interface::health::HealthCheck>>,
    pub(super) turn_starts: Vec<crate::plugin::TurnStart>,
}

pub(super) struct Wired {
    pub(super) resolved: Resolved,
    pub(super) config: Arc<base::context::EngineConfig>,
    pub(super) session: SessionManager,
    pub(super) session_state: Arc<base::context::SessionState>,
    pub(super) exec: base::interface::exec::ExecProviders,
    pub(super) attachment: base::context::EngineAttachment,
    pub(super) hooks: HookWiring,
    pub(super) events: EventWiring,
    pub(super) input_tx: InputSender,
    pub(super) input_rx: mpsc::UnboundedReceiver<InputMessage>,
    pub(super) skill_catalog: crate::module::LateSkillCatalog,
    pub(super) command_registry: Arc<crate::commands::CommandRegistry>,
    pub(super) mcp: Box<dyn base::interface::mcp_servers::McpServers>,
    pub(super) tool_policy: Arc<crate::tool_policy::ToolPolicy>,
    pub(super) compactor: Arc<dyn Compactor>,
    pub(super) agent_depth: u32,
    pub(super) chains: Chains,
}
