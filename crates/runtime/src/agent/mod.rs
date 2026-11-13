//! Engine struct and builder — the central AGENT orchestrator.

use base::event::AgentEvent;
use base::interface::model::Model;
use base::interface::permission::PermissionSet;
use base::interface::scene::SceneSpec;
use base::memory::MemoryStore;
use base::settings::LoadedSettings;
use base::tool::InMemoryToolRegistry;
use compaction::cached::CachedMicroCompact;
use compaction::compact::{Compactor, DefaultCompactor};
use hooks::{HookEvent, HookRunner};
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
pub use builder::{Builder, HostContracts, HostLayers};
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
    pub(crate) scene: Arc<SceneSpec>,
    pub(crate) settings: Arc<LoadedSettings>,
    /// Derived from `settings`; every tool call's `ToolExecCtx.config` is a
    /// clone of this, so tools see the configured sandbox/permission/shell
    /// settings rather than defaults.
    pub(crate) config: Arc<base::context::EngineConfig>,
    pub(crate) session: SessionManager,
    /// Distinct from `session`: `SessionManager` owns the *transcript*, this
    /// owns everything tools read and write across calls. One instance for the
    /// whole session, handed to every `ToolContext` — see `Builder::build()`.
    pub(crate) session_state: Arc<base::context::SessionState>,
    /// How this session's tool calls reach the machine: built once from
    /// `config` and the builder's process-group ledger, and bound to
    /// `session_state` for the permission that judges those calls.
    pub(crate) exec: base::interface::exec::ExecProviders,
    /// Outlives the turn it names: what is announced between turns (a
    /// `/compact`, a turn error) carries the last turn's id.
    pub(crate) current_turn_id: String,
    session_end: SessionEnd,
    pub(crate) identity: Identity,
    pub(crate) llm: ModelRoute,
    pub(crate) tooling: Tooling,
    pub(crate) gate: Gate,
    pub(crate) prompt: PromptContext,
    pub(crate) policies: Policies,
    pub(crate) memory: Memory,
    pub(crate) compaction: Compaction,
    pub(crate) token_budget: TokenBudget,
    pub(crate) turn: crate::turn::TurnState,
    /// Read by the *next* turn, to decide whether to rescan for skills; starts
    /// `true` so the first turn scans.
    pub(crate) last_had_tool_uses: bool,
    /// Set before the turn begins and taken once its prompt has been screened;
    /// a turn a hook blocks leaves them for the next one.
    pub(crate) slash_skill_grants: Vec<(String, Option<String>)>,
    /// Keyed by `session.turn_count`, which only a completed turn advances, so
    /// a cancelled turn's counts carry into the next.
    pub(crate) repeat_guard: crate::repeat_guard::RepeatGuard,
    pub(crate) telemetry: Telemetry,
    pub(crate) channels: Channels,
}

/// Who this session is among others: its place in delegation and teams.
pub(crate) struct Identity {
    pub(crate) agent_depth: u32,
    pub(crate) agent_type: Option<String>,
    pub(crate) team_seat: Option<base::interface::delegate::TeamSeat>,
    drive_goals: bool,
    pub(crate) skip_warmup: bool,
}

/// The model this session calls and what stands around each call.
pub(crate) struct ModelRoute {
    pub(crate) model: Arc<dyn Model>,
    /// Kept here, not only forwarded to `AgentTool`, so the turn's own
    /// background calls (memory extraction, web tasks) route the same way.
    pub(crate) task_router: Option<Arc<base::provider::TaskRouter>>,
    pub(crate) recovery_policy: Arc<dyn base::interface::recovery_policy::RecoveryPolicy>,
    pub(crate) model_interceptors:
        Arc<Vec<Arc<dyn base::interface::model_interceptor::ModelInterceptor>>>,
}

/// What the model can call, and what wraps a call.
pub(crate) struct Tooling {
    pub(crate) tools: Arc<dyn base::tool::ToolRegistry>,
    pub(crate) skills: Arc<dyn base::interface::skill_catalog::SkillCatalog>,
    pub(crate) commands: std::sync::Arc<crate::commands::CommandRegistry>,
    pub(crate) mcp: Box<dyn base::interface::mcp_servers::McpServers>,
    delegator: Arc<dyn base::interface::delegate::Delegator>,
    pub(crate) tool_middleware: Arc<Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>>,
    pub(crate) result_transformers:
        Arc<Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>>,
    /// Built-in/MCP name collisions already logged, so a persistent one does
    /// not warn on every request.
    pub(crate) mcp_shadow_warned: std::collections::HashSet<String>,
    pub(crate) tool_policy: Arc<crate::tool_policy::ToolPolicy>,
}

/// Permission, the questions put to the host, and hooks.
pub(crate) struct Gate {
    pub(crate) permission: PermissionSet,
    /// A `prompt_id` with no entry (timed out, stale or duplicate response)
    /// is ignored by whoever resolves it.
    pub(crate) pending_permissions: PendingPermissions,
    pub(crate) pending_questions: crate::elicitation::PendingQuestions,
    pub(crate) elicitation: Arc<dyn base::interface::elicitation::Elicitation>,
    /// Atomic and shared because the input demultiplexer resolves permission
    /// responses off the turn's thread, with no `&mut self`.
    pub(crate) permission_denial_count: Arc<std::sync::atomic::AtomicU32>,
    pub(crate) orphaned_permission: Option<crate::agent::PermissionDecision>,
    pub(crate) has_handled_orphaned_permission: bool,
    pub(crate) hooks: Arc<HookRunner>,
    pub(crate) turn_starts: Arc<Vec<crate::plugin::TurnStart>>,
}

