//! `Builder`: the single place a session is assembled from its parts; setters live here, the assembly in `build.rs`.

use super::*;

mod build;
mod hook_runner;
pub(in crate::agent) use hook_runner::{build_hook_runner, hook_ambient};

pub struct Builder {
    scene: Option<Arc<dyn Scene>>,
    model: Option<Arc<dyn Model>>,
    tools: Option<Arc<dyn base::tool::ToolRegistry>>,
    settings: Option<Arc<Settings>>,
    permission: Option<Arc<dyn Permission>>,
    memory_store: Option<Arc<MemoryStore>>,
    compactor: Option<Arc<dyn Compactor>>,
    hooks: Option<Arc<HookRunner>>,
    /// Backing store for session transcript persistence (JSONL append/resume).
    /// `None` = in-memory only, matching prior behavior — this is purely
    /// additive, existing callers that don't set it are unaffected.
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    telemetry_url: Option<String>,
    telemetry_handle_override: Option<TelemetryHandle>,
    instruction_file: Option<PathBuf>,
    session_id: Option<String>,
    /// The session that spawned this one, and which agent type it runs. Set by
    /// `AgentTool` at spawn time so the sub-agent knows its own lineage from
    /// its first call — waiting until something asks would be too late, since a
    /// recording's header is fixed by that first call.
    parent_session_id: Option<String>,
    agent_type: Option<String>,
    skip_warmup: bool,
    output_schema: Option<serde_json::Value>,
    contributed_tool_filter: Option<ToolFilter>,
    mcp_servers: Option<Box<dyn base::interface::mcp_servers::McpServers>>,
    /// Pre-built FrozenContext — skips lazy collection on first turn.
    /// When set, the Agent uses this snapshot instead of calling
    /// `FrozenContext::collect()`. Essential for deterministic replay.
    frozen: Option<base::frozen::FrozenContext>,
    wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    /// Rust plugins, in the order added — see [`Builder::plugin`].
    plugins: Vec<Arc<dyn crate::plugin::Plugin>>,
    /// Pre-built command registry shared across many sessions instead of
    /// re-scanning skill dirs per session — see `Builder::commands_override`.
    commands_override: Option<Arc<crate::commands::CommandRegistry>>,
    /// Multi-provider per-task-type model routing — see `Builder::task_router`.
    task_router: Option<Arc<base::provider::TaskRouter>>,
    /// Every scene this host registered, so an agent type's `scene:` id can
    /// name one beyond the built-in four (plugin scenes, in particular) —
    /// see `agents::agent_tool::AgentTool::set_scene_registry`.
    scene_registry: Option<Arc<scene::SceneRegistry>>,
    /// Where the standing instructions come from — see
    /// [`Builder::instruction_provider`].
    instruction_provider:
        Option<Arc<dyn base::interface::instruction_provider::InstructionProvider>>,
    /// Replaces the built-in way of asking a person something — see
    /// [`Builder::elicitation`].
    elicitation: Option<Arc<dyn base::interface::elicitation::Elicitation>>,
    /// See [`Builder::question_prompts`].
    question_prompts: bool,
    /// Contributions to the system prompt — see [`Builder::prompt_registry`].
    prompt_registry: Option<Arc<dyn base::interface::prompt_registry::PromptRegistry>>,
    /// Replaces prompt assembly itself — see [`Builder::prompt_assembler`].
    prompt_assembler: Option<Arc<dyn base::interface::prompt_assembler::PromptAssembler>>,
    /// Rings around tool dispatch — see [`Builder::tool_middleware`].
    tool_middleware: Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>,
    /// User-message screening — see [`Builder::prompt_middleware`].
    prompt_middleware: Vec<Arc<dyn base::interface::prompt_middleware::PromptMiddleware>>,
    /// Result policies — see [`Builder::tool_result_transformer`].
    result_transformers: Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>,
    /// Turn stop conditions — see [`Builder::turn_policy`].
    turn_policy: Option<Arc<dyn base::interface::turn_policy::TurnPolicy>>,
    /// Spend and context ceilings — see [`Builder::budget_policy`].
    budget_policy: Option<Arc<dyn base::interface::budget_policy::BudgetPolicy>>,
    /// Diagnostics — see [`Builder::health_check`].
    health_checks: Vec<Arc<dyn base::interface::health::HealthCheck>>,
    /// Kept time and ids — see [`Builder::environment`].
    environment: Option<Arc<dyn base::interface::environment::Environment>>,
    /// Model-failure recovery — see [`Builder::recovery_policy`].
    recovery_policy: Option<Arc<dyn base::interface::recovery_policy::RecoveryPolicy>>,
    /// Model request/response interception — see [`Builder::model_interceptor`].
    model_interceptors: Vec<Arc<dyn base::interface::model_interceptor::ModelInterceptor>>,
    /// Recall — see [`Builder::memory_retriever`].
    memory_retriever: Option<Arc<dyn base::interface::memory_contracts::MemoryRetriever>>,
    retrieval_hooks: Vec<Arc<dyn base::interface::memory_contracts::RetrievalHook>>,
    /// See [`Builder::memory_extraction`].
    memory_extraction: bool,
    /// Extra destinations for this session's events, beyond the `event_rx`
    /// `build()` returns — see [`Builder::event_sink`].
    event_sinks: Vec<Arc<dyn base::interface::event_sink::EventSink>>,
    /// How many delegation hops separate this agent from the root session —
    /// see [`Builder::agent_depth`].
    agent_depth: u32,
    /// See [`Builder::team_seat`].
    team_seat: Option<base::interface::agent_spawner::TeamSeat>,
    /// Scheduled jobs, kept and fired by whoever owns this store — see
    /// [`Builder::cron_store`].
    cron_store: Option<Arc<tools::cron::CronStore>>,
    /// See [`Builder::goal_rounds_from_owner`].
    goal_rounds_from_owner: bool,
    /// Where this session's commands record their process groups — see
    /// [`Builder::process_groups`].
    process_groups: Option<Arc<platform::GroupLedger>>,
    modules: Vec<Arc<dyn crate::module::Module>>,
    session_prompt: Option<SessionPrompt>,
}

