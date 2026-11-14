//! Engine struct and builder — the central AGENT orchestrator.

use base::event::AgentEvent;
use base::interface::model::Model;
use base::interface::permission::PermissionSet;
use base::interface::scene::SceneSpec;
use base::memory::MemoryStore;
use base::settings::LoadedSettings;
use base::tool::InMemoryToolRegistry;
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
mod instruments;
mod messages;
mod run;
mod tooling;
pub mod turn;
use crate::turn::{Compaction, Gate, Memory, ModelRoute, PromptContext};
pub use builder::{default_run_slots, Builder, HostContracts, HostLayers};
use instruments::Telemetry;
pub use messages::*;
#[cfg(test)]
pub(crate) use tooling::sync_mcp_adapters;
use tooling::Tooling;

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
/// be freed; whatever outlives a turn on the session's behalf (team members)
/// watches `ended`; and the engine takes itself off the session's state.
struct SessionEnd {
    ended: CancellationToken,
    /// The token [`Agent::run`] was handed: cancelled once the engine is
    /// being stopped, while `ended` waits for it to have stopped.
    stopping: CancellationToken,
    tools: Arc<InMemoryToolRegistry>,
    state: Arc<base::context::SessionState>,
    attachment: base::context::EngineAttachment,
    reason: EndReasonSlot,
}

impl Drop for SessionEnd {
    fn drop(&mut self) {
        self.ended.cancel();
        self.tools.clear();
        self.state.detach(self.attachment.generation());
    }
}

pub struct Agent {
    scene: Arc<SceneSpec>,
    settings: Arc<LoadedSettings>,
    /// Derived from `settings`; every tool call's `ToolExecCtx.config` is a
    /// clone of this, so tools see the configured sandbox/permission/shell
    /// settings rather than defaults.
    config: Arc<base::context::EngineConfig>,
    session: SessionManager,
    /// Distinct from `session`: `SessionManager` owns the *transcript*, this
    /// owns everything tools read and write across calls. One instance for the
    /// whole session, handed to every `ToolContext` — see `Builder::build()`.
    session_state: Arc<base::context::SessionState>,
    /// How this session's tool calls reach the machine: built once from
    /// `config` and the builder's process-group ledger, and bound to
    /// `session_state` for the permission that judges those calls.
    exec: base::interface::exec::ExecProviders,
    /// Outlives the turn it names: what is announced between turns (a
    /// `/compact`, a turn error) carries the last turn's id.
    current_turn_id: String,
    session_end: SessionEnd,
    identity: Identity,
    llm: ModelRoute,
    tooling: Tooling,
    gate: Gate,
    prompt: PromptContext,
    policies: Policies,
    memory: Memory,
    compaction: Compaction,
    turn: crate::turn::TurnState,
    /// Read by the *next* turn, to decide whether to rescan for skills; starts
    /// `true` so the first turn scans.
    last_had_tool_uses: bool,
    /// Keyed by `session.turn_count`, which only a completed turn advances, so
    /// a cancelled turn's counts carry into the next.
    repeat_guard: crate::repeat_guard::RepeatGuard,
    telemetry: Telemetry,
    channels: Channels,
}

/// Who this session is among others: its place in delegation and teams.
pub(crate) struct Identity {
    agent_depth: u32,
    agent_type: Option<String>,
    team_seat: Option<base::interface::delegate::TeamSeat>,
    drive_goals: bool,
    skip_warmup: bool,
}

impl Identity {
    pub(crate) fn agent_depth(&self) -> u32 {
        self.agent_depth
    }

    pub(crate) fn agent_type(&self) -> Option<&str> {
        self.agent_type.as_deref()
    }

    pub(crate) fn team_seat(&self) -> Option<&base::interface::delegate::TeamSeat> {
        self.team_seat.as_ref()
    }
}

/// Pluggable answers to "how long", "how much" and "is it healthy".
pub(crate) struct Policies {
    turn_policy: Arc<dyn base::interface::turn_policy::TurnPolicy>,
    budget_policy: Arc<dyn base::interface::budget_policy::BudgetPolicy>,
    health: Arc<base::interface::health::HealthChecks>,
    environment: Arc<dyn base::interface::environment::Environment>,
}

impl Policies {
    pub(crate) fn turn_policy(&self) -> &dyn base::interface::turn_policy::TurnPolicy {
        self.turn_policy.as_ref()
    }

    pub(crate) fn budget_policy(&self) -> &dyn base::interface::budget_policy::BudgetPolicy {
        self.budget_policy.as_ref()
    }

    pub(crate) fn environment(&self) -> &Arc<dyn base::interface::environment::Environment> {
        &self.environment
    }
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

impl Agent {
    pub(crate) fn emit(&self, event: AgentEvent) {
        let _ = self.channels.event_tx.send(event);
    }
}

pub(crate) fn lock_turn_cancel(
    cancel: &std::sync::Mutex<TurnCancel>,
) -> std::sync::MutexGuard<'_, TurnCancel> {
    cancel
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
