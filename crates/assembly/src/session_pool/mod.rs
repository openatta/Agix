//! SessionPool —— daemon 多 session 实例管理器。
//!
//! 每个 session 对应一个独立的 Agent 实例（后台 run loop + 独立 event channel）。
//! 池本身是门面：状态按所有权分给 `parts/` 里的协作者（在内存的会话、规格、
//! 绑定、场景、MCP、磁盘上的会话、造引擎、定时任务、守护进程事件），`facade/`
//! 里的方法在锁外编排它们。对外只用 `api/` 的领域类型。组成与锁序见
//! docs/ARCHITECTURE.md「会话池的组成与锁序」。

use base::event::AgentEvent;
use base::id::Id;
use base::interface::permission::{PermissionPolicy, PermissionSet};
use base::interface::scene::SceneSpec;
use base::settings::{LoadedSettings, Settings};
use base::settings::{RecorderConfig, RecorderMode};
use model::client::AnthropicClient;
use runtime::agent::{Builder, EngineCommand, EventReceiver, InputMessage, InputSender};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub mod api;
mod builder;
mod facade;
#[cfg_attr(feature = "mcp", path = "parts/mcp_hub/live.rs")]
#[cfg_attr(not(feature = "mcp"), path = "parts/mcp_hub/off.rs")]
mod mcp_hub;
mod parts;
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
use mcp_hub::McpHub;
pub(crate) use mcp_hub::McpSetup;
pub use mcp_hub::{McpOptions, COMPILED as MCP_COMPILED};
use parts::bindings::{
    BindingCache, Cached, HostOverrides, ManagedLayer, ProjectBinding, ProjectSceneKey,
};
use parts::directory::SessionDirectory;
use parts::events::DaemonEvents;
use parts::factory::{
    effective_permission_mode, BuildRequest, BuiltEngine, EngineFactory, SessionNamer,
};
use parts::hub::SessionHub;
use parts::live::{LiveSession, LiveSessions, SessionCore};
use parts::phase::{Generation, RunningTurn, TurnSlot, TurnTicket};
use parts::scenes::SceneCatalog;
use parts::scheduler::{Scheduler, TurnLauncher};
use parts::specs::{SessionSpec, SpecStore};
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
    /// This pool, for the tasks it starts that outlive the call that started
    /// them. Set by `Arc::new_cyclic` in the builder, so always there.
    this: std::sync::Weak<SessionPool>,
    /// The sessions in memory, and what brings them up and takes them down.
    live: LiveSessions,
    /// Each session's [`SessionSpec`]: what every rebuild of it applies.
    specs: SpecStore,
    /// This daemon's own settings, every `(project, scene)` pair's binding,
    /// and the watch on the files they come from.
    bindings: BindingCache,
    /// The default scene, the scenes served now, and every scene this pool
    /// can serve.
    scenes: SceneCatalog,
    /// MCP: the connections per `(project, scene)`, sign-ins and the policy
    /// servers are admitted by. A stub in a build without the `mcp` module.
    mcp: McpHub,
    /// The plugin subsystem — see `crate::plugins`. Everything plugins
    /// contribute arrives through it, and in a build without the `plugins`
    /// feature it is a stub that contributes nothing.
    plugins: Arc<crate::plugins::PluginSubsystem>,
    /// The sessions on disk: what their logs record, and the projects this
    /// pool has served.
    directory: SessionDirectory,
    /// Builds each session's engine.
    factory: EngineFactory,
    /// Every session's scheduled jobs, fired as turns of their sessions
    /// whether or not the session is in memory.
    scheduler: Scheduler,
    /// Daemon-level notifications (`daemon.subscribeEvents`) and the
    /// pool-level telemetry.
    events: DaemonEvents,
    /// Process-scope diagnostics, reported by `daemon.doctor` — the plugin
    /// fault records, plus whatever an embedder registered with
    /// [`SessionPool::register_health_check`]. Not the same set as a
    /// session's: a session's checks come and go with the session, and one
    /// shared registry would grow an entry per session forever.
    health: std::sync::RwLock<base::interface::health::HealthChecks>,
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

impl SessionPool {
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

    /// The `RunCode` budget this pool's sessions share: how many programs
    /// their conversations may run at once. Other pools have their own.
    pub fn run_slots(&self) -> Arc<tokio::sync::Semaphore> {
        self.factory.run_slots().clone()
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
