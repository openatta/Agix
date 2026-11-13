//! SessionPool —— daemon 多 session 实例管理器。
//!
//! 每个 session 对应一个独立的 Agent 实例（后台 run loop + 独立 event channel）。
//! 支持：
//! - 按 session_id 查找/创建
//! - 容量上限 + LRU 驱逐
//! - 空闲超时回收
//! - session.list 合并活跃 + 历史

use base::event::AgentEvent;
use base::id::Id;
use base::interface::permission::PermissionSet;
use base::interface::scene::SceneSpec;
use base::settings::{Divergence, RecorderConfig, RecorderMode};
use base::settings::{LoadedSettings, Settings};
#[cfg(feature = "mcp")]
use mcp::manager::McpManager;
use model::client::AnthropicClient;
use runtime::agent::{Builder, EngineCommand, EventReceiver, InputMessage, InputSender};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use telemetry::file_recorder::FileRecorder;
use telemetry::recorder::RecorderModel;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub mod api;
mod builder;
mod facade;
#[cfg(feature = "mcp")]
mod mcp_cache;
mod mcp_conn;
#[cfg(feature = "mcp")]
mod mcp_status;
mod parts;
mod permission_scripts;
mod pool_telemetry;
#[cfg(test)]
mod tests;

use api::{
    emitted_outside_turns, AskKind, Busy, Initiator, PoolError, SessionEvent, SessionFrame,
    SessionOptions, SessionWatcher, Subscribed, TurnOutcome, TurnRequest, WatcherId,
};
use facade::helpers::*;
use facade::history_rpc::*;
use facade::settings_reload::*;
#[cfg(feature = "mcp")]
use mcp_status::*;
#[cfg(feature = "mcp")]
use parts::bindings::{tier_covers, ProjectSceneKey};
use parts::bindings::{BindingCache, Cached, ManagedLayer, ProjectBinding};
use parts::directory::SessionDirectory;
use parts::events::DaemonEvents;
use parts::hub::SessionHub;
use parts::phase::{Generation, Generations, RunningTurn, TurnSlot, TurnTicket};
use parts::scenes::SceneCatalog;
use parts::scheduler::{Scheduler, TurnLauncher};
use parts::specs::{SessionSpec, SpecStore};
use permission_scripts::*;
use pool_telemetry::*;

/// Outcome of comparing a session's recorded `Meta.scene` (if any) against
/// every scene this daemon currently serves (`SceneCatalog::is_active`) —
/// see docs/ARCHITECTURE.md §3.1.
/// Resume/fork/close/delete don't yet take a per-request `scene` param
/// (every caller implicitly means "whichever active scene this session was
/// actually recorded under"), so `Mismatch`'s reported `requested_scene`
/// is always this pool's *default* scene — that only reflects what a
/// mismatch response echoes back, not what counts as a match.
enum SceneCheck {
    /// No `Meta` line, or one with no `scene` field (pre-v2 file, or a
    /// session with no history at all) — nothing to compare against.
    /// Callers proceed and report `scene_inferred: true`.
    Inferred,
    /// Recorded scene matches — proceed normally.
    Matches,
    /// Recorded scene differs — callers should reject with
    /// `SCENE_MISMATCH` before touching anything.
    Mismatch(String),
}

/// `session.create`'s `project_root` disambiguated three ways — JSON only
/// gives "key omitted" vs. "key present" for free; `null` vs. a string
/// needs an explicit check the wire layer (`server/`) does before this
/// ever reaches `SessionPool`.
pub enum ProjectSelector {
    /// `project_root` omitted — this pool's default project (`self.cwd()`).
    Default,
    /// `project_root: null` — a genuine no-project session (global tier).
    NoProject,
    /// `project_root: "<path>"` — must exist; `session.create` rejects it
    /// otherwise with `PROJECT_NOT_FOUND` (unlike resume, which tolerates a
    /// moved/missing project — see `settings_for_project`'s doc comment).
    Path(PathBuf),
}

/// What a [`SessionCustomizer`] is told about the session being built.
pub struct SessionCtx<'a> {
    pub session_id: &'a str,
    pub scene: &'a str,
    /// `None` for a no-project session.
    pub project_root: Option<&'a Path>,
    /// Whether the session already has a history this build continues —
    /// a resume, or a rebuild after a settings change or an eviction.
    pub resumed: bool,
}

