//! `Builder`: the single place a session is assembled from its parts; setters live here, the assembly in `build/`.

use super::*;

mod build;
mod fields;
mod hook_runner;
mod host;
use fields::{ExtSetup, HostSetup, SpecSetup};
pub(in crate::agent) use hook_runner::{build_hook_runner, hook_ambient};
pub use host::{HostContracts, HostLayers};

pub struct Builder {
    host: HostSetup,
    spec: SpecSetup,
    ext: ExtSetup,
}

/// A `RunCode` budget sized for this machine, for a host to share among the
/// engines it builds — see [`Builder::run_slots`]. Without `RunCode` in the
/// build no program runs, and the budget has no room.
pub fn default_run_slots() -> Arc<tokio::sync::Semaphore> {
    #[cfg(feature = "ptc")]
    return ptc::default_run_slots();
    #[cfg(not(feature = "ptc"))]
    Arc::new(tokio::sync::Semaphore::new(0))
}

/// `RunCode` listing the tools registered so far, on `slots` when given.
#[cfg(feature = "ptc")]
fn run_code_for(
    tools: &dyn base::tool::ToolRegistry,
    slots: Option<Arc<tokio::sync::Semaphore>>,
) -> Arc<ptc::RunCodeTool> {
    let run_code = ptc::RunCodeTool::for_registry(tools);
    Arc::new(match slots {
        Some(slots) => run_code.with_run_slots(slots),
        None => run_code,
    })
}

impl Builder {
    pub fn new() -> Self {
        Self {
            host: HostSetup::new(),
            spec: SpecSetup::new(),
            ext: ExtSetup::new(),
        }
    }

    /// Run the session on `state`, which outlives this engine, as its engine
    /// number `generation`: a host that rebuilds a session's engine hands each
    /// one the session's state and a number no engine before it on that
    /// state had. [`build`](Self::build) leaves the state as it is; the engine
    /// takes it over with [`Agent::attach_session`], once the engine before it
    /// has stopped, and takes only itself off again as it ends. Without this
    /// the engine gets a state of its own, attached as it is built.
    pub fn session_state(
        mut self,
        state: Arc<base::context::SessionState>,
        generation: u64,
    ) -> Self {
        self.spec.session_state = Some((state, generation));
        self
    }

    /// Take the messages sent while a turn runs from `queue`, which the host
    /// may keep across engines, instead of a queue of the engine's own.
    pub fn steering(mut self, queue: crate::steering::SteerHandle) -> Self {
        self.spec.steering = Some(queue);
        self
    }

    /// How many `RunCode` programs this session's conversation may run at
    /// once, counted together with every other engine given the same
    /// `slots`. Without this the session has a budget of its own.
    pub fn run_slots(mut self, slots: Arc<tokio::sync::Semaphore>) -> Self {
        self.spec.run_slots = Some(slots);
        self
    }

    /// A system-prompt append and/or override for this session's own agent,
    /// over whatever its settings say. Delegates it spawns are not given it:
    /// modules see the settings without it, so a sub-agent keeps the prompt
    /// its own type and scene give it.
    pub fn session_prompt(mut self, prompt: SessionPrompt) -> Self {
        self.spec.session_prompt = Some(prompt);
        self
    }

    /// Add `module` to this session. Modules run in the order they are added;
    /// see [`crate::module`].
    pub fn module(mut self, module: Arc<dyn crate::module::Module>) -> Self {
        self.host.modules.push(module);
        self
    }

    pub fn modules(
        mut self,
        modules: impl IntoIterator<Item = Arc<dyn crate::module::Module>>,
    ) -> Self {
        self.host.modules.extend(modules);
        self
    }

    /// Record in `ledger` every process group this session's commands start,
    /// its delegates' included, so that whoever owns the session can stop what
    /// they left running with [`GroupLedger::reap`] once it ends. Without one,
    /// a command that exits leaves its background processes to run on.
    ///
    /// [`GroupLedger::reap`]: platform::GroupLedger::reap
    pub fn process_groups(mut self, ledger: Arc<platform::GroupLedger>) -> Self {
        self.spec.process_groups = Some(ledger);
        self
    }

    /// Run this session's commands, file access, requests and sandboxing on
    /// `providers` instead of this machine. Delegates use the same set.
    ///
    /// The egress policy and the subprocess environment are then
    /// `providers`' to enforce; `sandbox.network_mode` and
    /// `sandbox.subprocess_env` configure only the default set. Not together
    /// with [`Builder::process_groups`]: record the groups in the providers
    /// passed here (`platform::local_providers_with_ledger`).
    pub fn exec(mut self, providers: base::interface::exec::ExecProviders) -> Self {
        self.host.exec = Some(providers);
        self
    }

    /// Judge this session's size — the number compaction triggers on — with
    /// `counter` instead of the engine's local estimate. Delegates judge
    /// theirs with it too.
    pub fn token_counter(
        mut self,
        counter: Arc<dyn base::interface::token_counter::TokenCounter>,
    ) -> Self {
        self.host.token_counter = Some(counter);
        self
    }