impl Builder {
    pub fn new() -> Self {
        Self {
            scene: None,
            model: None,
            tools: None,
            settings: None,
            permission: None,
            memory_store: None,
            compactor: None,
            hooks: None,
            history_store: None,
            telemetry_url: None,
            instruction_file: None,
            session_id: None,
            parent_session_id: None,
            agent_type: None,
            telemetry_handle_override: None,
            skip_warmup: false,
            output_schema: None,
            contributed_tool_filter: None,
            mcp_servers: None,
            frozen: None,
            wake_rx: None,
            plugins: Vec::new(),
            commands_override: None,
            task_router: None,
            scene_registry: None,
            instruction_provider: None,
            elicitation: None,
            question_prompts: false,
            prompt_registry: None,
            prompt_assembler: None,
            tool_middleware: Vec::new(),
            prompt_middleware: Vec::new(),
            result_transformers: Vec::new(),
            turn_policy: None,
            budget_policy: None,
            health_checks: Vec::new(),
            environment: None,
            recovery_policy: None,
            model_interceptors: Vec::new(),
            memory_retriever: None,
            retrieval_hooks: Vec::new(),
            memory_extraction: true,
            event_sinks: Vec::new(),
            agent_depth: 0,
            team_seat: None,
            cron_store: None,
            goal_rounds_from_owner: false,
            process_groups: None,
            modules: Vec::new(),
            session_prompt: None,
        }
    }

    /// A system-prompt append and/or override for this session's own agent,
    /// over whatever its settings say. Delegates it spawns are not given it:
    /// modules see the settings without it, so a sub-agent keeps the prompt
    /// its own type and scene give it.
    pub fn session_prompt(mut self, prompt: SessionPrompt) -> Self {
        self.session_prompt = Some(prompt);
        self
    }