/// A host's last word on every session engine the pool builds: it is handed
/// the configured [`Builder`] and returns the one to build. Middleware, event
/// sinks, an elicitation handler — whatever `runtime` exposes on `Builder`
/// and the daemon does not.
///
/// Applied on every build of a session, not only the first: a rebuild after a
/// settings change, an eviction or a restart goes through it again, so what
/// it adds is never quietly missing from the rebuilt engine.
pub type SessionCustomizer = Arc<dyn Fn(Builder, &SessionCtx<'_>) -> Builder + Send + Sync>;

/// How long a stopping Agent gets to finish its turn and persist it.
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(5);

/// How long a process group left behind by an ended session gets on
/// `SIGTERM` before `SIGKILL`.
const PROCESS_GROUP_GRACE: Duration = Duration::from_millis(500);

/// What `session.run_turn` does when the session is already running a turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnBusy {
    /// Refuse with `SESSION_BUSY`; the message is not delivered.
    #[default]
    Reject,
    /// Wait for the running turn to end, then run this one.
    Queue,
    /// Hand the message to the running turn, which adds it at its next step
    /// boundary. Falls back to `Queue` when the turn is already past its
    /// last step, or when the message has attachments.
    Steer,
}

/// Which part of a task's output `session.taskOutput` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutputRead {
    /// The last this many bytes.
    Tail(usize),
    /// From this byte of everything the work printed to the end.
    From(u64),
}

const ENGINE_NS_PREFIX: &str = "agix.";

/// Where a session's first-turn naming call goes: the session's own `title`
/// route when its project has one, else its conversation model.
///
/// Taken from the clients the session was built with, so a recorded session
/// records the call and a replayed one is answered from the recording.
#[derive(Clone)]
struct SessionNamer {
    session_id: String,
    model: Arc<dyn base::interface::model::Model>,
    model_name: String,
}

impl SessionNamer {
    fn of(
        session_id: &str,
        task_router: Option<&base::provider::TaskRouter>,
        model: &Arc<dyn base::interface::model::Model>,
        model_name: &str,
    ) -> Self {
        let title = base::interface::model::call_purpose::TITLE;
        let routed = task_router.and_then(|router| {
            Some((
                router.model_for(title),
                router.model_name_for(title)?.to_string(),
            ))
        });
        let (model, model_name) = routed.unwrap_or_else(|| (model.clone(), model_name.to_string()));
        Self {
            session_id: session_id.to_string(),
            model,
            model_name,
        }
    }

    /// `Ok(None)` when a replay holds no naming call: the session stays
    /// unnamed rather than the call going anywhere else.
    async fn name(&self, prompt: &str) -> Result<Option<String>, String> {
        use base::interface::model::{
            CallOrigin, MessageRole, ModelContentBlock, ModelErrorKind, ModelEvent, ModelMessage,
            StreamParams,
        };
        use base::prompt::PromptBlock;
        use futures::StreamExt;

        let system = PromptBlock::system(
            "你是一个简洁的标题生成器。只输出 3-5 个词的中文标题，不要任何解释。",
        );
        let messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: prompt.to_string(),
            }],
        }];
        let params = StreamParams {
            model: self.model_name.clone(),
            max_tokens: 50,
            thinking_mode: base::settings::ThinkingMode::Off,
            effort: None,
            fallback_model: None,
            cache_edits: vec![],
            origin: Some(CallOrigin::auxiliary(
                self.session_id.clone(),
                base::interface::model::call_purpose::TITLE,
            )),
            input_map: None,
            provider_search: None,
        };

        let stream = match self
            .model
            .stream(
                vec![system],
                vec![],
                messages,
                params,
                CancellationToken::new(),
            )
            .await
        {
            Ok(stream) => stream,
            Err(e) if e.kind == ModelErrorKind::NotRecorded => return Ok(None),
            Err(e) => return Err(format!("LLM name error: {e}")),
        };

        tokio::pin!(stream);
        let mut name = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(ModelEvent::TextDelta { text }) => name.push_str(&text),
                Ok(ModelEvent::EndTurn { .. }) => break,
                Err(e) => return Err(format!("LLM name stream error: {e}")),
                _ => {}
            }
        }

        let name = name.trim().trim_matches('"').trim().to_string();
        if name.is_empty() {
            Err("empty name generated".into())
        } else {
            Ok(Some(name))
        }
    }
}

