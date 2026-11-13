//! The setters' state, in three groups: what the host answers for the deployment, what this
//! session is, and the extension points a caller may plug into.

use super::*;

/// What the host answered for this machine and deployment: becomes `HostContracts` and is inherited whole by a delegate.
pub(super) struct HostSetup {
    pub(super) permission: Option<PermissionSet>,
    pub(super) memory_store: Option<Arc<MemoryStore>>,
    pub(super) compactor: Option<Arc<dyn Compactor>>,
    /// Backing store for session transcript persistence (JSONL append/resume).
    /// `None` = in-memory only, matching prior behavior — this is purely
    /// additive, existing callers that don't set it are unaffected.
    pub(super) history_store: Option<Arc<dyn history::store::HistoryStore>>,
    pub(super) telemetry_handle_override: Option<TelemetryHandle>,
    /// Rust plugins, in the order added — see [`Builder::plugin`].
    pub(super) plugins: Vec<Arc<dyn crate::plugin::Plugin>>,
    /// Multi-provider per-task-type model routing — see `Builder::task_router`.
    pub(super) task_router: Option<Arc<base::provider::TaskRouter>>,
    /// Every scene this host registered, so an agent type's `scene:` id can
    /// name one beyond the built-in four (plugin scenes, in particular), in
    /// this session and in every delegate under it.
    pub(super) scene_registry: Option<Arc<scene::SceneRegistry>>,
    /// Where the standing instructions come from — see
    /// [`Builder::instruction_provider`].
    pub(super) instruction_provider:
        Option<Arc<dyn base::interface::instruction_provider::InstructionProvider>>,
    /// Rings around tool dispatch — see [`Builder::tool_middleware`].
    pub(super) tool_middleware: Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>,
    /// User-message screening — see [`Builder::prompt_middleware`].
    pub(super) prompt_middleware:
        Vec<Arc<dyn base::interface::prompt_middleware::PromptMiddleware>>,
    /// Result policies — see [`Builder::tool_result_transformer`].
    pub(super) result_transformers:
        Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>,
    /// Kept time and ids — see [`Builder::environment`].
    pub(super) environment: Option<Arc<dyn base::interface::environment::Environment>>,
    /// Model-failure recovery — see [`Builder::recovery_policy`].
    pub(super) recovery_policy: Option<Arc<dyn base::interface::recovery_policy::RecoveryPolicy>>,
    /// Model request/response interception — see [`Builder::model_interceptor`].
    pub(super) model_interceptors:
        Vec<Arc<dyn base::interface::model_interceptor::ModelInterceptor>>,
    /// Recall — see [`Builder::memory_retriever`].
    pub(super) memory_retriever:
        Option<Arc<dyn base::interface::memory_contracts::MemoryRetriever>>,
    pub(super) retrieval_hooks: Vec<Arc<dyn base::interface::memory_contracts::RetrievalHook>>,
    /// Where this session reaches the machine — see [`Builder::exec`].
    pub(super) exec: Option<base::interface::exec::ExecProviders>,
    /// See [`Builder::token_counter`].
    pub(super) token_counter: Option<Arc<dyn base::interface::token_counter::TokenCounter>>,
    /// See [`Builder::rule_provider`].
    pub(super) rule_provider: Option<Arc<dyn base::interface::instruction_provider::RuleProvider>>,
    pub(super) modules: Vec<Arc<dyn crate::module::Module>>,
}

impl HostSetup {
    pub(super) fn new() -> Self {
        Self {
            permission: None,
            memory_store: None,
            compactor: None,
            history_store: None,
            telemetry_handle_override: None,
            plugins: Vec::new(),
            task_router: None,
            scene_registry: None,
            instruction_provider: None,
            tool_middleware: Vec::new(),
            prompt_middleware: Vec::new(),
            result_transformers: Vec::new(),
            environment: None,
            recovery_policy: None,
            model_interceptors: Vec::new(),
            memory_retriever: None,
            retrieval_hooks: Vec::new(),
            exec: None,
            token_counter: None,
            rule_provider: None,
            modules: Vec::new(),
        }
    }
}

