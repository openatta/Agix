//! Engine struct and builder — the central AGENT orchestrator.

use base::interface::event::AgentEvent;
use base::interface::memory::MemoryStore;
use base::interface::model::Model;
use base::interface::permission::Permission;
use base::interface::scene::Scene;
use base::interface::settings::Settings;
use base::tool::InMemoryToolRegistry;
use compaction::cached::CachedMicroCompact;
use compaction::compact::{Compactor, DefaultCompactor};
use hooks::HookRunner;
use session::session::SessionManager;
use session::session_memory::SessionMemory;
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::perf::PerfCollector;
use telemetry::{TelemetryHandle, TelemetryRecorder};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing;

mod builder;
mod commands;
mod messages;
mod run;
pub use builder::Builder;
pub use messages::*;

/// The `task_models` key that routes `WebSearch` to a provider of its own.
const WEB_SEARCH_TASK: &str = "web_search";

/// The `task_models` key that turns on `WebFetch` page extraction.
const WEB_FETCH_TASK: &str = "web_fetch";

/// The one tool in `NEEDS_A_USER` an attended session also needs a host for:
/// see [`Builder::question_prompts`].
const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// What a session's end sets off. Its tools close over the session — the
/// `Agent` tool over the registry it sits in, the team tools over the spawner
/// that reaches it — so the registry is emptied here, or none of it would ever
/// be freed; and whatever outlives a turn on the session's behalf (team
/// members) watches `ended`.
struct SessionEnd {
    ended: CancellationToken,
    tools: Arc<InMemoryToolRegistry>,
}

impl Drop for SessionEnd {
    fn drop(&mut self) {
        self.ended.cancel();
        self.tools.clear();
    }
}

pub struct Agent {
    pub(crate) scene: Arc<dyn Scene>,
    pub(crate) model: Arc<dyn Model>,
    pub(crate) tools: Arc<dyn base::tool::ToolRegistry>,
    pub(crate) settings: Arc<Settings>,
    /// Derived from `settings`; every tool call's `ToolExecCtx.config` is a
    /// clone of this, so tools see the configured sandbox/permission/shell
    /// settings rather than defaults.
    pub(crate) config: Arc<base::context::EngineConfig>,
    /// This session's own position in the delegation chain — see
    /// [`Builder::agent_depth`]. Reaches tools as `ToolContext::agent_depth`.
    pub(crate) agent_depth: u32,
    pub(crate) permission: Arc<dyn Permission>,
    /// Permission requests currently awaiting a host response, keyed by
    /// `prompt_id`. `execute_tool_inner` registers a `oneshot` sender here
    /// when `Permission::check` returns `PermissionOutcome::Prompt`, then
    /// awaits the paired receiver (with a timeout); the
    /// `InputMessage::PermissionResponse` branch below looks the sender up
    /// by `prompt_id` and fires it. A `prompt_id` with no entry (already
    /// timed out, or a stale/duplicate response) is silently ignored.
    pub(crate) pending_permissions: PendingPermissions,
    /// Questions put to the host and not yet answered, woken by
    /// `InputMessage::QuestionResponse` the same way.
    pub(crate) pending_questions: crate::elicitation::PendingQuestions,
    /// How this session asks a person something — see
    /// [`base::interface::elicitation::Elicitation`]. Defaults to
    /// [`crate::elicitation::ChannelElicitation`], which speaks the event /
    /// input-channel protocol hosts already use.
    pub(crate) elicitation: Arc<dyn base::interface::elicitation::Elicitation>,
    /// What else contributes to this session's system prompt — see
    /// [`Builder::prompt_registry`].
    pub(crate) prompt_registry: Arc<dyn base::interface::prompt_registry::PromptRegistry>,
    /// How those contributions become the blocks a request carries — see
    /// [`Builder::prompt_assembler`].
    pub(crate) prompt_assembler: Arc<dyn base::interface::prompt_assembler::PromptAssembler>,
    /// Rings around every tool call — see [`Builder::tool_middleware`].
    pub(crate) tool_middleware: Arc<Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>>,
    /// What every user message passes on its way in — see
    /// [`Builder::prompt_middleware`].
    pub(crate) prompt_middleware:
        Arc<Vec<Arc<dyn base::interface::prompt_middleware::PromptMiddleware>>>,
    /// The last hands on a tool's output — see
    /// [`Builder::tool_result_transformer`].
    pub(crate) result_transformers:
        Arc<Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>>,
    /// When this session's turns have gone on long enough — see
    /// [`Builder::turn_policy`].
    pub(crate) turn_policy: Arc<dyn base::interface::turn_policy::TurnPolicy>,
    /// What this session's turns may spend, and how much context they may
    /// carry — see [`Builder::budget_policy`].
    pub(crate) budget_policy: Arc<dyn base::interface::budget_policy::BudgetPolicy>,
    /// Whether this session's subsystems are working — see
    /// [`Builder::health_check`] and [`Agent::health`].
    pub(crate) health: Arc<base::interface::health::HealthChecks>,
    /// Kept time and kept identifiers — see [`Builder::environment`].
    pub(crate) environment: Arc<dyn base::interface::environment::Environment>,
    /// What plugins asked to be told as each turn begins.
    pub(crate) turn_starts: Arc<Vec<crate::plugin::TurnStart>>,
    /// What to do when a model call goes wrong — see
    /// [`Builder::recovery_policy`].
    pub(crate) recovery_policy: Arc<dyn base::interface::recovery_policy::RecoveryPolicy>,
    /// The request on its way out and the message on its way back — see
    /// [`Builder::model_interceptor`].
    pub(crate) model_interceptors:
        Arc<Vec<Arc<dyn base::interface::model_interceptor::ModelInterceptor>>>,
    /// Which memories a turn sees — see [`Builder::memory_retriever`].
    pub(crate) memory_retriever: Arc<dyn base::interface::memory_contracts::MemoryRetriever>,
    /// A look at the recall question before, and the answer after.
    pub(crate) retrieval_hooks: Arc<Vec<Arc<dyn base::interface::memory_contracts::RetrievalHook>>>,
    /// Cancellation token of the turn currently in flight — replaced by
    /// `run()` before each turn, cancelled by the input demultiplexer on
    /// `EngineCommand::CancelTurn`. `Arc`-shared for the same reason
    /// `pending_permissions` is: the demultiplexer task holds no `&mut self`.
    ///
    /// Holds a *finished* turn's token between turns, which is why
    /// `CancelTurn` arriving while idle does nothing rather than poisoning
    /// the next turn.
    pub(crate) current_turn_cancel: Arc<std::sync::Mutex<CancellationToken>>,
    /// User messages sent while a turn runs — see [`crate::steering`].
    /// Shared with the input demultiplexer for the same reason
    /// `current_turn_cancel` is.
    pub(crate) steering: crate::steering::SharedSteerQueue,
    pub(crate) memory_store: Arc<MemoryStore>,
    pub(crate) session: SessionManager,
    /// The session's mutable tool-facing state — one instance for the whole
    /// session, handed to every `ToolContext`.
    ///
    /// Distinct from `session` above: `SessionManager` owns the *transcript*
    /// (messages, token accounting, persistence), this owns everything tools
    /// read and write across calls (permission mode, read cache, file
    /// snapshots, todos, tasks). See `Builder::build()` for why sharing one
    /// instance matters.
    pub(crate) session_state: Arc<base::context::SessionState>,
    /// Whether [`Agent::run`] starts the goal's automatic rounds — see
    /// [`Builder::goal_rounds_from_owner`].
    drive_goals: bool,
    agent_spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner>,
    _session_end: SessionEnd,
    pub(crate) perf: Arc<PerfCollector>,