struct LiveSession {
    input_tx: InputSender,
    /// Which engine this entry is, what it is doing, and the receiver its
    /// turns take. A session runs one turn at a time; the running one is
    /// named here, so a refusal can name it
    /// (`SESSION_BUSY.data.current_turn_id`) and `session.get` can report
    /// `turn_state`.
    turn: TurnSlot,
    cancel: CancellationToken,
    /// Cancelled once the Agent's task has returned and dropped the Agent.
    exited: CancellationToken,
    /// How full the context was after the last turn that called the model.
    context: Option<base::event::ContextUsage>,
    /// The scene this session was created under — the one its `Meta` records,
    /// not this pool's default. Auto-naming is the one behaviour decided per
    /// turn rather than at build time, so it is the one that needs the scene
    /// kept here to ask.
    scene: Arc<SceneSpec>,
    /// The project this session is bound to, `None` for a no-project session.
    project: Option<PathBuf>,
    /// Session name（CHAT 场景首轮后由 LLM 生成；CODING 为 None）。
    name: Option<String>,
    created_at: Instant,
    /// 用于首轮命名判断。
    is_first_turn: bool,
    /// Set by `session.rename`. The first-turn auto-namer skips a session
    /// that has one: a name somebody typed outranks a name a model guessed,
    /// and the guess arrives later.
    named_by_user: bool,
    namer: SessionNamer,
    /// The `stale` flag of the binding this Agent was built from. `run_turn`
    /// rebuilds the session before its next turn once it is set — lazily,
    /// so an invalidation never has to walk the sessions.
    binding_stale: Arc<std::sync::atomic::AtomicBool>,
    /// The engine's own session state: the tasks `session.tasks` lists.
    state: Option<Arc<base::context::SessionState>>,
    /// How the engine delegates, for `session.startAgent`.
    delegator: Option<Arc<dyn base::interface::delegate::Delegator>>,
    /// What this session's bound scripts did, if it bound any.
    ///
    /// Kept because a script that quietly does nothing is the failure mode of
    /// this whole tier, and a log line on the daemon's stderr is not something
    /// the person asking "why is my script not working" has. `session.get`
    /// reads it.
    scripts: Option<Arc<base::interface::script::ScriptLedger>>,
    /// The checks this session's plugins registered, for `session.get`.
    health: Arc<base::interface::health::HealthChecks>,
    /// Who watches this session, what it is still asking, and when it was
    /// last active. Every frame the session produces goes to all of its
    /// watchers; a turn is not owned by the one that started it. Kept across
    /// a rebuild, so the watchers and the open asks go on with the session.
    hub: Arc<SessionHub>,
    /// The process groups this session's commands started, kept when
    /// `sandbox.reap_process_groups` is on and stopped when the session ends.
    process_groups: Option<Arc<platform::GroupLedger>>,
}

/// What `session.respondToPrompt` delivers, by `prompt_type`.
#[derive(Debug, Clone)]
pub enum PromptAnswer {
    Permission(runtime::agent::PermissionDecision),
    Question(runtime::agent::QuestionAnswer),
}

impl PromptAnswer {
    pub fn kind(&self) -> AskKind {
        match self {
            Self::Permission(_) => AskKind::Permission,
            Self::Question(_) => AskKind::Question,
        }
    }
}