/// What distinguishes this session from another on the same host.
pub(super) struct SpecSetup {
    pub(super) scene: Option<Arc<SceneSpec>>,
    pub(super) model: Option<Arc<dyn Model>>,
    pub(super) tools: Option<Arc<dyn base::tool::ToolRegistry>>,
    pub(super) settings: Option<Arc<LoadedSettings>>,
    pub(super) telemetry_url: Option<String>,
    pub(super) instruction_file: Option<PathBuf>,
    pub(super) session_id: Option<String>,
    /// The session that spawned this one, and which agent type it runs. Set by
    /// `AgentTool` at spawn time so the sub-agent knows its own lineage from
    /// its first call — waiting until something asks would be too late, since a
    /// recording's header is fixed by that first call.
    pub(super) parent_session_id: Option<String>,
    pub(super) agent_type: Option<String>,
    pub(super) skip_warmup: bool,
    pub(super) output_schema: Option<serde_json::Value>,
    pub(super) tool_selector: base::interface::tool_selector::ToolSelector,
    /// Pre-built FrozenContext — skips lazy collection on first turn.
    /// When set, the Agent uses this snapshot instead of calling
    /// `FrozenContext::collect()`. Essential for deterministic replay.
    pub(super) frozen: Option<base::frozen::FrozenContext>,
    /// See [`Builder::question_prompts`].
    pub(super) question_prompts: bool,
    /// See [`Builder::memory_extraction`].
    pub(super) memory_extraction: bool,
    /// How many delegation hops separate this agent from the root session —
    /// see [`Builder::agent_depth`].
    pub(super) agent_depth: u32,
    /// See [`Builder::team_seat`].
    pub(super) team_seat: Option<base::interface::delegate::TeamSeat>,
    /// Scheduled jobs, kept and fired by whoever owns this store — see
    /// [`Builder::cron_store`].
    pub(super) cron_store: Option<Arc<tools::cron::CronStore>>,
    /// See [`Builder::goal_rounds_from_owner`].
    pub(super) goal_rounds_from_owner: bool,
    /// Where this session's commands record their process groups — see
    /// [`Builder::process_groups`].
    pub(super) process_groups: Option<Arc<platform::GroupLedger>>,
    pub(super) session_prompt: Option<SessionPrompt>,
    /// See [`Builder::session_state`].
    pub(super) session_state: Option<(Arc<base::context::SessionState>, u64)>,
    /// See [`Builder::steering`].
    pub(super) steering: Option<crate::steering::SteerHandle>,
    /// See [`Builder::run_slots`].
    #[cfg_attr(not(feature = "ptc"), allow(dead_code))]
    pub(super) run_slots: Option<Arc<tokio::sync::Semaphore>>,
}

impl SpecSetup {
    pub(super) fn new() -> Self {
        Self {
            scene: None,
            model: None,
            tools: None,
            settings: None,
            telemetry_url: None,
            instruction_file: None,
            session_id: None,
            parent_session_id: None,
            agent_type: None,
            skip_warmup: false,
            output_schema: None,
            tool_selector: base::interface::tool_selector::ToolSelector::all(),
            frozen: None,
            question_prompts: false,
            memory_extraction: true,
            agent_depth: 0,
            team_seat: None,
            cron_store: None,
            goal_rounds_from_owner: false,
            process_groups: None,
            session_prompt: None,
            session_state: None,
            steering: None,
            run_slots: None,
        }
    }
}

/// Extension points: what a caller plugs in around the session.
pub(super) struct ExtSetup {
    pub(super) hooks: Option<Arc<HookRunner>>,
    pub(super) mcp_servers: Option<Box<dyn base::interface::mcp_servers::McpServers>>,
    pub(super) wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    /// Pre-built command registry shared across many sessions instead of
    /// re-scanning skill dirs per session — see `Builder::commands_override`.
    pub(super) commands_override: Option<Arc<crate::commands::CommandRegistry>>,
    /// Replaces the built-in way of asking a person something — see
    /// [`Builder::elicitation`].
    pub(super) elicitation: Option<Arc<dyn base::interface::elicitation::Elicitation>>,
    /// Contributions to the system prompt — see [`Builder::prompt_registry`].
    pub(super) prompt_registry: Option<Arc<dyn base::interface::prompt_registry::PromptRegistry>>,
    /// Replaces prompt assembly itself — see [`Builder::prompt_assembler`].
    pub(super) prompt_assembler:
        Option<Arc<dyn base::interface::prompt_assembler::PromptAssembler>>,
    /// Turn stop conditions — see [`Builder::turn_policy`].
    pub(super) turn_policy: Option<Arc<dyn base::interface::turn_policy::TurnPolicy>>,
    /// Spend and context ceilings — see [`Builder::budget_policy`].
    pub(super) budget_policy: Option<Arc<dyn base::interface::budget_policy::BudgetPolicy>>,
    /// Diagnostics — see [`Builder::health_check`].
    pub(super) health_checks: Vec<Arc<dyn base::interface::health::HealthCheck>>,
    /// Extra destinations for this session's events, beyond the `event_rx`
    /// `build()` returns — see [`Builder::event_sink`].
    pub(super) event_sinks: Vec<Arc<dyn base::interface::event_sink::EventSink>>,
}

impl ExtSetup {
    pub(super) fn new() -> Self {
        Self {
            hooks: None,
            mcp_servers: None,
            wake_rx: None,
            commands_override: None,
            elicitation: None,
            prompt_registry: None,
            prompt_assembler: None,
            turn_policy: None,
            budget_policy: None,
            health_checks: Vec::new(),
            event_sinks: Vec::new(),
        }
    }
}
