//! Engine struct and builder — the central AGENT orchestrator.

use base::interface::event::AgentEvent;
use base::interface::memory::MemoryStore;
use base::interface::model::Model;
use base::interface::permission::Permission;
use base::interface::scene::AgentScene;
use base::interface::settings::Settings;
use base::tool::InMemoryToolRegistry;
use compaction::cached::CachedMicroCompact;
use compaction::compact::{Compactor, DefaultCompactor};
use hooks::HookRunner;
use mcp::manager::McpManager;
use session::session::SessionManager;
use session::session_memory::SessionMemory;
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::perf::PerfCollector;
use telemetry::{TelemetryHandle, TelemetryRecorder};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing;

// ── Channel types ──

#[derive(Debug)]
pub enum InputMessage {
    User {
        content: String,
        attachments: Vec<Attachment>,
        turn_id: String,
    },
    ToolResult {
        tool_use_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    PermissionResponse {
        prompt_id: String,
        decision: PermissionDecision,
    },
    System {
        kind: EngineCommand,
        content: String,
    },
    /// A user message meant for the turn that is running, read by the model
    /// at the turn's next step boundary. Only [`Agent::run`] routes these.
    ///
    /// With `ack`, the sender learns whether a turn took it, and keeps the
    /// message when none did. Without `ack`, a message that finds no turn
    /// running starts one, as an ordinary `User` message would.
    Steer {
        content: String,
        message_id: String,
        ack: Option<tokio::sync::oneshot::Sender<crate::steering::SteerAck>>,
    },
    /// A look at, or a change to, this session's context, handled between
    /// turns. The answer — or why not — goes back on `reply`.
    Context {
        request: ContextRequest,
        reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    },
}

/// What a host asks of a session's context (`InputMessage::Context`).
///
/// Positions are those of the session's messages as `session.history`
/// numbers them. `total`, when given, is how many messages the asker saw: a
/// change refuses to apply if the context has since grown or been compacted,
/// rather than land on a message the asker did not mean.
#[derive(Debug, Clone)]
pub enum ContextRequest {
    /// What the next model call would carry, without making it.
    Inspect,
    /// Blank out user messages or tool results (see `SessionManager::drop_message`).
    Drop {
        at: Vec<usize>,
        total: Option<usize>,
    },
    /// Rewrite a message the user typed.
    Edit {
        at: usize,
        text: String,
        total: Option<usize>,
    },
}

/// Something the user attached to their message — a pasted screenshot, a
/// dragged-in file, an editor selection.
///
/// This type existed before but was never consumed: `run_user_turn` built the
/// user message from `content` alone, and every producer passed `vec![]`. The
/// variants below are the three shapes hosts actually have, so that a host can
/// hand over whichever it already holds instead of being forced to pre-read
/// (or pre-decode) on the runtime's behalf.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attachment {
    /// A path the runtime should read. Images (by extension) become
    /// [`ModelContentBlock::Image`]; anything else is inlined as text.
    /// Use this when the host only has a path — e.g. a drag-and-drop.
    File { path: String },
    /// Text the host has already read. `path` is kept for the label shown to
    /// the model, and may be a synthetic name for non-file text such as an
    /// editor selection.
    Text { path: String, content: String },
    /// An image the host already decoded — the clipboard-paste case, where
    /// there is no file on disk at all. `data` is base64 without a data-URI
    /// prefix.
    Image { media_type: String, data: String },
}

/// Where a `PermitAlways` decision's effect should be retained.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistScope {
    /// In-memory only, this session.
    Session,
    /// In-memory for this session AND written to `.agix/settings.local.json`
    /// so future sessions in this project also allow it.
    Local,
}

/// Wire shape: `{"type":"permit"}` / `{"type":"deny","reason":"..."}` /
/// `{"type":"permit_always","scope":"session"}` /
/// `{"type":"permit_always","scope":"local"}` — what
/// `session.respondToPrompt` (daemon RPC) parses out of its `decision`
/// param and what `InputMessage::PermissionResponse` carries.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionDecision {
    Permit,
    Deny {
        reason: String,
    },
    /// Like `Permit`, but also persists an Allow rule beyond this single
    /// tool call — see `base::interface::permission::Permission::add_persistent_allow`.
    /// `scope: Session` keeps it in-memory only (rest of this session);
    /// `scope: Local` additionally writes it to `.agix/settings.local.json`
    /// so future sessions in this project inherit it too.
    PermitAlways {
        scope: PersistScope,
    },
}

#[derive(Debug, Clone)]
pub enum EngineCommand {
    SetSessionId,
    CompactNow,
    RefreshMcp,
    UpdateModel,
    /// Interrupt the turn in flight and keep the session alive — what a host
    /// binds to Esc. Distinct from cancelling the token passed to
    /// [`Agent::run`], which ends the whole session.
    ///
    /// Handled in the input demultiplexer, never in the main loop: the loop
    /// awaits `process_turn` inline, so while a turn is running it is not
    /// reading the channel and could only act on this once the turn it was
    /// meant to interrupt had already finished. Same reason
    /// `PermissionResponse` is handled there.
    ///
    /// A no-op when no turn is running (the token it cancels belongs to the
    /// turn that just ended); it does not arm the next one.
    CancelTurn,
    Shutdown,
}

pub type InputSender = mpsc::UnboundedSender<InputMessage>;
pub type InputReceiver = mpsc::UnboundedReceiver<InputMessage>;
/// Where the engine emits. Every emission site in the runtime holds one of
/// these, which is why swapping the raw channel for [`EventBus`] reached all
/// of them at once — the bus keeps `send`'s signature deliberately.
///
/// [`EventBus`]: crate::event_bus::EventBus
pub type EventSender = crate::event_bus::EventBus;
pub type EventReceiver = mpsc::UnboundedReceiver<AgentEvent>;

/// Registry of permission requests awaiting a host response, keyed by
/// `prompt_id`. Shared between `Agent::pending_permissions` and
/// `agent_tool::ParentPermission` (a sub-agent registers into the *parent's*
/// copy of this same map — see `ParentPermission`'s doc comment).
pub(crate) type PendingPermissions = Arc<
    std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<PermissionDecision>>,
    >,
>;

// ── Engine ──

pub struct Agent {
    pub(crate) scene: Arc<dyn AgentScene>,
    pub(crate) model: Arc<dyn Model>,
    pub(crate) tools: Arc<dyn base::tool::ToolRegistry>,
    pub(crate) settings: Arc<Settings>,
    /// Real `EngineConfig` derived from `settings` via `EngineConfig::from_settings`
    /// — the tool-dispatch closure (`ToolExecCtx.config`) clones this instead of
    /// the `defaults_for("unknown")` placeholder it used to hardcode, so every
    /// tool call sees the actual configured sandbox/permission/shell-execution
    /// settings, not silent defaults.
    pub(crate) config: Arc<base::context::EngineConfig>,
    /// This session's own position in the delegation chain — see
    /// [`Builder::agent_depth`]. Reaches tools as `ToolContext::agent_depth`,
    /// which was hardcoded to `0` before there was a real depth to report.
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
    /// Every script carrier bound in this session, so a turn can reset the
    /// per-turn quotas — see [`Builder::script_carriers`].
    pub(crate) script_carriers: Arc<Vec<Arc<base::interface::script::ScriptCarrier>>>,
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
    /// instance matters — dispatch used to mint a fresh one per tool call.
    pub(crate) session_state: Arc<base::context::SessionState>,
    pub(crate) perf: Arc<PerfCollector>,

    pub(crate) compactor: Arc<dyn Compactor>,
    pub(crate) hooks: Arc<HookRunner>,
    pub(crate) skills: std::sync::Arc<skills::manager::SkillManager>,
    pub(crate) commands: std::sync::Arc<crate::commands::CommandRegistry>,
    pub(crate) mcp: McpManager,

    pub(crate) telemetry_handle: TelemetryHandle,
    pub(crate) current_turn_id: String,
    /// Compaction circuit breaker state — tracks consecutive failures and
    /// prevents infinite compaction loops.
    pub(crate) compaction_state: compaction::reactive::CompactionState,
    /// Session-start frozen environment snapshot. Computed lazily on first turn.
    pub(crate) frozen: Option<base::frozen::FrozenContext>,
    /// Pre-read AGENTS.md/CLAUDE.md content for userContext injection.
    pub(crate) claude_md_content: Option<String>,
    /// Whether CLAUDE.md has been injected as a synthetic user message this session.
    pub(crate) claude_md_injected: bool,
    /// Track invoked skill names during the turn (for post-compact recovery T1.4).
    pub(crate) invoked_skills: Vec<String>,
    /// This turn's user message: a fingerprint of its text and what the text is
    /// made of, for `StreamParams::input_map`.
    ///
    /// The message is identified by content rather than by the index it had
    /// when it was pushed, because compaction rewrites the message list before
    /// the request is assembled. A message compaction dropped simply fails to
    /// resolve, which is the honest outcome — better than an index that now
    /// names a different message.
    pub(crate) pending_input: Option<(u64, Vec<base::interface::model::InputSpan>)>,
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
    /// Whether the agent is currently in plan mode (for post-compact recovery T1.4).
    pub(crate) in_plan_mode: bool,
    /// Plan file content when in plan mode (for post-compact recovery T1.4).
    pub(crate) plan_content: Option<String>,
    /// Running background task summaries: (task_id, status). Populated by AgentTool.
    pub(crate) running_task_summaries: Vec<(String, String)>,
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
    /// prompt section — without this, that section was injected unconditionally
    /// every turn, telling the model "results have been cleared" even on turn 1
    /// when nothing had been, which could push it to needlessly re-read/re-run
    /// things it still actually has.
    pub(crate) tool_results_ever_cleared: bool,
    /// Tool names for which `build_tool_defs` has already logged a
    /// built-in/MCP name-collision warning this session — dedups the log so
    /// a persistent collision doesn't warn on every single API call.
    pub(crate) mcp_shadow_warned: std::collections::HashSet<String>,
    /// P2-3: Time-based micro-compact configuration. Controls when old tool
    /// results are cleared based on wall-clock age. Default: 15 minutes.
    pub(crate) time_based_mc_config: compaction::time_based_mc::TimeBasedMcConfig,
    /// Cached micro-compact state: time-driven cache edit generation.
    /// When enabled, clears old tool results and records their tool_use_ids
    /// as `cache_edits` to send to the Anthropic API, avoiding cache invalidation.
    pub(crate) cached_mc: CachedMicroCompact,
    /// Team ID if this agent is a worker in a team (teammate lifecycle hooks).
    pub(crate) team_id: Option<String>,
    /// Orphaned permission from a previous session (for resume recovery).
    pub(crate) orphaned_permission: Option<crate::agent::PermissionDecision>,
    /// Whether we've already handled the orphaned permission this session.
    pub(crate) has_handled_orphaned_permission: bool,
    /// Message replay / acknowledgement state.
    #[allow(dead_code)]
    pub(crate) messages_to_ack: Vec<String>,
    /// Output token budget target (e.g., 500k, 2M). `None` = no budget active.
    pub(crate) output_token_target: Option<u64>,
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

impl Agent {
    /// The session's own state — its todos, tasks, permission mode — shared
    /// with every tool call, for a host that shows or acts on it.
    pub fn session_state(&self) -> Arc<base::context::SessionState> {
        self.session_state.clone()
    }

    /// The slash commands this session will actually resolve.
    ///
    /// For a host rendering a completion popup or a command palette: this is
    /// the same registry the turn loop dispatches against, and its
    /// skill-derived half is live, so what the user is shown cannot drift
    /// from what typing it will do. Hosts that keep their own catalog to get
    /// lower-latency updates should still reconcile against this one when
    /// [`AgentEvent::SkillsChanged`] arrives.
    ///
    /// Hands out the `Arc`, not a borrow: `run()` takes `&mut self` for the
    /// life of the session, so a host that has spawned the engine has no
    /// `&Agent` left to ask. Take this before spawning and keep it.
    pub fn commands(&self) -> Arc<crate::commands::CommandRegistry> {
        Arc::clone(&self.commands)
    }

    /// The diagnostics registered for this session, for a host that wants to
    /// know whether the engine is well.
    ///
    /// Hands out the `Arc` for the same reason [`Agent::commands`] does:
    /// `run()` holds `&mut self` for the life of the session, so a host that
    /// has spawned the engine has no `&Agent` left to ask. Take this before
    /// spawning, and call [`HealthChecks::report`] whenever something asks —
    /// a status endpoint, a supervisor, a log line every minute.
    ///
    /// [`HealthChecks::report`]: base::interface::health::HealthChecks::report
    pub fn health(&self) -> Arc<base::interface::health::HealthChecks> {
        Arc::clone(&self.health)
    }