pub struct SessionPool {
    sessions: AsyncMutex<HashMap<String, LiveSession>>,
    /// Per-`session_id` mutexes serializing a session's own disk-mutating
    /// critical sections against each other — see `with_session_lock`.
    /// Entries are dropped again once nothing references them, so this
    /// stays proportional to concurrently-contended ids, not every id this
    /// pool has ever touched.
    session_locks: AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    cap: usize,
    idle_timeout: Duration,
    /// The default scene, the scenes served now, and every scene this pool
    /// can serve.
    scenes: SceneCatalog,
    /// The host's own permission, handed to a session whose effective mode
    /// is `bypassPermissions` in place of the rule gate (see
    /// `resolve_session_permission`). `None` when the host supplied none:
    /// such a session gets the gate too, which under bypass consults no rule
    /// but still validates input and still follows a later `EnterPlanMode`.
    permission: Option<PermissionSet>,
    /// Built-in tool registry (Bash/Read/Write/Edit/Grep/Glob/...), shared
    /// (cheap `Arc` clone) by every session `Builder`. Built once here via
    /// `tools::register_builtin_tools`; `Builder::build()` on its own registers
    /// only Skill/TaskStop/TaskOutput/Import/Agent/MCP. See
    /// `tools::register_builtin_tools`'s doc comment for what's deliberately
    /// excluded.
    tools: Arc<base::tool::InMemoryToolRegistry>,
    /// The sessions on disk: what their logs record, and the projects this
    /// pool has served.
    directory: SessionDirectory,
    /// Centrally-connected MCP servers, partitioned by project — keyed the
    /// same way `settings_for_project`/`ProjectSelector` are (`None` = the
    /// no-project tier, `Some(root)` = that project). The pool's default
    /// project (this daemon's `--cwd`) connects eagerly at startup (see
    /// `connect_mcp_servers_in_background`); every other project connects
    /// lazily on its first session (see `mcp_for_project`). Each new session
    /// still gets its own owned `McpManager` built from the cached entry's
    /// `McpClientHandle`s (cheap `Arc` clones, no new connection) — see
    /// `create()`.
    /// Keyed by `(project, scene)` for the same reason `projects` is: a
    /// scene tier can declare its own `mcp_servers`, so two scenes on one
    /// project are not interchangeable.
    #[cfg(feature = "mcp")]
    mcp_by_project: mcp_cache::McpCache,
    /// This pool's MCP authorization: sign-in state per server, the flows in
    /// progress, and the credential store (files under its own user data
    /// root unless the host gave one).
    #[cfg(feature = "mcp")]
    mcp_auth: Arc<mcp::oauth::McpAuth>,
    /// What every MCP server this pool connects is admitted by — see
    /// `admit_mcp`.
    #[cfg(feature = "mcp")]
    mcp_policy: Arc<mcp::policy::McpPolicyGate>,
    /// Every session's scheduled jobs, fired as turns of their sessions
    /// whether or not the session is in memory.
    scheduler: Scheduler,
    /// This pool, for the tasks it starts that outlive the call that started
    /// them. Set by `Arc::new_cyclic` in the builder, so always there.
    this: std::sync::Weak<SessionPool>,
    /// Daemon-level notifications (`daemon.subscribeEvents`) and the
    /// pool-level telemetry.
    events: DaemonEvents,
    /// The plugin subsystem — see `crate::plugins`. Everything plugins
    /// contribute arrives through it, and in a build without the `plugins`
    /// feature it is a stub that contributes nothing.
    plugins: Arc<crate::plugins::PluginSubsystem>,
    /// This daemon's own settings, every `(project, scene)` pair's binding,
    /// and the watch on the files they come from.
    bindings: BindingCache,
    /// Process-scope diagnostics, reported by `daemon.doctor` — the plugin
    /// fault records, plus whatever an embedder registered with
    /// [`SessionPool::register_health_check`]. Not the same set as a
    /// session's: a session's checks come and go with the session, and one
    /// shared registry would grow an entry per session forever.
    health: std::sync::RwLock<base::interface::health::HealthChecks>,
    /// Each session's [`SessionSpec`]: what every rebuild of it applies.
    specs: SpecStore,
    /// The engines taken out of `sessions` that may still be stopping, by
    /// session id. A session brought back under one of these ids waits for
    /// that engine first, so the two never run side by side.
    stopping: std::sync::Mutex<HashMap<String, CancellationToken>>,
    /// Set once [`Self::shutdown_all`] starts; no session is created after.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Numbers every engine this pool brings up, so a turn can tell the entry
    /// it started on from one brought up later under the same id.
    generations: Generations,
    /// Cancelled when [`Self::shutdown_all`] has finished.
    shut_down: CancellationToken,
    /// How long a `kind:"prompt"` permission ask may sit unanswered before
    /// the daemon answers it with a `Deny` on the client's behalf.
    /// `Duration::ZERO` disables the timer entirely (wait forever — the
    /// engine's own semantics, appropriate for a host that really does keep
    /// a human in the loop indefinitely).
    ///
    /// Defaults to [`DEFAULT_PERMISSION_PROMPT_TIMEOUT`]; `daemon/src/main.rs`
    /// overrides it from `--permission-prompt-timeout`. See
    /// `LiveSession.hub` for why this lives here rather than in
    /// the engine.
    permission_prompt_timeout: Duration,
    rust_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    customize: Option<SessionCustomizer>,
}