    pub(crate) compactor: Arc<dyn Compactor>,
    pub(crate) hooks: Arc<HookRunner>,
    pub(crate) skills: Arc<dyn base::interface::skill_catalog::SkillCatalog>,
    pub(crate) commands: std::sync::Arc<crate::commands::CommandRegistry>,
    pub(crate) mcp: Box<dyn base::interface::mcp_servers::McpServers>,

    pub(crate) telemetry_handle: TelemetryHandle,
    pub(crate) current_turn_id: String,
    /// Compaction circuit breaker state — tracks consecutive failures and
    /// prevents infinite compaction loops.
    pub(crate) compaction_state: compaction::reactive::CompactionState,
    /// Session-start frozen environment snapshot. Computed lazily on first turn.
    pub(crate) frozen: Option<base::frozen::FrozenContext>,
    /// Pre-read AGENTS.md/CLAUDE.md content for userContext injection.
    pub(crate) claude_md_content: Option<String>,
    /// Whether the opening context message (session facts, CLAUDE.md, date)
    /// is in the conversation. Cleared by `/clear` and by compaction.
    pub(crate) session_context_injected: bool,
    /// Skills invoked so far, re-injected after compaction.
    pub(crate) invoked_skills: Vec<String>,
    /// The skill names this session was last shown — `None` until its first
    /// turn, whose prompt lists them all.
    pub(crate) announced_skills: Option<std::collections::HashSet<String>>,
    /// What the `/name` command that started this turn grants, applied once
    /// the turn has cleared the previous turn's grants.
    pub(crate) slash_skill_grants: Vec<(String, Option<String>)>,
    /// This turn's user message: a fingerprint of its text and what the text is
    /// made of, for `StreamParams::input_map`.
    ///
    /// The message is identified by content rather than by the index it had
    /// when it was pushed, because compaction rewrites the message list before
    /// the request is assembled. A message compaction dropped simply fails to
    /// resolve, which is the honest outcome — better than an index that has
    /// come to name a different message.
    pub(crate) pending_input: Option<(u64, Vec<base::interface::model::InputSpan>)>,
    /// When this turn's user message was pushed. Compaction moves message
    /// indices; the time a message was pushed survives it.
    pub(crate) turn_started_at: Option<std::time::Instant>,
    /// Which agent type this session runs, for a sub-agent spawned as one.
    /// `None` for a top-level session. Travels onto every `CallOrigin` so a
    /// recording says what kind of agent produced it.
    pub(crate) agent_type: Option<String>,
    /// Tokens the last assembled request spent *before* any conversation
    /// content: the system prompt blocks plus every tool definition.
    ///
    /// Recorded by `build_prompt_for_turn` and added to the message total by
    /// `context_tokens()`, which is what the compaction threshold is compared
    /// against. Without it the budget check saw only message content and
    /// systematically under-counted the real context — by a large, constant
    /// amount here, since this workspace ships 30+ tools each carrying a full
    /// JSON schema. Zero until the first prompt is assembled.
    pub(crate) request_overhead_tokens: usize,
    /// Whether the previous turn had tool uses (findWritePivot guard for skill
    /// discovery prefetch).
    pub(crate) last_had_tool_uses: bool,
    /// The current turn's most recent model call, `None` before it makes one.
    pub(crate) last_call_usage: Option<base::interface::model::Usage>,
    /// A model call of the current turn ended without the provider marking
    /// the end of its reply.
    pub(crate) turn_truncated: bool,
    /// What this session's turns have spent since it was loaded.
    pub(crate) spent: SessionSpend,
    /// Permission denials this session, for telemetry.
    ///
    /// Atomic and `Arc`-shared rather than a plain `u32` because the input
    /// demultiplexer (see `Agent::run`) resolves permission responses off the
    /// turn's thread and has no `&mut self`.
    pub(crate) permission_denial_count: Arc<std::sync::atomic::AtomicU32>,
    /// Whether a compact warning has been issued this cycle. Reset after compaction
    /// so the warning can fire again if the token budget is exhausted again later.
    pub(crate) compact_warning_issued: bool,
    /// Whether any tool result has actually been cleared (time-based
    /// micro-compact, cached micro-compact, or the per-message tool-result
    /// budget) at any point in this session. Gates the `function_result_clearing`
    /// prompt section, so the model is not told results were cleared when none
    /// were and does not re-run things it still has.
    pub(crate) tool_results_ever_cleared: bool,
    /// Tool names for which `build_tool_defs` has already logged a
    /// built-in/MCP name-collision warning this session — dedups the log so
    /// a persistent collision doesn't warn on every single API call.
    pub(crate) mcp_shadow_warned: std::collections::HashSet<String>,
    /// When old tool results are cleared by wall-clock age. Default: 15 minutes.
    pub(crate) time_based_mc_config: compaction::time_based_mc::TimeBasedMcConfig,
    /// Cached micro-compact state: time-driven cache edit generation.
    /// When enabled, clears old tool results and records their tool_use_ids
    /// as `cache_edits` to send to the Anthropic API, avoiding cache invalidation.
    pub(crate) cached_mc: CachedMicroCompact,
    /// The team this agent works in, if it is a member of one: its turns end
    /// with `TaskCompleted` and `TeammateIdle`.
    pub(crate) team_seat: Option<base::interface::agent_spawner::TeamSeat>,
    /// Orphaned permission from a previous session (for resume recovery).
    pub(crate) orphaned_permission: Option<crate::agent::PermissionDecision>,
    /// Whether we've already handled the orphaned permission this session.
    pub(crate) has_handled_orphaned_permission: bool,
    /// Output token budget target (e.g., 500k, 2M). `None` = no budget active.
    pub(crate) output_token_target: Option<u64>,
    /// A target was set by the message being handled and the model has not
    /// been told what it means yet.
    pub(crate) output_token_target_unannounced: bool,
    pub(crate) repeat_guard: crate::repeat_guard::RepeatGuard,
    /// Accumulated output tokens for the current budget session.
    /// Reset each time a new budget target is set.
    pub(crate) accumulated_output_tokens: u64,
    /// How many continuation turns have been injected for the current budget.
    /// Guarded by diminishing-returns, not a hard cap.
    pub(crate) token_budget_continuation_count: u32,
    /// Output-token delta from the previous continuation — for diminishing-returns
    /// detection.
    pub(crate) last_delta_tokens: u64,
    pub(crate) input_rx: InputReceiver,
    pub(crate) event_tx: EventSender,
    /// Skip startup warmup (for tests).
    pub(crate) skip_warmup: bool,
    /// Multi-provider per-task-type model routing — see `Builder::task_router`.
    /// Retained here (not just forwarded to `AgentTool`) so turn-processing
    /// code (`turn.rs`) can route its own background LLM calls — e.g. post-turn
    /// memory extraction's `task_models.memory` — through the same mechanism
    /// already proven for sub-agent spawns (`"subagent"` task type).
    pub(crate) task_router: Option<Arc<base::provider::TaskRouter>>,
}

/// A session's model calls and tool calls, summed over its turns.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SessionSpend {
    pub usage: base::interface::model::Usage,
    pub api_calls: u32,
    pub tool_calls: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine shutdown requested")]
    Shutdown,
    #[error("{0}")]
    Internal(String),
}

/// Whether a tool, by name, is let through.
pub type ToolFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

fn admits(filter: &Option<ToolFilter>, name: &str) -> bool {
    filter.as_ref().map_or(true, |admits| admits(name))
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