    /// Where the rule index the model is shown comes from. Replaces the
    /// three tiers (`base::rules::RuleTiers::for_paths`); compose with
    /// `RuleTiers::new`. Delegates are shown the same index.
    pub fn rule_provider(
        mut self,
        rules: Arc<dyn base::interface::instruction_provider::RuleProvider>,
    ) -> Self {
        self.host.rule_provider = Some(rules);
        self
    }

    /// Keep this session's scheduled jobs in `store`, and leave firing them to
    /// its owner. A session pool hands every session one store and runs a
    /// scheduler over it, so a job fires through the same path as any other
    /// turn — and fires whether or not its session is in memory at the time.
    /// Without one, the session keeps its own store and fires due jobs from
    /// its input loop.
    pub fn cron_store(mut self, store: Arc<tools::cron::CronStore>) -> Self {
        self.spec.cron_store = Some(store);
        self
    }

    /// Leave starting the goal's automatic rounds to this session's owner,
    /// which asks [`Agent::goal_round_after`] when a turn ends. A session
    /// pool does, so a round is a turn like any other; without it,
    /// [`Agent::run`] starts them itself.
    pub fn goal_rounds_from_owner(mut self) -> Self {
        self.spec.goal_rounds_from_owner = true;
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
        self.ext.event_sinks.push(sink);
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
        self.ext.elicitation = Some(e);
        self
    }

    /// Whether the host answers the model's questions over the channel
    /// protocol: `AgentEvent::QuestionPrompt` out, `InputMessage::QuestionResponse`
    /// back. Off by default, and with it off and no [`Builder::elicitation`]
    /// the session has no `AskUserQuestion` — a tool whose every call is
    /// declined is one the model should not be offered.
    pub fn question_prompts(mut self, on: bool) -> Self {
        self.spec.question_prompts = on;
        self
    }

    /// Whether a clarification this session asks can reach a person.
    fn questions_answered(&self) -> bool {
        self.spec.question_prompts
            || self
                .ext
                .elicitation
                .as_ref()
                .is_some_and(|e| e.can_clarify())
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
        self.ext.prompt_registry = Some(r);
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
        self.ext.prompt_assembler = Some(a);
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
        self.host.tool_middleware.push(m);
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
        self.host.prompt_middleware.push(m);
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
        self.host.result_transformers.push(t);
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
        self.ext.turn_policy = Some(p);
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
        self.ext.budget_policy = Some(p);
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
        self.host.environment = Some(e);
        self
    }

    /// Add a plugin — Rust, or the QuickJS layer `script-host` provides.
    /// Within a layer, plugins register in the order added, after this
    /// builder's own setters; every delegate this session spawns is built
    /// with them too.
    pub fn plugin(mut self, plugin: Arc<dyn crate::plugin::Plugin>) -> Self {
        self.host.plugins.push(plugin);
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
        self.ext.health_checks.push(check);
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
        self.host.recovery_policy = Some(p);
        self
    }

    pub fn model_interceptor(
        mut self,
        i: Arc<dyn base::interface::model_interceptor::ModelInterceptor>,
    ) -> Self {
        self.host.model_interceptors.push(i);
        self
    }

    pub fn memory_retriever(
        mut self,
        r: Arc<dyn base::interface::memory_contracts::MemoryRetriever>,
    ) -> Self {
        self.host.memory_retriever = Some(r);
        self
    }

    /// Whether a finished turn starts extracting durable memories from the
    /// conversation (on by default, and only while `memory_enabled`). Recall
    /// is unaffected. Off for an agent dropped as soon as its turn returns:
    /// its extraction would be paid for and then abandoned unfinished.
    pub fn memory_extraction(mut self, enabled: bool) -> Self {
        self.spec.memory_extraction = enabled;
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
        self.host.retrieval_hooks.push(h);
        self
    }

    /// Position of the agent being built in the delegation chain: `0` for a
    /// root session, `parent + 1` for anything spawned through
    /// `agents::delegate::Delegator::delegate`.
    ///
    /// `build()` stops registering the `Agent` tool once this reaches
    /// `EngineConfig::max_agent_depth`, so the deepest agents simply never
    /// see a way to delegate further. The count is what actually bounds the
    /// chain, not which tools a delegate is handed.
    pub fn agent_depth(mut self, depth: u32) -> Self {
        self.spec.agent_depth = depth;
        self
    }

    /// The team this agent is a member of, and its name there.
    pub fn team_seat(mut self, seat: base::interface::delegate::TeamSeat) -> Self {
        self.spec.team_seat = Some(seat);
        self
    }