    /// Add `module` to this session. Modules run in the order they are added;
    /// see [`crate::module`].
    pub fn module(mut self, module: Arc<dyn crate::module::Module>) -> Self {
        self.modules.push(module);
        self
    }

    pub fn modules(
        mut self,
        modules: impl IntoIterator<Item = Arc<dyn crate::module::Module>>,
    ) -> Self {
        self.modules.extend(modules);
        self
    }

    /// Record in `ledger` every process group this session's commands start,
    /// its delegates' included, so that whoever owns the session can stop what
    /// they left running with [`GroupLedger::reap`] once it ends. Without one,
    /// a command that exits leaves its background processes to run on.
    ///
    /// [`GroupLedger::reap`]: platform::GroupLedger::reap
    pub fn process_groups(mut self, ledger: Arc<platform::GroupLedger>) -> Self {
        self.process_groups = Some(ledger);
        self
    }

    /// Keep this session's scheduled jobs in `store`, and leave firing them to
    /// its owner. A session pool hands every session one store and runs a
    /// scheduler over it, so a job fires through the same path as any other
    /// turn — and fires whether or not its session is in memory at the time.
    /// Without one, the session keeps its own store and fires due jobs from
    /// its input loop.
    pub fn cron_store(mut self, store: Arc<tools::cron::CronStore>) -> Self {
        self.cron_store = Some(store);
        self
    }

    /// Leave starting the goal's automatic rounds to this session's owner,
    /// which asks [`Agent::goal_round_after`] when a turn ends. A session
    /// pool does, so a round is a turn like any other; without it,
    /// [`Agent::run`] starts them itself.
    pub fn goal_rounds_from_owner(mut self) -> Self {
        self.goal_rounds_from_owner = true;
        self
    }

    /// Send this session's events to `sink` as well as to the `event_rx`
    /// `build()` returns. Call it once per sink; sinks accumulate.
    ///
    /// The sink runs on its own task behind its own bounded queue, so it
    /// cannot slow a turn down however slow it is — see
    /// [`crate::event_bus`] for what that costs when a sink falls behind.
    /// Sinks can only be attached before `build()`: one joining mid-session
    /// would see a stream that starts in the middle and have no way to tell.
    pub fn event_sink(mut self, sink: Arc<dyn base::interface::event_sink::EventSink>) -> Self {
        self.event_sinks.push(sink);
        self
    }

    /// Answer this session's questions to a human here instead of over the
    /// event / input-channel protocol.
    ///
    /// A library embedder that can put a dialog on screen implements
    /// [`Elicitation`] and gets the question directly, rather than pumping
    /// `AgentEvent::PermissionPrompt` and `InputMessage::PermissionResponse`
    /// by hand. Whatever is registered here is also what the engine asks when
    /// it needs a clarification or an import confirmation, so an attended
    /// session built with one keeps `AskUserQuestion`.
    ///
    /// [`Elicitation`]: base::interface::elicitation::Elicitation
    pub fn elicitation(mut self, e: Arc<dyn base::interface::elicitation::Elicitation>) -> Self {
        self.elicitation = Some(e);
        self
    }

    /// Whether the host answers the model's questions over the channel
    /// protocol: `AgentEvent::QuestionPrompt` out, `InputMessage::QuestionResponse`
    /// back. Off by default, and with it off and no [`Builder::elicitation`]
    /// the session has no `AskUserQuestion` — a tool whose every call is
    /// declined is one the model should not be offered.
    pub fn question_prompts(mut self, on: bool) -> Self {
        self.question_prompts = on;
        self
    }

    /// Let something other than the engine contribute to the system prompt.
    ///
    /// Registered blocks are merged with the kernel's stages and sorted with
    /// them, so a contribution can sit before the skills inventory rather than
    /// only after everything. Registering nothing leaves the kernel's prompt
    /// unchanged.
    ///
    /// [`PromptRegistry`]: base::interface::prompt_registry::PromptRegistry
    pub fn prompt_registry(
        mut self,
        r: Arc<dyn base::interface::prompt_registry::PromptRegistry>,
    ) -> Self {
        self.prompt_registry = Some(r);
        self
    }