/// What a request's system prompt and opening context are built from.
pub(crate) struct PromptContext {
    pub(crate) prompt_registry: Arc<dyn base::interface::prompt_registry::PromptRegistry>,
    pub(crate) prompt_assembler: Arc<dyn base::interface::prompt_assembler::PromptAssembler>,
    pub(crate) prompt_middleware:
        Arc<Vec<Arc<dyn base::interface::prompt_middleware::PromptMiddleware>>>,
    /// Computed lazily on the first turn unless the builder seeded it.
    pub(crate) frozen: Option<base::frozen::FrozenContext>,
    pub(crate) claude_md_content: Option<String>,
    /// Read on every assembly, as the rule directories used to be scanned.
    pub(crate) rules: Arc<dyn base::interface::instruction_provider::RuleProvider>,
    /// Cleared by `/clear` and by compaction.
    pub(crate) session_context_injected: bool,
    /// Re-injected after compaction.
    pub(crate) invoked_skills: Vec<String>,
    /// `None` until the first turn, whose prompt lists them all.
    pub(crate) announced_skills: Option<std::collections::HashSet<String>>,
}

/// Pluggable answers to "how long", "how much" and "is it healthy".
pub(crate) struct Policies {
    pub(crate) turn_policy: Arc<dyn base::interface::turn_policy::TurnPolicy>,
    pub(crate) budget_policy: Arc<dyn base::interface::budget_policy::BudgetPolicy>,
    pub(crate) health: Arc<base::interface::health::HealthChecks>,
    pub(crate) environment: Arc<dyn base::interface::environment::Environment>,
}

/// Durable memory and how a turn recalls from it.
pub(crate) struct Memory {
    pub(crate) memory_store: Arc<MemoryStore>,
    pub(crate) memory_retriever: Arc<dyn base::interface::memory_contracts::MemoryRetriever>,
    pub(crate) retrieval_hooks: Arc<Vec<Arc<dyn base::interface::memory_contracts::RetrievalHook>>>,
    pub(crate) extract_after_turn: bool,
    /// Extractions started at the end of a turn, finished or abandoned when
    /// the session stops.
    pub(crate) extractions: tokio::task::JoinSet<()>,
}

/// Compaction and micro-compaction, and what they remember between turns.
pub(crate) struct Compaction {
    pub(crate) compactor: Arc<dyn Compactor>,
    pub(crate) compaction_state: compaction::reactive::CompactionState,
    /// Reset after compaction so the warning can fire again.
    pub(crate) compact_warning_issued: bool,
    /// Gates the `function_result_clearing` prompt section, so the model is
    /// not told results were cleared when none were.
    pub(crate) tool_results_ever_cleared: bool,
    pub(crate) time_based_mc_config: compaction::time_based_mc::TimeBasedMcConfig,
    pub(crate) cached_mc: CachedMicroCompact,
    /// The system prompt and tool definitions of the last request, added to
    /// the message total by `context_tokens()`: without it the threshold check
    /// sees only message content and under-counts by a large constant.
    pub(crate) request_overhead_tokens: usize,
    pub(crate) auto_backoff: crate::turn::AutoCompactBackoff,
}

/// An output token target, kept until it is met, given up on, or cleared —
/// a turn that ends otherwise leaves it running into the next.
pub(crate) struct TokenBudget {
    pub(crate) target: Option<u64>,
    /// Set by the message that set the target; the model has not been told yet.
    pub(crate) unannounced: bool,
    pub(crate) accumulated: u64,
    pub(crate) continuations: u32,
    pub(crate) last_delta: u64,
}

/// What this session has spent and how long its parts took.
pub(crate) struct Telemetry {
    pub(crate) telemetry_handle: TelemetryHandle,
    pub(crate) perf: Arc<PerfCollector>,
    pub(crate) spent: SessionSpend,
}

/// How input reaches this session and events leave it.
pub(crate) struct Channels {
    pub(crate) input_rx: InputReceiver,
    pub(crate) event_tx: EventSender,
    /// The turn last handed to the turn loop, replaced when its `User`
    /// message is queued and cancelled by the input demultiplexer, which
    /// holds no `&mut self`.
    pub(crate) current_turn_cancel: Arc<std::sync::Mutex<TurnCancel>>,
    pub(crate) steering: crate::steering::SharedSteerQueue,
}

/// Which turn `EngineCommand::CancelTurn` lands on.
#[derive(Default)]
pub(crate) struct TurnCancel {
    turn_id: String,
    token: CancellationToken,
    ended: bool,
    /// Turns cancelled by id before their message was queued.
    cancelled_ahead: std::collections::VecDeque<String>,
}

impl TurnCancel {
    const AHEAD_KEPT: usize = 16;

    pub(crate) fn queued(&mut self, turn_id: &str, token: CancellationToken) {
        if let Some(at) = self.cancelled_ahead.iter().position(|id| id == turn_id) {
            self.cancelled_ahead.remove(at);
            token.cancel();
        }
        self.turn_id = turn_id.to_string();
        self.token = token;
        self.ended = false;
    }

    pub(crate) fn cancel(&mut self, turn_id: Option<&str>) {
        match turn_id {
            Some(id) if id != self.turn_id => {
                if self.cancelled_ahead.len() == Self::AHEAD_KEPT {
                    self.cancelled_ahead.pop_front();
                }
                self.cancelled_ahead.push_back(id.to_string());
            }
            _ if self.ended => {}
            _ => self.token.cancel(),
        }
    }

    pub(crate) fn ended(&mut self, turn_id: &str) {
        if self.turn_id == turn_id {
            self.ended = true;
        }
    }
}

pub(crate) fn lock_turn_cancel(
    cancel: &std::sync::Mutex<TurnCancel>,
) -> std::sync::MutexGuard<'_, TurnCancel> {
    cancel
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