    /// Start the agent event loop. Runs until cancelled (caller calls `.cancel()` on the token)
    /// or the input channel closes. Does NOT consume self — the agent can be reused after stop.
    pub async fn run(&mut self, cancel: CancellationToken) {
        tracing::info!(scene = %self.scene.id(), "Engine started");

        // Emit SystemInit
        let tools: Vec<_> = self
            .tools
            .list()
            .iter()
            .map(|t| base::interface::event::ToolInfo {
                name: t.name().to_string(),
                description: t.description().to_string(),
            })
            .collect();
        let _ = self.event_tx.send(AgentEvent::SystemInit {
            scene: self.scene.id().to_string(),
            tools,
            mcp_servers: vec![],
        });

        // `Setup` then `SessionStart`, both fired here rather than in
        // `Builder::build()`: `build()` is sync, and a `Builder` that is
        // constructed but never run is not a session. This is the first point
        // at which the session is genuinely live.
        //
        // `Setup` is documented as "fired during setup / initialization" and
        // had no trigger at all. It runs *before* `SessionStart` and carries
        // the resolved environment, so a hook can validate or prepare the
        // workspace (fetch credentials, warm a cache, refuse to start on a
        // dirty tree) while there is still nothing to undo.
        {
            let scope = self.settings.paths.scope.clone();
            let model = self.settings.model.model_name.clone();
            self.fire_lifecycle_hook(hooks::HookEvent::Setup, |i| {
                i.with_reason(format!("scene={scope} model={model}"))
            })
            .await;
        }
        self.fire_lifecycle_hook(hooks::HookEvent::SessionStart, |i| i)
            .await;

        // `CwdChanged`: the session's working directory as resolved at
        // startup. Also never fired before. A single event at session start
        // is the honest scope of it today — nothing in the engine relocates a
        // live session's cwd, so this reports where work will happen rather
        // than pretending to track a change that cannot occur yet.
        {
            let cwd = self.settings.paths.project_root().display().to_string();
            self.fire_lifecycle_hook(hooks::HookEvent::CwdChanged, |i| {
                i.with_reason(format!("session working directory: {cwd}"))
            })
            .await;
        }

        // P1: Orphaned permission recovery.
        // If the previous session was interrupted while a permission prompt was
        // pending, re-inject the stored decision so the agent can continue.
        if let Some(decision) = self.orphaned_permission.take() {
            if !self.has_handled_orphaned_permission {
                tracing::info!(
                    ?decision,
                    "Recovering orphaned permission from previous session"
                );
                self.has_handled_orphaned_permission = true;
                let _ = self
                    .process_turn(
                        InputMessage::PermissionResponse {
                            prompt_id: "orphaned".into(),
                            decision,
                        },
                        cancel.clone(),
                    )
                    .await;
            }
        }
        self.has_handled_orphaned_permission = true;

        // P2: Startup warmup — pre-compute frozen context, re-scan skills, pre-connect API.
        // Runs after SystemInit but before the main input loop. Gate on skip_warmup for tests.
        if !self.skip_warmup {
            self.warmup().await;
        }

        // ── Input demultiplexer ──
        //
        // The loop below `await`s `process_turn` inline, so while a turn is
        // running it is not reading input. Three messages only mean something
        // *during* a turn and would deadlock or arrive too late there: a
        // `PermissionResponse` the running turn is blocked on, `CancelTurn`,
        // and a `Steer` meant for the turn's next step. This task owns the
        // real receiver and handles those three through `Arc`-shared state;
        // everything else is forwarded to the loop untouched.
        let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel();
        {
            // `self.input_rx` is moved into the task; the field is left
            // holding a receiver whose sender is dropped immediately, so any
            // stray read of it yields `None` rather than blocking forever.
            let (_closed_tx, closed_rx) = mpsc::unbounded_channel();
            let mut raw_rx = std::mem::replace(&mut self.input_rx, closed_rx);
            let pending = self.pending_permissions.clone();
            let denials = self.permission_denial_count.clone();
            let turn_cancel = self.current_turn_cancel.clone();
            let steering = self.steering.clone();
            tokio::spawn(async move {
                while let Some(msg) = raw_rx.recv().await {
                    match msg {
                        InputMessage::Steer {
                            content,
                            message_id,
                            ack,
                        } => {
                            let offered = crate::steering::lock(&steering).offer(
                                base::event::QueuedUserMessage {
                                    message_id,
                                    content,
                                },
                            );
                            match (offered, ack) {
                                (Ok(()), Some(ack)) => {
                                    let _ = ack.send(crate::steering::SteerAck::Accepted);
                                }
                                (Ok(()), None) => {}
                                (Err(_), Some(ack)) => {
                                    let _ = ack.send(crate::steering::SteerAck::NotRunning);
                                }
                                (Err(m), None) => {
                                    let turn = InputMessage::User {
                                        content: m.content,
                                        attachments: vec![],
                                        turn_id: m.message_id,
                                    };
                                    if fwd_tx.send(turn).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        InputMessage::PermissionResponse {
                            prompt_id,
                            decision,
                        } => {
                            crate::turn::resolve_permission_response(
                                &pending, &denials, prompt_id, decision,
                            );
                        }
                        InputMessage::System {
                            kind: EngineCommand::CancelTurn,
                            ..
                        } => {
                            tracing::info!("Cancelling in-flight turn");
                            turn_cancel.lock().unwrap().cancel();
                        }
                        // Receiver gone means the engine loop has exited;
                        // nothing left to forward to.
                        other => {
                            if fwd_tx.send(other).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

        let run_started = std::time::Instant::now();
        let exit_reason: &'static str;
        loop {
            // P2: Between turns, check for wake signals and re-execute any
            // pending async rewake hooks (ones that returned `{rewake: true}`
            // and whose config has `async_rewake: true`).
            self.hooks.check_rewakes().await;

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Engine cancelled");
                    exit_reason = "cancelled";
                    break;
                }
                msg = fwd_rx.recv() => {
                    match msg {
                        Some(input) => {
                            // Each turn runs under its own child token, so
                            // `CancelTurn` interrupts one turn and leaves the
                            // session able to take the next message.
                            // Cancelling the session's own token still
                            // cascades into whatever turn is in flight.
                            let turn_cancel = cancel.child_token();
                            *self.current_turn_cancel.lock().unwrap() = turn_cancel.clone();
                            match self.process_turn(input, turn_cancel).await {
                                Ok(_) => {}
                                Err(crate::turn::TurnError::Shutdown) => {
                                    exit_reason = "shutdown_command";
                                    break;
                                }
                                Err(e) => {
                                    let _ = self.event_tx.send(AgentEvent::Error {
                                        code: "turn_error".into(),
                                        message: e.to_string(),
                                        turn_id: self.current_turn_id.clone(),
                                    });
                                }
                            }
                        }
                        None => {
                            tracing::info!("Input channel closed");
                            exit_reason = "input_channel_closed";
                            break;
                        }
                    }
                }
            }
        }
        // SessionEnd — one exit point for all three ways the loop ends
        // (shutdown command, input channel closed, cancellation), so a hook
        // that cleans up gets to run regardless of which one happened.
        self.fire_lifecycle_hook(hooks::HookEvent::SessionEnd, |i| i)
            .await;

        // Token/cost/api-call/tool-call/error totals aren't accumulated
        // anywhere on `Agent` today (see `/cost`'s own char-count estimate,
        // which is explicitly rough) — left at 0 rather than fabricated.
        let duration_ms = run_started.elapsed().as_millis() as i64;
        let _ = self
            .telemetry_handle
            .record(telemetry::TelemetryEvent::session_end(
                &self.session.session_id,
                0,
                None,
                telemetry::SessionEndPayload {
                    duration_ms,
                    total_turns: self.session.turn_count,
                    total_api_calls: 0,
                    total_tool_calls: 0,
                    total_permission_denials: self
                        .permission_denial_count
                        .load(std::sync::atomic::Ordering::Relaxed),
                    total_errors: 0,
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    total_cache_creation: 0,
                    total_cache_read: 0,
                    total_cost_usd: 0.0,
                    stop_reason: exit_reason.to_string(),
                },
            ));
        let _ = self
            .telemetry_handle
            .record(telemetry::TelemetryEvent::shutdown_signal(
                &self.session.session_id,
                0,
                None,
                telemetry::ShutdownSignalPayload {
                    reason: exit_reason.to_string(),
                    duration_ms,
                    total_turns: self.session.turn_count,
                    had_errors: false,
                    exit_code: 0,
                },
            ));
        tracing::info!("Engine stopped");
    }

    /// Warm up the agent by pre-computing the frozen environment snapshot,
    /// creating the session-memory sidecar, and pre-connecting to the API
    /// endpoint. All operations run in parallel via `tokio::join!` to
    /// minimize startup latency.
    async fn warmup(&mut self) {
        // NOTE: the actual project root, not `local_data_dir` itself (which
        // is `<project_root>/.agix` — see `PathSettings::project_root` docs).
        let cwd = self.settings.paths.project_root();
        let paths = base::paths::ConfigPaths::from_settings(&self.settings.paths);
        let base_url = self.settings.model.base_url.clone();
        let session_memory = self.session.session_memory.clone();

        let (frozen, _, _) = tokio::join!(
            // 1. Pre-compute the frozen environment snapshot (git status, branch, platform, etc.)
            base::frozen::FrozenContext::collect(cwd.clone(), &paths, &*self.environment),
            // 2. Fire-and-forget pre-connect GET to the API base URL (warms TCP/TLS)
            async move {
                if !base_url.is_empty() {
                    let net = base::interface::exec::local::LocalNetwork::default();
                    let warming = base::interface::exec::Network::send(
                        &net,
                        base::interface::exec::HttpRequest::get(&base_url),
                        base::interface::exec::Origin::Operator,
                    );
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), warming).await;
                }
            },
            // 3. Create the session-memory sidecar file if this session has
            // one (see `Builder::build()`) and it doesn't already exist.
            async move {
                if let Some(sm) = session_memory {
                    if let Err(e) = sm.init_session_memory().await {
                        tracing::warn!(error = %e, "failed to init session_memory.md sidecar");
                    }
                }
            },
        );
        // Skills directories are *not* rescanned here anymore. That rescan
        // used to be the only way a session ever noticed new skill files —
        // back before `SkillManager::enable_watching` had a caller (see the
        // `Builder::build()` comment on `enable_watching`, a few lines above
        // where this same `self.skills` gets its live watcher). Now that the
        // watcher is live *before* `warmup()` runs (it's wired synchronously
        // inside `Builder::build()`, which always completes before `run()` —
        // and therefore `warmup()` — starts), re-scanning here was pure
        // redundant disk I/O: the exact same three directories `build()` had
        // just finished scanning milliseconds earlier, on every single
        // session creation, for no observable benefit — anything that
        // could've changed in that window is already covered by the watcher
        // going forward.
        self.frozen = Some(frozen);
        tracing::debug!("Startup warmup complete");
    }

    /// Get current session summary. External interface (read-only).
    pub fn session_info(&self) -> session::session::SessionSummary {
        self.session.summary()
    }

    /// List persisted sessions. External interface (read-only, from HistoryStore).
    pub async fn list_sessions(
        &self,
    ) -> Result<Vec<session::session::SessionSummary>, session::session::SessionError> {
        self.session.list_sessions().await
    }

    /// Delete a persisted session from HistoryStore. External interface.
    pub async fn delete_session(&self, id: &str) -> Result<(), session::session::SessionError> {
        self.session.delete_session(id).await
    }

    /// Load a persisted session's messages from `HistoryStore` back into
    /// this agent's in-memory conversation state. Call before `run()` starts
    /// processing turns — it's a plain field replacement, not safe to race
    /// against an active turn.
    pub async fn resume_session(&mut self, id: &str) -> Result<(), session::session::SessionError> {
        self.session.resume(id).await
    }

    /// Access the performance collector.
    pub fn perf(&self) -> &PerfCollector {
        &self.perf
    }

    /// Access the tool registry (read-only).
    pub fn tools(&self) -> &dyn base::tool::ToolRegistry {
        self.tools.as_ref()
    }

    /// Access the engine settings (read-only).
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Switch the model at runtime (for `/model` slash command).
    pub fn set_model(&mut self, model_name: String) {
        Arc::make_mut(&mut self.settings).model.model_name = model_name;
    }

    /// Access telemetry recorder for external event recording.
    pub fn telemetry(&self) -> &dyn TelemetryRecorder {
        &self.telemetry_handle
    }

    /// Recorder status — None when no recording is configured.
    pub fn recorder(&self) -> Option<&base::interface::settings::RecorderConfig> {
        self.settings.recorder.as_ref()
    }

    /// Access the permission handler (read-only).
    pub fn permission(&self) -> &dyn Permission {
        &*self.permission
    }

    /// Access the memory store (read-only).
    pub fn memory(&self) -> &MemoryStore {
        &self.memory_store
    }

    /// Access the skill manager for runtime loading/listing/reloading.
    pub fn skills(&self) -> &skills::manager::SkillManager {
        &self.skills
    }

    /// Get shared Arc to the skill manager (for tool registration).
    pub fn skills_arc(&self) -> std::sync::Arc<skills::manager::SkillManager> {
        self.skills.clone()
    }

    /// Access the hooks runner (read-only).
    pub fn hooks(&self) -> &HookRunner {
        &self.hooks
    }

    /// Access the MCP manager (read-only).
    pub fn mcp(&self) -> &McpManager {
        &self.mcp
    }

    /// Initialize MCP skills after the agent is constructed.
    ///
    /// For each connected MCP server, fetches the tool list and registers
    /// each tool as a user-invocable skill (name: `mcp__{server}__{tool}`).
    /// Must be called from an async context with a tokio runtime.
    pub async fn init_mcp_skills(&self) {
        for client in self.mcp.clients() {
            match client.list_tools().await {
                Ok(metas) => {
                    let tool_defs: Vec<base::interface::model::ToolDef> = metas
                        .into_iter()
                        .map(|m| base::interface::model::ToolDef {
                            name: m.name,
                            description: m.description.unwrap_or_default(),
                            input_schema: m.input_schema,
                            source: Some(format!("mcp:{}", client.server_name())),
                        })
                        .collect();
                    if !tool_defs.is_empty() {
                        let count = self
                            .skills
                            .register_mcp_skills(client.server_name(), &tool_defs);
                        tracing::info!(
                            server = %client.server_name(),
                            count,
                            "MCP skills registered"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        server = %client.server_name(),
                        error = %e,
                        "Failed to list MCP tools for skill registration"
                    );
                }
            }
        }
    }

    /// Set plan mode state and optional plan content (for post-compact recovery).
    pub fn set_plan_mode(&mut self, active: bool, content: Option<String>) {
        self.in_plan_mode = active;
        self.plan_content = content;
    }

    /// Register a running background task summary (for post-compact recovery).
    pub fn register_running_task(&mut self, task_id: String, status: String) {
        self.running_task_summaries.push((task_id, status));
    }

    /// Clear completed/cancelled running task summaries.
    pub fn clear_running_tasks(&mut self) {
        self.running_task_summaries.clear();
    }

    /// Compact the conversation now, whatever the budget says — the same
    /// path automatic compaction takes, with an LLM summary whenever a
    /// summarizer is configured. Without one, the compactor's cascade aims at
    /// half of the history's current size. `None` when there was nothing to
    /// compact or a `PreCompact` hook declined.
    pub async fn compact_now(&mut self) -> Option<crate::turn::CompactSummary> {
        let target = (compaction::grouping::estimate_tokens(self.session.messages()) / 2).max(1);
        let keep = self
            .budget_policy
            .context_budget(&self.scene.token_budget())
            .compact_keep_recent;
        self.run_compaction(target, keep, crate::turn::CompactTrigger::Manual)
            .await
    }

    /// Convenience: run a single turn from a user message string.
    /// Sends events through the engine's event channel during execution.
    /// Returns `TurnOutcome` on completion.
    pub async fn run_turn(
        &mut self,
        content: String,
        turn_id: String,
        cancel: CancellationToken,
    ) -> Result<crate::turn::TurnOutcome, crate::turn::TurnError> {
        self.process_turn(
            InputMessage::User {
                content,
                attachments: vec![],
                turn_id,
            },
            cancel,
        )
        .await
    }

    /// Run hooks for a lifecycle event.
    /// Returns hook outputs that may block actions or inject text.
    pub async fn run_hooks(
        &self,
        event: hooks::HookEvent,
        input: &hooks::HookInput,
    ) -> hooks::runner::HookRunResult {
        self.hooks.run(event, input).await
    }

    // ── Slash command handlers ──

    /// `/help` — list all available slash commands.
    pub(crate) fn handle_help_command(&self) -> String {
        let commands = self.commands.list();
        if commands.is_empty() {
            return "No commands registered.".into();
        }
        let mut out = String::from("Available slash commands:\n\n");
        for (name, desc) in &commands {
            out.push_str(&format!("/{} — {}\n", name, desc));
        }
        out.push_str("\nUse /<name> [args] to invoke a command.");
        out
    }

    /// `/skills` — list all available skills.
    pub(crate) fn handle_skills_command(&self) -> String {
        let skills = self.skills.list();
        if skills.is_empty() {
            return "No skills loaded.".into();
        }
        let mut out = String::from("Available skills:\n\n");
        for s in &skills {
            let src = s.source.as_str();
            out.push_str(&format!("/{} — {} [{}]\n", s.name, s.description, src));
        }
        out
    }

    /// `/clear` — clear session message history.
    pub(crate) async fn handle_clear_command(&mut self) {
        if let Err(e) = self.session.clear().await {
            tracing::warn!(error = %e, "failed to record /clear in the transcript");
        }
        self.claude_md_injected = false; // re-inject CLAUDE.md on next turn
        self.invoked_skills.clear();
        self.permission_denial_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.output_token_target = None;
        self.accumulated_output_tokens = 0;
        self.token_budget_continuation_count = 0;
    }

    /// `/cost` — show estimated session API cost.
    pub(crate) fn handle_cost_command(&self) -> String {
        let messages = self.session.messages();
        let total_chars: usize = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|b| match b {
                base::interface::model::ModelContentBlock::Text { text } => text.len(),
                _ => 100, // rough estimate for tool blocks
            })
            .sum();
        let est_tokens = total_chars / 4;
        let est_cost = est_tokens as f64 * 3.0 / 1_000_000.0; // input cost estimate
        format!(
            "Session: {} messages, ~{} tokens, est. cost ${:.4}",
            messages.len(),
            est_tokens,
            est_cost
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine shutdown requested")]
    Shutdown,
    #[error("{0}")]
    Internal(String),
}

// ── Builder ──

pub struct Builder {
    scene: Option<Arc<dyn AgentScene>>,
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
    mcp_servers: Option<Vec<String>>,
    mcp_manager_override: Option<McpManager>,
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
    /// Pre-built FrozenContext — skips lazy collection on first turn.
    /// When set, the Agent uses this snapshot instead of calling
    /// `FrozenContext::collect()`. Essential for deterministic replay.
    frozen: Option<base::frozen::FrozenContext>,
    wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    /// Everything installed plugins contribute to this session — see
    /// [`crate::plugin_host::PluginHost`]. `None` (the default) means no
    /// plugins, which is also what a build with the plugin subsystem
    /// compiled out always sees.
    plugin_host: Option<Arc<dyn crate::plugin_host::PluginHost>>,
    /// Pre-built command registry shared across many sessions instead of
    /// re-scanning skill dirs per session — see `Builder::commands_override`.
    commands_override: Option<Arc<crate::commands::CommandRegistry>>,
    /// Multi-provider per-task-type model routing — see `Builder::task_router`.
    task_router: Option<Arc<base::provider::TaskRouter>>,
    /// Every scene this host registered, so an agent type's `scene:` id can
    /// name one beyond the built-in four (plugin scenes, in particular) —
    /// see `crate::agent_tool::AgentTool::set_scene_registry`.
    scene_registry: Option<Arc<scene::scene::SceneRegistry>>,
    /// Pool-level shared agent-type catalog — see
    /// `crate::agent_tool::SharedAgentTypeCatalog`. When set, the session's
    /// `AgentTool` attaches to it (`AgentTool::with_shared_agent_types`)
    /// instead of merging its own catalog and starting its own file watcher
    /// thread. `None` (the default) preserves prior per-session behavior for
    /// non-daemon callers.
    shared_agent_types: Option<crate::agent_tool::SharedAgentTypeMap>,
    /// Pre-built, pool-level shared skill catalog — see
    /// `Builder::skill_catalog`. When set, `build()` uses it directly instead
    /// of scanning the three skill-directory tiers from disk itself. `None`
    /// (the default) preserves prior per-session behavior for non-daemon
    /// callers (tests, library embedding): `build()` self-scans and starts
    /// its own watcher, same as always.
    skill_catalog: Option<Arc<skills::manager::SkillManager>>,
    /// Skill sources beyond the engine's own three — see
    /// [`Builder::skill_provider`].
    skill_providers: Vec<Arc<dyn base::interface::skill_provider::SkillProvider>>,
    /// Where the standing instructions come from — see
    /// [`Builder::instruction_provider`].
    instruction_provider:
        Option<Arc<dyn base::interface::instruction_provider::InstructionProvider>>,
    /// Replaces the built-in way of asking a person something — see
    /// [`Builder::elicitation`].
    elicitation: Option<Arc<dyn base::interface::elicitation::Elicitation>>,
    /// Contributions to the system prompt — see [`Builder::prompt_registry`].
    prompt_registry: Option<Arc<dyn base::interface::prompt_registry::PromptRegistry>>,
    /// Replaces prompt assembly itself — see [`Builder::prompt_assembler`].
    prompt_assembler: Option<Arc<dyn base::interface::prompt_assembler::PromptAssembler>>,
    /// Rings around tool dispatch — see [`Builder::tool_middleware`].
    tool_middleware: Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>,
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
    /// Script carriers whose quota resets each turn — see
    /// [`Builder::script_carriers`].
    script_carriers: Vec<Arc<base::interface::script::ScriptCarrier>>,
    /// Model-failure recovery — see [`Builder::recovery_policy`].
    recovery_policy: Option<Arc<dyn base::interface::recovery_policy::RecoveryPolicy>>,
    /// Model request/response interception — see [`Builder::model_interceptor`].
    model_interceptors: Vec<Arc<dyn base::interface::model_interceptor::ModelInterceptor>>,
    /// Recall — see [`Builder::memory_retriever`].
    memory_retriever: Option<Arc<dyn base::interface::memory_contracts::MemoryRetriever>>,
    retrieval_hooks: Vec<Arc<dyn base::interface::memory_contracts::RetrievalHook>>,
    /// The scripts a delegated agent inherits — see
    /// [`BoundScripts::for_delegate`](base::interface::script_adapters::BoundScripts::for_delegate).
    delegate_scripts: Option<Arc<base::interface::script_adapters::BoundScripts>>,
    /// Extra destinations for this session's events, beyond the `event_rx`
    /// `build()` returns — see [`Builder::event_sink`].
    event_sinks: Vec<Arc<dyn base::interface::event_sink::EventSink>>,
    /// How many delegation hops separate this agent from the root session —
    /// see [`Builder::agent_depth`].
    agent_depth: u32,
    /// Scheduled jobs, kept and fired by whoever owns this store — see
    /// [`Builder::cron_store`].
    cron_store: Option<Arc<tools::cron::CronStore>>,
}

/// Scan the three skill-directory tiers from disk and register bundled
/// skills — `Builder::build()`'s fallback for callers that don't inject a
/// pre-built catalog via `Builder::skill_catalog` (tests, library
/// embedding). A daemon-style caller builds this once at the pool level
/// instead of calling it per session — see `Builder::skill_catalog`'s doc
/// comment.
///
/// Scans `~/.agix/skills/` (global default), then
/// `~/.agix/scenes/<scope>/skills/` (scene override — same name wins,
/// `load_dir_subdirs` overwrites by name), then `<project>/.agents/skills/`
/// (project — external fact standard, also scanned by Codex).
/// `load_dir_subdirs` (not the flat-only `load_dir`) so both `<name>.md` and
/// the `<name>/SKILL.md` subdirectory convention (what
/// `discover_for_paths`/`reload_skill` already supported) are
/// picked up at startup — a project skill authored the SKILL.md way used to
/// silently never load until its first live-reload edit. Found via the recording
/// test suite: the fixture project's `code-review` skill (subdirectory
/// format) was completely absent from every recorded system prompt.
///
/// Also starts this manager's own live-reload watcher (`notify` setup
/// failing degrades to no live reload, logged, not a hard error — every
/// other watcher in this codebase handles it the same way).
///
/// A host that already watches these directories wants
/// [`build_skill_manager_unwatched`] instead: one watcher per manager is
/// fine for one manager, and a `notify` backend has a per-process ceiling
/// that a host building one of these per project will reach.
pub fn build_default_skill_manager(settings: &Settings) -> skills::manager::SkillManager {
    build_skill_manager(settings, true)
}

/// [`build_default_skill_manager`] without the watcher — the same catalog,
/// reloaded by whoever is already watching rather than by a `notify` thread
/// of its own.
pub fn build_skill_manager_unwatched(settings: &Settings) -> skills::manager::SkillManager {
    build_skill_manager(settings, false)
}

fn build_skill_manager(settings: &Settings, watch: bool) -> skills::manager::SkillManager {
    use skills::manager::SkillSource;
    use skills::sources::{BundledSkills, SkillDirectory};

    let skill_mgr = skills::manager::SkillManager::new();
    let project_skills_dir = settings.paths.project_root().join(".agents").join("skills");
    let tiers: [Arc<dyn base::interface::skill_provider::SkillProvider>; 3] = [
        Arc::new(SkillDirectory::tier(
            settings.paths.global_data_dir.join("skills"),
            SkillSource::User,
        )),
        Arc::new(SkillDirectory::tier(
            settings.paths.user_data_dir.join("skills"),
            SkillSource::User,
        )),
        Arc::new(SkillDirectory::tier(
            project_skills_dir.clone(),
            SkillSource::Project,
        )),
    ];
    let loaded_count: usize = tiers
        .into_iter()
        .map(|tier| skill_mgr.load_from(tier))
        .sum();
    // Built-ins come last and defer: a project that writes its own skill of
    // the same name means to replace the one in the binary.
    skill_mgr.load_from(Arc::new(BundledSkills));
    let total_skills = skill_mgr.list().len();
    tracing::info!(loaded_count, total_skills, "skills loaded (incl. bundled)");

    if watch {
        if let Err(e) = skill_mgr.enable_watching(&[
            settings.paths.global_data_dir.join("skills"),
            settings.paths.user_data_dir.join("skills"),
            project_skills_dir,
        ]) {
            tracing::warn!(error = %e, "failed to enable skill file watching; live skill reload disabled for this session");
        }
    }
    skill_mgr
}

/// Build a `HookRunner` from `settings.hooks_config` (raw JSON from
/// settings.json's `hooks` section) when the caller didn't inject one
/// explicitly via `Builder::hooks(...)`.
///
/// A malformed value degrades to no hooks + a `tracing::warn!` rather than
/// failing the build — same soft-degrade philosophy as the rest of
/// settings.json. Also wires the `.agix/hooks/` three-tier search path
/// (project > scene > global) so `Command` hooks can reference a bare
/// script name — see `hooks::HookRunner::with_hooks_search_dirs` docs.
///
/// `model`/`agent_tool` back the `"type": "prompt"` / `"type": "agent"`
/// hook executors (`crate::hook_executors`) — previously neither was ever
/// injected anywhere in the app, so those two hook types always fell
/// through to `HookOutcome::Skipped` with "no executor configured".
fn build_hook_runner(
    settings: &Settings,
    model: Arc<dyn Model>,
    agent_tool: Arc<crate::agent_tool::AgentTool>,
    plugin_host: Option<&Arc<dyn crate::plugin_host::PluginHost>>,
    session_id: Option<String>,
) -> Arc<HookRunner> {
    // Key by key, not whole-map. `HooksSettings` is keyed by `HookEvent`,
    // which has no serde fallback, so a single unrecognized event name used to
    // fail the entire map and drop the caller into an empty runner — every
    // hook the user configured, silently off, because of one typo.
    let parsed: hooks::HooksSettings = match &settings.hooks_config {
        Some(v) => {
            let (parsed, report) = hooks::parse_hooks_settings(v);
            for name in &report.unknown_events {
                tracing::warn!(
                    event = %name,
                    "settings.json `hooks` names an event this engine does not have; \
                     that entry is ignored, the rest are unaffected"
                );
            }
            for (event, err) in &report.invalid_configs {
                tracing::warn!(
                    event = %event,
                    error = %err,
                    "settings.json `hooks` entry failed to parse; that entry is ignored, \
                     the rest are unaffected"
                );
            }
            parsed
        }
        None => Default::default(),
    };
    // A hook configured for an event nothing fires is indistinguishable, from
    // the outside, from a hook that runs and does nothing — the author waits
    // for it forever. The engine knows which events those are, so it says so
    // here rather than leaving the answer in a doc comment on the enum that
    // whoever wrote the config is not reading.
    for (event, configs) in &parsed {
        if !event.is_wired() {
            tracing::warn!(
                event = ?event,
                hooks = configs.len(),
                "hook configured for an event this engine never fires — it will not run; \
                 see `hooks::UNWIRED_EVENTS` for why"
            );
        }
    }
    let hooks_search_dirs = vec![
        settings.paths.project_root().join(".agix").join("hooks"),
        settings.paths.user_data_dir.join("hooks"),
        settings.paths.global_data_dir.join("hooks"),
    ];
    let prompt_executor = Arc::new(crate::hook_executors::ModelPromptHookExecutor::new(
        model,
        settings.model.model_name.clone(),
        settings.model.max_tokens,
        session_id,
    ));
    let agent_spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> = Arc::new(
        crate::agent_spawner_impl::RuntimeAgentSpawner::new(agent_tool),
    );
    let agent_executor = Arc::new(crate::hook_executors::AgentSpawnerHookExecutor::new(
        agent_spawner,
        settings.paths.project_root(),
    ));
    let mut runner = HookRunner::new(parsed)
        .with_hooks_search_dirs(hooks_search_dirs)
        .with_subprocess_env(settings.sandbox.subprocess_env.clone())
        .with_prompt_executor(prompt_executor)
        .with_agent_executor(agent_executor);
    if let Some(host) = plugin_host {
        if let Some(executor) = host.hook_executor() {
            runner = runner.with_wasm_executor(executor);
        }
        for (event, config) in host.hook_configs() {
            runner.register_hook(event, config);
        }
    }
    Arc::new(runner)
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
            mcp_servers: None,
            mcp_manager_override: None,
            telemetry_url: None,
            instruction_file: None,
            session_id: None,
            parent_session_id: None,
            agent_type: None,
            telemetry_handle_override: None,
            skip_warmup: false,
            output_schema: None,
            frozen: None,
            wake_rx: None,
            plugin_host: None,
            commands_override: None,
            task_router: None,
            scene_registry: None,
            shared_agent_types: None,
            skill_catalog: None,
            skill_providers: Vec::new(),
            instruction_provider: None,
            elicitation: None,
            prompt_registry: None,
            prompt_assembler: None,
            tool_middleware: Vec::new(),
            result_transformers: Vec::new(),
            turn_policy: None,
            budget_policy: None,
            health_checks: Vec::new(),
            environment: None,
            script_carriers: Vec::new(),
            recovery_policy: None,
            model_interceptors: Vec::new(),
            memory_retriever: None,
            retrieval_hooks: Vec::new(),
            delegate_scripts: None,
            event_sinks: Vec::new(),
            agent_depth: 0,
            cron_store: None,
        }
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
    /// it needs a clarification or an import confirmation — the built-in
    /// implementation declines those, because the channel protocol has no
    /// wire form for them.
    ///
    /// [`Elicitation`]: base::interface::elicitation::Elicitation
    pub fn elicitation(mut self, e: Arc<dyn base::interface::elicitation::Elicitation>) -> Self {
        self.elicitation = Some(e);
        self
    }

    /// Let something other than the engine contribute to the system prompt.
    ///
    /// Registered blocks are merged with the kernel's stages and sorted with
    /// them, so a contribution can sit before the skills inventory rather than
    /// only after everything. Registering nothing assembles exactly the prompt
    /// the engine assembled before this existed.
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

    /// Install everything a set of script bindings produced.
    ///
    /// The adapters belong in three places — a prompt registry, this builder,
    /// and the list a turn resets — and getting one of them wrong looks
    /// exactly like a script that was never bound. So the knowledge of which
    /// goes where lives here, once, rather than in every caller that has a
    /// `BoundScripts` in hand.
    ///
    /// A registry is created only if something needs one: a session with no
    /// prompt contributions should not acquire an empty registry that
    /// overrides whatever else was going to provide it.
    pub fn bound_scripts(mut self, bound: base::interface::script_adapters::BoundScripts) -> Self {
        if bound.registers_on_prompt_registry() {
            let registry = base::interface::prompt_registry::InMemoryPromptRegistry::new();
            bound.apply_to_registry(registry.as_ref());
            self = self.prompt_registry(registry);
        }
        for t in &bound.tool_results {
            self = self.tool_result_transformer(t.clone());
        }
        for h in &bound.retrieval_hooks {
            self = self.retrieval_hook(h.clone());
        }
        for m in &bound.tool_middleware {
            self = self.tool_middleware(m.clone());
        }
        for i in &bound.model_interceptors {
            self = self.model_interceptor(i.clone());
        }
        // Held so the `Agent` tool can hand it to whatever it spawns. A policy
        // that stops at the first delegation is not a policy.
        self.delegate_scripts = Some(Arc::new(bound.for_delegate()));
        self.script_carriers(bound.carriers)
    }

    /// The script carriers bound in this session.
    ///
    /// A carrier counts calls against a *per-turn* quota, and a quota only
    /// means that if something says when a turn began. Nothing did: the
    /// carriers were moved into their adapters and never seen again, so the
    /// budget was really per session — a script on a per-tool-call point would
    /// go quiet partway through a long one and stay quiet, which is the least
    /// debuggable failure a hook point has.
    ///
    /// [`ScriptCarrier`]: base::interface::script::ScriptCarrier
    pub fn script_carriers(
        mut self,
        carriers: Vec<Arc<base::interface::script::ScriptCarrier>>,
    ) -> Self {
        self.script_carriers = carriers;
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
    /// chain; `AgentTool::resolve_tools` dropping "Agent" from a sub-agent's
    /// registry does not, because `build()` re-registers the tool into the
    /// per-session registry it creates.
    pub fn agent_depth(mut self, depth: u32) -> Self {
        self.agent_depth = depth;
        self
    }

    pub fn scene(mut self, s: Arc<dyn AgentScene>) -> Self {
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
    /// P2: Inject a wake channel receiver for async rewake support.
    /// When background work completes, something sends `()` on the
    /// associated sender; the hooks runner picks up the signal and
    /// re-executes any pending rewake hooks.
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
    /// Inject a pre-built MCP manager with live connections. To share one
    /// centrally-connected set of MCP servers across many sessions without
    /// reconnecting per session, build this from the shared connection's
    /// `McpClientHandle`s via `McpManager::from_clients(...)` +
    /// `refresh_tools()` (see `daemon::SessionPool::create()`), rather than
    /// re-running `McpManager::connect_all(...)` for every session.
    pub fn mcp_manager(mut self, m: McpManager) -> Self {
        self.mcp_servers = None;
        self.mcp_manager_override = Some(m);
        self
    }
    pub fn mcp_servers(mut self, names: Vec<String>) -> Self {
        self.mcp_servers = Some(names);
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

    /// Disable startup warmup (for tests).
    pub fn skip_warmup(mut self, val: bool) -> Self {
        self.skip_warmup = val;
        self
    }

    /// Inject the daemon-discovered plugin list (see
    /// `plugin::discover_plugins`) so this session's hooks and agent types
    /// include plugin-declared ones. Defaults to none — non-daemon callers
    /// (tests, single-process CLI mode) are unaffected. Plugin-contributed
    /// slash commands are *not* installed from this list — see
    /// `commands_override` — since daemon builds one shared command
    /// registry up front instead of per session.
    pub fn plugin_host(mut self, h: Arc<dyn crate::plugin_host::PluginHost>) -> Self {
        self.plugin_host = Some(h);
        self
    }

    /// Inject a pre-built command registry (built once, shared across many
    /// sessions — see `daemon::SessionPool`) instead of scanning skill dirs
    /// fresh for every session. Falls back to the existing per-session
    /// `CommandRegistry::from_skill_manager` scan when not set.
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
    pub fn scene_registry(mut self, r: Arc<scene::scene::SceneRegistry>) -> Self {
        self.scene_registry = Some(r);
        self
    }

    /// Share a pool-level agent-type catalog instead of building (and
    /// starting a dedicated file watcher for) this session's own — see
    /// `crate::agent_tool::SharedAgentTypeCatalog`.
    pub fn shared_agent_types(mut self, handle: crate::agent_tool::SharedAgentTypeMap) -> Self {
        self.shared_agent_types = Some(handle);
        self
    }

    /// Use a pre-built, pool-level shared `SkillManager` instead of scanning
    /// the three skill-directory tiers from disk in `build()` — see
    /// `Builder`'s field doc comment. The catalog is expected to already be
    /// attached to a live-reload watcher if the caller wants one (see
    /// `skills::manager::SkillManager::attach_watcher`); `build()` does not
    /// attach one itself when a catalog is injected.
    pub fn skill_catalog(mut self, catalog: Arc<skills::manager::SkillManager>) -> Self {
        self.skill_catalog = Some(catalog);
        self
    }

    /// Add a skill source of the host's own.
    ///
    /// The engine's three — the directory tiers, the built-ins, the tools of
    /// each connected MCP server — are `SkillProvider`s too; this one is
    /// registered after them, so by default it fills names nobody claimed and
    /// leaves the rest alone. A source that means to replace a skill the
    /// engine ships says so with
    /// [`SkillPrecedence::Override`](base::interface::skill_provider::SkillPrecedence::Override).
    ///
    /// Can be called more than once; sources are consulted in the order they
    /// were added.
    pub fn skill_provider(
        mut self,
        provider: Arc<dyn base::interface::skill_provider::SkillProvider>,
    ) -> Self {
        self.skill_providers.push(provider);
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

    pub fn build(self) -> Result<(Agent, EventReceiver, InputSender), EngineError> {
        let scene = self
            .scene
            .ok_or_else(|| EngineError::Internal("scene required".into()))?;
        let model = self
            .model
            .ok_or_else(|| EngineError::Internal("model required".into()))?;
        let tools = self
            .tools
            .unwrap_or_else(|| Arc::new(InMemoryToolRegistry::new()));
        // Per-session tool registry, seeded from whatever was passed in (a
        // daemon's shared, immutable-after-startup template of built-in
        // tools — see `SessionPool.tools`). Every per-session-stateful tool
        // this function registers below (`Skill`/`Agent`/`TeamCreate`/
        // `TeamDelete`/`TaskStop`/`TaskOutput`/MCP adapters)
        // goes into *this* fresh copy, not the caller-supplied one.
        //
        // Before this existed, `Builder::build()` pushed straight into the
        // caller-supplied registry — in a daemon, that's the same `Arc`
        // every session shares, and `InMemoryToolRegistry::register` just
        // pushes (no overwrite-by-name), so N concurrently-built sessions
        // meant N duplicate entries per name silently piling up in one
        // shared `Vec`. That was worse than a simple "last write wins":
        // `InMemoryToolRegistry::get(name)` returns the *first* match
        // (oldest session's instance, `Vec::find`'s iteration order) while
        // `Agent::build_tool_defs`'s `BTreeMap`-based dedup keeps the
        // *last* (newest session's) when advertising tool descriptions to
        // the model — so the description/schema the model was shown could
        // point at a different tool instance than the one a call actually
        // dispatched to.
        //
        // The scene's deferred policy is *not* applied here — it runs once,
        // after every registration below, so it can reach the engine-state
        // tools too. Applying it here as well would only wrap the built-ins a
        // second time.
        let tools = {
            let session_tools = Arc::new(InMemoryToolRegistry::new());
            let mut had_tool_search = false;
            for t in tools.list() {
                // `ToolSearchTool` closes over the registry it searches. The
                // instance in the incoming template closes over the
                // *template*, so it would report on the template's tools —
                // which have neither this session's deferred wrappers nor any
                // of the engine-state tools registered below. Drop it and
                // rebind one to this session's registry instead. (Same `Arc`,
                // read at call time, so later registrations are visible.)
                if t.name() == "ToolSearch" {
                    had_tool_search = true;
                    continue;
                }
                session_tools.register(t);
            }
            if had_tool_search {
                session_tools.register(Arc::new(tools::tool_search::ToolSearchTool::new(
                    session_tools.clone(),
                )));
            }
            session_tools
        };

        let settings = self
            .settings
            .ok_or_else(|| EngineError::Internal("settings required".into()))?;

        // Telemetry: use pre-built handle if injected, else noop (events
        // silently dropped).
        let telemetry_handle = self.telemetry_handle_override.unwrap_or_else(|| {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            TelemetryHandle::new(tx)
        });
        // Every call this session makes is now accounted for, including the
        // ones no turn asked for — see `telemetry::model::TelemetryModel`.
        // Wrapped here rather than further down because the sub-agent spawner
        // built below takes its own copy of the model, and a sub-agent's
        // memory extraction costs exactly as much as a top-level one's.
        let model: Arc<dyn Model> = Arc::new(telemetry::model::TelemetryModel::new(
            model,
            telemetry_handle.clone(),
            settings.model.model_name.clone(),
        ));
        let permission = self.permission.unwrap_or_else(|| {
            struct AllowAll;
            #[async_trait::async_trait]
            impl Permission for AllowAll {
                async fn check(
                    &self,
                    _: &str,
                    _: &serde_json::Value,
                    _: &std::path::Path,
                    _: &str,
                ) -> base::interface::permission::PermissionOutcome {
                    base::interface::permission::PermissionOutcome::Permit
                }
            }
            Arc::new(AllowAll)
        });
        let instruction_file = self.instruction_file.or(settings.instruction_file.clone());
        // Pre-read AGENTS.md / CLAUDE.md content for userContext injection.
        let instruction_provider: Arc<
            dyn base::interface::instruction_provider::InstructionProvider,
        > = self.instruction_provider.clone().unwrap_or_else(|| {
            Arc::new(base::interface::instruction_provider::InstructionFile::new(
                instruction_file.clone(),
            ))
        });
        let claude_md_content = {
            let docs = instruction_provider.documents();
            if docs.is_empty() {
                None
            } else {
                Some(
                    docs.into_iter()
                        .map(|d| d.content)
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                )
            }
        };
        let environment = self
            .environment
            .clone()
            .unwrap_or_else(|| Arc::new(base::interface::environment::SystemEnvironment));
        let memory_store = self.memory_store.unwrap_or_else(|| {
            let p = &settings.paths;
            // memory 不分 scene，只分全局/项目——见 base::paths 模块文档。
            Arc::new(
                MemoryStore::new(
                    p.global_data_dir.join("memory"),
                    p.local_data_dir.join("memory"),
                )
                .with_environment(environment.clone()),
            )
        });
        // Session: `self.history_store` (set via `Builder::history_store(...)`)
        // makes this incrementally persisted to disk once per turn; `None`
        // (the default) keeps it in-memory only, matching prior behavior.
        let mut session = SessionManager::new(
            self.history_store.clone(),
            self.session_id,
            self.parent_session_id.clone(),
        );
        // Session-memory sidecar (`session_memory.md`) — only for sessions
        // that actually persist (matches `resume`'s only being meaningful
        // with a `HistoryStore`; an in-memory-only session's id doesn't
        // outlive the process, so a sidecar for it would just be orphaned
        // disk state nothing ever reads back). File creation itself happens
        // later, in `warmup()` — this only constructs the handle, no I/O.
        //
        // A caller-supplied id that doesn't parse fails the build outright
        // rather than degrading: `SessionManager::persist` parses the same id
        // before writing anything, so an unparseable one means transcript
        // persistence and the sidecar are both dead for the whole session —
        // previously visible only as a per-turn `warn!` (and, here, as
        // nothing at all).
        if self.history_store.is_some() {
            let sid = base::session::SessionId::parse(session.session_id_str()).map_err(|e| {
                EngineError::Internal(format!(
                    "session_id {:?} is not a valid SessionId ({e}); \
                     a history_store requires a parseable id — use \
                     `SessionId::new().to_string()` or omit `Builder::session_id`",
                    session.session_id_str()
                ))
            })?;
            // Every other session sidecar (metadata, input history, prompt
            // state) resolves through `history::path`, so this has to as
            // well — building the root separately is how the memory file
            // ended up somewhere nothing would look for it. Both now derive
            // from the one `global_data_dir` this instance was given, so
            // there is no second root to disagree with.
            let roots = history::path::HistoryRoots::under(&settings.paths.global_data_dir);
            let path = history::path::session_memory_file(&roots.sessions, &sid);
            session = session.with_session_memory(SessionMemory::new(path));
        }
        let compactor = self
            .compactor
            .unwrap_or_else(|| Arc::new(DefaultCompactor) as Arc<dyn Compactor>);
        let plugin_agent_types: Vec<crate::agent_tool::AgentTypeDefinition> = self
            .plugin_host
            .as_ref()
            .map(|h| h.agent_types())
            .unwrap_or_default();
        // AgentTool ("Agent") — lets the model spawn sub-agents. Previously
        // had zero production construction sites despite the CodingScene
        // system prompt instructing the model to use it (see
        // `render_session_guidance`'s `has_agent` check). Catalog is built
        // from the 6 built-in types + any custom `.agix/agents/*.md`
        // definitions, project > scene > global (same override order as
        // skills). `EngineConfig` isn't threaded through `Builder` today, so
        // this derives a minimal one from `settings.model` — just enough for
        // `AgentTool::sub_settings()`'s needs (model name/max_tokens/fallback).
        // Constructed here (before `hooks`, not at its registration point
        // further down) so its `AgentSpawner` wrapper can be handed to the
        // "agent"-type hook executor below — `tools.register(...)` for it
        // happens later, but `InMemoryToolRegistry` is a shared `Arc`, so
        // construction order doesn't affect which tools end up visible to
        // sub-agents (`resolve_tools()` reads the registry at call time).
        let agent_depth = self.agent_depth;
        let max_agent_depth =
            base::context::EngineConfig::defaults_for(settings.model.model_name.clone())
                .max_agent_depth;
        let agent_tool_arc = {
            let agent_engine_config = Arc::new({
                let mut c =
                    base::context::EngineConfig::defaults_for(settings.model.model_name.clone());
                c.max_tokens = settings.model.max_tokens;
                c.fallback_model = settings.model.fallback_model.clone();
                c
            });
            let mut agent_tool = if let Some(shared) = self.shared_agent_types.clone() {
                // Daemon path: attach to the `SessionPool`-level catalog +
                // watcher instead of merging our own and starting a
                // dedicated file-watcher thread for it — see
                // `Builder::shared_agent_types`.
                crate::agent_tool::AgentTool::with_shared_agent_types(
                    model.clone(),
                    agent_engine_config,
                    tools.clone(),
                    tools.clone(),
                    shared,
                )
            } else {
                let project_agents_dir = settings.paths.project_root().join(".agix").join("agents");
                let scene_agents_dir = settings.paths.user_data_dir.join("agents");
                let global_agents_dir = settings.paths.global_data_dir.join("agents");
                let agent_dirs: [&std::path::Path; 3] =
                    [&global_agents_dir, &scene_agents_dir, &project_agents_dir];
                crate::agent_tool::AgentTool::with_parent_tools(
                    model.clone(),
                    agent_engine_config,
                    tools.clone(),
                    tools.clone(),
                    &agent_dirs,
                    &plugin_agent_types,
                )
            }
            .with_settings(settings.clone())
            .with_depth(agent_depth);
            if let Some(router) = self.task_router.clone() {
                agent_tool = agent_tool.with_task_router(router);
            }
            if let Some(scripts) = self.delegate_scripts.clone() {
                agent_tool = agent_tool.with_delegate_scripts(scripts);
            }
            Arc::new(agent_tool)
        };
        let hooks = self.hooks.unwrap_or_else(|| {
            build_hook_runner(
                &settings,
                model.clone(),
                agent_tool_arc.clone(),
                self.plugin_host.as_ref(),
                Some(session.session_id_str().to_string()),
            )
        });
        // P2: Wire the wake receiver into hooks for async rewake support.
        if let Some(rx) = self.wake_rx {
            hooks.set_wake_receiver(rx);
        }
        // `FileWatcher`/`HookEvent::FileChanged` (crates/hooks/src/watcher.rs)
        // has existed fully implemented with no caller — only start the
        // background watcher thread when a `FileChanged` hook is actually
        // configured, since it isn't free (one OS-level watch + a dedicated
        // thread). The instruction file is the one file this engine already
        // treats as "loaded once into the session and never re-read" (see
        // `claude_md_injected` above) — a user editing it mid-session is
        // exactly the case a `FileChanged` hook exists to let a host react to.
        if hooks.has_hooks_for(hooks::HookEvent::FileChanged) {
            if let Some(ref path) = instruction_file {
                if let Err(e) = hooks.enable_file_watching(std::slice::from_ref(path), 300) {
                    tracing::warn!(error = %e, "failed to start FileChanged watcher");
                }
            }
        }
        // MCP: use pre-built manager if injected, else empty (no servers).
        let mut mcp = self.mcp_manager_override.unwrap_or_else(McpManager::empty);
        // `McpManager::set_elicitation_callback` existed with zero production
        // callers — an MCP server embedding an elicitation URL (mcp:// or
        // elicitation://) in a tool result silently never reached the hook
        // system. Wire it to this session's real `HookRunner`; the sync
        // callback signature (`ElicitationCallback`) needs a `tokio::spawn`
        // to reach the async `run_elicitation`, same pattern as
        // `hooks::watcher::FileWatcher`'s background-thread dispatch.
        {
            let hooks_for_elicitation = hooks.clone();
            mcp.set_elicitation_callback(std::sync::Arc::new(move |server_name, url| {
                let hooks = hooks_for_elicitation.clone();
                tokio::spawn(async move {
                    hooks.run_elicitation(&server_name, &url).await;
                });
            }));
        }
        // Hand `agent_tool_arc` a snapshot of the connected MCP tools now,
        // for `AgentTypeDefinition.mcp_servers` — see
        // `Inner::mcp_tool_adapters`'s doc comment for why a snapshot Vec,
        // not a live `McpManager` reference.
        agent_tool_arc.set_mcp_tool_adapters(mcp.tool_adapters().to_vec());
        // Skill catalog: a daemon-style caller hands in a pre-built, shared
        // `SkillManager` via `Builder::skill_catalog` (built once at the
        // pool level, not re-scanned per session — see that method's doc
        // comment). Without one (tests, library embedding), fall back to
        // scanning the three tiers here, same as always.
        let skill_mgr_arc = match self.skill_catalog.clone() {
            Some(catalog) => catalog,
            None => std::sync::Arc::new(build_default_skill_manager(&settings)),
        };
        for provider in self.skill_providers.clone() {
            let id = provider.id().to_string();
            let count = skill_mgr_arc.load_from(provider);
            tracing::info!(source = %id, count, "host skill source registered");
        }
        // `agent_tool_arc` was built before skills finished loading (see the
        // comment at its own construction site) — hand it the manager now,
        // via interior mutability, so `AgentTypeDefinition.skills` (preload)
        // can resolve when a subagent is spawned later in this session.
        agent_tool_arc.set_skill_manager(skill_mgr_arc.clone());
        let command_registry = self.commands_override.clone().unwrap_or_else(|| {
            let mut registry =
                crate::commands::CommandRegistry::from_skill_manager(skill_mgr_arc.clone());
            // MCP prompts become `/mcp__<server>__<prompt>` slash commands,
            // the same way skills become slash commands above. `all_prompts()`
            // is already populated — `McpManager::connect_all` runs
            // `prompts/list` on every server it connects (see
            // `collect_prompts`), it just had no consumer until now.
            //
            // Only the registry *this* builder owns can be extended; a
            // `commands_override` is shared across sessions, so the turn loop
            // falls back to resolving `/mcp__…` off the live `McpManager`
            // instead — see the slash-command interception in `turn.rs`.
            let mcp_prompts = mcp.all_prompts();
            if !mcp_prompts.is_empty() {
                tracing::info!(
                    count = mcp_prompts.len(),
                    "registering MCP prompts as slash commands"
                );
                registry.register_mcp_prompts(mcp_prompts);
            }
            std::sync::Arc::new(registry)
        });
        // Register SkillTool using the populated skill manager. Also gives
        // it a real `AgentSpawner` — previously `register_skill_tool` never
        // received one at all, so a `context: fork` skill's spawner branch
        // (`SkillTool::call`) was unreachable in every real session; it
        // silently fell through to running inline instead of forking, no
        // matter what the skill's frontmatter said.
        let skill_tool_spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> = Arc::new(
            crate::agent_spawner_impl::RuntimeAgentSpawner::new(agent_tool_arc.clone()),
        );
        tools::register_skill_tool(
            &tools,
            Arc::clone(&skill_mgr_arc),
            Some(skill_tool_spawner),
            permission.clone(),
        );
        // Register TaskStopTool — stop running background tasks by ID
        tools.register(std::sync::Arc::new(tools::task_stop::TaskStopTool));
        // Register TaskOutputTool — retrieve output from running/completed tasks
        tools.register(std::sync::Arc::new(tools::task_output::TaskOutputTool));
        // Register AgentTool ("Agent") into the tool registry the model calls
        // (`agent_tool_arc` itself was constructed earlier, before `hooks`,
        // so its `AgentSpawner` wrapper could be handed to the "agent"-type
        // hook executor — see the `hooks` construction above).
        //
        // Withheld at the depth limit so the deepest agents are never even
        // offered the tool. This registry is a fresh per-session one built a
        // few hundred lines up, *not* the caller's — which is why
        // `AgentTool::resolve_tools` filtering "Agent" out of what it hands a
        // sub-agent never had any effect on its own.
        if agent_depth < max_agent_depth {
            tools.register(agent_tool_arc.clone());
        }
        // Register MCP resource tools if clients are available
        if !mcp.clients().is_empty() {
            tools.register(std::sync::Arc::new(mcp::tools::ListMcpResourcesTool::new(
                mcp.clients().to_vec(),
            )));
            tools.register(std::sync::Arc::new(mcp::tools::ReadMcpResourceTool::new(
                mcp.clients().to_vec(),
            )));
            tools.register(std::sync::Arc::new(mcp::tools::DispatchMcpTool::new(
                mcp.clients().to_vec(),
            )));
            // Dispatch looks tools up in this registry only, so MCP adapters
            // live here too; `sync_mcp_adapters` (turn.rs) keeps them current.
            for adapter in mcp.tool_adapters() {
                tools.register(adapter.clone());
            }
        }

        // Register TeamCreate/TeamDelete — multi-agent staged orchestration
        // (`crates/team`). Wired with the same `RuntimeAgentSpawner` used for
        // `Skill`'s `context: fork` above, over the same shared `tools`
        // registry used as both parent and sub-agent pool (matching
        // `AgentTool::with_parent_tools`'s `tools.clone(), tools.clone()`
        // pattern a few lines up) — team workers get the full tool set,
        // scene white/blacklisting still applies at prompt-build time.
        //
        // Gated on `settings.execution.team_enabled` (default `false`): an
        // always-registered tool means its prompt.md is always in the
        // system prompt and its schema fields are always visible to the
        // model, whether or not the operator ever intends to allow team
        // coordination. Only affects sessions built after the setting
        // changes — same as every other `Settings` field, no separate
        // restart requirement.
        let team_config = Arc::new(base::context::EngineConfig::from_settings(&settings));
        // Bound outside the `team_enabled` block so the event-channel wiring
        // below can reach it; `None` when team tools are not registered.
        let mut team_create_tool: Option<Arc<team::tool::TeamCreateTool>> = None;
        // Both have to agree: the setting is the operator's switch, the
        // scene's capability is what the scene is *for*. A scene that does
        // not do teamwork never registers the tools, so the model does not
        // see them — as opposed to seeing them and being refused.
        if settings.execution.team_enabled && scene.supports_team() {
            let team_spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> = Arc::new(
                crate::agent_spawner_impl::RuntimeAgentSpawner::new(agent_tool_arc.clone()),
            );
            // Shared across TeamCreate/TeamList/TeamDelete — each wraps its
            // own separate `DefaultCoordinator` instance, so team state has
            // to live here, not inside any one of them. See
            // `team::registry`'s module doc. Also handed to `agent_tool_arc`
            // itself (`set_team_registry`) so persistent team members
            // spawned via the plain `Agent` tool (`team_name`+`name`, not
            // through `TeamCreate`) update the same state `TeamList` reads.
            let team_registry = Arc::new(team::registry::TeamRegistry::new());
            agent_tool_arc.set_team_registry(team_registry.clone());
            let create_tool = std::sync::Arc::new(team::tool::TeamCreateTool::with_spawner(
                model.clone(),
                team_config.clone(),
                tools.clone(),
                tools.clone(),
                team_spawner.clone(),
                team_registry.clone(),
                scene.id().to_string(),
            ));
            tools.register(create_tool.clone());
            team_create_tool = Some(create_tool);
            tools.register(std::sync::Arc::new(team::tool::TeamListTool::new(
                team_registry.clone(),
            )));
            // `TeamDelete` needs the spawner too (not just the registry) so
            // it can actually stop any still-alive persistent members it
            // finds on record, not just clean up the registry entry and
            // directory — see `DefaultCoordinator::cleanup_team`.
            tools.register(std::sync::Arc::new(team::tool::TeamDeleteTool::new(
                Box::new(
                    team::coordinator::DefaultCoordinator::with_agent_spawner(team_spawner)
                        .with_registry(team_registry),
                ),
            )));
        }
        let config = team_config;

        // ── One `SessionState` for the whole session ──
        //
        // `SessionState` is the session's mutable side: permission mode
        // (which `EnterPlanMode`/`ExitPlanMode` flip), the read cache that
        // `Edit`/`Write` consult for read-before-edit staleness, the read
        // dedup marker, file snapshots backing `/rewind`, todos, the task
        // list, running sub-agent tasks, and the accumulated
        // "allow writes here for this project" paths.
        //
        // The tool-dispatch path used to build a **brand new** one for every
        // single tool call (`turn.rs`'s `ToolContext { session: Arc::new(
        // SessionState::new(cwd)) }`), so none of that state survived from
        // one call to the next: plan mode never took effect, read-before-edit
        // staleness detection never fired, read dedup never deduped, and
        // `/rewind` had nothing recorded. Own exactly one here and hand the
        // same `Arc` to every call.
        let session_state = Arc::new(
            base::context::SessionState::new(settings.paths.project_root())
                .with_permission_mode(config.permission_mode),
        );

        // WebSearch — registered here rather than in
        // `tools::register_builtin_tools` because choosing a `SearchProvider`
        // needs resolved `Settings` (endpoint + credentials) and that function
        // deliberately takes nothing but a registry. Both `ChatScene::tools()`
        // and `ResearchScene::tools()` list "WebSearch", and a scene whitelist
        // is *intersected* with the registry — so an unregistered name is not
        // an error, it just silently disappears, which is how Research ended
        // up with no search capability at all.
        tools::register_web_search(&tools, &settings);

        // Worktree tools — one `WorktreeRegistry` per session (it holds at
        // most one active worktree; `EnterWorktree` refuses a second and
        // `ExitWorktree` cleans up what this session created).
        tools::register_worktree_tools(
            &tools,
            Arc::new(tools::worktree_tools::WorktreeRegistry::new()),
        );

        // Cron tools, and — when no owner handed in a store — the scheduler
        // that drains this session's own. `~/.agix/scenes/<scope>/scheduled_tasks.json`
        // is the exact path `CronCreate`'s own prompt promises the user for
        // `durable: true` jobs; session-only jobs never touch disk.
        //
        // Known limitation of the store-per-session case only: two concurrent
        // sessions in the same scope each load the durable file and each
        // schedule their own fire for a durable job. A session pool avoids it
        // by owning one store and one scheduler (`Builder::cron_store`).
        let fire_jobs_here = self.cron_store.is_none();
        let cron_store = match self.cron_store.clone() {
            Some(store) => store,
            None => {
                let _ = std::fs::create_dir_all(&settings.paths.user_data_dir);
                Arc::new(tools::cron::CronStore::load_or_default(Some(
                    settings.paths.user_data_dir.join("scheduled_tasks.json"),
                )))
            }
        };
        tools::register_cron_tools(&tools, cron_store.clone());

        // ── Registration is complete; everything below reads the final set ──

        // Scene-contributed tools (`AgentScene::extra_tools`, default empty).
        //
        // Deliberately last. The collision check is the whole point of this
        // block, and it can only see names already in the registry — running
        // it before `Agent`/`Skill`/`WebSearch`/`Cron*`/`Worktree*` are
        // registered would let a scene claim one of those names, pass the
        // check against an empty slot, and then win the lookup anyway
        // (`get` returns the first match, and the engine's own instance is
        // appended after). That is precisely the shadowing this rejects.
        for t in scene.extra_tools() {
            if tools.get(t.name()).is_some() {
                return Err(EngineError::Internal(format!(
                    "scene {:?} contributes a tool named {:?}, which is already \
                     registered; rename the scene's tool",
                    scene.id(),
                    t.name()
                )));
            }
            tools.register(t);
        }

        // Tools installed plugins export.
        //
        // After the scene, because a plugin that owns a scene hands that scene
        // its own tools (`PluginScene::from_plugin`) and they arrive here a
        // second time, as the same `Arc`s. Identity is what separates that
        // from a real collision: the same allocation means the scene already
        // contributed this exact tool, and a different one under the same name
        // means two things are claiming it, which is refused for the same
        // reason a scene's collision is.
        if let Some(host) = self.plugin_host.as_ref() {
            for t in host.tools() {
                match tools.get(t.name()) {
                    Some(existing) if Arc::ptr_eq(&existing, &t) => continue,
                    Some(_) => {
                        return Err(EngineError::Internal(format!(
                            "a plugin contributes a tool named {:?}, which is already                              registered by something else",
                            t.name()
                        )));
                    }
                    None => {
                        tools.register(t);
                    }
                }
            }
        }

        // The scene's deferred policy, applied once over the complete set.
        //
        // It has to run here rather than on the registry the caller handed in:
        // `Agent`, `Skill`, `Cron*`, `EnterWorktree`/`ExitWorktree`,
        // `WebSearch`, `Team*`, the MCP adapters and the scene's own
        // contributions are all registered above, and those are most of what a
        // scene would want to defer.
        //
        // Substituting by name over the same `Arc` (rather than rebuilding the
        // registry) is what keeps `ToolSearchTool` working: it closes over this
        // exact registry and reads it at call time, so a tool wrapped
        // underneath it stays discoverable.
        let host_schema = self.output_schema.clone();
        if let Some(schema) = &host_schema {
            if tools.get("StructuredOutput").is_some() {
                tools.replace(Arc::new(
                    tools::structured_output::StructuredOutputTool::with_schema(schema.clone()),
                ));
            }
        }
        {
            let mut deferred = scene.deferred_tools();
            if host_schema.is_some() {
                deferred.retain(|n| n != "StructuredOutput");
            }
            if !deferred.is_empty() {
                for t in tools::deferred::apply_deferred_policy(tools.list(), &deferred) {
                    // `ToolSearch` must never be deferred — it is the only way
                    // back to a deferred tool's schema, so hiding its own
                    // schema would strand every other deferred tool.
                    if t.name() != "ToolSearch" {
                        tools.replace(t);
                    }
                }
            }
        }

        // The scene's tool policy, applied to the registry itself rather than
        // only to what is advertised: `ToolSearch` and dispatch read this same
        // registry, so a tool the scene excludes has to be absent from it.
        for t in tools.list() {
            if !scene.permits_tool(t.name()) {
                tools.remove(t.name());
            }
        }

        let (input_tx, input_rx) = mpsc::unbounded_channel();

        // The scheduler half of the cron bridge: without this, `CronCreate`
        // would happily report "Scheduled recurring job <id>" for a job that
        // could never fire. Ticks once a minute (the resolution of a 5-field
        // cron expression) and feeds each due job's prompt back in as a user
        // turn. Ends by itself when the session's input channel closes.
        //
        // `Handle::try_current` because `build()` is sync and reachable from
        // non-async contexts, where `tokio::spawn` would panic; there the
        // tools still create/list/delete jobs, they just never fire.
        match tokio::runtime::Handle::try_current() {
            Ok(_) if !fire_jobs_here => {}
            Ok(handle) => {
                let store = cron_store.clone();
                let tx = input_tx.clone();
                handle.spawn(async move {
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                    ticker.tick().await; // first tick is immediate — skip it
                    loop {
                        ticker.tick().await;
                        for job in store.pop_due() {
                            let msg = InputMessage::User {
                                content: job.prompt.clone(),
                                attachments: Vec::new(),
                                turn_id: format!("cron-{}", job.id),
                            };
                            if tx.send(msg).is_err() {
                                return; // session gone
                            }
                            tracing::info!(job_id = %job.id, cron = %job.cron, "cron job fired");
                        }
                    }
                });
            }
            Err(_) => {
                tracing::debug!(
                    "no tokio runtime at Builder::build(); cron jobs will be stored but not fired"
                );
            }
        }
        let (primary_tx, event_rx) = mpsc::unbounded_channel();
        let event_tx = crate::event_bus::EventBus::with_sinks(primary_tx, self.event_sinks);

        // ── Sub-agent / team inheritance wiring ──
        // The `AgentTool` and `TeamCreateTool` are constructed above (they
        // have to be — hooks and the team spawner need them), but several of
        // the things a sub-agent should inherit only exist down here (the
        // event channel most of all). All of these are set-after-construction
        // on purpose; see the corresponding doc comments in `agent_tool.rs`.
        //
        // * event channel — sub-agent + team-stage progress is mirrored onto
        //   the parent's channel so the host renders it live (S-1 / T-3);
        // * permission — sub-agents route their checks to this session's real
        //   `Permission` impl instead of allow-all (S-3);
        // * scene / instruction file / history store — sub-agents inherit the
        //   parent's scene, AGENTS.md and transcript store (S-5 / S-2 / S-4).
        // Created here, before the `AgentTool` wiring below, because a
        // sub-agent's permission prompts are registered in this same map —
        // see `AgentTool::set_parent_pending_permissions`.
        let pending_permissions: Arc<
            std::sync::Mutex<
                std::collections::HashMap<String, tokio::sync::oneshot::Sender<PermissionDecision>>,
            >,
        > = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

        // ── Permission binding ──
        //
        // The `Permission` this builder was handed was constructed before
        // the session registry above existed — in a daemon it was built over
        // `SessionPool.tools`, the pool-level template of self-contained
        // built-ins. Everything registered into `tools` in this function
        // (`Skill`, `Agent`, `Team*`, `WebSearch`, `EnterWorktree`/
        // `ExitWorktree`, `Cron*`, `TaskStop`/`TaskOutput`, `Import`, the MCP
        // resource tools and every `mcp__*` adapter) was therefore invisible
        // to it, and its "unknown tool" branch let all of them run with no
        // check at all — the entire MCP surface included. Bind it to the
        // registry dispatch actually uses, now that it is fully populated.
        permission.bind_tool_registry(tools.clone());
        // And to the session's live state, so `EnterPlanMode`/`ExitPlanMode`
        // (which flip `SessionState.permission_mode`) actually move the gate
        // instead of being decorative.
        permission.bind_session_state(session_state.clone());

        // Built before the `Agent` literal takes ownership of `scene` and
        // `settings`. The ceilings are read once per session, which is when
        // they are decided — neither can change mid-session.
        let recovery_policy: Arc<dyn base::interface::recovery_policy::RecoveryPolicy> =
            self.recovery_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::recovery_policy::DefaultRecovery::new(
                    settings.model.fallback_model.clone(),
                ))
            });
        let turn_policy: Arc<dyn base::interface::turn_policy::TurnPolicy> =
            self.turn_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::turn_policy::LimitsPolicy::new(
                    settings.execution.max_api_calls_per_turn,
                    scene.execution_params().max_api_calls_per_turn,
                    // Was a `const` in the turn function; the value is
                    // unchanged, its home is not.
                    5,
                ))
            });

        let budget_policy: Arc<dyn base::interface::budget_policy::BudgetPolicy> =
            self.budget_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::budget_policy::EngineBudget::new(
                    settings.execution.max_budget_tokens,
                ))
            });

        agent_tool_arc.set_event_sender(event_tx.clone());
        agent_tool_arc.set_hooks(hooks.clone());
        agent_tool_arc.set_parent_permission(permission.clone());
        agent_tool_arc.set_parent_pending_permissions(pending_permissions.clone());
        agent_tool_arc.set_scene(scene.clone());
        if let Some(registry) = self.scene_registry.clone() {
            agent_tool_arc.set_scene_registry(registry);
        }
        agent_tool_arc.set_instruction_file(instruction_file.clone());
        if let Some(store) = self.history_store.clone() {
            agent_tool_arc.set_history_store(store);
        }
        agent_tool_arc.set_parent_session_id(session.session_id_str().to_string());
        agent_tool_arc.set_telemetry_handle(telemetry_handle.clone());
        // Only present when `settings.execution.team_enabled` registered the
        // team tools above.
        if let Some(ref t) = team_create_tool {
            // `team` sits below `runtime` and takes a plain channel; the
            // bridge keeps its progress events on the same stream as
            // everything else, sinks included.
            t.set_event_sender(event_tx.unbounded_bridge());
            t.set_telemetry_handle(telemetry_handle.clone());
        }

        // Capture feature flags before settings is moved into the Agent struct
        let cached_mc_enabled = settings.feature_flags.cached_microcompact;

        Ok((
            Agent {
                script_carriers: Arc::new(self.script_carriers),
                scene,
                model,
                tools,
                settings,
                config,
                agent_depth,
                permission,
                elicitation: self.elicitation.unwrap_or_else(|| {
                    Arc::new(crate::elicitation::ChannelElicitation::new(
                        event_tx.clone(),
                        pending_permissions.clone(),
                    ))
                }),
                prompt_registry: self
                    .prompt_registry
                    .unwrap_or_else(|| Arc::new(base::interface::prompt_registry::NoRegistrations)),
                prompt_assembler: self.prompt_assembler.unwrap_or_else(|| {
                    Arc::new(base::interface::prompt_assembler::DefaultAssembler)
                }),
                tool_middleware: Arc::new(self.tool_middleware),
                result_transformers: Arc::new(self.result_transformers),
                turn_policy,
                budget_policy,
                health: Arc::new(base::interface::health::HealthChecks::from_vec(
                    self.health_checks,
                )),
                environment,
                recovery_policy,
                model_interceptors: Arc::new(self.model_interceptors),
                memory_retriever: self
                    .memory_retriever
                    .unwrap_or_else(|| Arc::new(base::interface::memory_contracts::LlmRetriever)),
                retrieval_hooks: Arc::new(self.retrieval_hooks),
                pending_permissions,
                current_turn_cancel: Arc::new(std::sync::Mutex::new(CancellationToken::new())),
                steering: Default::default(),
                memory_store,
                session,
                session_state,
                perf: Arc::new(PerfCollector::new()),
                compactor,
                hooks,
                mcp,
                skills: skill_mgr_arc,
                commands: command_registry,
                telemetry_handle,
                current_turn_id: String::new(),
                frozen: self.frozen, // pre-seeded or lazily computed on first turn
                claude_md_content,
                claude_md_injected: false,
                pending_input: None,
                agent_type: self.agent_type,
                invoked_skills: Vec::new(),
                request_overhead_tokens: 0,
                last_had_tool_uses: true, // true → first turn scans for skills
                last_call_usage: None,
                in_plan_mode: false,
                plan_content: None,
                running_task_summaries: Vec::new(),
                permission_denial_count: Arc::new(std::sync::atomic::AtomicU32::new(0)),
                compact_warning_issued: false,
                tool_results_ever_cleared: false,
                mcp_shadow_warned: std::collections::HashSet::new(),
                time_based_mc_config: compaction::time_based_mc::TimeBasedMcConfig::default(),
                cached_mc: CachedMicroCompact::new(compaction::cached::CachedMcConfig {
                    enabled: cached_mc_enabled,
                    ..Default::default()
                }),
                compaction_state: compaction::reactive::CompactionState::default(),
                team_id: None,
                orphaned_permission: None,
                has_handled_orphaned_permission: false,
                messages_to_ack: Vec::new(),
                output_token_target: None,
                accumulated_output_tokens: 0,
                token_budget_continuation_count: 0,
                last_delta_tokens: 0,
                input_rx,
                event_tx,
                skip_warmup: self.skip_warmup,
                task_router: self.task_router,
            },
            event_rx,
            input_tx,
        ))
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> base::interface::settings::Settings {
        use base::interface::settings::*;
        Settings {
            model: ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: String::new(),
                auth_token: String::new(),
                model_name: "test".into(),
                max_tokens: 2000,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: None,
            },
            paths: PathSettings {
                user_data_dir: "/tmp".into(),
                global_data_dir: "/tmp".into(),
                local_data_dir: "/tmp".into(),
                scope: "code".into(),
            },
            execution: ExecutionSettings::default(),
            compaction: CompactionConfig::default(),
            sandbox: SandboxConfig::default(),
            plugins: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            session_dir: None,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            local_permission_rules: Vec::new(),
            allow_client_permission_override: false,
            telemetry_enabled: false,
            hooks_config: None,
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
            language: None,
            feature_flags: Default::default(),
            scripts: Vec::new(),
        }
    }

    #[test]
    fn builder_requires_scene() {
        assert!(Builder::new().build().is_err());
    }

    #[tokio::test]
    async fn run_emits_session_end_and_shutdown_signal_on_exit() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel))
            .settings(Arc::new(test_settings()))
            .tools(Arc::new(base::tool::InMemoryToolRegistry::new()))
            .permission(Arc::new(PromptingPermission))
            .telemetry_handle(telemetry::TelemetryHandle::new(tx))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        // Already-cancelled token: `run()` exits on the loop's first
        // iteration without ever needing the model or a real turn.
        let cancel = CancellationToken::new();
        cancel.cancel();
        agent.run(cancel).await;

        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            kinds.push(event.kind().to_string());
        }
        assert!(kinds.contains(&"session_end".to_string()), "got: {kinds:?}");
        assert!(
            kinds.contains(&"shutdown_signal".to_string()),
            "got: {kinds:?}"
        );
    }

    struct DummyModel;
    #[async_trait::async_trait]
    impl Model for DummyModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            unimplemented!("not exercised by these tests")
        }
    }

    /// Fails every call, the way an unreachable endpoint or a 5xx does.
    /// `DummyModel` can't stand in here — it panics rather than returning
    /// `Err`, so it never reaches the turn's model-error return path.
    struct FailingModel;
    #[async_trait::async_trait]
    impl Model for FailingModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            Err(base::interface::model::ModelError::Api {
                status: 503,
                message: "simulated endpoint failure".into(),
            })
        }
    }

    /// A turn that fails still has to leave the user's message on disk.
    ///
    /// Persistence used to run only at the successful tail of
    /// `run_user_turn`, so a session whose turns all failed wrote no file at
    /// all — `HistoryStore::append` creates it lazily — and `--continue`
    /// could not find it afterwards. That is exactly the session a user
    /// wants back: the one that just broke.
    #[tokio::test]
    async fn a_failed_turn_still_persists_the_users_message() {
        let cwd_tmp = tempfile::tempdir().unwrap();
        let projects_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                cwd_tmp.path(),
                history::path::HistoryRoots::under(projects_tmp.path()),
            )
            .await
            .expect("store should build"),
        );

        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(FailingModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let sid = base::session::SessionId::new().to_string();

        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .history_store(store.clone())
            .session_id(sid.clone())
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let outcome = agent
            .run_turn(
                "remember this".into(),
                "turn-1".into(),
                CancellationToken::new(),
            )
            .await;
        assert!(outcome.is_err(), "FailingModel should fail the turn");

        let parsed = base::session::SessionId::parse(&sid).unwrap();
        let entries = history::store::HistoryStore::load(&*store, parsed)
            .await
            .expect("the failed turn must have created a transcript on disk");
        assert!(
            entries
                .iter()
                .any(|e| format!("{:?}", e.entry).contains("remember this")),
            "the user's message must survive a failed turn, got: {entries:?}"
        );
    }

    /// `/model` has to reach the engine. `UpdateModel` fell through a
    /// catch-all `_ =>` arm in `process_turn`'s command match and did
    /// nothing, so a host could send the command, see no error, and keep
    /// talking to the old model. The match is exhaustive now, so a new
    /// `EngineCommand` fails the build instead of being swallowed.
    #[tokio::test]
    async fn update_model_command_changes_the_model_the_next_turn_uses() {
        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert_eq!(agent.settings.model.model_name, "test");

        agent
            .process_turn(
                InputMessage::System {
                    kind: EngineCommand::UpdateModel,
                    content: "claude-opus-5".into(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("UpdateModel should be handled");

        assert_eq!(agent.settings.model.model_name, "claude-opus-5");
    }

    fn dummy_agent_tool() -> Arc<crate::agent_tool::AgentTool> {
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(base::context::EngineConfig::defaults_for("test-model"));
        let tools = Arc::new(InMemoryToolRegistry::new());
        Arc::new(crate::agent_tool::AgentTool::new(model, config, tools))
    }

    #[tokio::test]
    async fn build_registers_mcp_tool_adapters_into_the_executable_tool_registry() {
        // Regression test: `build_tool_defs()` (turn.rs) advertises every
        // `mcp.tool_adapters()` entry to the model as a callable tool, but
        // `Builder::build()` previously only registered the 3 generic
        // List/Read/Dispatch MCP tools into `self.tools` — never the
        // individual per-tool adapters. The model would see
        // `mcp__test-server__do-thing` in its tool list and calling it would
        // always fail with "Tool not found" (`turn.rs::execute_tool_inner`
        // looks up `ctx.tools`, a clone of this same registry).
        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let mock_client = Arc::new(mcp::client::MockMcpClient::new(
            "test-server",
            vec![mcp::client::McpToolMeta {
                name: "do-thing".into(),
                description: Some("does a thing".into()),
                input_schema: serde_json::json!({"type": "object"}),
                hints: Default::default(),
            }],
        ));
        let mut mcp_manager = McpManager::from_clients(vec![mock_client]);
        mcp_manager.refresh_tools().await;
        assert_eq!(
            mcp_manager.tool_adapters().len(),
            1,
            "sanity check: mock client should have produced one adapter"
        );

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .mcp_manager(mcp_manager)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert!(
            agent.tools.get("mcp__test-server__do-thing").is_some(),
            "MCP tool adapter should be registered in the executable tool registry, found: {:?}",
            agent.tools.names()
        );
    }

    /// A scene's whitelist decides what the session *has*, not only what it
    /// advertises: dispatch and `ToolSearch` read the same registry.
    #[tokio::test]
    async fn build_leaves_out_of_the_registry_every_tool_the_scene_excludes() {
        async fn mcp_with_one_tool() -> McpManager {
            let client = Arc::new(mcp::client::MockMcpClient::new(
                "srv",
                vec![mcp::client::McpToolMeta {
                    name: "do-thing".into(),
                    description: None,
                    input_schema: serde_json::json!({"type": "object"}),
                    hints: Default::default(),
                }],
            ));
            let mut m = McpManager::from_clients(vec![client]);
            m.refresh_tools().await;
            m
        }
        let build = |scene: Arc<dyn AgentScene>, mcp: McpManager| {
            let template = Arc::new(InMemoryToolRegistry::new());
            tools::register_builtin_tools(&template);
            Builder::new()
                .scene(scene)
                .model(Arc::new(DummyModel))
                .settings(Arc::new(test_settings()))
                .tools(template)
                .mcp_manager(mcp)
                .skip_warmup(true)
                .build()
                .expect("build should succeed")
                .0
        };

        let chat = build(
            Arc::new(scene::scene::chat::ChatScene),
            mcp_with_one_tool().await,
        );
        for absent in ["Bash", "Write", "Edit", "Agent"] {
            assert!(chat.tools.get(absent).is_none(), "{absent} in chat");
        }
        for present in ["Read", "ToolSearch", "mcp__srv__do-thing"] {
            assert!(
                chat.tools.get(present).is_some(),
                "{present} missing in chat"
            );
        }

        struct ReadOnly;
        impl AgentScene for ReadOnly {
            fn id(&self) -> &str {
                "read-only"
            }
            fn name(&self) -> &str {
                "read-only"
            }
            fn description(&self) -> &str {
                ""
            }
            fn build_system_prompt(
                &self,
                ctx: &base::interface::scene::ScenePromptContext,
            ) -> Vec<base::prompt::PromptBlock> {
                scene::scene::chat::ChatScene.build_system_prompt(ctx)
            }
            fn tools(&self) -> Vec<String> {
                vec!["Read".into()]
            }
            fn token_budget(&self) -> base::interface::scene::TokenBudget {
                scene::scene::chat::ChatScene.token_budget()
            }
        }
        let narrow = build(Arc::new(ReadOnly), mcp_with_one_tool().await);
        assert_eq!(narrow.tools.names(), vec!["Read".to_string()]);
    }

    /// Regression: `PluginHost::tools()` had no production caller at all.
    /// Plugin tools reached a session only through `PluginScene::extra_tools`,
    /// so a plugin that shipped tools without owning a scene — which the
    /// manifest fully allows — registered them nowhere and the model never
    /// saw them.
    #[tokio::test]
    async fn build_registers_the_tools_installed_plugins_export() {
        struct OnePluginTool(Arc<dyn base::tool::Tool>);
        impl crate::plugin_host::PluginHost for OnePluginTool {
            fn tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
                vec![self.0.clone()]
            }
            fn mcp_servers(&self) -> Vec<(String, serde_json::Value)> {
                Vec::new()
            }
            fn hook_configs(&self) -> Vec<(hooks::config::HookEvent, hooks::config::HookConfig)> {
                Vec::new()
            }
            fn scenes(&self) -> Vec<Arc<dyn AgentScene>> {
                Vec::new()
            }
            fn agent_types(&self) -> Vec<crate::agent_tool::AgentTypeDefinition> {
                Vec::new()
            }
        }

        struct Exported;
        #[async_trait::async_trait]
        impl base::tool::Tool for Exported {
            fn name(&self) -> &str {
                "plugin__demo__do_thing"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(
                &self,
                _: serde_json::Value,
                _: base::tool::ToolContext,
                _: base::tool::ProgressSender,
            ) -> Result<base::tool::ToolResult, base::error::ToolError> {
                Ok(base::tool::ToolResult::text("done"))
            }
        }

        let exported: Arc<dyn base::tool::Tool> = Arc::new(Exported);
        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .plugin_host(Arc::new(OnePluginTool(exported)))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert!(
            agent.tools.get("plugin__demo__do_thing").is_some(),
            "a plugin's exported tool must be in the registry the turn loop \
             dispatches against, found: {:?}",
            agent.tools.names()
        );
    }

    /// Regression: `McpManager::set_elicitation_callback` existed with zero
    /// production callers — an MCP tool result containing an elicitation URL
    /// silently never reached the hook system no matter what hooks were
    /// configured. This exercises the real path end to end: an MCP tool call
    /// returns a result with an `elicitation://` URL in it, and a configured
    /// `Elicitation` hook must actually fire.
    #[tokio::test]
    async fn build_wires_mcp_elicitation_urls_to_the_elicitation_hook() {
        // Observed through an injected hook executor, in this process. The
        // previous version configured a `command` hook that appended to a file
        // and polled for it, which measured how long the machine took to fork
        // a shell under a full `cargo test --workspace` — not what this test is
        // about, and it timed out at sixty seconds saying so. An `http` hook is
        // not an option either: the SSRF guard blocks loopback, correctly.
        struct RecordingPromptExecutor {
            fired: tokio::sync::mpsc::Sender<serde_json::Value>,
        }

        #[async_trait::async_trait]
        impl hooks::runner::PromptHookExecutor for RecordingPromptExecutor {
            async fn execute(
                &self,
                _prompt: &str,
                _model: Option<&str>,
                payload: &hooks::HookInput,
            ) -> Result<String, String> {
                let _ = self
                    .fired
                    .send(payload.tool_input.clone().unwrap_or_default())
                    .await;
                Ok("{}".to_string())
            }
        }

        let (fired_tx, mut fired_rx) = tokio::sync::mpsc::channel(1);
        let mut hooks_settings: hooks::HooksSettings = Default::default();
        hooks_settings.insert(
            hooks::HookEvent::Elicitation,
            vec![hooks::config::HookConfig::Prompt {
                prompt: "an elicitation happened".into(),
                timeout: None,
                model: None,
            }],
        );
        let runner = Arc::new(
            hooks::HookRunner::new(hooks_settings)
                .with_prompt_executor(Arc::new(RecordingPromptExecutor { fired: fired_tx })),
        );

        let dir = tempfile::tempdir().unwrap();
        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let mock_client = Arc::new(mcp::client::MockMcpClient::new(
            "test-server",
            vec![mcp::client::McpToolMeta {
                name: "authorize".into(),
                description: Some("needs out-of-band authorization".into()),
                input_schema: serde_json::json!({"type": "object"}),
                hints: Default::default(),
            }],
        ));
        mock_client.push_response(
            "authorize",
            mcp::client::McpCallResult {
                content: vec![mcp::client::McpContent::Text(
                    "Please authorize at elicitation://example.com/auth/123".into(),
                )],
                is_error: false,
                meta: None,
            },
        );
        let mut mcp_manager = McpManager::from_clients(vec![mock_client]);
        mcp_manager.refresh_tools().await;

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .hooks(runner)
            .mcp_manager(mcp_manager)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert!(agent.hooks().has_hooks_for(hooks::HookEvent::Elicitation));

        let tool = agent
            .tools
            .get("mcp__test-server__authorize")
            .expect("MCP tool adapter registered");
        let ctx = base::tool::ToolContext::for_test(dir.path().to_path_buf());
        let progress = base::tool::ProgressSender::noop("test-tool-use");
        tool.call(serde_json::json!({}), ctx, progress)
            .await
            .expect("mock tool call succeeds");

        // The callback dispatches through `tokio::spawn`, so this arrives
        // after `call` returns. Waiting on the dispatch itself means a slow
        // machine makes the test slower and never makes it fail.
        let payload = tokio::time::timeout(std::time::Duration::from_secs(30), fired_rx.recv())
            .await
            .expect(
                "an MCP tool result containing an elicitation URL should have fired the \
                 Elicitation hook",
            )
            .expect("the executor sends before returning");
        assert_eq!(
            payload["url"],
            serde_json::json!("elicitation://example.com/auth/123"),
            "the hook must be told which URL, got: {payload}"
        );
        assert_eq!(payload["server_name"], serde_json::json!("test-server"));
    }

    /// P2: `Builder::skill_catalog` must be used as-is, with `build()` never
    /// falling back to its own disk scan — a skill that only exists in the
    /// injected catalog (never written to `settings.paths`' skill
    /// directories) must still resolve as a slash command.
    #[tokio::test]
    async fn build_uses_the_injected_skill_catalog_instead_of_scanning_disk() {
        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let catalog = skills::manager::SkillManager::new();
        catalog.register_bundled(base::frozen::SkillEntry {
            name: "p2-injected-only".into(),
            description: "only exists in the injected catalog".into(),
            source: base::frozen::SkillSource::User,
            path: std::path::PathBuf::from("(test:p2-injected-only)"),
            ..Default::default()
        });
        let catalog = Arc::new(catalog);

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .skill_catalog(catalog)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert!(
            agent.commands.resolve("p2-injected-only").is_some(),
            "the injected catalog's skill should resolve as a slash command"
        );
    }

    /// P3-12's acceptance: a fourth skill source, registered from outside the
    /// engine, is a source in full — its skills list *and* expand, without
    /// `SkillManager` learning anything about where they came from.
    #[tokio::test]
    async fn a_host_can_register_a_fourth_skill_source() {
        use base::interface::skill_provider::StaticSkills;

        let source = StaticSkills::new(
            "company-bundle",
            vec![base::frozen::SkillEntry {
                name: "file-a-ticket".into(),
                description: "Open a ticket in the company tracker".into(),
                source: base::frozen::SkillSource::Plugin,
                // Deliberately not a readable path: the source owns the body.
                path: std::path::PathBuf::from("(company:file-a-ticket)"),
                ..Default::default()
            }],
        )
        .with_body("file-a-ticket", "Ask for a summary, then POST it.");

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .skill_provider(Arc::new(source))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let listed = agent.skills().get("file-a-ticket");
        assert!(
            listed.is_some(),
            "the host's source did not reach the catalog: {:?}",
            agent
                .skills()
                .list()
                .iter()
                .map(|s| &s.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            agent.skills().get_skill_content("file-a-ticket").as_deref(),
            Some("Ask for a summary, then POST it."),
            "a skill that lists but cannot be expanded is not a source"
        );
    }

    /// P3-12's other half: the instruction file is one implementation of a
    /// contract, so a host that already holds its conventions needs no file.
    #[tokio::test]
    async fn a_host_can_supply_the_standing_instructions_without_a_file() {
        use base::interface::instruction_provider::InlineInstructions;

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .instruction_provider(Arc::new(InlineInstructions::new(
                "service://conventions",
                "Always run the linter.",
            )))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert_eq!(
            agent.claude_md_content.as_deref(),
            Some("Always run the linter.")
        );
    }

    /// Regression test: `TeamCreateTool`/`TeamDeleteTool` (`crates/team`)
    /// were never registered by `Builder::build()` — the coordinator's
    /// bounded-concurrency stage dispatch was fully implemented and tested
    /// in isolation, but the model had no way to reach it in a real session.
    #[tokio::test]
    async fn build_registers_team_create_and_delete_tools_when_enabled() {
        let mut settings = test_settings();
        settings.execution.team_enabled = true;
        let settings = Arc::new(settings);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert!(
            agent.tools.get("TeamCreate").is_some(),
            "TeamCreate should be registered, found: {:?}",
            agent.tools.names()
        );
        assert!(
            agent.tools.get("TeamDelete").is_some(),
            "TeamDelete should be registered, found: {:?}",
            agent.tools.names()
        );
        assert!(
            agent.tools.get("TeamList").is_some(),
            "TeamList should be registered, found: {:?}",
            agent.tools.names()
        );
    }

    /// `team_enabled` defaults to `false` — a session built from plain
    /// `test_settings()` (which uses `ExecutionSettings::default()`) must
    /// not see either team tool at all, not just be unable to use it.
    #[tokio::test]
    async fn build_does_not_register_team_tools_by_default() {
        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert!(
            agent.tools.get("TeamCreate").is_none(),
            "TeamCreate must not be registered when team_enabled is false, found: {:?}",
            agent.tools.names()
        );
        assert!(
            agent.tools.get("TeamDelete").is_none(),
            "TeamDelete must not be registered when team_enabled is false, found: {:?}",
            agent.tools.names()
        );
        assert!(
            agent.tools.get("TeamList").is_none(),
            "TeamList must not be registered when team_enabled is false, found: {:?}",
            agent.tools.names()
        );
    }

    /// The depth bound has to be asserted on the registry the sub-agent
    /// actually runs with, which is the one `build()` creates — not on what
    /// `AgentTool::resolve_tools` returned on the way in.
    ///
    /// `resolve_tools_full_access_excludes_agent_itself` (agent_tool.rs)
    /// checks the latter and passed throughout the period when a sub-agent
    /// could delegate without limit: `build()` re-registered "Agent" into its
    /// fresh per-session registry regardless, so the filter it asserts had no
    /// effect on the running agent. Delegation chains recursed until the
    /// process hit `fatal runtime error: stack overflow`.
    #[tokio::test]
    async fn build_withholds_the_agent_tool_at_the_delegation_depth_limit() {
        let max_depth =
            base::context::EngineConfig::defaults_for("test".to_string()).max_agent_depth;

        let build_at = |depth: u32| {
            let settings = Arc::new(test_settings());
            let model: Arc<dyn Model> = Arc::new(DummyModel);
            let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
            Builder::new()
                .scene(scene)
                .model(model)
                .settings(settings)
                .agent_depth(depth)
                .skip_warmup(true)
                .build()
                .expect("build should succeed")
        };

        for depth in 0..max_depth {
            let (agent, _rx, _tx) = build_at(depth);
            assert!(
                agent.tools.get("Agent").is_some(),
                "depth {depth} is below the limit {max_depth} and should still be able to delegate"
            );
            assert_eq!(agent.agent_depth, depth);
        }

        let (agent, _rx, _tx) = build_at(max_depth);
        assert!(
            agent.tools.get("Agent").is_none(),
            "an agent at the depth limit ({max_depth}) must not be handed the Agent tool, \
             found: {:?}",
            agent.tools.names()
        );
    }

    /// The coding scene's deferred policy has to reach the tools registered
    /// *after* the seeding copy is built, and has to actually collapse their
    /// schemas.
    ///
    /// Two separate defects made the policy a near-no-op before, and this
    /// asserts the outcome rather than either mechanism:
    ///
    /// - `apply_deferred_policy` skipped any tool whose own `is_deferred()`
    ///   already reported `true`. Two dozen built-ins declare it, and the
    ///   wrapper is the only thing that collapses a schema — so those tools
    ///   were announced as deferred while still shipping full schemas.
    /// - The policy ran once, on the seeding registry, before `Agent`,
    ///   `Skill`, `Cron*`, `Worktree*`, `WebSearch` and `Team*` were
    ///   registered, exempting exactly the tools a scene most wants deferred.
    #[tokio::test]
    async fn coding_scene_defers_everything_outside_its_resident_set() {
        let (agent, _rx, _tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .tools({
                let t = Arc::new(InMemoryToolRegistry::new());
                tools::register_builtin_tools(&t);
                t
            })
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let resident: Vec<String> = agent
            .tools
            .list()
            .iter()
            .filter(|t| !t.is_deferred())
            .map(|t| t.name().to_string())
            .collect();

        let mut sorted = resident.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                "Agent",
                "AskUserQuestion",
                "Bash",
                "Edit",
                "Glob",
                "Grep",
                "Read",
                "Skill",
                "ToolSearch",
                "Write",
            ],
            "resident set drifted from CodingScene::RESIDENT_TOOLS"
        );

        // Registered after the seeding copy — the set the old single-pass
        // policy could not reach.
        for late in ["CronCreate", "EnterWorktree", "WebSearch"] {
            let t = agent.tools.get(late).expect(late);
            assert!(t.is_deferred(), "{late} must be deferred");
            let schema = serde_json::to_string(&t.input_schema()).unwrap();
            assert!(
                schema.len() < 400,
                "{late} still ships a full schema ({} bytes) — reported as \
                 deferred but never wrapped",
                schema.len()
            );
        }

        // The way back to every deferred schema must never itself be deferred.
        assert!(!agent.tools.get("ToolSearch").unwrap().is_deferred());

        // Duplicate entries would shadow the wrapped instance, since `get`
        // returns the first name match.
        let mut names = agent.tools.names();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(
            before,
            names.len(),
            "registry contains duplicate tool names"
        );
    }

    /// Every tool a coding session registers must appear in
    /// `CodingScene::ALL_DEFERRABLE_TOOLS`, so adding a tool is a deliberate
    /// choice between resident and deferred rather than a silent default.
    ///
    /// The list is spelled out in the scene because `deferred_tools()` is a
    /// policy with no registry to consult — it answers before the session's
    /// registry exists. Nothing kept the two in sync, so a newly registered
    /// tool would quietly join the resident set and its schema would ride
    /// along on every request unnoticed. This is the sync check.
    #[tokio::test]
    async fn every_registered_tool_has_a_deferral_policy() {
        let scene = scene::scene::coding::CodingScene;
        let (agent, _rx, _tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .tools({
                let t = Arc::new(InMemoryToolRegistry::new());
                tools::register_builtin_tools(&t);
                t
            })
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        // Resident ∪ deferred, as the scene understands them.
        let deferred = scene.deferred_tools();
        let known: std::collections::HashSet<&str> = scene::scene::coding::RESIDENT_TOOLS
            .iter()
            .copied()
            .chain(deferred.iter().map(|s| s.as_str()))
            .collect();

        let unclassified: Vec<String> = agent
            .tools
            .names()
            .into_iter()
            .filter(|n| !known.contains(n.as_str()))
            .collect();

        assert!(
            unclassified.is_empty(),
            "these tools are registered but absent from CodingScene's \
             ALL_DEFERRABLE_TOOLS, so they silently stay resident: {unclassified:?}"
        );
    }

    /// A scene can contribute a tool the host's registry never had, and it is
    /// subject to the scene's own deferred policy like any other.
    #[tokio::test]
    async fn scene_extra_tools_are_registered_and_follow_the_deferred_policy() {
        struct SceneWithExtra;
        impl AgentScene for SceneWithExtra {
            fn id(&self) -> &str {
                "extra"
            }
            fn name(&self) -> &str {
                "Extra"
            }
            fn description(&self) -> &str {
                "scene contributing its own tool"
            }
            fn build_system_prompt(
                &self,
                _: &base::interface::scene::ScenePromptContext,
            ) -> Vec<base::interface::prompt::PromptBlock> {
                vec![]
            }
            fn tools(&self) -> Vec<String> {
                vec![]
            }
            fn token_budget(&self) -> base::interface::scene::TokenBudget {
                base::interface::scene::TokenBudget {
                    compact_threshold: 150_000,
                    compact_keep_recent: 20,
                }
            }
            fn extra_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
                vec![Arc::new(ProbeTool), Arc::new(SecondProbeTool)]
            }
            fn deferred_tools(&self) -> Vec<String> {
                vec!["SecondProbe".into()]
            }
        }

        let (agent, _rx, _tx) = Builder::new()
            .scene(Arc::new(SceneWithExtra) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let probe = agent.tools.get("Probe").expect("scene tool registered");
        assert!(!probe.is_deferred(), "not named in deferred_tools");

        let second = agent.tools.get("SecondProbe").expect("registered");
        assert!(
            second.is_deferred(),
            "a scene's own tool must obey its own deferred policy"
        );
    }

    /// Shadowing an existing tool from a scene has to fail loudly: the
    /// registry returns the first name match, so the scene's version would
    /// never be reached and every call site would silently keep the original.
    #[tokio::test]
    async fn scene_extra_tool_colliding_with_a_registered_name_fails_the_build() {
        struct Shadower;
        impl AgentScene for Shadower {
            fn id(&self) -> &str {
                "shadow"
            }
            fn name(&self) -> &str {
                "Shadow"
            }
            fn description(&self) -> &str {
                "scene shadowing a built-in"
            }
            fn build_system_prompt(
                &self,
                _: &base::interface::scene::ScenePromptContext,
            ) -> Vec<base::interface::prompt::PromptBlock> {
                vec![]
            }
            fn tools(&self) -> Vec<String> {
                vec![]
            }
            fn token_budget(&self) -> base::interface::scene::TokenBudget {
                base::interface::scene::TokenBudget {
                    compact_threshold: 150_000,
                    compact_keep_recent: 20,
                }
            }
            fn extra_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
                vec![Arc::new(ProbeTool)]
            }
        }

        let seeded = Arc::new(InMemoryToolRegistry::new());
        seeded.register(Arc::new(ProbeTool));

        let err = match Builder::new()
            .scene(Arc::new(Shadower) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .tools(seeded)
            .skip_warmup(true)
            .build()
        {
            Ok(_) => panic!("a colliding scene tool must fail the build"),
            Err(e) => e,
        };
        assert!(
            format!("{err}").contains("Probe"),
            "error should name the colliding tool, got: {err}"
        );
    }

    /// The collision check has to see the *engine-state* tools too, not just
    /// whatever registry the caller seeded.
    ///
    /// `Agent`, `Skill`, `WebSearch`, `Cron*` and `Worktree*` are registered
    /// by `build()` itself. While the check ran before them, a scene could
    /// claim one of those names, pass against an empty slot, and then win
    /// every lookup regardless — `get` returns the first match and the
    /// engine's own instance is appended afterwards. The check ran, and the
    /// exact shadowing it exists to prevent happened anyway.
    #[tokio::test]
    async fn scene_extra_tool_cannot_claim_an_engine_state_tool_name() {
        struct ClaimsAgent;
        impl AgentScene for ClaimsAgent {
            fn id(&self) -> &str {
                "claims-agent"
            }
            fn name(&self) -> &str {
                "ClaimsAgent"
            }
            fn description(&self) -> &str {
                "scene claiming an engine-registered name"
            }
            fn build_system_prompt(
                &self,
                _: &base::interface::scene::ScenePromptContext,
            ) -> Vec<base::interface::prompt::PromptBlock> {
                vec![]
            }
            fn tools(&self) -> Vec<String> {
                vec![]
            }
            fn token_budget(&self) -> base::interface::scene::TokenBudget {
                base::interface::scene::TokenBudget {
                    compact_threshold: 150_000,
                    compact_keep_recent: 20,
                }
            }
            fn extra_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
                struct FakeAgent;
                #[async_trait::async_trait]
                impl base::tool::Tool for FakeAgent {
                    fn name(&self) -> &str {
                        "Agent"
                    }
                    fn input_schema(&self) -> serde_json::Value {
                        serde_json::json!({"type": "object"})
                    }
                    async fn call(
                        &self,
                        _: serde_json::Value,
                        _: base::tool::ToolContext,
                        _: base::tool::ProgressSender,
                    ) -> Result<base::tool::ToolResult, base::error::ToolError>
                    {
                        Ok(base::tool::ToolResult::text("hijacked"))
                    }
                }
                vec![Arc::new(FakeAgent)]
            }
        }

        let err = match Builder::new()
            .scene(Arc::new(ClaimsAgent) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .skip_warmup(true)
            .build()
        {
            Ok(_) => panic!("a scene must not be able to claim the `Agent` name"),
            Err(e) => e,
        };
        assert!(
            format!("{err}").contains("Agent"),
            "error should name the colliding tool, got: {err}"
        );
    }

    /// W-1 regression: `WebSearchTool` was never registered anywhere, while
    /// `ChatScene::tools()` and `ResearchScene::tools()` both list
    /// `"WebSearch"`. A scene whitelist is intersected with the registry, so
    /// the failure mode was silent — no error, the model just never saw the
    /// tool and the Research scenario had no way to search the web.

    #[tokio::test]
    async fn build_registers_web_search_and_the_other_bridged_tools() {
        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        for expected in [
            "WebSearch",
            "CronCreate",
            "CronDelete",
            "CronList",
            "EnterWorktree",
            "ExitWorktree",
        ] {
            assert!(
                agent.tools.get(expected).is_some(),
                "{expected} should be registered, found: {:?}",
                agent.tools.names()
            );
        }

        // Reachable from the two scenes that ask for it: whitelist ∩ registry.
        for scene in [
            Arc::new(scene::scene::chat::ChatScene) as Arc<dyn AgentScene>,
            Arc::new(scene::scene::research::ResearchScene),
        ] {
            assert!(
                scene.tools().contains(&"WebSearch".to_string())
                    && agent.tools.get("WebSearch").is_some(),
                "WebSearch must be both whitelisted by {} and present in the registry",
                scene.id()
            );
        }
    }

    /// W-5: a scene's `deferred_tools()` must strip the schema from the
    /// advertised entry while leaving the tool named, discoverable via
    /// `ToolSearch`, and callable.
    #[tokio::test]
    async fn build_applies_the_scenes_deferred_tool_policy() {
        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);

        // Registry seeded the way a daemon does it.
        let template = Arc::new(InMemoryToolRegistry::new());
        tools::register_builtin_tools(&template);
        let full_grep_schema = template.get("Grep").unwrap().input_schema();
        assert!(
            full_grep_schema["properties"].as_object().unwrap().len() > 1,
            "precondition: Grep normally ships a real schema"
        );

        let (agent, _rx, _tx) = Builder::new()
            .scene(Arc::new(scene::scene::chat::ChatScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(settings)
            .tools(template)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let grep = agent.tools.get("Grep").expect("Grep still registered");
        assert!(grep.is_deferred(), "ChatScene defers Grep");
        assert_eq!(
            grep.input_schema()["properties"],
            serde_json::json!({}),
            "deferred tool must not carry its real schema"
        );
        assert!(
            grep.short_description().is_some_and(|d| !d.is_empty()),
            "deferred tool must stay described, or ToolSearch lists a bare name"
        );

        // Read is not in the deferred list — untouched.
        let read = agent.tools.get("Read").unwrap();
        assert!(!read.is_deferred());
        assert!(!read.input_schema()["properties"]
            .as_object()
            .unwrap()
            .is_empty());

        // And the scene whitelists ToolSearch, without which a deferred tool
        // could never be activated.
        assert!(scene::scene::chat::ChatScene
            .tools()
            .contains(&"ToolSearch".to_string()));

        // The session's ToolSearch must see the session's deferred wrappers,
        // not the template's un-wrapped originals — i.e. `select:Grep` has to
        // actually find something, or the name is advertised with no way to
        // get the schema back.
        let found = agent
            .tools
            .get("ToolSearch")
            .expect("ToolSearch rebound onto the session registry")
            .call(
                serde_json::json!({"query": "select:Grep"}),
                base::tool::ToolContext::for_test("/tmp".into()),
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match &found.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("Grep") && !s.contains("No matches"), "{s}")
            }
            _ => panic!("expected text"),
        }
    }

    /// Regression test for the shared-registry duplicate/stale bug: two
    /// sessions built from the *same* shared `Arc<InMemoryToolRegistry>`
    /// (the daemon `SessionPool.tools` scenario) must each end up with
    /// their own independent registry — not both mutating the one they were
    /// handed. Before the fix, `Builder::build()` pushed `Skill`/`Agent`/
    /// `TeamCreate`/etc. straight into the caller-supplied `Arc`, so a
    /// second session's build would silently accumulate duplicate entries
    /// in the *first* session's (and the pool's) registry too —
    /// `InMemoryToolRegistry::get` returns the *first* match, so the
    /// oldest session would keep dispatching to its own original tool
    /// instance forever while newer sessions' `build_tool_defs` (which
    /// dedups via a `BTreeMap`, keeping the *last* entry) advertised a
    /// different instance's description to the model.
    #[tokio::test]
    async fn each_session_gets_an_independent_tool_registry_not_a_shared_mutated_one() {
        let mut settings = test_settings();
        settings.execution.team_enabled = true;
        let settings = Arc::new(settings);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        // Simulates `SessionPool.tools`: built once, handed to every session's Builder.
        let shared_template = Arc::new(InMemoryToolRegistry::new());
        let before_count = shared_template.list().len();

        let (agent1, _rx1, _tx1) = Builder::new()
            .scene(scene.clone())
            .model(model.clone())
            .settings(settings.clone())
            .tools(shared_template.clone())
            .skip_warmup(true)
            .build()
            .expect("first build should succeed");

        let (agent2, _rx2, _tx2) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .tools(shared_template.clone())
            .skip_warmup(true)
            .build()
            .expect("second build should succeed");

        assert_eq!(
            shared_template.list().len(),
            before_count,
            "the shared template registry handed to every session's Builder must not accumulate \
             per-session tools (Skill/Agent/TeamCreate/...) — it stayed at {before_count} entries \
             before either build and must still be exactly that many after both"
        );
        assert_eq!(
            agent1.tools.list().iter().filter(|t| t.name() == "TeamCreate").count(),
            1,
            "session 1's own registry should have exactly one TeamCreate, not accumulated duplicates"
        );
        assert_eq!(
            agent2.tools.list().iter().filter(|t| t.name() == "TeamCreate").count(),
            1,
            "session 2's own registry should have exactly one TeamCreate, not accumulated duplicates"
        );
    }

    /// Regression test: `SessionManager.session_memory` was fully
    /// implemented (`crates/session/src/session_memory.rs`) but
    /// `Builder::build()` never called `with_session_memory` — the sidecar
    /// was dead code, and `turn.rs`'s staleness-reminder branch never ran.
    /// A session with a `HistoryStore` should get a sidecar handle whose
    /// path lives under `global_data_dir/sessions/<id>/session_memory.md`.
    #[tokio::test]
    async fn build_wires_session_memory_when_history_store_present() {
        let cwd_tmp = tempfile::tempdir().unwrap();
        let projects_tmp = tempfile::tempdir().unwrap();
        let store = history::store::JsonlHistoryStore::with_roots(
            cwd_tmp.path(),
            history::path::HistoryRoots::under(projects_tmp.path()),
        )
        .await
        .expect("store should build");

        let mut settings = test_settings();
        settings.paths.global_data_dir = projects_tmp.path().to_path_buf();
        let settings = Arc::new(settings);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let sid = base::session::SessionId::new().to_string();

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .history_store(Arc::new(store))
            .session_id(sid.clone())
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let sm = agent
            .session
            .session_memory
            .as_ref()
            .expect("session_memory should be wired when a history_store is set");
        // Must land in the same sidecar directory the rest of the history
        // crate reads and writes (metadata, input history, prompt state).
        // Both derive from the settings' `global_data_dir` now; when they
        // came from different roots the memory file was written where
        // nothing would ever look for it.
        let expected = history::path::session_memory_file(
            &history::path::HistoryRoots::under(projects_tmp.path()).sessions,
            &base::session::SessionId::parse(&sid).unwrap(),
        );
        assert_eq!(sm.path(), expected);
    }

    /// **N-1 regression, and the most important test in this file.**
    ///
    /// Every tool the model can actually call must be resolvable by the
    /// session's `Permission` handler. It was not: the handler is built from
    /// whatever registry the caller passes (in a daemon, the pool's template
    /// of self-contained built-ins), while `build()` creates a *fresh*
    /// per-session registry and registers `Skill`, `Agent`, `Team*`,
    /// `WebSearch`, `EnterWorktree`/`ExitWorktree`, `Cron*`, `TaskStop`/
    /// `TaskOutput`, `Import` and every `mcp__*` adapter into that one. Every
    /// name in that list hit `RuleSetPermission`'s "unknown tool" branch and
    /// was permitted unchecked — the whole MCP surface included.
    ///
    /// This asserts the invariant directly rather than the fix: whatever the
    /// registry ends up holding, the permission handler can see it. A future
    /// tool registered in `build()` is covered automatically.
    #[tokio::test]
    async fn every_registered_tool_is_visible_to_the_permission_handler() {
        use base::interface::permission::{Permission, PermissionOutcome};

        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        // Exactly what a daemon does: seed a template with the self-contained
        // built-ins and build the handler over *that*.
        let template = Arc::new(InMemoryToolRegistry::new());
        tools::register_builtin_tools(&template);
        let permission: Arc<dyn Permission> =
            Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
                Arc::new(permissions::gate::PermissionGate::empty()),
                template.clone(),
                base::permission::PermissionMode::DontAsk,
            ));

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .tools(template.clone())
            .permission(permission.clone())
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        // Sanity: `build()` really did register tools the template lacks —
        // otherwise this test would pass vacuously.
        let session_names: Vec<String> = agent
            .tools
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(
            session_names.iter().any(|n| n == "Skill"),
            "expected build() to register engine-state tools; got {session_names:?}"
        );
        assert!(
            template.get("Skill").is_none(),
            "the template must stay untouched — `build()` populates a copy, which is \
             precisely why the handler has to be rebound to that copy"
        );

        // Put the *session* into `DontAsk` — the handler reads the live
        // session state, not its construction-time fallback (that is N-7's
        // fix, and relying on it here keeps the two bindings honest).
        agent
            .session_state
            .set_permission_mode(base::permission::PermissionMode::DontAsk);

        // The invariant: nothing the model can call is unknown to permissions.
        // `DontAsk` with no rules denies everything it *can* resolve, so a
        // `Prompt` here means "unresolvable", which is the bug.
        for name in &session_names {
            let outcome = permission
                .check(
                    name,
                    &serde_json::json!({}),
                    std::path::Path::new("/tmp"),
                    "s1",
                )
                .await;
            assert!(
                !matches!(outcome, PermissionOutcome::Prompt { .. }),
                "`{name}` is not resolvable by the permission handler — it would have \
                 executed without any check. Got {outcome:?}"
            );
        }
    }

    /// N-7 regression: the permission handler must read the session's *live*
    /// mode, so `EnterPlanMode` moves the gate. It used to construct a
    /// throwaway `SessionState` per check, which froze the mode at session
    /// construction and made plan mode decorative.
    #[tokio::test]
    async fn entering_plan_mode_moves_the_permission_gate() {
        use base::interface::permission::{Permission, PermissionOutcome};

        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let template = Arc::new(InMemoryToolRegistry::new());
        tools::register_builtin_tools(&template);
        let permission: Arc<dyn Permission> =
            Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
                Arc::new(permissions::gate::PermissionGate::empty()),
                template.clone(),
                base::permission::PermissionMode::Default,
            ));

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .tools(template.clone())
            .permission(permission.clone())
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let write = serde_json::json!({"file_path": "/tmp/x.txt", "content": "hi"});
        assert!(
            matches!(
                permission
                    .check("Write", &write, std::path::Path::new("/tmp"), "s1")
                    .await,
                PermissionOutcome::Permit
            ),
            "an in-project write is allowed in Default mode"
        );

        agent
            .session_state
            .set_permission_mode(base::permission::PermissionMode::Plan);

        assert!(
            matches!(
                permission
                    .check("Write", &write, std::path::Path::new("/tmp"), "s1")
                    .await,
                PermissionOutcome::Deny { .. }
            ),
            "plan mode must deny a write, even one the tool allows itself"
        );
    }

    /// Sibling of the above: no `HistoryStore` → no sidecar. An in-memory-only
    /// session's id doesn't outlive the process, so a sidecar for it would
    /// just be orphaned disk state nothing ever reads back.
    #[tokio::test]
    async fn build_skips_session_memory_without_history_store() {
        let settings = Arc::new(test_settings());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(settings)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert!(agent.session.session_memory.is_none());
    }

    /// Test tool: always succeeds, records nothing — just needs to exist so
    /// the mock model's `ToolUse` call has something real to dispatch to.
    struct ProbeTool;
    #[async_trait::async_trait]
    impl base::tool::Tool for ProbeTool {
        fn name(&self) -> &str {
            "Probe"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult::text("probe-ok"))
        }
    }

    /// Second stand-in, so a scene can contribute two tools and have the
    /// deferred policy apply to one of them but not the other.
    struct SecondProbeTool;
    #[async_trait::async_trait]
    impl base::tool::Tool for SecondProbeTool {
        fn name(&self) -> &str {
            "SecondProbe"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult::text("second-probe-ok"))
        }
    }

    /// Test tool that never finishes on its own — it parks until the turn's
    /// token fires. Stands in for whatever a user presses Esc on: a long
    /// command, a slow subprocess, a model that won't stop talking.
    struct HangingTool;
    #[async_trait::async_trait]
    impl base::tool::Tool for HangingTool {
        fn name(&self) -> &str {
            "Hang"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            ctx.cancel.cancelled().await;
            Err(base::error::ToolError::Cancelled)
        }
    }

    /// Mock model: 1st call emits a `Probe` tool call, every later call just
    /// ends the turn normally. Used to prove the discontinue path actually
    /// stops the outer loop — if it didn't, the agent would call the model
    /// a 2nd time asking what to do with the tool result.
    struct ToolThenStopModel {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tool: &'static str,
    }
    #[async_trait::async_trait]
    impl Model for ToolThenStopModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let call_no = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events: Vec<
                Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
            > = if call_no == 0 {
                vec![
                    Ok(base::interface::model::ModelEvent::ToolUse {
                        id: "toolu_1".into(),
                        name: self.tool.into(),
                        input: serde_json::json!({}),
                    }),
                    Ok(base::interface::model::ModelEvent::EndTurn {
                        stop_reason: "tool_use".into(),
                        usage: Default::default(),
                    }),
                ]
            } else {
                vec![
                    Ok(base::interface::model::ModelEvent::TextDelta {
                        text: "done".into(),
                    }),
                    Ok(base::interface::model::ModelEvent::EndTurn {
                        stop_reason: "end_turn".into(),
                        usage: Default::default(),
                    }),
                ]
            };
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn structured_output_calls_from_earlier_turns_do_not_cut_this_one_short() {
        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Probe",
        });
        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene))
            .model(model)
            .settings(Arc::new(test_settings()))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        for i in 0..5 {
            let id = format!("so_{i}");
            agent
                .session
                .push_message(base::interface::model::ModelMessage {
                    role: base::interface::model::MessageRole::Assistant,
                    content: vec![base::interface::model::ModelContentBlock::ToolUse {
                        id: id.clone(),
                        name: "StructuredOutput".into(),
                        input: serde_json::json!({}),
                    }],
                });
            agent
                .session
                .push_message(base::interface::model::ModelMessage {
                    role: base::interface::model::MessageRole::User,
                    content: vec![base::interface::model::ModelContentBlock::ToolResult {
                        tool_use_id: id,
                        content: "ok".into(),
                        is_error: None,
                    }],
                });
        }

        let outcome = agent
            .process_turn(
                InputMessage::User {
                    content: "go".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("process_turn should succeed");

        assert_eq!(outcome.stop_reason, "end_turn");
        assert_eq!(outcome.api_calls, 2);
    }

    #[tokio::test]
    async fn a_tool_hook_is_told_which_tool_use_it_is_about_and_the_live_mode() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("pre_tool_use.json");
        let mut settings = test_settings();
        settings.hooks_config = Some(serde_json::json!({
            "PreToolUse": [
                { "type": "command", "command": format!("cat > '{}'", seen.display()) }
            ]
        }));
        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Probe",
        });
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(ProbeTool));
        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene))
            .model(model)
            .settings(Arc::new(settings))
            .tools(tools)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        agent
            .session_state
            .set_permission_mode(base::permission::PermissionMode::Plan);

        agent
            .process_turn(
                InputMessage::User {
                    content: "go".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("process_turn should succeed");

        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&seen).expect("the hook ran")).unwrap();
        assert_eq!(payload["tool_use_id"], "toolu_1");
        assert_eq!(payload["permission_mode"], "plan");
    }

    #[tokio::test]
    async fn post_tool_use_hook_discontinue_ends_the_turn_early() {
        // End-to-end regression test for the PostToolUse -> discontinue
        // wiring: a PostToolUse hook returning `continue: false` must stop
        // the turn right after the tool that triggered it, instead of
        // looping back to the model for another round. Proven two ways:
        // stop_reason == "stopped_by_hook", and the model is called exactly
        // once (a working-but-broken discontinue would still call it a 2nd
        // time to hand back the tool result).
        let mut settings = test_settings();
        settings.hooks_config = Some(serde_json::json!({
            "PostToolUse": [
                { "type": "command", "command": "echo '{\"continue\":false}'" }
            ]
        }));
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: call_count.clone(),
            tool: "Probe",
        });
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(ProbeTool));

        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(Arc::new(settings))
            .tools(tools)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let outcome = agent
            .process_turn(
                InputMessage::User {
                    content: "please use the probe tool".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("process_turn should succeed");

        assert_eq!(outcome.stop_reason, "stopped_by_hook");
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "model should only have been called once — the turn must not have looped back for a 2nd round"
        );
    }

    /// Returns `EndTurn` with no `ToolUse` on the very first call whose
    /// first message text contains `"You are selecting memories"` — that's
    /// `select_memories_with_llm`'s fixed system-prompt opener, the one
    /// reliable way to tell "this is the memory-prefetch call" apart from
    /// the turn's own main model call using only the `Model` trait's public
    /// interface. The prefetch branch returns a stream that never yields
    /// anything (`futures::stream::pending()`) — simulating a slow/stuck
    /// provider call — so a test asserting the *turn* still completes
    /// promptly is actually exercising the abort-on-no-tool-use path, not
    /// just getting lucky with timing.
    struct AnswersDirectlySlowPrefetchModel;

    #[async_trait::async_trait]
    impl Model for AnswersDirectlySlowPrefetchModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let is_prefetch = messages.first().is_some_and(|m| {
                m.content.iter().any(|b| {
                    matches!(
                        b,
                        base::interface::model::ModelContentBlock::Text { text }
                            if text.contains("You are selecting memories")
                    )
                })
            });
            if is_prefetch {
                return Ok(Box::new(futures::stream::pending()));
            }
            let events: Vec<
                Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
            > = vec![
                Ok(base::interface::model::ModelEvent::TextDelta {
                    text: "no tools needed here".into(),
                }),
                Ok(base::interface::model::ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// Regression test: a turn that answers directly (no tool calls) used
    /// to unconditionally `.await` the memory-prefetch background task
    /// (with only a 30s timeout as an upper bound) before it could
    /// complete — even though the result, once collected, would only ever
    /// be injected when `has_tool_uses` is true. A slow/stuck prefetch call
    /// therefore held up the user's response for no reason. It should now
    /// be aborted rather than awaited when this round has no tool calls, so
    /// the turn finishes promptly regardless of how long the prefetch would
    /// have taken.
    #[tokio::test]
    async fn turn_without_tool_use_does_not_wait_on_a_slow_memory_prefetch() {
        let mut settings = test_settings();
        settings.memory_enabled = true;
        let model: Arc<dyn Model> = Arc::new(AnswersDirectlySlowPrefetchModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        // >5 entries so `select_memories_with_llm` doesn't short-circuit
        // (skip the model call entirely) before ever reaching our mock's
        // prefetch branch — see its own "headers.len() <= max_results"
        // early return.
        let tmp = tempfile::tempdir().unwrap();
        let memory_store = Arc::new(base::interface::memory::MemoryStore::new(
            tmp.path().join("user"),
            tmp.path().join("local"),
        ));
        let memories: Vec<base::interface::memory::DurableMemory> = (0..8)
            .map(|i| base::interface::memory::DurableMemory {
                name: format!("mem-{i}"),
                description: format!("test memory {i}"),
                memory_type: Default::default(),
                content: "content".into(),
                source_session_id: String::new(),
                confidence: 0.8,
                last_seen: String::new(),
                recall_count: 0,
            })
            .collect();
        memory_store.persist_batch(memories).unwrap();

        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(Arc::new(settings))
            .memory_store(memory_store)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent.process_turn(
                InputMessage::User {
                    content: "just answer, don't use any tools".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                CancellationToken::new(),
            ),
        )
        .await
        .expect(
            "turn should have completed well within 5s — it must not have waited on the \
             (deliberately never-resolving) memory prefetch",
        )
        .expect("process_turn should succeed");

        assert_eq!(outcome.stop_reason, "end_turn");
    }

    /// Always asks. Enough to drive a real permission prompt through the
    /// engine loop.
    struct PromptingPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for PromptingPermission {
        async fn check(
            &self,
            tool_name: &str,
            _tool_input: &serde_json::Value,
            _cwd: &std::path::Path,
            _session_id: &str,
        ) -> base::interface::permission::PermissionOutcome {
            base::interface::permission::PermissionOutcome::Prompt {
                prompt_id: "p1".into(),
                message: format!("allow {tool_name}?"),
                paths: vec![],
            }
        }
    }

    /// The regression that motivated the input demultiplexer.
    ///
    /// `Agent::run` was the only reader of `input_rx` and `await`ed
    /// `process_turn` inline, while the `PermissionResponse` arm that wakes a
    /// blocked tool call lives *inside* `process_turn`. So a turn waiting on a
    /// permission prompt could never dequeue its own answer — both halves
    /// deadlocked and `session.respondToPrompt` had never once worked. It went
    /// unnoticed because the daemon defaulted to allow-all (so the prompt path
    /// never ran) and because the only test covering it *simulated* the
    /// message instead of driving the real loop.
    ///
    /// This test drives the real loop. Before the fix it hangs until the
    /// timeout; after it, the answer gets through and the turn completes.
    #[tokio::test]
    async fn a_permission_answer_reaches_a_turn_that_is_blocked_waiting_for_it() {
        // Local rather than reusing turn.rs's ProbeTool — that lives in a
        // private `mod tests` and exporting it just for this would widen its
        // visibility for one caller.
        struct ProbeTool;
        #[async_trait::async_trait]
        impl base::tool::Tool for ProbeTool {
            fn name(&self) -> &str {
                "Probe"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(
                &self,
                _input: serde_json::Value,
                _ctx: base::tool::ToolContext,
                _progress: base::tool::ProgressSender,
            ) -> Result<base::tool::ToolResult, base::error::ToolError> {
                Ok(base::tool::ToolResult::text("probe-ran"))
            }
        }

        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool));

        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Probe",
        });
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(tools)
            .permission(Arc::new(PromptingPermission))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let cancel = CancellationToken::new();
        let engine = tokio::spawn(async move { agent.run(cancel).await });

        input_tx
            .send(InputMessage::User {
                content: "use the probe tool".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        // Wait for the engine to actually ask.
        let prompt_id = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::PermissionPrompt { prompt_id, .. }) => break prompt_id,
                    Some(_) => continue,
                    None => panic!("event channel closed before any permission prompt"),
                }
            }
        })
        .await
        .expect("engine should have emitted a permission prompt");

        // Answer it on the same channel the daemon uses.
        input_tx
            .send(InputMessage::PermissionResponse {
                prompt_id,
                decision: PermissionDecision::Permit,
            })
            .unwrap();

        // The turn must now finish. This is the assertion that used to hang.
        let completed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                    Some(_) => continue,
                    None => panic!("event channel closed before the turn completed"),
                }
            }
        })
        .await
        .expect(
            "the turn must complete once its permission answer arrives — if this times out, \
             the answer is not reaching the blocked tool call",
        );
        assert_eq!(completed, "end_turn");

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// `EngineCommand::CancelTurn` must interrupt the turn in flight and
    /// leave the session able to take the next message.
    ///
    /// Before per-turn tokens existed there was no way to express this: every
    /// turn ran under the token passed to `run()`, so the only interrupt a
    /// host could send took the whole session down with it. The second half
    /// of this test — a normal turn served *after* the cancellation — is the
    /// part that would have failed.
    ///
    /// It also pins the delivery path. `CancelTurn` is handled in the input
    /// demultiplexer, not the main loop, because the main loop is inside
    /// `process_turn().await` for the entire time a turn is running; handled
    /// there, this test would hang until the timeout with the tool still
    /// parked.
    #[tokio::test]
    async fn cancel_turn_interrupts_the_turn_and_leaves_the_session_usable() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(HangingTool));

        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Hang",
        });
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(tools)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let session_cancel = CancellationToken::new();
        let engine = {
            let session_cancel = session_cancel.clone();
            tokio::spawn(async move { agent.run(session_cancel).await })
        };

        input_tx
            .send(InputMessage::User {
                content: "start something long".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        // Wait until the tool is actually running, so the cancel lands
        // mid-turn rather than before the turn starts.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::ToolUse { name, .. }) if name == "Hang" => break,
                    Some(_) => continue,
                    None => panic!("event channel closed before the tool ran"),
                }
            }
        })
        .await
        .expect("the hanging tool should have started");

        input_tx
            .send(InputMessage::System {
                kind: EngineCommand::CancelTurn,
                content: String::new(),
            })
            .unwrap();

        let stop_reason = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                    Some(_) => continue,
                    None => panic!("event channel closed before the turn ended"),
                }
            }
        })
        .await
        .expect("CancelTurn must reach the running turn while it is still running");
        assert_eq!(stop_reason, "cancelled");
        assert!(
            !session_cancel.is_cancelled(),
            "cancelling a turn must not cancel the session"
        );

        // The session is still live: a second turn runs to completion.
        input_tx
            .send(InputMessage::User {
                content: "still there?".into(),
                attachments: vec![],
                turn_id: "t2".into(),
            })
            .unwrap();
        let second = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                    Some(_) => continue,
                    None => panic!("event channel closed — the engine died with the turn"),
                }
            }
        })
        .await
        .expect("the session must still serve turns after one was cancelled");
        assert_eq!(second, "end_turn");

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// A host takes `Agent::commands()` before spawning the engine and keeps
    /// it for the life of the session: it must stay a live view of the same
    /// catalog the turn loop resolves against, including skills that appear
    /// after the engine has been moved into its task.
    #[tokio::test]
    async fn commands_handle_outlives_the_agent_and_stays_live() {
        let dir = tempfile::tempdir().unwrap();
        let skills = Arc::new(skills::manager::SkillManager::new());
        skills
            .load_dir_subdirs(dir.path(), skills::manager::SkillSource::Project)
            .unwrap();

        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel))
            .settings(Arc::new(test_settings()))
            .tools(Arc::new(InMemoryToolRegistry::new()))
            .skill_catalog(skills.clone())
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let commands = agent.commands();
        drop(agent);
        assert!(commands.resolve("mid-session").is_none());

        let skill_dir = dir.path().join("mid-session");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: mid-session\ndescription: added while the engine runs\n---\n\nbody\n",
        )
        .unwrap();
        skills
            .load_dir_subdirs(dir.path(), skills::manager::SkillSource::Project)
            .unwrap();

        assert!(
            commands.resolve("mid-session").is_some(),
            "the handle a host holds must track the catalog, not a snapshot"
        );
    }

    /// Cancelling the session's own token still cascades into the turn in
    /// flight — per-turn tokens are children, not replacements.
    #[tokio::test]
    async fn cancelling_the_session_still_stops_the_turn_in_flight() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(HangingTool));

        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Hang",
        });
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(tools)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let session_cancel = CancellationToken::new();
        let engine = {
            let session_cancel = session_cancel.clone();
            tokio::spawn(async move { agent.run(session_cancel).await })
        };

        input_tx
            .send(InputMessage::User {
                content: "start something long".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::ToolUse { name, .. }) if name == "Hang" => break,
                    Some(_) => continue,
                    None => panic!("event channel closed before the tool ran"),
                }
            }
        })
        .await
        .expect("the hanging tool should have started");

        session_cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(10), engine)
            .await
            .expect("the engine must exit when its own token is cancelled")
            .expect("engine task panicked");
    }

    /// Regression test: `UserPromptSubmit` was defined in the `HookEvent`
    /// enum but had zero trigger points anywhere — CodingScene's system
    /// prompt told the model to expect `<user-prompt-submit-hook>` feedback
    /// even though it could never fire. `decision: "block"` should now
    /// refuse to process the message at all: the turn ends immediately with
    /// `stopped_by_hook`, and the model must never be called.
    #[tokio::test]
    async fn user_prompt_submit_hook_block_ends_the_turn_before_any_model_call() {
        let mut settings = test_settings();
        settings.hooks_config = Some(serde_json::json!({
            "UserPromptSubmit": [
                { "type": "command", "command": "echo '{\"decision\":\"block\",\"message\":\"not now\"}'" }
            ]
        }));
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: call_count.clone(),
            tool: "Probe",
        });
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(Arc::new(settings))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let outcome = agent
            .process_turn(
                InputMessage::User {
                    content: "please use the probe tool".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("process_turn should succeed");

        assert_eq!(outcome.stop_reason, "stopped_by_hook");
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the model must never be called once UserPromptSubmit blocked the message"
        );
    }

    /// `updated_input` should rewrite the message text that actually gets
    /// processed — same field `PreToolUse` uses for rewriting tool input,
    /// repurposed for the one thing there is to rewrite here.
    #[tokio::test]
    async fn user_prompt_submit_hook_rewrites_the_message_content() {
        let mut settings = test_settings();
        settings.hooks_config = Some(serde_json::json!({
            "UserPromptSubmit": [
                { "type": "command", "command": "echo '{\"updated_input\":\"REWRITTEN\"}'" }
            ]
        }));
        // Not `DummyModel` — its `stream()` is `unimplemented!()`, and this
        // test (unlike the block test above) lets the turn actually reach
        // the model call.
        let model: Arc<dyn Model> = Arc::new(AnswersDirectlySlowPrefetchModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);

        let (mut agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(Arc::new(settings))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        agent
            .process_turn(
                InputMessage::User {
                    content: "original message".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("process_turn should succeed");

        let pushed_text = agent
            .session
            .messages()
            .iter()
            .find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    base::interface::model::ModelContentBlock::Text { text }
                        if text == "original message" || text == "REWRITTEN" =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
            })
            .expect("the user message text should be present in session.messages()");
        assert_eq!(
            pushed_text, "REWRITTEN",
            "the hook's updated_input should have replaced the original message content"
        );
    }

    #[test]
    fn build_hook_runner_defaults_to_empty_without_hooks_config() {
        let settings = test_settings();
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let runner = build_hook_runner(&settings, model, dummy_agent_tool(), None, None);
        assert!(runner.is_empty());
    }

    #[test]
    fn build_hook_runner_parses_hooks_config_into_real_hooks() {
        let mut settings = test_settings();
        settings.hooks_config = Some(serde_json::json!({
            "PreToolUse": [
                { "type": "command", "command": "echo hi" }
            ]
        }));
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let runner = build_hook_runner(&settings, model, dummy_agent_tool(), None, None);
        assert!(!runner.is_empty());
        assert!(runner.has_hooks_for(hooks::HookEvent::PreToolUse));
    }

    /// The unwired-event warning must stay a *warning*. Refusing the hook, or
    /// dropping it from the runner, would be a behavior change dressed up as
    /// a diagnostic — and it would break anyone who has such a hook
    /// configured today against the day the event gets wired up.
    #[test]
    fn build_hook_runner_keeps_hooks_for_unwired_events_and_only_warns() {
        let mut settings = test_settings();
        settings.hooks_config = Some(serde_json::json!({
            "ConfigChange": [
                { "type": "command", "command": "echo reconfigured" }
            ],
            "PreToolUse": [
                { "type": "command", "command": "echo hi" }
            ]
        }));
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let runner = build_hook_runner(&settings, model, dummy_agent_tool(), None, None);
        assert!(
            runner.has_hooks_for(hooks::HookEvent::ConfigChange),
            "an unwired event's hook must still be registered — the engine warns, it does \
             not silently discard"
        );
        assert!(
            runner.has_hooks_for(hooks::HookEvent::PreToolUse),
            "an unwired event in the same config must not affect the wired ones"
        );
    }

    #[test]
    fn build_hook_runner_degrades_to_empty_on_malformed_hooks_config() {
        let mut settings = test_settings();
        // Wrong shape: hooks value must be a map of event -> Vec<HookConfig>.
        settings.hooks_config = Some(serde_json::json!("not-a-hooks-map"));
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let runner = build_hook_runner(&settings, model, dummy_agent_tool(), None, None);
        assert!(runner.is_empty());
    }

    /// Regression: `hooks::watcher::FileWatcher` was fully implemented
    /// (`HookRunner::enable_file_watching`) but `Builder::build()` never
    /// called it, so a configured `FileChanged` hook could never fire in a
    /// real session no matter how it was set up. This exercises the actual
    /// wiring: build an `Agent` with a `FileChanged` hook configured and an
    /// instruction file, edit that file, and confirm the hook really runs.
    #[tokio::test]
    async fn build_starts_file_watching_for_the_instruction_file_when_a_hook_wants_it() {
        // Two guesses about machine speed used to stand between this test and
        // its assertion: 200ms for the OS watcher to register, then 1500ms for
        // the debounce, the dispatch and a shell to run. Both are gone. The
        // hook is observed in this process through an injected executor, and
        // the file is rewritten until the notification arrives — so a watcher
        // that registers late costs another loop rather than a failure.
        struct RecordingPromptExecutor {
            fired: tokio::sync::mpsc::Sender<()>,
        }

        #[async_trait::async_trait]
        impl hooks::runner::PromptHookExecutor for RecordingPromptExecutor {
            async fn execute(
                &self,
                _prompt: &str,
                _model: Option<&str>,
                _payload: &hooks::HookInput,
            ) -> Result<String, String> {
                let _ = self.fired.try_send(());
                Ok("{}".to_string())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let instruction_path = dir.path().join("AGENTS.md");
        std::fs::write(&instruction_path, "# instructions").unwrap();

        let (fired_tx, mut fired_rx) = tokio::sync::mpsc::channel(1);
        let mut hooks_settings: hooks::HooksSettings = Default::default();
        hooks_settings.insert(
            hooks::HookEvent::FileChanged,
            vec![hooks::config::HookConfig::Prompt {
                prompt: "the instruction file changed".into(),
                timeout: None,
                model: None,
            }],
        );
        let runner = Arc::new(
            hooks::HookRunner::new(hooks_settings)
                .with_prompt_executor(Arc::new(RecordingPromptExecutor { fired: fired_tx })),
        );

        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let scene: Arc<dyn AgentScene> = Arc::new(scene::scene::coding::CodingScene);
        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(scene)
            .model(model)
            .settings(Arc::new(test_settings()))
            .hooks(runner)
            .instruction(instruction_path.clone())
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        assert!(agent.hooks().has_hooks_for(hooks::HookEvent::FileChanged));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut edits = 0u32;
        let fired = loop {
            edits += 1;
            std::fs::write(&instruction_path, format!("# instructions, edit {edits}")).unwrap();
            match tokio::time::timeout(std::time::Duration::from_millis(500), fired_rx.recv()).await
            {
                Ok(Some(())) => break true,
                Ok(None) => break false,
                Err(_) if std::time::Instant::now() >= deadline => break false,
                Err(_) => continue,
            }
        };

        assert!(
            fired,
            "editing the watched instruction file should have fired the FileChanged hook \
             ({edits} edits over 30s without one)"
        );
    }

    /// P2-4's acceptance: a wrapper gives one tool a deadline, and the tool
    /// actually feels it. Without the wrapper `Hang` waits for the session's
    /// own cancellation, which this test never sends.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_registered_wrapper_can_put_a_deadline_on_one_tool() {
        use base::interface::tool_middleware::{
            NextDispatch, ToolCall, ToolExec, ToolMiddleware, ToolOutcome,
        };

        struct DeadlineFor {
            tool: &'static str,
            after: std::time::Duration,
            wrapped: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ToolMiddleware for DeadlineFor {
            async fn around(
                &self,
                call: &ToolCall,
                exec: &mut ToolExec,
                next: NextDispatch<'_>,
            ) -> ToolOutcome {
                if call.name == self.tool {
                    self.wrapped
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    exec.with_timeout(self.after);
                }
                next.run(call, exec).await
            }
        }

        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(HangingTool));
        let wrapped = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Hang",
        });
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(tools)
            .tool_middleware(Arc::new(DeadlineFor {
                tool: "Hang",
                after: std::time::Duration::from_millis(50),
                wrapped: wrapped.clone(),
            }))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let session = CancellationToken::new();
        let engine = {
            let session = session.clone();
            tokio::spawn(async move { agent.run(session).await })
        };
        input_tx
            .send(InputMessage::User {
                content: "hang please".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        // The turn completes without the session ever being cancelled, which
        // it could not do if `Hang` were still waiting on the session token.
        let stop = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                    Some(_) => continue,
                    None => panic!("event channel closed before the turn completed"),
                }
            }
        })
        .await
        .expect(
            "the wrapper's deadline must end the hanging tool — if this times out, the \
             signal it installed is not reaching dispatch",
        );
        assert_eq!(stop, "end_turn");
        assert_eq!(
            wrapped.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the wrapper must have seen the call it was registered for"
        );

        session.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// P2-5's point: a deployment can guarantee a string never reaches the
    /// model, and the guarantee holds through the path a tool result actually
    /// takes — including when the tool failed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_result_transformer_is_the_last_thing_to_touch_a_tool_result() {
        use base::interface::tool_result::RedactLiterals;

        struct LeaksATokenTool;
        #[async_trait::async_trait]
        impl base::tool::Tool for LeaksATokenTool {
            fn name(&self) -> &str {
                "Leak"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(
                &self,
                _input: serde_json::Value,
                _ctx: base::tool::ToolContext,
                _progress: base::tool::ProgressSender,
            ) -> Result<base::tool::ToolResult, base::error::ToolError> {
                Ok(base::tool::ToolResult {
                    content: base::tool::ToolResultContent::Text(
                        "ANTHROPIC_API_KEY=sk-live-abc123".into(),
                    ),
                    ..Default::default()
                })
            }
        }

        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(LeaksATokenTool));

        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Leak",
        });
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(tools)
            .tool_result_transformer(Arc::new(RedactLiterals::new(["sk-live-abc123"])))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
        input_tx
            .send(InputMessage::User {
                content: "run it".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        let result_text = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::ToolResult { content, .. }) => break content,
                    Some(AgentEvent::TurnComplete { .. }) => {
                        panic!("the turn ended without a tool result")
                    }
                    Some(_) => continue,
                    None => panic!("event channel closed"),
                }
            }
        })
        .await
        .expect("the tool should have run");

        assert!(
            !result_text.contains("sk-live-abc123"),
            "the secret reached the result the model is shown: {result_text}"
        );
        assert!(result_text.contains("<redacted>"), "{result_text}");

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// P2-9: recall goes through the contract, so a host can replace the
    /// judgement with something deterministic — and a hook can see both ends
    /// of it — without touching the engine.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_substituted_retriever_and_its_hooks_decide_what_is_recalled() {
        use base::interface::memory_contracts::{
            MemoryRetriever, MemoryStorage, RetrievalHook, RetrievalRequest,
        };

        struct Records {
            asked: Arc<std::sync::Mutex<Vec<String>>>,
        }

        #[async_trait::async_trait]
        impl MemoryRetriever for Records {
            async fn retrieve(
                &self,
                _storage: &dyn MemoryStorage,
                _model: &dyn Model,
                request: &RetrievalRequest,
            ) -> Vec<String> {
                self.asked.lock().unwrap().push(request.query.clone());
                vec!["from-the-substitute".to_string()]
            }
        }

        struct Expands;
        impl RetrievalHook for Expands {
            fn before_retrieve(&self, request: &mut RetrievalRequest) {
                request.query = format!("[expanded] {}", request.query);
            }
        }

        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let model: Arc<dyn Model> = Arc::new(PlainReplyModel);
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(Arc::new(InMemoryToolRegistry::new()))
            .memory_retriever(Arc::new(Records {
                asked: asked.clone(),
            }))
            .retrieval_hook(Arc::new(Expands))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
        input_tx
            .send(InputMessage::User {
                content: "what did we decide about deploys".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::TurnComplete { .. }) => break,
                    Some(_) => continue,
                    None => panic!("event channel closed"),
                }
            }
        })
        .await
        .expect("the turn should complete");

        let asked = asked.lock().unwrap().clone();
        assert_eq!(
            asked.len(),
            1,
            "recall must have gone through the substitute"
        );
        assert_eq!(
            asked[0], "[expanded] what did we decide about deploys",
            "the hook must have seen the question before the retriever did"
        );

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// Emits many text deltas, so a per-chunk hook would be obvious.
    struct ChattyModel;

    #[async_trait::async_trait]
    impl Model for ChattyModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let mut events: Vec<
                Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
            > = (0..64)
                .map(|i| {
                    Ok(base::interface::model::ModelEvent::TextDelta {
                        text: format!("chunk {i} "),
                    })
                })
                .collect();
            events.push(Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }));
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// P2-10's acceptance: the interceptor sees the request and the finished
    /// message, and is *not* called per chunk. Sixty-four deltas produce one
    /// message, and the counts say so.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_interceptor_sees_whole_things_never_chunks() {
        use base::interface::model_interceptor::{ModelInterceptor, ModelRequestView};
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct Counting {
            requests: AtomicUsize,
            messages: AtomicUsize,
        }

        impl ModelInterceptor for Counting {
            fn on_request(&self, request: &mut ModelRequestView) {
                self.requests.fetch_add(1, Ordering::SeqCst);
                request.params.max_tokens = 4096;
            }
            fn on_message(&self, _message: &mut base::interface::model::ModelMessage) {
                self.messages.fetch_add(1, Ordering::SeqCst);
            }
        }

        let counting = Arc::new(Counting::default());
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(ChattyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .tools(Arc::new(InMemoryToolRegistry::new()))
            .model_interceptor(counting.clone())
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
        input_tx
            .send(InputMessage::User {
                content: "say a lot".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        let deltas = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let mut deltas = 0usize;
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::TextDelta { .. }) => deltas += 1,
                    Some(AgentEvent::TurnComplete { .. }) => break deltas,
                    Some(_) => continue,
                    None => panic!("event channel closed"),
                }
            }
        })
        .await
        .expect("the turn should complete");

        assert!(deltas > 20, "the model streamed {deltas} deltas");
        assert_eq!(
            counting.requests.load(Ordering::SeqCst),
            1,
            "one request, one call"
        );
        let messages = counting.messages.load(Ordering::SeqCst);
        assert_eq!(
            messages, 1,
            "{deltas} chunks must produce one interception, not {messages}"
        );

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// P3-2's acceptance: a host changes "how many steps before stopping"
    /// with a policy, and the loop is not touched.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_policy_decides_when_the_loop_stops() {
        use base::interface::turn_policy::{TurnPolicy, TurnProgress, TurnStep};

        struct StopAfterOne;
        impl TurnPolicy for StopAfterOne {
            fn before_model_call(&self, progress: &TurnProgress<'_>) -> TurnStep {
                if progress.api_calls >= 1 {
                    return TurnStep::stop("host_said_enough");
                }
                TurnStep::Continue
            }
        }

        // Asks for a tool every time, so only a stop condition ends this.
        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Probe",
        });
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(ProbeTool));

        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(tools)
            .turn_policy(Arc::new(StopAfterOne))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
        input_tx
            .send(InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        let stop = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                    Some(_) => continue,
                    None => panic!("event channel closed"),
                }
            }
        })
        .await
        .expect("the policy should have ended the turn");

        assert_eq!(
            stop, "host_said_enough",
            "the policy's reason must reach the host, not be translated into the engine's"
        );

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// P3-6's acceptance: a deployment configures a ceiling on request size
    /// and the engine honours it. There was no such configuration before —
    /// `TokenBudget` said when to start compacting and nothing said how large
    /// was too large.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_context_hard_cap_ends_a_turn_the_scene_would_have_allowed() {
        use base::interface::budget_policy::{Capped, EngineBudget};

        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Probe",
        });
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(ProbeTool));

        // Zero: the system prompt alone is over it, so the first request is
        // one this deployment has said must never be sent.
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(tools)
            .budget_policy(Arc::new(Capped {
                inner: EngineBudget::new(None),
                context_hard_cap: 0,
            }))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
        input_tx
            .send(InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        let stop = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                    Some(_) => continue,
                    None => panic!("event channel closed"),
                }
            }
        })
        .await
        .expect("the cap should have ended the turn");

        assert_eq!(stop, "context_exceeded");

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// P4-4's other half: the policy is computed correctly *and* it reaches a
    /// tool.
    ///
    /// The work order existed because `allowed_domains` was configured, read,
    /// and never arrived where the model could feel it. Testing the
    /// computation alone would have left exactly that gap open a second time,
    /// so this one goes through a real turn and asks a tool what its own
    /// provider says.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_egress_policy_reaches_the_tool_that_has_to_obey_it() {
        use base::interface::exec::Origin;

        struct EgressProbe;
        #[async_trait::async_trait]
        impl base::tool::Tool for EgressProbe {
            fn name(&self) -> &str {
                "Probe"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(
                &self,
                _input: serde_json::Value,
                ctx: base::tool::ToolContext,
                _progress: base::tool::ProgressSender,
            ) -> Result<base::tool::ToolResult, base::error::ToolError> {
                let blocked = ctx.exec.network.permits("blocked.test", Origin::Agent);
                let allowed = ctx.exec.network.permits("allowed.test", Origin::Agent);
                let endpoint = ctx.exec.network.permits("blocked.test", Origin::Operator);
                Ok(base::tool::ToolResult::text(format!(
                    "blocked={blocked} allowed={allowed} endpoint={endpoint}"
                )))
            }
        }

        let mut settings = test_settings();
        settings.sandbox.network_mode = base::context::config::NetworkModeConfig::Allowlist;
        settings.sandbox.allowed_domains = vec!["allowed.test".into()];

        let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            tool: "Probe",
        });
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(EgressProbe));

        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(settings))
            .tools(tools)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
        input_tx
            .send(InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        let verdict = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::ToolResult { content, .. })
                        if content.contains("blocked=") =>
                    {
                        break content
                    }
                    Some(_) => continue,
                    None => panic!("event channel closed before the tool answered"),
                }
            }
        })
        .await
        .expect("the probe should have run");

        assert_eq!(
            verdict, "blocked=false allowed=true endpoint=true",
            "the settings have to arrive at the tool's own provider: a configured \
             allowlist that the model cannot feel is the defect this work order \
             was written for"
        );

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// P3-15's acceptance: an embedder registers a diagnostic of its own and
    /// reads the engine's health back — after handing the agent off to its
    /// own task, which is when a supervisor actually asks.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_registers_a_health_check_and_reads_the_report_back() {
        use base::interface::health::{CheckResult, HealthCheck, HealthStatus};
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Queue(Arc<AtomicBool>);
        impl HealthCheck for Queue {
            fn name(&self) -> &str {
                "acme.queue"
            }
            fn check(&self) -> CheckResult {
                if self.0.load(Ordering::SeqCst) {
                    CheckResult::degraded("queue backed up")
                        .with_details(serde_json::json!({ "depth": 4096 }))
                } else {
                    CheckResult::ok("queue drained")
                }
            }
        }

        let backed_up = Arc::new(AtomicBool::new(false));
        let (agent, _event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(DummyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .health_check(Arc::new(Queue(backed_up.clone())))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let health = agent.health();
        // `run()` takes the agent by `&mut self` for the life of the session,
        // so a handle taken before that is the only way to ask afterwards.
        let mut agent = agent;
        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });

        let report = health.report();
        assert_eq!(report.status, HealthStatus::Ok);
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].name, "acme.queue");

        backed_up.store(true, Ordering::SeqCst);
        let report = health.report();
        assert_eq!(
            report.status,
            HealthStatus::Degraded,
            "the report is asked fresh every time, not cached at build"
        );
        assert_eq!(report.to_json()["checks"][0]["details"]["depth"], 4096);

        engine.abort();
    }

    /// P3-3's acceptance: a policy turns an overload from "switch to the
    /// fallback" into "fail", without the loop knowing anything about it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovery_policy_can_refuse_the_fallback_the_engine_would_take() {
        use base::interface::recovery_policy::NeverRecover;

        struct AlwaysOverloaded;

        #[async_trait::async_trait]
        impl Model for AlwaysOverloaded {
            fn api_type(&self) -> base::provider::ApiType {
                base::provider::ApiType::Anthropic
            }
            async fn stream(
                &self,
                _p: Vec<base::interface::prompt::PromptBlock>,
                _t: Vec<base::interface::model::ToolDef>,
                _m: Vec<base::interface::model::ModelMessage>,
                _s: base::interface::model::StreamParams,
                _c: CancellationToken,
            ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
            {
                Err(base::interface::model::ModelError::Overloaded)
            }
        }

        // A fallback *is* configured, so the engine's own policy would switch
        // to it. The point is that the host's policy wins.
        let mut settings = test_settings();
        settings.model.fallback_model = Some("some-other-model".into());

        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(AlwaysOverloaded) as Arc<dyn Model>)
            .settings(Arc::new(settings))
            .tools(Arc::new(InMemoryToolRegistry::new()))
            .recovery_policy(Arc::new(NeverRecover))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
        input_tx
            .send(InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        let saw_error = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::Error { message, .. }) => break message,
                    Some(AgentEvent::TurnComplete { stop_reason, .. }) => {
                        panic!(
                            "the turn completed with `{stop_reason}` instead of failing — \
                                the policy's refusal did not reach the loop"
                        )
                    }
                    Some(_) => continue,
                    None => panic!("event channel closed"),
                }
            }
        })
        .await
        .expect("the turn should have failed");

        assert!(
            !saw_error.contains("some-other-model"),
            "the engine must not have switched to the fallback: {saw_error}"
        );

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    /// Records the prompt blocks of the first request and ends the turn.
    struct RecordingModel(Arc<std::sync::Mutex<Vec<base::interface::prompt::PromptBlock>>>);

    #[async_trait::async_trait]
    impl Model for RecordingModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            *self.0.lock().unwrap() = prompt_blocks;
            let events: Vec<
                Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
            > = vec![Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            })];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// Runs one turn and returns the prompt blocks the model was given.
    async fn prompt_blocks_for_one_turn(
        assembler: Option<Arc<dyn base::interface::prompt_assembler::PromptAssembler>>,
    ) -> Vec<base::interface::prompt::PromptBlock> {
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(RecordingModel(recorded.clone())) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .tools(Arc::new(InMemoryToolRegistry::new()))
            .skip_warmup(true);
        if let Some(a) = assembler {
            builder = builder.prompt_assembler(a);
        }
        let (mut agent, mut event_rx, input_tx) = builder.build().expect("build should succeed");

        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
        input_tx
            .send(InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match event_rx.recv().await {
                    Some(AgentEvent::TurnComplete { .. }) | None => break,
                    Some(_) => continue,
                }
            }
        })
        .await;
        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;

        let blocks = recorded.lock().unwrap().clone();
        assert!(!blocks.is_empty(), "the model was never called");
        blocks
    }

    /// P3-11's acceptance: the assembler is replaceable outright, not merely
    /// contributed to. Nothing the kernel would have assembled survives.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_can_replace_prompt_assembly_outright() {
        use base::interface::prompt_assembler::{AssemblyRequest, PromptAssembler};

        struct OnlyThis;
        impl PromptAssembler for OnlyThis {
            fn assemble(
                &self,
                _request: &AssemblyRequest<'_>,
            ) -> Vec<base::interface::prompt::PromptBlock> {
                vec![base::interface::prompt::PromptBlock::system("only this").named("host.only")]
            }
        }

        let blocks = prompt_blocks_for_one_turn(Some(Arc::new(OnlyThis))).await;
        assert_eq!(
            blocks.len(),
            1,
            "the kernel's stages still reached the request: {:?}",
            blocks.iter().map(|b| &b.name).collect::<Vec<_>>()
        );
        assert_eq!(blocks[0].content, "only this");
    }

    /// The second implementation, in a real session: same content, one block
    /// and one cache breakpoint instead of several.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_merging_assembler_folds_a_real_session_prompt_into_one_block() {
        use base::interface::prompt_assembler::MergedSystemPrompt;

        let default_blocks = prompt_blocks_for_one_turn(None).await;
        assert!(
            default_blocks.len() > 1,
            "this assertion is only meaningful when the default produces several blocks"
        );

        let merged =
            prompt_blocks_for_one_turn(Some(Arc::new(MergedSystemPrompt::default()))).await;
        assert_eq!(merged.len(), 1);
        for block in &default_blocks {
            assert!(
                merged[0].content.contains(&block.content),
                "folding dropped a stage: {:?}",
                block.name
            );
        }
    }

    #[test]
    fn channel_types_construct() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let _sender: EventSender = crate::event_bus::EventBus::new(tx);
    }

    /// Answers in one text block and stops. Enough to make a turn that emits
    /// a handful of events and finishes immediately.
    struct PlainReplyModel;

    #[async_trait::async_trait]
    impl Model for PlainReplyModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let events: Vec<
                Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
            > = vec![
                Ok(base::interface::model::ModelEvent::TextDelta {
                    text: "hello".into(),
                }),
                Ok(base::interface::model::ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// Drives one turn to completion and returns the events the host's
    /// channel saw.
    async fn run_one_turn_with_sinks(
        sinks: Vec<Arc<dyn base::interface::event_sink::EventSink>>,
    ) -> Vec<AgentEvent> {
        let mut builder = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(PlainReplyModel) as Arc<dyn Model>)
            .settings(Arc::new(test_settings()))
            .tools(Arc::new(InMemoryToolRegistry::new()))
            .skip_warmup(true);
        for sink in sinks {
            builder = builder.event_sink(sink);
        }
        let (mut agent, mut event_rx, input_tx) = builder.build().expect("build should succeed");

        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
        input_tx
            .send(InputMessage::User {
                content: "say something".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();

        let observed = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let mut seen = Vec::new();
            loop {
                match event_rx.recv().await {
                    Some(ev) => {
                        let done = matches!(ev, AgentEvent::TurnComplete { .. });
                        seen.push(ev);
                        if done {
                            break seen;
                        }
                    }
                    None => panic!("event channel closed before the turn completed"),
                }
            }
        })
        .await
        .expect("the turn should complete");

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
        observed
    }

    /// P1-2: a host can take the event stream in-process, without standing a
    /// forwarding task between the engine's channel and its own world.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_registered_sink_sees_the_same_stream_as_the_returned_channel() {
        let sink = base::interface::event_sink::CollectingSink::new();
        let observed = run_one_turn_with_sinks(vec![sink.clone()]).await;
        assert!(
            observed.len() >= 2,
            "a turn should emit more than just its completion"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while sink.len() < observed.len() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let as_json = |evs: &[AgentEvent]| {
            evs.iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            as_json(&sink.events())[..observed.len()],
            as_json(&observed)[..],
            "the sink must see the same events, in the same order, as the channel"
        );
    }

    /// P1-2: registering a sink is observation, not control.
    ///
    /// The sink here blocks forever rather than merely being slow, so the
    /// test needs no timing threshold to be meaningful: if the turn were
    /// waiting on it at any point, the turn would never complete and the
    /// helper's timeout would fire.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stuck_sink_cannot_stop_a_turn_from_completing() {
        #[derive(Default)]
        struct Gate {
            released: std::sync::Mutex<bool>,
            wake: std::sync::Condvar,
        }
        struct Wedged(Arc<Gate>);
        impl base::interface::event_sink::EventSink for Wedged {
            fn emit(&self, _event: &AgentEvent) {
                let mut released = self.0.released.lock().unwrap();
                while !*released {
                    released = self.0.wake.wait(released).unwrap();
                }
            }
        }

        let gate = Arc::new(Gate::default());
        let observed = run_one_turn_with_sinks(vec![Arc::new(Wedged(gate.clone()))]).await;
        assert!(
            observed.len() >= 2,
            "the turn ran to completion past a sink that never returns"
        );

        // Let the lane's task out before the runtime goes away.
        *gate.released.lock().unwrap() = true;
        gate.wake.notify_all();
    }

    #[test]
    fn permission_decision_wire_format_round_trips() {
        let permit = PermissionDecision::Permit;
        let s = serde_json::to_value(&permit).unwrap();
        assert_eq!(s, serde_json::json!({"type": "permit"}));
        assert!(matches!(
            serde_json::from_value::<PermissionDecision>(s).unwrap(),
            PermissionDecision::Permit
        ));

        let deny = PermissionDecision::Deny {
            reason: "no".into(),
        };
        let s = serde_json::to_value(&deny).unwrap();
        assert_eq!(s, serde_json::json!({"type": "deny", "reason": "no"}));
        match serde_json::from_value::<PermissionDecision>(s).unwrap() {
            PermissionDecision::Deny { reason } => assert_eq!(reason, "no"),
            other => panic!("expected Deny, got {other:?}"),
        }

        let permit_always_session = PermissionDecision::PermitAlways {
            scope: PersistScope::Session,
        };
        let s = serde_json::to_value(&permit_always_session).unwrap();
        assert_eq!(
            s,
            serde_json::json!({"type": "permit_always", "scope": "session"})
        );
        match serde_json::from_value::<PermissionDecision>(s).unwrap() {
            PermissionDecision::PermitAlways {
                scope: PersistScope::Session,
            } => {}
            other => panic!("expected PermitAlways{{scope: Session}}, got {other:?}"),
        }

        let permit_always_local = PermissionDecision::PermitAlways {
            scope: PersistScope::Local,
        };
        let s = serde_json::to_value(&permit_always_local).unwrap();
        assert_eq!(
            s,
            serde_json::json!({"type": "permit_always", "scope": "local"})
        );
        match serde_json::from_value::<PermissionDecision>(s).unwrap() {
            PermissionDecision::PermitAlways {
                scope: PersistScope::Local,
            } => {}
            other => panic!("expected PermitAlways{{scope: Local}}, got {other:?}"),
        }

        // Also verify direct JSON string parsing of the exact wire shapes
        // named in the RPC docs, not just round-tripping our own output.
        let from_str: PermissionDecision =
            serde_json::from_str(r#"{"type":"permit_always","scope":"local"}"#).unwrap();
        assert!(matches!(
            from_str,
            PermissionDecision::PermitAlways {
                scope: PersistScope::Local
            }
        ));
    }

    /// Released by the test, so a tool step lasts exactly as long as the
    /// test needs to land a message in the middle of it.
    struct GateTool(Arc<tokio::sync::Notify>);
    #[async_trait::async_trait]
    impl base::tool::Tool for GateTool {
        fn name(&self) -> &str {
            "Gate"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            tokio::select! {
                _ = self.0.notified() => Ok(base::tool::ToolResult::text("gate opened")),
                _ = ctx.cancel.cancelled() => Err(base::error::ToolError::Cancelled),
            }
        }
    }

    /// Calls `tool` once if given, then answers; records what every call
    /// was sent. `hold_first` keeps the first call open until released.
    struct StepRecordingModel {
        tool: Option<&'static str>,
        hold_first: Option<Arc<tokio::sync::Notify>>,
        sent: Arc<std::sync::Mutex<Vec<Vec<base::interface::model::ModelMessage>>>>,
    }
    #[async_trait::async_trait]
    impl Model for StepRecordingModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            messages: Vec<base::interface::model::ModelMessage>,
            params: base::interface::model::StreamParams,
            _cancel: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            use base::interface::model::ModelEvent;
            let turn_call = matches!(
                params.origin.as_ref().map(|o| &o.kind),
                Some(base::interface::model::CallKind::Turn { .. })
            );
            if !turn_call {
                return Ok(Box::new(futures::stream::iter(vec![Ok(
                    ModelEvent::EndTurn {
                        stop_reason: "end_turn".into(),
                        usage: Default::default(),
                    },
                )])));
            }
            let call_no = {
                let mut sent = self.sent.lock().unwrap();
                sent.push(messages);
                sent.len() - 1
            };
            if call_no == 0 {
                if let Some(hold) = &self.hold_first {
                    hold.notified().await;
                }
            }
            let events = match (call_no, self.tool) {
                (0, Some(tool)) => vec![
                    Ok(ModelEvent::ToolUse {
                        id: "toolu_1".into(),
                        name: tool.into(),
                        input: serde_json::json!({}),
                    }),
                    Ok(ModelEvent::EndTurn {
                        stop_reason: "tool_use".into(),
                        usage: Default::default(),
                    }),
                ],
                _ => vec![
                    Ok(ModelEvent::TextDelta {
                        text: format!("answer {call_no}"),
                    }),
                    Ok(ModelEvent::EndTurn {
                        stop_reason: "end_turn".into(),
                        usage: Default::default(),
                    }),
                ],
            };
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    fn text_of(m: &base::interface::model::ModelMessage) -> String {
        m.content
            .iter()
            .filter_map(|b| match b {
                base::interface::model::ModelContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    async fn next_event(
        rx: &mut EventReceiver,
        mut want: impl FnMut(&AgentEvent) -> bool,
        what: &str,
    ) -> AgentEvent {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Some(e) if want(&e) => break e,
                    Some(_) => continue,
                    None => panic!("event channel closed while waiting for {what}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    async fn steer(input_tx: &InputSender, id: &str, text: &str) -> crate::steering::SteerAck {
        let (ack, acked) = tokio::sync::oneshot::channel();
        input_tx
            .send(InputMessage::Steer {
                content: text.into(),
                message_id: id.into(),
                ack: Some(ack),
            })
            .unwrap();
        acked.await.expect("the demultiplexer always answers")
    }

    #[tokio::test]
    async fn compacting_on_request_works_far_below_the_threshold() {
        let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
            tool: None,
            hold_first: None,
            sent: Default::default(),
        });
        let (mut agent, mut event_rx, _input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        for i in 0..40 {
            agent
                .session
                .push_message(base::interface::model::ModelMessage {
                    role: base::interface::model::MessageRole::User,
                    content: vec![base::interface::model::ModelContentBlock::Text {
                        text: format!("question {i}: {}", "words ".repeat(50)),
                    }],
                });
            agent
                .session
                .push_message(base::interface::model::ModelMessage {
                    role: base::interface::model::MessageRole::Assistant,
                    content: vec![base::interface::model::ModelContentBlock::Text {
                        text: format!("answer {i}: {}", "words ".repeat(50)),
                    }],
                });
        }
        let before = agent.session.messages().len();
        assert!(
            agent.context_tokens() < agent.context_budget().compact_threshold,
            "the point is that the budget would not have compacted this"
        );

        let done = agent.compact_now().await.expect("something was compacted");
        assert_eq!(done.messages_before, before);
        assert!(done.messages_after < before, "{done:?}");
        assert!(done.tokens_after < done.tokens_before, "{done:?}");
        assert_eq!(agent.session.messages().len(), done.messages_after);

        let event = next_event(
            &mut event_rx,
            |e| matches!(e, AgentEvent::CompactAction { .. }),
            "the compaction event",
        )
        .await;
        assert!(matches!(
            event,
            AgentEvent::CompactAction { tokens_before, tokens_after, .. }
                if tokens_before == done.tokens_before && tokens_after == done.tokens_after
        ));

        let mut empty = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(Arc::new(StepRecordingModel {
                tool: None,
                hold_first: None,
                sent: Default::default(),
            }))
            .settings(Arc::new(test_settings()))
            .skip_warmup(true)
            .build()
            .expect("build should succeed")
            .0;
        assert_eq!(empty.compact_now().await, None, "nothing to compact");
    }

    #[tokio::test]
    async fn a_message_sent_during_a_tool_step_reaches_the_next_model_call() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(GateTool(gate.clone())));
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
            tool: Some("Gate"),
            hold_first: None,
            sent: sent.clone(),
        });
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(tools)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });

        input_tx
            .send(InputMessage::User {
                content: "start".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();
        next_event(
            &mut event_rx,
            |e| matches!(e, AgentEvent::ToolUse { name, .. } if name == "Gate"),
            "the tool call",
        )
        .await;

        assert_eq!(
            steer(&input_tx, "m1", "also check the tests").await,
            crate::steering::SteerAck::Accepted
        );
        gate.notify_one();

        let delivered = next_event(
            &mut event_rx,
            |e| matches!(e, AgentEvent::UserMessageDelivered { .. }),
            "the delivery",
        )
        .await;
        assert!(matches!(
            delivered,
            AgentEvent::UserMessageDelivered { ref message_id, ref turn_id, .. }
                if message_id == "m1" && turn_id == "t1"
        ));
        next_event(
            &mut event_rx,
            |e| matches!(e, AgentEvent::TurnComplete { .. }),
            "the end of the turn",
        )
        .await;

        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2, "one call for the tool, one after it");
        let second = &sent[1];
        let at = second
            .iter()
            .position(|m| text_of(m) == "also check the tests")
            .expect("the second call carries the message");
        let result_at = second
            .iter()
            .position(|m| {
                m.content.iter().any(|b| {
                    matches!(
                        b,
                        base::interface::model::ModelContentBlock::ToolResult { .. }
                    )
                })
            })
            .expect("and the tool result");
        assert!(
            result_at < at,
            "after the tool result, never between the pair"
        );
        assert_eq!(at, second.len() - 1);

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    #[tokio::test]
    async fn a_message_sent_while_the_model_answers_is_answered_in_the_same_turn() {
        let hold = Arc::new(tokio::sync::Notify::new());
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
            tool: None,
            hold_first: Some(hold.clone()),
            sent: sent.clone(),
        });
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });

        input_tx
            .send(InputMessage::User {
                content: "first".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while sent.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the first call should start");

        assert_eq!(
            steer(&input_tx, "m1", "and one more thing").await,
            crate::steering::SteerAck::Accepted
        );
        hold.notify_one();

        let done = next_event(
            &mut event_rx,
            |e| matches!(e, AgentEvent::TurnComplete { .. }),
            "the end of the turn",
        )
        .await;
        assert!(matches!(
            done,
            AgentEvent::TurnComplete { api_calls: 2, .. }
        ));
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(text_of(sent[1].last().unwrap()), "and one more thing");

        assert_eq!(
            steer(&input_tx, "m2", "too late").await,
            crate::steering::SteerAck::NotRunning,
            "an idle session takes no steering; the sender keeps the message"
        );

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }

    #[tokio::test]
    async fn an_interrupted_turn_hands_back_what_it_never_delivered() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(GateTool(gate)));
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
            tool: Some("Gate"),
            hold_first: None,
            sent: sent.clone(),
        });
        let (mut agent, mut event_rx, input_tx) = Builder::new()
            .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
            .model(model)
            .settings(Arc::new(test_settings()))
            .tools(tools)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });

        input_tx
            .send(InputMessage::User {
                content: "start".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            })
            .unwrap();
        next_event(
            &mut event_rx,
            |e| matches!(e, AgentEvent::ToolUse { .. }),
            "the tool call",
        )
        .await;
        assert_eq!(
            steer(&input_tx, "m1", "never mind").await,
            crate::steering::SteerAck::Accepted
        );
        input_tx
            .send(InputMessage::System {
                kind: EngineCommand::CancelTurn,
                content: String::new(),
            })
            .unwrap();

        let returned = next_event(
            &mut event_rx,
            |e| {
                matches!(
                    e,
                    AgentEvent::UserMessagesReturned { .. }
                        | AgentEvent::UserMessageDelivered { .. }
                        | AgentEvent::TurnComplete { .. }
                )
            },
            "the hand-back",
        )
        .await;
        match returned {
            AgentEvent::UserMessagesReturned { messages, .. } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].message_id, "m1");
            }
            other => panic!("expected the message back before the turn ended, got {other:?}"),
        }
        let done = next_event(
            &mut event_rx,
            |e| matches!(e, AgentEvent::TurnComplete { .. }),
            "the end of the turn",
        )
        .await;
        assert!(matches!(
            done,
            AgentEvent::TurnComplete { ref stop_reason, .. } if stop_reason == "cancelled"
        ));
        assert_eq!(sent.lock().unwrap().len(), 1, "the model never saw it");

        drop(input_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    }
}