    /// Replace prompt assembly itself.
    ///
    /// [`prompt_registry`](Self::prompt_registry) opens what goes in and
    /// `AssemblyHook` opens what happens to the result; this opens the part
    /// in between — the order the stages are placed in, how a contribution's
    /// order merges with the kernel's, where the cache boundaries fall,
    /// whether two blocks stay two blocks.
    ///
    /// The default is
    /// [`DefaultAssembler`](base::interface::prompt_assembler::DefaultAssembler),
    /// which is the assembly the engine has always done.
    pub fn prompt_assembler(
        mut self,
        a: Arc<dyn base::interface::prompt_assembler::PromptAssembler>,
    ) -> Self {
        self.prompt_assembler = Some(a);
        self
    }

    /// Wrap every tool call in this session.
    ///
    /// Wrappers nest in the order they are added: the first added is
    /// outermost, so it sees an inner one's retries as one call. A wrapper can
    /// impose a stricter deadline, answer without dispatching, or dispatch
    /// more than once; it cannot rewrite the call's arguments, which is a
    /// different act with different trust rules and belongs to a different
    /// hook point.
    ///
    /// [`ToolMiddleware`]: base::interface::tool_middleware::ToolMiddleware
    pub fn tool_middleware(
        mut self,
        m: Arc<dyn base::interface::tool_middleware::ToolMiddleware>,
    ) -> Self {
        self.tool_middleware.push(m);
        self
    }

    /// Look at every user message before it enters the conversation — the one
    /// that opens a turn and the ones steered into a running turn — and let it
    /// through, rewrite it, or refuse it.
    ///
    /// Middlewares run after the `UserPromptSubmit` hooks configured in
    /// settings, in the order they are added; each sees the text the previous
    /// one left. A refused turn ends as a hook block ends it
    /// (`stopped_by_hook`, no model call); a refused steered message stays out
    /// of the transcript, the turn goes on, and `AgentEvent::UserMessageRefused`
    /// tells the sender.
    pub fn prompt_middleware(
        mut self,
        m: Arc<dyn base::interface::prompt_middleware::PromptMiddleware>,
    ) -> Self {
        self.prompt_middleware.push(m);
        self
    }

    /// Decide what a tool result may look like before the model reads it.
    ///
    /// Transformers run in the order they are added, and they run *last* —
    /// after every hook, immediately before the outcome is handed back. That
    /// is what makes a redacting transformer a guarantee rather than a
    /// suggestion: nothing after it can put back what it removed.
    ///
    /// [`ToolResultTransformer`]: base::interface::tool_result::ToolResultTransformer
    pub fn tool_result_transformer(
        mut self,
        t: Arc<dyn base::interface::tool_result::ToolResultTransformer>,
    ) -> Self {
        self.result_transformers.push(t);
        self
    }

    /// Decide which memories a turn recalls.
    ///
    /// The default asks the model, which is what the engine has always done
    /// and is genuinely the right tool for judging relevance — it costs a
    /// model call, which is why this seam exists. A deployment with an index,
    /// or a test that needs recall to be a function of the store rather than a
    /// judgement, supplies its own.
    ///
    /// [`MemoryRetriever`]: base::interface::memory_contracts::MemoryRetriever
    /// See every model request before it is sent, and every complete message
    /// before it is recorded.
    ///
    /// Both are called once per model call. There is deliberately no
    /// per-chunk equivalent: a turn produces thousands of chunks, the cost of
    /// a callback there is invisible to whoever writes it, and a hook that can
    /// rewrite chunks can produce a message that never existed as a coherent
    /// whole.
    ///
    /// [`ModelInterceptor`]: base::interface::model_interceptor::ModelInterceptor
    /// Decide when this session's turns have gone on long enough.
    ///
    /// The default holds the engine's two ceilings — model calls per turn, and
    /// structured-output retries — with the same values as before. A host that
    /// wants a tighter one should usually compose rather than replace:
    /// `FirstOf(vec![engine_default, mine])` keeps the engine's limits and adds
    /// its own.
    ///
    /// This decides only judgements about *progress*. Cancellation, a hook
    /// ending the turn, and "the model asked for tools so there is more to do"
    /// are not policy and cannot be overridden here — see the contract's
    /// module documentation for why each one is excluded.
    ///
    /// [`TurnPolicy`]: base::interface::turn_policy::TurnPolicy
    pub fn turn_policy(mut self, p: Arc<dyn base::interface::turn_policy::TurnPolicy>) -> Self {
        self.turn_policy = Some(p);
        self
    }