/// Default ceiling on an unanswered `kind:"prompt"` permission ask.
///
/// Five minutes: long enough that a human being asked "allow Bash?" in an
/// IDE has time to read it and switch windows, short enough that a client
/// which simply doesn't implement `session.respondToPrompt` fails visibly
/// (one denied tool call, turn continues) instead of hanging its RPC
/// forever. Fail *closed* — the timeout denies, it never auto-approves.
pub const DEFAULT_PERMISSION_PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// session.list 返回的单条记录。
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SessionInfo {
    pub session_id: String,
    pub name: Option<String>,
    pub preview: Option<String>,
    pub message_count: u32,
    pub created_at: String,
    pub last_active: String,
    pub status: SessionStatus,
    #[schemars(with = "api::SessionKindName")]
    pub session_kind: history::entry::SessionKind,
    /// Scene the session was created under, and the project it is bound to.
    /// Both always present, `null` when the log does not record them (pre-v2
    /// files) — a client grouping by either can then tell "no project" from
    /// "not known" without a presence check.
    pub scene: Option<String>,
    pub project_root: Option<String>,
    /// How many times this session's context has been compacted. Counted as
    /// the log is written, so a session that is only on disk answers too.
    pub compact_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// `false` only for a sidechain that already ran its one-shot task to
    /// conclusion — `session.resume` against it returns `SIDECHAIN_TERMINAL`.
    pub resumable: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Inactive,
}

/// What `SessionPool::admit_mcp_servers` lets through, and what it does not:
/// `refused` names every entry not connected (invalid or denied), `denied`
/// the policy's share of them, kept for `mcp.status`.
#[cfg(feature = "mcp")]
#[derive(Default)]
struct AdmittedMcp {
    servers: HashMap<String, mcp::config::McpServerConfig>,
    denied: Vec<mcp::manager::DeniedServer>,
    refused: Vec<String>,
}

/// The `mcp_servers` keys the project tier's own files (`settings.json`,
/// `settings.local.json`) declare. A key a project sets at all, even one
/// field of a global server's, makes that server the project's.
#[cfg(feature = "mcp")]
fn project_tier_mcp_servers(settings: &LoadedSettings) -> std::collections::HashSet<String> {
    settings.project_mcp_servers().keys().cloned().collect()
}

/// What `project.status` answers for `root`, from the settings its sessions
/// are built with.
fn project_trust_report(
    root: &Path,
    settings: &LoadedSettings,
    plugin_agent_types: &[base::agent_type::AgentTypeDefinition],
) -> api::ProjectStatus {
    #[cfg_attr(not(any(feature = "agents", feature = "skills")), allow(unused_mut))]
    let mut requires_trust = settings.requires_trust().to_vec();
    #[cfg(feature = "agents")]
    requires_trust.extend(agents::agent_tool::project_agent_types_requiring_trust(
        settings,
        plugin_agent_types,
    ));
    #[cfg(not(feature = "agents"))]
    let _ = plugin_agent_types;
    #[cfg(feature = "skills")]
    requires_trust.extend(skills::module::project_skills_requiring_trust(settings));
    api::ProjectStatus {
        project_root: root.to_path_buf(),
        trust: settings.trust().into(),
        matched: settings.trust_pattern().map(str::to_string),
        default: settings.project_trust.default,
        requires_trust,
    }
}

/// How long a session's creation waits for its project's MCP servers to
/// connect. One still connecting then goes on in the background and joins
/// the project's sessions at their next turn.
#[cfg(feature = "mcp")]
const MCP_SESSION_CONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(feature = "mcp")]
fn mcp_connect_failed_event(manager: &McpManager, name: &str) -> api::DaemonEvent {
    let error = manager
        .failed()
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.error.clone())
        .unwrap_or_else(|| "connect failed — see daemon log for the specific reason".into());
    api::DaemonEvent::McpConnectFailed {
        server: name.to_string(),
        error,
    }
}