    pub fn scene(mut self, s: Arc<SceneSpec>) -> Self {
        self.spec.scene = Some(s);
        self
    }
    pub fn model(mut self, m: Arc<dyn Model>) -> Self {
        self.spec.model = Some(m);
        self
    }
    pub fn tools(mut self, t: Arc<dyn base::tool::ToolRegistry>) -> Self {
        self.spec.tools = Some(t);
        self
    }
    pub fn settings(mut self, s: Arc<LoadedSettings>) -> Self {
        self.spec.settings = Some(s);
        self
    }
    pub fn permission(mut self, p: PermissionSet) -> Self {
        self.host.permission = Some(p);
        self
    }
    pub fn memory_store(mut self, m: Arc<MemoryStore>) -> Self {
        self.host.memory_store = Some(m);
        self
    }
    pub fn compactor(mut self, c: Arc<dyn Compactor>) -> Self {
        self.host.compactor = Some(c);
        self
    }
    pub fn hooks(mut self, h: Arc<HookRunner>) -> Self {
        self.ext.hooks = Some(h);
        self
    }
    /// Run the hooks of events whose answer is ignored in `background`, which
    /// the host drains on its way out ([`hooks::BackgroundHooks::drain`]).
    /// For the runner this session builds; one handed to [`Builder::hooks`]
    /// keeps its own.
    pub fn background_hooks(mut self, background: hooks::BackgroundHooks) -> Self {
        self.ext.background_hooks = Some(background);
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
        self.host.history_store = Some(store);
        self
    }
    /// When background work completes, something sends `()` on the
    /// associated sender; the hooks runner picks up the signal and
    /// re-executes any pending rewake hooks. Without one, a session that
    /// builds its own hooks runner wakes them when one of its running tasks
    /// ends.
    pub fn wake_receiver(mut self, rx: tokio::sync::mpsc::UnboundedReceiver<()>) -> Self {
        self.ext.wake_rx = Some(rx);
        self
    }
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.spec.session_id = Some(id.into());
        self
    }
    /// The lineage a sub-agent spawn already knows: who spawned it and as what.
    pub fn lineage(mut self, parent_session_id: Option<String>, agent_type: Option<&str>) -> Self {
        self.spec.parent_session_id = parent_session_id;
        self.spec.agent_type = agent_type.map(str::to_string);
        self
    }
    pub fn instruction(mut self, path: impl Into<PathBuf>) -> Self {
        self.spec.instruction_file = Some(path.into());
        self
    }
    pub fn telemetry_url(mut self, url: Option<String>) -> Self {
        self.spec.telemetry_url = url;
        self
    }
    /// Inject a pre-built telemetry handle (from e.g. CLI's `telemetry::spawn()`).
    /// Takes precedence over `telemetry_url`.
    pub fn telemetry_handle(mut self, h: TelemetryHandle) -> Self {
        self.host.telemetry_handle_override = Some(h);
        self
    }
    /// Pre-seed the FrozenContext snapshot (skips lazy collection on first turn).
    /// Essential for deterministic replay across runs.
    pub fn frozen(mut self, ctx: base::frozen::FrozenContext) -> Self {
        self.spec.frozen = Some(ctx);
        self
    }

    /// The JSON Schema this session's structured output must satisfy.
    ///
    /// `StructuredOutput` then shows the model this schema, refuses a call
    /// that does not match it, and is announced up front rather than left
    /// for `ToolSearch` to find. What it accepts arrives on `TurnComplete`.
    pub fn output_schema(mut self, schema: serde_json::Value) -> Self {
        self.spec.output_schema = Some(schema);
        self
    }

    /// Narrows every tool this session would have to what `selector` admits,
    /// on top of its scene: the ones it is handed, the engine's and modules',
    /// the scene's and plugins', and its MCP servers', those connected later
    /// included. `ToolSearch` and `StructuredOutput` are not narrowed. A
    /// delegate is held to its type's and its caller's tool lists this way.
    pub fn tool_selector(mut self, selector: base::interface::tool_selector::ToolSelector) -> Self {
        self.spec.tool_selector = selector;
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
        self.ext.mcp_servers = Some(servers);
        self
    }

    /// Disable startup warmup (for tests).
    pub fn skip_warmup(mut self, val: bool) -> Self {
        self.spec.skip_warmup = val;
        self
    }

    /// Inject a pre-built command registry (built once, shared across many
    /// sessions — see `daemon::SessionPool`) instead of scanning skill dirs
    /// fresh for every session. Falls back to a registry over this session's
    /// own skill catalog when not set.
    pub fn commands_override(mut self, c: Arc<crate::commands::CommandRegistry>) -> Self {
        self.ext.commands_override = Some(c);
        self
    }

    /// Multi-provider per-task-type model routing (see
    /// `base::provider::TaskRouter`). When set, forwarded to the
    /// `AgentTool` this session's engine builds so its sub-agent spawn
    /// points (`Delegator::delegate`, resume) route through
    /// `TaskRouter::model_for("subagent")` instead of always inheriting
    /// this session's own `model`. Unset (the default) preserves prior
    /// behavior exactly — sub-agents always use the parent's model.
    pub fn task_router(mut self, r: Arc<base::provider::TaskRouter>) -> Self {
        self.host.task_router = Some(r);
        self
    }

    /// Let an agent type's `scene:` id resolve against every scene this host
    /// registered, not just the built-in four. Without it a `plugin:<name>`
    /// scene is unresolvable and the sub-agent silently inherits its parent's
    /// scene instead.
    pub fn scene_registry(mut self, r: Arc<scene::SceneRegistry>) -> Self {
        self.host.scene_registry = Some(r);
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
        self.host.instruction_provider = Some(provider);
        self
    }
}