    /// Decide what this session's turns may spend, and how large a request
    /// may get.
    ///
    /// The default carries the engine's numbers: the cumulative token ceiling
    /// from settings, the output-volume rules, and the scene's compaction
    /// threshold with no ceiling above it. `Capped` adds the ceiling — a
    /// deployment that must never send a request past some size wraps its
    /// policy in it, and a turn that cannot be compacted under the cap ends
    /// instead of sending the request.
    ///
    /// [`BudgetPolicy`]: base::interface::budget_policy::BudgetPolicy
    pub fn budget_policy(
        mut self,
        p: Arc<dyn base::interface::budget_policy::BudgetPolicy>,
    ) -> Self {
        self.budget_policy = Some(p);
        self
    }

    /// Take kept time and kept identifiers from somewhere other than the
    /// machine.
    ///
    /// Only the answers that get *kept* come from here — a timestamp written
    /// into the log, an id naming an entry, the date the prompt tells the
    /// model. Measurement is not on the contract and does not change.
    ///
    /// A history store carries its own; configure both to replay a session.
    ///
    /// [`Environment`]: base::interface::environment::Environment
    pub fn environment(mut self, e: Arc<dyn base::interface::environment::Environment>) -> Self {
        self.environment = Some(e);
        self
    }

    /// Add a plugin — Rust, or the QuickJS layer `script-host` provides.
    /// Within a layer, plugins register in the order added, after this
    /// builder's own setters; every delegate this session spawns is built
    /// with them too.
    pub fn plugin(mut self, plugin: Arc<dyn crate::plugin::Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Register a diagnostic, reported by [`Agent::health`] beside whatever
    /// else is registered.
    ///
    /// A check reports and never repairs: there is nothing an implementation
    /// can return that reopens a circuit breaker, reloads configuration or
    /// restarts anything. It is also expected to answer promptly from state
    /// it already holds — a probe that blocks on the subsystem it is probing
    /// hangs exactly when the answer matters most.
    ///
    /// [`HealthCheck`]: base::interface::health::HealthCheck
    pub fn health_check(mut self, check: Arc<dyn base::interface::health::HealthCheck>) -> Self {
        self.health_checks.push(check);
        self
    }

    /// Decide what happens when a model call goes wrong.
    ///
    /// The default is what the engine has always done: an overload switches to
    /// the configured `fallback_model` or fails, a request refused for size is
    /// compacted and retried once, and a response cut off at the output limit
    /// escalates to 64K then 8K, three times, with a nudge.
    ///
    /// The policy chooses; the turn still performs. Compacting, rebuilding a
    /// request and sending it are things only the loop can sequence, so they
    /// stay there.
    ///
    /// [`RecoveryPolicy`]: base::interface::recovery_policy::RecoveryPolicy
    pub fn recovery_policy(
        mut self,
        p: Arc<dyn base::interface::recovery_policy::RecoveryPolicy>,
    ) -> Self {
        self.recovery_policy = Some(p);
        self
    }

    pub fn model_interceptor(
        mut self,
        i: Arc<dyn base::interface::model_interceptor::ModelInterceptor>,
    ) -> Self {
        self.model_interceptors.push(i);
        self
    }

    pub fn memory_retriever(
        mut self,
        r: Arc<dyn base::interface::memory_contracts::MemoryRetriever>,
    ) -> Self {
        self.memory_retriever = Some(r);
        self
    }

    /// Whether a finished turn starts extracting durable memories from the
    /// conversation (on by default, and only while `memory_enabled`). Recall
    /// is unaffected. Off for an agent dropped as soon as its turn returns:
    /// its extraction would be paid for and then abandoned unfinished.
    pub fn memory_extraction(mut self, enabled: bool) -> Self {
        self.memory_extraction = enabled;
        self
    }

    /// See the recall question before it is asked and the answer before it is
    /// used — expand the query, or drop results this session must not see.
    ///
    /// [`RetrievalHook`]: base::interface::memory_contracts::RetrievalHook
    pub fn retrieval_hook(
        mut self,
        h: Arc<dyn base::interface::memory_contracts::RetrievalHook>,
    ) -> Self {
        self.retrieval_hooks.push(h);
        self
    }

    /// Position of the agent being built in the delegation chain: `0` for a
    /// root session, `parent + 1` for anything spawned through
    /// `AgentTool::run_sub` and friends.
    ///
    /// `build()` stops registering the `Agent` tool once this reaches
    /// `EngineConfig::max_agent_depth`, so the deepest agents simply never
    /// see a way to delegate further. The count is what actually bounds the
    /// chain, not which tools a delegate is handed.
    pub fn agent_depth(mut self, depth: u32) -> Self {
        self.agent_depth = depth;
        self
    }

    /// The team this agent is a member of, and its name there.
    pub fn team_seat(mut self, seat: base::interface::agent_spawner::TeamSeat) -> Self {
        self.team_seat = Some(seat);
        self
    }

    pub fn scene(mut self, s: Arc<dyn Scene>) -> Self {
        self.scene = Some(s);
        self
    }
    pub fn model(mut self, m: Arc<dyn Model>) -> Self {
        self.model = Some(m);
        self
    }
    pub fn tools(mut self, t: Arc<dyn base::tool::ToolRegistry>) -> Self {
        self.tools = Some(t);
        self
    }
    pub fn settings(mut self, s: Arc<Settings>) -> Self {
        self.settings = Some(s);
        self
    }
    pub fn permission(mut self, p: Arc<dyn Permission>) -> Self {
        self.permission = Some(p);
        self
    }
    pub fn memory_store(mut self, m: Arc<MemoryStore>) -> Self {
        self.memory_store = Some(m);
        self
    }
    pub fn compactor(mut self, c: Arc<dyn Compactor>) -> Self {
        self.compactor = Some(c);
        self
    }
    pub fn hooks(mut self, h: Arc<HookRunner>) -> Self {
        self.hooks = Some(h);
        self
    }
    /// Inject a backing store for session transcript persistence. When set,
    /// the session's messages are incrementally appended to disk once per
    /// turn (see `session::session::SessionManager::persist`); `None` (the
    /// default) keeps sessions purely in-memory.
    ///
    /// Requires the session id to be a valid `base::session::SessionId` —
    /// either omit [`Builder::session_id`] and take the generated default, or
    /// pass one built from `SessionId`. [`Builder::build`] rejects anything
    /// else rather than persisting nothing.
    pub fn history_store(mut self, store: Arc<dyn history::store::HistoryStore>) -> Self {
        self.history_store = Some(store);
        self
    }
    /// When background work completes, something sends `()` on the
    /// associated sender; the hooks runner picks up the signal and
    /// re-executes any pending rewake hooks. Without one, a session that
    /// builds its own hooks runner wakes them when one of its running tasks
    /// ends.
    pub fn wake_receiver(mut self, rx: tokio::sync::mpsc::UnboundedReceiver<()>) -> Self {
        self.wake_rx = Some(rx);
        self
    }
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }
    /// The lineage a sub-agent spawn already knows: who spawned it and as what.
    pub fn lineage(mut self, parent_session_id: Option<String>, agent_type: Option<&str>) -> Self {
        self.parent_session_id = parent_session_id;
        self.agent_type = agent_type.map(str::to_string);
        self
    }
    pub fn instruction(mut self, path: impl Into<PathBuf>) -> Self {
        self.instruction_file = Some(path.into());
        self
    }
    pub fn telemetry_url(mut self, url: Option<String>) -> Self {
        self.telemetry_url = url;
        self
    }
    /// Inject a pre-built telemetry handle (from e.g. CLI's `telemetry::spawn()`).
    /// Takes precedence over `telemetry_url`.
    pub fn telemetry_handle(mut self, h: TelemetryHandle) -> Self {
        self.telemetry_handle_override = Some(h);
        self
    }
    /// Pre-seed the FrozenContext snapshot (skips lazy collection on first turn).
    /// Essential for deterministic replay across runs.
    pub fn frozen(mut self, ctx: base::frozen::FrozenContext) -> Self {
        self.frozen = Some(ctx);
        self
    }