#[cfg(feature = "mcp")]
fn mcp_connected_event(status: &mcp::manager::ServerStatus) -> api::DaemonEvent {
    api::DaemonEvent::McpConnected {
        server: status.name.clone(),
        transport: status.transport.to_string(),
        tool_count: status.tool_count,
    }
}

impl SessionPool {
    /// This pool's MCP authorization state.
    #[cfg(feature = "mcp")]
    pub fn mcp_auth(&self) -> Arc<mcp::oauth::McpAuth> {
        self.mcp_auth.clone()
    }

    /// Subscribe to daemon-level async notifications (MCP connect
    /// outcomes, import auto-detection, ...) — see `daemon.subscribeEvents`.
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<api::DaemonEvent> {
        self.events.subscribe()
    }

    /// Publish a daemon-level notification to every current subscriber. A
    /// `send` error just means nobody's listening right now — not a real
    /// failure, so this is intentionally infallible from the caller's side.
    pub fn emit_event(&self, event: api::DaemonEvent) {
        self.events.emit(event);
    }

    /// Record how long the daemon took to start, in the pool-level
    /// telemetry.
    pub fn record_startup(&self, timing: telemetry::StartupTimingPayload) {
        self.events.record_startup(timing);
    }

    /// The project directory this pool's sessions run in.
    pub fn cwd(&self) -> &Path {
        self.bindings.cwd()
    }

    /// Full slash command catalog — built-in locals + skill-derived
    /// (see `build_shared_commands`) — for the
    /// `commands.list` RPC. Executing one is not a separate RPC: send
    /// `/name args` as the `message` of `session.run_turn`, which already
    /// intercepts and runs it (see `runtime::turn::process_turn`).
    pub async fn list_commands(&self) -> Vec<api::CommandInfo> {
        self.default_binding()
            .await
            .commands
            .list_detailed()
            .into_iter()
            .map(api::CommandInfo::from)
            .collect()
    }
}

/// A session in `Rebuilding`, the entry of `generation`.
struct HeldSlot {
    generation: Generation,
}

/// Why [`SessionPool::rebuild_idle`] did not rebuild.
enum Rebuild {
    NotLive,
    Busy(Busy),
    Failed(CreateError),
}

/// Why a session was not brought up under its id.
#[derive(Debug)]
pub(super) enum CreateError {
    /// The engine the id last ran on is still running past
    /// [`AGENT_EXIT_GRACE`]; a second one would write the same transcript.
    PreviousEngineRunning {
        session_id: String,
        message: String,
    },
    Failed(String),
}

impl From<String> for CreateError {
    fn from(e: String) -> Self {
        Self::Failed(e)
    }
}

impl CreateError {
    pub(super) fn previous_engine_running(sid: &str) -> Self {
        Self::PreviousEngineRunning {
            session_id: sid.to_string(),
            message: format!(
                "session `{sid}` is still stopping on its previous engine; try again once it has"
            ),
        }
    }

    pub(super) fn context(self, outer: impl FnOnce(String) -> String) -> Self {
        match self {
            Self::PreviousEngineRunning {
                session_id,
                message,
            } => Self::PreviousEngineRunning {
                session_id,
                message: outer(message),
            },
            Self::Failed(e) => Self::Failed(outer(e)),
        }
    }
}

impl From<CreateError> for PoolError {
    fn from(e: CreateError) -> Self {
        match e {
            CreateError::PreviousEngineRunning {
                session_id,
                message,
            } => Self::SessionBusy(Busy::previous_engine(&session_id, message)),
            CreateError::Failed(e) => Self::Internal(e),
        }
    }
}

impl Rebuild {
    fn into_error(self, session_id: &str) -> PoolError {
        match self {
            Rebuild::Busy(busy) => PoolError::SessionBusy(busy),
            Rebuild::NotLive => PoolError::session_gone(session_id, "session is no longer running"),
            Rebuild::Failed(e) => e.into(),
        }
    }
}

/// What a host asks of a session's goal.
#[derive(Debug, Clone)]
pub enum GoalChange {
    Set {
        objective: String,
        max_rounds: Option<u32>,
    },
    Edit {
        objective: Option<String>,
        max_rounds: Option<u32>,
    },
    Pause,
    Resume,
    Clear,
}

/// Run `work` — disk reads and directory scans — on the blocking pool rather
/// than on a runtime worker.
async fn off_the_runtime<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => panic!("blocking work did not finish: {e}"),
    }
}