    /// The JSON Schema this session's structured output must satisfy.
    ///
    /// `StructuredOutput` then shows the model this schema, refuses a call
    /// that does not match it, and is announced up front rather than left
    /// for `ToolSearch` to find. What it accepts arrives on `TurnComplete`.
    pub fn output_schema(mut self, schema: serde_json::Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Which of the tools contributed at build — the scene's own
    /// (`Scene::extra_tools`) and the plugins' — this session gets; the rest
    /// are not registered. A sub-agent narrows those with its type's tool
    /// lists this way, as the tools it inherits from its parent are narrowed
    /// before they reach `tools`.
    pub fn contributed_tool_filter(mut self, admits: ToolFilter) -> Self {
        self.contributed_tool_filter = Some(admits);
        self
    }

    /// MCP servers the session owns from the start: a delegate's grant of
    /// its parent's connections. Their tools are registered and offered like
    /// those an mcp module provides; a module that provides servers replaces
    /// these.
    pub fn mcp_servers(
        mut self,
        servers: Box<dyn base::interface::mcp_servers::McpServers>,
    ) -> Self {
        self.mcp_servers = Some(servers);
        self
    }

    /// Disable startup warmup (for tests).
    pub fn skip_warmup(mut self, val: bool) -> Self {
        self.skip_warmup = val;
        self
    }

    /// Inject a pre-built command registry (built once, shared across many
    /// sessions — see `daemon::SessionPool`) instead of scanning skill dirs
    /// fresh for every session. Falls back to a registry over this session's
    /// own skill catalog when not set.
    pub fn commands_override(mut self, c: Arc<crate::commands::CommandRegistry>) -> Self {
        self.commands_override = Some(c);
        self
    }

    /// Multi-provider per-task-type model routing (see
    /// `base::provider::TaskRouter`). When set, forwarded to the
    /// `AgentTool` this session's engine builds so its sub-agent spawn
    /// points (`run_sub`/`run_sub_inner`/resume) route through
    /// `TaskRouter::model_for("subagent")` instead of always inheriting
    /// this session's own `model`. Unset (the default) preserves prior
    /// behavior exactly — sub-agents always use the parent's model.
    pub fn task_router(mut self, r: Arc<base::provider::TaskRouter>) -> Self {
        self.task_router = Some(r);
        self
    }

    /// Let an agent type's `scene:` id resolve against every scene this host
    /// registered, not just the built-in four. Without it a `plugin:<name>`
    /// scene is unresolvable and the sub-agent silently inherits its parent's
    /// scene instead.
    pub fn scene_registry(mut self, r: Arc<scene::SceneRegistry>) -> Self {
        self.scene_registry = Some(r);
        self
    }

    /// Replace where the standing instructions (`AGENTS.md` / `CLAUDE.md`)
    /// come from.
    ///
    /// The default reads the one path `instruction_file` names, which is what
    /// a deployment whose conventions live in a file wants. One whose
    /// conventions live in a service, or that has already loaded them, hands
    /// them over instead of staging a file for the engine to read back.
    pub fn instruction_provider(
        mut self,
        provider: Arc<dyn base::interface::instruction_provider::InstructionProvider>,
    ) -> Self {
        self.instruction_provider = Some(provider);
        self
    }
}
