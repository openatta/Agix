//! SessionPool —— daemon 多 session 实例管理器。
//!
//! 每个 session 对应一个独立的 Agent 实例（后台 run loop + 独立 event channel）。
//! 支持：
//! - 按 session_id 查找/创建
//! - 容量上限 + LRU 驱逐
//! - 空闲超时回收
//! - session.list 合并活跃 + 历史

use crate::rpc::{codes, RpcResponse, SessionOptions, StreamFrame};
use base::id::Id;
use base::interface::event::AgentEvent;
use base::interface::permission::Permission;
use base::interface::scene::Scene;
use base::interface::settings::Settings;
use base::interface::settings::{Divergence, RecorderConfig, RecorderMode};
#[cfg(feature = "mcp")]
use mcp::manager::McpManager;
use model::adapter::AnthropicModel;
use model::client::AnthropicClient;
use runtime::agent::{Builder, EngineCommand, EventReceiver, InputMessage, InputSender};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use telemetry::file_recorder::FileRecorder;
use telemetry::recorder::RecorderModel;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::RwLock as AsyncRwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

mod background;
mod bindings;
mod create;
mod goal;
mod helpers;
mod history_rpc;
mod listing;
mod mcp_conn;
mod permission_scripts;
mod plugins;
mod pool_telemetry;
mod resume;
mod scenes;
mod session_ops;
mod settings_reload;
mod subscribers;
mod tasks;
mod teardown;
#[cfg(test)]
mod tests;
mod turn;

use helpers::*;
use history_rpc::*;
use permission_scripts::*;
use pool_telemetry::*;
use settings_reload::*;

/// Outcome of comparing a session's recorded `Meta.scene` (if any) against
/// every scene this daemon currently serves (`SessionPool::active_scenes`) —
/// see docs/ARCHITECTURE.md §3.1.
/// Resume/fork/close/delete don't yet take a per-request `scene` param
/// (every caller implicitly means "whichever active scene this session was
/// actually recorded under"), so `Mismatch`'s reported `requested_scene`
/// is always this pool's *default* scene — that only reflects what a
/// mismatch response echoes back, not what counts as a match.
pub enum SceneCheck {
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

/// `SessionPool::resume_session`'s failure type — a plain `RpcError` for
/// most cases, plus a `SidechainTerminal` variant carrying the extra
/// `parent_session_id`/`final_state` fields `server.rs` needs to build the
/// `SIDECHAIN_TERMINAL` error's structured `data` (an `RpcError` is just
/// `(code, message)`, with no room for that). The terminal-state check runs
/// inside `resume_session`'s `with_session_lock` critical section, so its
/// outcome travels back through the return type.
pub enum ResumeError {
    SidechainTerminal {
        session_id: String,
        parent_session_id: Option<String>,
        final_state: history::entry::SessionEndState,
    },
    Rpc(RpcError),
}

/// `SessionPool::set_session_model`'s failure type. `Busy` carries the turn
/// id `server.rs` needs for `SESSION_BUSY`'s structured `data`, which a plain
/// `RpcError` — `(code, message)` — has no room for. Same shape and same
/// reason as [`ResumeError`].
pub enum SetModelError {
    /// Running the turn named, if one has started yet, or — idle but for
    /// them — the background tasks named, which a rebuild would cut off.
    Busy {
        current_turn_id: Option<String>,
        running_task_ids: Vec<String>,
    },
    Rpc(RpcError),
}

/// `session.create`'s `project_root` disambiguated three ways — JSON only
/// gives "key omitted" vs. "key present" for free; `null` vs. a string
/// needs an explicit check the wire layer (`server.rs`) does before this
/// ever reaches `SessionPool`.
/// What a per-session settings entry is keyed by: the project it belongs to
/// (`None` = the global no-project tier) and the scene it runs under.
///
/// Both halves matter. The scene contributes a real settings layer
/// (`~/.agix/scenes/<scene>/settings.json`, plus the skills, agents and
/// plugins beside it), so one project under two scenes is two different
/// resolutions, not one.
type ProjectSceneKey = (Option<PathBuf>, String);

/// Whether what `key` resolves to was merged from `tier`. The global tier is
/// under every pair; a scene or project tier only under its own.
fn tier_covers(tier: &crate::watch_hub::Tier, key: &ProjectSceneKey) -> bool {
    use crate::watch_hub::Tier;
    match tier {
        Tier::Global => true,
        Tier::Scene(scene) => &key.1 == scene,
        Tier::Project(root) => key.0.as_deref() == Some(root.as_path()),
    }
}

/// Everything a session reads that the pair `(project, scene)` decides: the
/// merged settings, and the skills, agent types and commands that live in the
/// directories those settings name.
///
/// One per pair, shared by every session on it. They are here together
/// because they are the same fact stated four times: a binding is built from
/// one `Settings`, and rebuilding it is how any of them changes.
struct ProjectBinding {
    settings: Arc<Settings>,
    /// The conversation's model for a session on this pair, and the
    /// per-task-type routing behind it. `providers` and `task_models` are
    /// settings fields like any other, so they are decided by the pair: a
    /// project that configures its own provider is served by it.
    model: Arc<dyn base::interface::model::Model>,
    task_router: Option<Arc<base::provider::TaskRouter>>,
    /// What building `task_router` had to say — reported by `config.reload`
    /// and `config.setProvider` for the pair they targeted.
    routing: ReloadReport,
    modules: crate::modules::ProjectModules,
    commands: Arc<runtime::commands::CommandRegistry>,
    /// Subscriptions to the settings files this binding was merged from.
    /// Held, not read: dropping them is how a binding that gets invalidated
    /// releases its share of the watch (`WatchHub` refcounts, so the tiers
    /// several projects have in common are watched once).
    _watch: Vec<crate::watch_hub::WatchToken>,
    /// The settings files and skill directories of this binding's tiers that
    /// did not exist when it was built, so are not watched — see
    /// [`ProjectBinding::appeared`].
    unwatched_settings: Vec<(crate::watch_hub::Tier, PathBuf)>,
    /// Set once a tier this binding was merged from changed. Every session
    /// built on it holds the same flag and rebuilds before its next turn.
    stale: Arc<std::sync::atomic::AtomicBool>,
}

impl ProjectBinding {
    fn build(
        settings: Arc<Settings>,
        plugins: &crate::plugins::Contributions,
        watch: Vec<crate::watch_hub::WatchToken>,
        client: &Arc<dyn AnthropicClient>,
        last_good_router: Option<Arc<base::provider::TaskRouter>>,
        credentials: &Arc<dyn base::interface::credentials::CredentialSource>,
    ) -> Self {
        let (task_router, routing) = resolve_router(&settings, last_good_router, credentials);
        // `task_models.main` is what points the conversation itself at a
        // configured provider; with none configured this is the plain
        // Anthropic client the daemon was started with.
        let model: Arc<dyn base::interface::model::Model> = match task_router.as_ref() {
            Some(router) => router.model_for("main"),
            None => Arc::new(AnthropicModel::new(client.clone())),
        };
        let modules = crate::modules::ProjectModules::build(&settings, plugins);
        let commands = modules.commands();
        Self {
            settings,
            model,
            task_router,
            routing,
            modules,
            commands,
            _watch: watch,
            unwatched_settings: Vec::new(),
            stale: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn mark_stale(&self) {
        self.stale.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The tier of a settings file or skill directory that has been created
    /// since this binding was built. Nothing watched it — a missing path is
    /// not subscribed, see `SessionPool::watch_tiers_of` — so this is the
    /// only way its first contents (a rule `permit_always` wrote to a new
    /// `settings.local.json`, a project's first skill) reach the sessions
    /// without a `config.reload`.
    fn appeared(&self) -> Option<&crate::watch_hub::Tier> {
        self.unwatched_settings
            .iter()
            .find(|(_, path)| path.exists())
            .map(|(tier, _)| tier)
    }

    /// The client for provider `id`, when the settings configure one that
    /// built.
    fn provider_client(&self, id: &str) -> Option<Arc<dyn base::interface::model::Model>> {
        self.task_router.as_ref()?.provider(id)
    }

    /// Whether a session on this pair may talk to `provider` with `model`.
    ///
    /// A model is checked against the `models` allow-list of the provider it
    /// would be sent to — the one named, else the one the conversation is
    /// routed to. An empty list, or no provider at all, accepts any name: the
    /// daemon has no catalog to check against.
    fn check_model_choice(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), String> {
        if let Some(id) = provider {
            if self.provider_client(id).is_none() {
                return Err(format!(
                    "provider `{id}` is not configured for this session's project"
                ));
            }
        }
        let Some(model) = model else {
            return Ok(());
        };
        let routed = provider
            .map(str::to_string)
            .or_else(|| {
                self.task_router
                    .as_ref()
                    .and_then(|r| r.provider_id_for("main").map(str::to_string))
            })
            .or_else(|| self.settings.default_provider.clone());
        let Some(id) = routed else {
            return Ok(());
        };
        match self.settings.providers.get(&id) {
            Some(config)
                if !config.models.is_empty() && !config.models.iter().any(|m| m == model) =>
            {
                Err(format!(
                    "model `{model}` is not one of provider `{id}`'s models"
                ))
            }
            _ => Ok(()),
        }
    }

    fn reload_skills(&self) {
        self.modules.reload_skills(&self.settings);
    }

    /// Re-merge what the plugin set now contributes, in place: every session
    /// on the pair shares these catalogs, and `commands` is a view onto the
    /// skill one.
    fn refresh_plugins(&self, plugins: &crate::plugins::Contributions) {
        self.modules.refresh_plugins(&self.settings, plugins);
    }

    fn refresh_agent_types(&self, plugin_agent_types: &[base::agent_type::AgentTypeDefinition]) {
        self.modules
            .refresh_agent_types(&self.settings, plugin_agent_types);
    }
}

pub enum ProjectSelector {
    /// `project_root` omitted — this pool's default project (`self.cwd`).
    Default,
    /// `project_root: null` — a genuine no-project session (global tier).
    NoProject,
    /// `project_root: "<path>"` — must exist; `session.create` rejects it
    /// otherwise with `PROJECT_NOT_FOUND` (unlike resume, which tolerates a
    /// moved/missing project — see `settings_for_project`'s doc comment).
    Path(PathBuf),
}

/// `(code, message)` for a failed pool operation, ready to hand to
/// `RpcResponse::err`. The older pool methods return a bare `String` and let
/// `server.rs` pick one fixed code per method; the session read/branch
/// surface (`session.history`/`.fork`/`.resume`) genuinely needs three
/// different codes from the *same* method — a bad id is `INVALID_PARAMS`, an
/// unknown one is `SESSION_NOT_FOUND`, an unreadable log is
/// `INTERNAL_ERROR` — so those carry the code with the message instead of
/// inventing a new response envelope for it.
pub type RpcError = (i32, String);

/// The first `LogEntry::Meta` line in `entries`, if any — every session
/// history has at most one (see `create()`), always first when present, but
/// a linear `find_map` rather than `entries.first()` since pre-v2 files can
/// have none at all. Shared by the handful of pool methods that only need
/// one or two `Meta` fields (`session_kind_parent_and_resumable`,
/// `session_kind_and_scene`, `recorded_project`, `check_scene`,
/// `sidechain_terminal_state`) so they don't each repeat the same scan
/// shape.
fn find_meta(entries: &[history::entry::EnvelopedEntry]) -> Option<&history::entry::LogEntry> {
    entries
        .iter()
        .map(|env| &env.entry)
        .find(|entry| matches!(entry, history::entry::LogEntry::Meta { .. }))
}

struct SceneTableChange {
    added: Vec<String>,
    removed: Vec<String>,
    refused: Vec<String>,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
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

/// What a rebuild of a session has to reproduce: the options it was created
/// with and what has been changed on it since.
///
/// An `Agent` is rebuilt whenever its binding goes stale, the janitor evicts
/// it, or the daemon restarts; every rebuild reproduces this, not whatever the
/// triggering call happens to pass.
#[derive(Clone, Default)]
struct SessionSpec {
    options: Option<SessionOptions>,
    model: Option<String>,
    provider: Option<String>,
    prompt: Option<runtime::agent::SessionPrompt>,
    /// Shared by every Agent this process builds for the session, so a
    /// rebuild continues a replay where it stood instead of from the first
    /// recorded call. Not persisted: a recording in progress does not outlive
    /// the process that made it.
    recorder: Option<Arc<telemetry::recorder::Recorder>>,
}

impl SessionSpec {
    /// The provider the conversation goes to: chosen since, else asked for
    /// at creation. `None` follows the settings' routing.
    fn provider(&self) -> Option<String> {
        self.provider
            .clone()
            .or_else(|| self.options.as_ref().and_then(|o| o.provider.clone()))
    }

    /// The session's own prompt: set since, else given at creation.
    fn prompt(&self) -> Option<runtime::agent::SessionPrompt> {
        self.prompt
            .clone()
            .or_else(|| self.options.as_ref().and_then(|o| o.prompt.clone()))
            .filter(|p| !p.is_empty())
    }

    /// The conversation's model, by the same precedence as `provider`.
    fn model(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| self.options.as_ref().and_then(|o| o.model.clone()))
    }
}

/// The persisted part of a [`SessionSpec`], written to the session's log as
/// an extension entry each time it changes; the last one wins. An extension
/// entry rather than a `Meta` field because the spec changes after `Meta` is
/// written, and readers that do not know the namespace skip it.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct RecordedSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    options: Option<SessionOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt: Option<runtime::agent::SessionPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
}

const SPEC_NS: &str = "agix.session";
const ENGINE_NS_PREFIX: &str = "agix.";
const SPEC_EVENT: &str = "spec";

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
            CallOrigin, MessageRole, ModelContentBlock, ModelError, ModelEvent, ModelMessage,
            StreamParams,
        };
        use base::interface::prompt::PromptBlock;
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
            thinking_mode: base::interface::settings::ThinkingMode::Off,
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
            Err(ModelError::NotRecorded(_)) => return Ok(None),
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
    event_rx: Arc<AsyncMutex<Option<EventReceiver>>>,
    cancel: CancellationToken,
    /// Cancelled once the Agent's task has returned and dropped the Agent.
    exited: CancellationToken,
    /// How full the context was after the last turn that called the model.
    context: Option<base::event::ContextUsage>,
    /// The scene this session was created under — the one its `Meta` records,
    /// not this pool's default. Auto-naming is the one behaviour decided per
    /// turn rather than at build time, so it is the one that needs the scene
    /// kept here to ask.
    scene: Arc<dyn Scene>,
    /// The project this session is bound to, `None` for a no-project session.
    project: Option<PathBuf>,
    /// Session name（CHAT 场景首轮后由 LLM 生成；CODING 为 None）。
    name: Option<String>,
    created_at: Instant,
    last_active: Instant,
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
    /// The turn currently running on this session, if any.
    ///
    /// A session runs one turn at a time — `event_rx` is taken exclusively
    /// for the duration, so the second caller could never have been served
    /// anyway. This records *which* turn holds it, so the refusal can name it
    /// (`SESSION_BUSY.data.current_turn_id`) and `session.get` can report
    /// `turn_state` instead of the caller having to infer it.
    current_turn: Option<String>,
    /// Woken whenever the session stops running a turn — a `run_turn` with
    /// `on_busy: queue` waits on it for its slot.
    idle: Arc<tokio::sync::Notify>,
    /// Who started the running turn: `"client"`, `"schedule"` or `"goal"`.
    turn_initiator: Option<&'static str>,
    /// The engine's own session state: the tasks `session.tasks` lists.
    state: Option<Arc<base::context::SessionState>>,
    /// How the engine starts sub-agents, for `session.startAgent`.
    spawner: Option<Arc<dyn base::interface::agent_spawner::AgentSpawner>>,
    /// What this session's bound scripts did, if it bound any.
    ///
    /// Kept because a script that quietly does nothing is the failure mode of
    /// this whole tier, and a log line on the daemon's stderr is not something
    /// the person asking "why is my script not working" has. `session.get`
    /// reads it.
    scripts: Option<Arc<base::interface::script::ScriptLedger>>,
    /// The checks this session's plugins registered, for `session.get`.
    health: Arc<base::interface::health::HealthChecks>,
    /// Connections watching this session.
    ///
    /// Every frame the session produces goes to all of them. A turn is not
    /// owned by the connection that started it — several browser tabs can
    /// watch one conversation, and closing the one that sent the message is
    /// not a reason to stop.
    subscribers: HashMap<crate::rpc::ConnectionId, Arc<crate::rpc::Client>>,
    /// Asks (permission prompts and questions) raised and not yet answered,
    /// by `prompt_id`.
    ///
    /// Kept so a connection that subscribes later — a second tab, or the
    /// same tab after a refresh — is handed the question instead of finding
    /// a session that appears stuck. The engine remains the authority on
    /// whether an answer is still wanted; this is a mirror for redelivery,
    /// and an answer to a `prompt_id` nobody awaits is already a documented
    /// no-op.
    pending_prompts: HashMap<String, PendingPrompt>,
    /// The process groups this session's commands started, kept when
    /// `sandbox.reap_process_groups` is on and stopped when the session ends.
    process_groups: Option<Arc<platform::GroupLedger>>,
}

/// A raised ask: its `prompt_id`, and the background task asking it when
/// one does.
type Ask = (String, Option<String>);

/// What `session.respondToPrompt` delivers, by `prompt_type`.
#[derive(Debug, Clone)]
pub enum PromptAnswer {
    Permission(runtime::agent::PermissionDecision),
    Question(runtime::agent::QuestionAnswer),
}

impl PromptAnswer {
    pub fn prompt_type(&self) -> &'static str {
        match self {
            Self::Permission(_) => crate::rpc::PROMPT_TYPE_PERMISSION,
            Self::Question(_) => crate::rpc::PROMPT_TYPE_QUESTION,
        }
    }
}

/// A raised ask, as redelivered to a late subscriber.
struct PendingPrompt {
    frame: serde_json::Value,
    /// The background task whose sub-agent asks, when one does. Such an ask
    /// is not the running turn's and outlives it; it goes when it is answered
    /// or its task ends.
    task_id: Option<String>,
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
    /// 共享的 LLM client（用于 session 命名等）。
    client: Arc<dyn AnthropicClient>,
    /// Hot-swappable so `config.setProvider` can take effect for sessions
    /// created *after* the call without a daemon restart. Sessions already
    /// running keep whatever `Arc<Settings>` snapshot they were built with —
    /// only new `create()` calls observe the update.
    settings: AsyncRwLock<Arc<Settings>>,
    /// This daemon instance's *default* scene (resolved + validated from
    /// `--scene` at startup — see `main.rs::resolve_scene`). Every session
    /// created without an explicit `scene` uses this one.
    scene: Arc<dyn Scene>,
    /// Every scene this daemon instance currently serves, keyed by id —
    /// always at least `{self.scene.id(): self.scene}`, plus whatever
    /// `--scenes` added at startup or `scene.activate` added at runtime.
    /// `session.create`'s optional `scene` param resolves against this, not
    /// `self.scene` — the pool's *default* scene concept and *which scenes
    /// are currently servable* are related but distinct (the default is
    /// always servable; not everything servable is the default).
    active_scenes: AsyncRwLock<HashMap<String, Arc<dyn Scene>>>,
    /// Every scene this pool can serve, active or not — what `scene.list`
    /// enumerates and `scene.activate` resolves against: the built-in ones,
    /// the host's and the plugins'. Swapped wholesale when the plugin set
    /// changes or the host registers a scene; a session already running
    /// holds its own `Arc<dyn Scene>` and is unaffected.
    scene_registry: std::sync::RwLock<Arc<scene::SceneRegistry>>,
    /// Scenes the host defined rather than the binary or a plugin: the one
    /// the pool was started with, and every one passed to
    /// [`register_scene`](Self::register_scene). Kept so each rebuild of the
    /// registry has them; without that, a host scene once deactivated could
    /// never be activated again.
    host_scenes: std::sync::RwLock<Vec<Arc<dyn Scene>>>,
    /// Scenes `scene.register` added, held in memory only.
    registered_scenes: std::sync::RwLock<Vec<Arc<dyn Scene>>>,
    /// The host's own `Permission`, handed to a session whose effective mode
    /// is `bypassPermissions` in place of the rule gate (see
    /// `resolve_session_permission`). `None` when the host supplied none:
    /// such a session gets the gate too, which under bypass consults no rule
    /// but still validates input and still follows a later `EnterPlanMode`.
    permission: Option<Arc<dyn Permission>>,
    /// Built-in tool registry (Bash/Read/Write/Edit/Grep/Glob/...), shared
    /// (cheap `Arc` clone) by every session `Builder`. Built once here via
    /// `tools::register_builtin_tools`; `Builder::build()` on its own registers
    /// only Skill/TaskStop/TaskOutput/Import/Agent/MCP. See
    /// `tools::register_builtin_tools`'s doc comment for what's deliberately
    /// excluded.
    tools: Arc<base::tool::InMemoryToolRegistry>,
    cwd: PathBuf,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    paths: Arc<dyn crate::config::DaemonPaths>,
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
    mcp_by_project: AsyncMutex<HashMap<ProjectSceneKey, Arc<McpManager>>>,
    /// Held over a pair's first MCP connect, so a session created while the
    /// startup connect is still running waits for it instead of starting
    /// every server a second time. One for all pairs: a first connect is
    /// bounded by `MCP_SESSION_CONNECT_WAIT`.
    #[cfg(feature = "mcp")]
    mcp_first_connect: AsyncMutex<()>,
    /// This pool's MCP authorization: sign-in state per server, the flows in
    /// progress, and the credential store (files under its own user data
    /// root unless the host gave one).
    #[cfg(feature = "mcp")]
    mcp_auth: Arc<mcp::oauth::McpAuth>,
    /// What every MCP server this pool connects is admitted by — see
    /// `admit_mcp`.
    #[cfg(feature = "mcp")]
    mcp_policy: Arc<mcp::policy::McpPolicyGate>,
    /// Where this pool looked for the deployment's managed settings, and what
    /// it found there: applied on top of every settings it loads.
    managed_settings_path: PathBuf,
    managed_settings: Option<base::managed_settings::ManagedSettings>,
    /// Every session's scheduled jobs, fired by this pool's scheduler (see
    /// `start_scheduler`) whether or not the session is in memory.
    schedule: Arc<tools::cron::CronStore>,
    /// Ids for the connections the pool opens for itself — a scheduled turn
    /// has no client — counted down from the top so they never meet the ids
    /// the server hands to real connections.
    internal_connections: std::sync::atomic::AtomicU64,
    /// This pool, for the tasks it starts that outlive the call that started
    /// them. Set when the pool is started (`start_scheduler`).
    this: std::sync::OnceLock<std::sync::Weak<SessionPool>>,
    /// Daemon-level async notifications (MCP connect outcomes, import
    /// auto-detection, future error events) — see `daemon.subscribeEvents`.
    /// `send()` returning `Err` just means "no subscribers right now", not
    /// a real error, so callers of `emit_event` ignore it.
    events_tx: tokio::sync::broadcast::Sender<serde_json::Value>,
    /// The plugin subsystem — see `crate::plugins`. Everything plugins
    /// contribute arrives through it, and in a build without the `plugins`
    /// feature it is a stub that contributes nothing.
    plugins: crate::plugins::PluginSubsystem,
    /// Process-scope diagnostics, reported by `daemon.doctor` — the plugin
    /// fault records, plus whatever an embedder registered with
    /// [`SessionPool::register_health_check`]. Not the same set as a
    /// session's: a session's checks come and go with the session, and one
    /// shared registry would grow an entry per session forever.
    health: std::sync::RwLock<base::interface::health::HealthChecks>,
    /// The last router each `(project, scene)` pair was successfully served
    /// by, kept across binding invalidation so a provider config that stops
    /// resolving — or stops producing a buildable client — leaves working
    /// routing in place instead of tearing it down (see `resolve_router`).
    last_good_routers: AsyncMutex<HashMap<ProjectSceneKey, Arc<base::provider::TaskRouter>>>,
    /// One [`ProjectBinding`] per `(project_root, scene)` pair this pool has
    /// built a session for — the settings, skills, agent types and commands
    /// that pair resolves to. `project_root` is understood exactly as
    /// `session.create`'s param is: `None` = the global no-project tier,
    /// `Some(root)` = that project's `settings.json` over the global/scene
    /// tiers.
    ///
    /// Scene is part of the key because the scene tier is a real layer
    /// (`~/.agix/scenes/<scene>/settings.json`, and the skills, agents and
    /// plugins beside it). Keying by project alone meant every session got
    /// whichever scene happened to be built first, so a `harnix` session on a
    /// daemon started as `--scene code` silently read code's tier.
    ///
    /// Entries are built on demand and dropped wholesale by
    /// `invalidate_bindings` — a reload, or a plugin set that changed. The
    /// pool's own pair is in here like any other; `self.settings` remains the
    /// separate, hot-swappable copy that `config.reload` writes and that
    /// everything not tied to a session reads.
    bindings: AsyncMutex<HashMap<ProjectSceneKey, Arc<ProjectBinding>>>,
    /// One `notify` watcher for every settings file every binding was merged
    /// from — see [`crate::watch_hub`]. `None` when the watcher could not be
    /// started (permissions, kernel limits): the documented degrade is that
    /// configuration changes reach the daemon through an explicit
    /// `config.reload` and not on their own.
    watch: Option<crate::watch_hub::WatchHub>,
    /// The watch on the tiers of this daemon's own pair, held for the pool's
    /// lifetime. Binding watches come and go with the bindings, and with every
    /// binding invalidated nothing would be left to notice the next change to
    /// the global tier or to the settings `self.settings` mirrors.
    pool_watch: std::sync::Mutex<Vec<crate::watch_hub::WatchToken>>,
    /// Each session's [`SessionSpec`], by id — what every rebuild of it
    /// applies. Filled from the session's log the first time a session not
    /// built in this process is resumed.
    specs: std::sync::Mutex<HashMap<String, SessionSpec>>,
    /// Sessions out of memory whose spec is still in `specs`, oldest first:
    /// those the log cannot give back (a recorder's progress, a session with
    /// no log). At most `cap` of them.
    kept_specs: std::sync::Mutex<std::collections::VecDeque<String>>,
    /// The engines taken out of `sessions` that may still be stopping, by
    /// session id. A session brought back under one of these ids waits for
    /// that engine first, so the two never run side by side.
    stopping: std::sync::Mutex<HashMap<String, CancellationToken>>,
    /// Set once [`Self::shutdown_all`] starts; no session is created after.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Cancelled when [`Self::shutdown_all`] has finished.
    shut_down: CancellationToken,
    /// Every project this pool has built a session for, `None` being the
    /// no-project tier. What `session.list` enumerates cold sessions under —
    /// kept apart from `bindings`, which a settings change empties.
    served: std::sync::Mutex<Vec<Option<PathBuf>>>,
    /// How long a `kind:"prompt"` permission ask may sit unanswered before
    /// the daemon answers it with a `Deny` on the client's behalf.
    /// `Duration::ZERO` disables the timer entirely (wait forever — the
    /// engine's own semantics, appropriate for a host that really does keep
    /// a human in the loop indefinitely).
    ///
    /// Defaults to [`DEFAULT_PERMISSION_PROMPT_TIMEOUT`]; `daemon/src/main.rs`
    /// overrides it from `--permission-prompt-timeout`. See
    /// `LiveSession.pending_prompts` for why this lives here rather than in
    /// the engine.
    permission_prompt_timeout: Duration,
    /// Where provider keys come from when a binding builds its router.
    credentials: Arc<dyn base::interface::credentials::CredentialSource>,
    rust_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    customize: Option<SessionCustomizer>,
    /// Pool-level telemetry sink — for events that happen *before* any
    /// per-session `Agent`/handle exists (`session.resume` against a
    /// session that's already live, a terminal sidechain, or an unknown
    /// id), which have nowhere else to record to. Every per-session event
    /// still goes through that session's own handle (see `create()`); this
    /// one is only for the handful of pool-level exits that never reach
    /// `create()` at all. Writes to `<local_data_dir>/telemetry/pool.jsonl`,
    /// gated by the same `settings.telemetry_enabled` flag — noop if
    /// disabled or the path can't be resolved/opened.
    pool_telemetry: telemetry::TelemetryHandle,
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
#[derive(serde::Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub name: Option<String>,
    pub preview: Option<String>,
    pub message_count: u32,
    pub created_at: String,
    pub last_active: String,
    pub status: SessionStatus,
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

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Inactive,
}

/// Outcome of `SessionPool::apply_reloaded_settings()` — how a
/// `config.setProvider`/`config.reload` call's routing re-resolution went.
///
/// Two genuinely different checks, kept in separate fields on purpose:
/// `warnings`/`error` are `base::provider::resolve_task_models`'s own
/// soft-fallback outcome — pure config-shape validation (does every
/// `task_models` entry resolve to a provider that exists, etc.), unaware of
/// `api_type` or whether a real client could actually be built. A config can
/// resolve cleanly (`error: None`) yet still fail to produce a live
/// `TaskRouter` (`router_rebuilt: false`, `router_error: Some(..)`) — e.g.
/// `api_type: openai_compatible`, which the schema accepts but has no
/// runtime implementation. Collapsing these into one `ok` flag would be a
/// breaking change to what `routing.ok` has always meant to callers (see
/// `set_provider_writes_project_settings_and_doctor_sees_it`'s test, which
/// deliberately uses `openai_compatible` and asserts `routing.ok == true`).
#[derive(Debug, Default, Clone)]
struct ReloadReport {
    warnings: Vec<String>,
    error: Option<String>,
    router_rebuilt: bool,
    router_error: Option<String>,
    /// MCP servers from the freshly-reloaded config that are now actually
    /// connected — see `SessionPool::reconcile_mcp_servers`.
    mcp_connected: Vec<String>,
    /// MCP servers from the freshly-reloaded config that failed to connect
    /// (or had an invalid config) — specific reasons are in the daemon log
    /// and the `mcp_connect_failed` async notification, same contract as
    /// startup connection failures.
    mcp_failed: Vec<String>,
    /// MCP servers still connecting when the reload returned; each one's
    /// outcome follows as a `mcp_connected` / `mcp_connect_failed` event.
    mcp_connecting: Vec<String>,
}

impl ReloadReport {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": self.error.is_none(),
            "warnings": self.warnings,
            "error": self.error,
            "router_rebuilt": self.router_rebuilt,
            "router_error": self.router_error,
            "mcp_connected": self.mcp_connected,
            "mcp_failed": self.mcp_failed,
            "mcp_connecting": self.mcp_connecting,
        })
    }
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
fn project_tier_mcp_servers(settings: &Settings) -> std::collections::HashSet<String> {
    settings.project_layer.mcp_servers.keys().cloned().collect()
}

/// What `project.status` answers for `root`, from the settings its sessions
/// are built with.
fn project_trust_report(
    root: &Path,
    settings: &Settings,
    plugin_agent_types: &[base::agent_type::AgentTypeDefinition],
) -> serde_json::Value {
    let layer = &settings.project_layer;
    #[cfg_attr(not(any(feature = "agents", feature = "skills")), allow(unused_mut))]
    let mut requires_trust = layer.requires_trust.clone();
    #[cfg(feature = "agents")]
    requires_trust.extend(agents::agent_tool::project_agent_types_requiring_trust(
        settings,
        plugin_agent_types,
    ));
    #[cfg(not(feature = "agents"))]
    let _ = plugin_agent_types;
    #[cfg(feature = "skills")]
    requires_trust.extend(skills::module::project_skills_requiring_trust(settings));
    serde_json::json!({
        "project_root": root,
        "trust": layer.trust.as_str(),
        "matched": layer.matched,
        "default": settings.project_trust.default,
        "requires_trust": requires_trust,
    })
}

/// `mcp.status` entries for the project's own servers that are not started
/// because the project is not trusted.
#[cfg(feature = "mcp")]
fn untrusted_mcp_entries(settings: &Settings) -> Vec<serde_json::Value> {
    if !settings.project_layer.trust.is_untrusted() {
        return Vec::new();
    }
    settings
        .project_layer
        .mcp_servers
        .iter()
        .filter(|(name, _)| !settings.mcp_servers.contains_key(*name))
        .map(|(name, cfg)| {
            let transport = serde_json::from_value::<mcp::config::McpServerConfig>(cfg.clone())
                .map(|c| c.transport_kind().to_string())
                .unwrap_or_else(|_| "unknown".into());
            serde_json::json!({
                "name": name, "transport": transport, "tool_count": 0,
                "state": "untrusted",
                "error": "the project is not trusted; its own MCP servers start once it is (project.trust)",
                "auth": {"state": "none"},
            })
        })
        .collect()
}

#[cfg(feature = "mcp")]
fn auth_flow_error(e: mcp::oauth::AuthFlowError) -> RpcError {
    use mcp::oauth::AuthFlowError;
    match e {
        AuthFlowError::Invalid(message) => (codes::INVALID_PARAMS, message),
        AuthFlowError::Denied(message) => (codes::MCP_SERVER_DENIED, message),
        AuthFlowError::Failed(message) => (codes::MCP_AUTH_FAILED, message),
    }
}

/// `mcp.status`'s entries: the connected servers, those waiting for a
/// sign-in, then the denied ones. Each carries its authorization state.
#[cfg(feature = "mcp")]
fn mcp_status_entries(manager: &McpManager) -> Vec<serde_json::Value> {
    let auth = |name: &str| {
        manager
            .config_of(name)
            .and_then(|cfg| manager.auth()?.status(cfg))
            .and_then(|status| serde_json::to_value(status).ok())
            .unwrap_or_else(|| serde_json::json!({"state": "none"}))
    };
    let connected = manager.server_statuses().into_iter().map(|s| {
        serde_json::json!({
            "name": s.name, "transport": s.transport, "tool_count": s.tool_count,
            "state": if s.connected { "connected" } else { "disconnected" },
            "auth": auth(&s.name),
        })
    });
    let connecting = manager.connecting().iter().map(|c| {
        serde_json::json!({
            "name": c.name, "transport": c.transport, "tool_count": 0,
            "state": "connecting", "auth": auth(&c.name),
        })
    });
    let failed = manager.failed().iter().map(|f| {
        serde_json::json!({
            "name": f.name, "transport": f.transport, "tool_count": 0,
            "state": "failed", "error": f.error, "auth": auth(&f.name),
        })
    });
    let pending = manager.pending().iter().map(|p| {
        serde_json::json!({
            "name": p.name, "transport": p.transport, "tool_count": 0,
            "state": "auth_required", "auth": auth(&p.name),
        })
    });
    let denied = manager.denied().iter().map(|d| {
        serde_json::json!({
            "name": d.name, "transport": d.transport, "tool_count": 0,
            "state": "denied", "error": d.reason, "auth": {"state": "none"},
        })
    });
    connected
        .chain(connecting)
        .chain(pending)
        .chain(failed)
        .chain(denied)
        .collect()
}

/// How long a session's creation waits for its project's MCP servers to
/// connect. One still connecting then goes on in the background and joins
/// the project's sessions at their next turn.
#[cfg(feature = "mcp")]
const MCP_SESSION_CONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(feature = "mcp")]
fn mcp_connect_failed_event(manager: &McpManager, name: &str) -> serde_json::Value {
    let error = manager
        .failed()
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.error.clone())
        .unwrap_or_else(|| "connect failed — see daemon log for the specific reason".into());
    serde_json::json!({ "kind": "mcp_connect_failed", "server": name, "error": error })
}

/// Build the router a settings tier's `providers`/`task_models` describe.
///
/// `last_good` is what this pair was already being served by. A config that
/// no longer resolves, or that resolves but cannot produce a live client
/// (`api_type: openai_compatible` is accepted by the schema and has no
/// runtime implementation), keeps it rather than tearing working routing down
/// over a bad edit — the failure is reported, not acted on.
fn resolve_router(
    settings: &Settings,
    last_good: Option<Arc<base::provider::TaskRouter>>,
    credentials: &Arc<dyn base::interface::credentials::CredentialSource>,
) -> (Option<Arc<base::provider::TaskRouter>>, ReloadReport) {
    let mut report = ReloadReport::default();
    if settings.providers.is_empty() {
        // No providers configured (or the last one was just deleted) —
        // routing reverts to "no router", same as if multi-provider config
        // was never touched.
        return (None, report);
    }
    match base::provider::resolve_task_models(
        &settings.providers,
        settings.default_provider.as_deref(),
        &settings.task_models,
    ) {
        Ok((resolved, warnings)) => {
            report.warnings = warnings;
            // `resolve_task_models` succeeding guarantees `default_provider`
            // is `Some` and non-empty (one of its own hard-error checks).
            let default_provider = settings
                .default_provider
                .as_deref()
                .expect("resolve_task_models already validated default_provider is set");
            match crate::model_router::build_task_router(
                &settings.providers,
                default_provider,
                resolved,
                credentials,
            ) {
                Ok(router) => {
                    report.router_rebuilt = true;
                    (Some(Arc::new(router)), report)
                }
                Err(e) => {
                    // Deliberately `router_error`, not `error` —
                    // `resolve_task_models` itself succeeded (the warnings
                    // above already reflect that), only the build step
                    // failed; see `ReloadReport`'s doc comment.
                    warn!(error = %e, "task router build failed; keeping the previous router");
                    report.router_error = Some(e);
                    (last_good, report)
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "provider config no longer resolves; keeping the previous router");
            report.error = Some(e);
            (last_good, report)
        }
    }
}

impl SessionPool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cap: usize,
        idle_timeout_secs: u64,
        client: Arc<dyn AnthropicClient>,
        settings: Arc<Settings>,
        scene: Arc<dyn Scene>,
        permission: Option<Arc<dyn Permission>>,
        cwd: PathBuf,
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
        paths: Arc<dyn crate::config::DaemonPaths>,
    ) -> Self {
        let (events_tx, _) = tokio::sync::broadcast::channel(1024);
        #[cfg(feature = "mcp")]
        let mcp_auth = Arc::new(mcp::oauth::McpAuth::with_file_store(
            settings.paths.user_data_dir.join("mcp").join("oauth"),
        ));
        let _ = std::fs::create_dir_all(&settings.paths.user_data_dir);
        let schedule = Arc::new(tools::cron::CronStore::load_or_default(Some(
            settings.paths.user_data_dir.join("scheduled_tasks.json"),
        )));

        let plugins = crate::plugins::PluginSubsystem::new(paths.clone(), settings.plugins.clone());
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools::register_builtin_tools(&tools);

        let host_scenes = vec![scene.clone()];
        let scene_registry = std::sync::RwLock::new(Arc::new(
            registry_of(&host_scenes, &[], &paths.global_root().join("scenes"), &[]).0,
        ));
        let mut active_scenes = HashMap::new();
        active_scenes.insert(scene.id().to_string(), scene.clone());

        let pool_telemetry = build_pool_telemetry(&settings);

        Self {
            sessions: AsyncMutex::new(HashMap::new()),
            session_locks: AsyncMutex::new(HashMap::new()),
            cap,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            client,
            settings: AsyncRwLock::new(settings),
            scene,
            active_scenes: AsyncRwLock::new(active_scenes),
            scene_registry,
            host_scenes: std::sync::RwLock::new(host_scenes),
            registered_scenes: std::sync::RwLock::new(Vec::new()),
            permission,
            tools,
            cwd,
            history_store,
            paths,
            #[cfg(feature = "mcp")]
            mcp_by_project: AsyncMutex::new(HashMap::new()),
            #[cfg(feature = "mcp")]
            mcp_first_connect: AsyncMutex::new(()),
            #[cfg(feature = "mcp")]
            mcp_auth,
            #[cfg(feature = "mcp")]
            mcp_policy: Arc::new(mcp::policy::McpPolicyGate::open()),
            managed_settings_path: base::managed_settings::managed_settings_path(),
            managed_settings: None,
            schedule,
            internal_connections: std::sync::atomic::AtomicU64::new(u64::MAX),
            this: std::sync::OnceLock::new(),
            events_tx,
            health: std::sync::RwLock::new(base::interface::health::HealthChecks::new()),
            plugins,
            last_good_routers: AsyncMutex::new(HashMap::new()),
            bindings: AsyncMutex::new(HashMap::new()),
            watch: match crate::watch_hub::WatchHub::new() {
                Ok(hub) => Some(hub),
                Err(e) => {
                    warn!(error = %e, "config file watching unavailable; changes need config.reload");
                    None
                }
            },
            pool_watch: std::sync::Mutex::new(Vec::new()),
            specs: std::sync::Mutex::new(HashMap::new()),
            kept_specs: std::sync::Mutex::new(std::collections::VecDeque::new()),
            stopping: std::sync::Mutex::new(HashMap::new()),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            shut_down: CancellationToken::new(),
            served: std::sync::Mutex::new(Vec::new()),
            permission_prompt_timeout: DEFAULT_PERMISSION_PROMPT_TIMEOUT,
            credentials: Arc::new(base::interface::credentials::ConfigCredentials),
            rust_plugins: Vec::new(),
            customize: None,
            pool_telemetry,
        }
    }

    /// Resolve provider keys through `credentials` instead of only what each
    /// provider entry says.
    pub fn with_credentials(
        mut self,
        credentials: Arc<dyn base::interface::credentials::CredentialSource>,
    ) -> Self {
        self.credentials = credentials;
        self
    }

    /// Keep MCP sign-ins in `store` instead of files under the user data
    /// root, and present `client` to authorization servers.
    #[cfg(feature = "mcp")]
    pub fn with_mcp_auth(
        mut self,
        store: Option<Arc<dyn mcp::oauth::McpCredentialStore>>,
        client: Option<mcp::oauth::McpOAuthClient>,
    ) -> Self {
        let auth = match store {
            Some(store) => mcp::oauth::McpAuth::new(store),
            None => match self.mcp_auth.credential_dir() {
                Some(dir) => mcp::oauth::McpAuth::with_file_store(dir.to_path_buf()),
                None => mcp::oauth::McpAuth::new(self.mcp_auth.store()),
            },
        };
        self.mcp_auth = Arc::new(match client {
            Some(client) => auth.with_client(client),
            None => auth,
        });
        self.mcp_auth.set_policy(self.mcp_policy.clone());
        self
    }

    /// This pool's MCP authorization state.
    #[cfg(feature = "mcp")]
    pub fn mcp_auth(&self) -> Arc<mcp::oauth::McpAuth> {
        self.mcp_auth.clone()
    }

    /// The deployment's managed settings, read from `path`: put on top of the
    /// pool's own settings now and of every settings it loads later.
    pub fn with_managed_settings(
        mut self,
        path: PathBuf,
        managed: Option<base::managed_settings::ManagedSettings>,
    ) -> Self {
        let mut settings = (**self.settings.get_mut()).clone();
        settings.apply_managed(managed.clone());
        *self.settings.get_mut() = Arc::new(settings);
        self.managed_settings_path = path;
        self.managed_settings = managed;
        self
    }

    /// `settings` with this pool's managed layer on top.
    pub(crate) fn with_managed_layer(&self, mut settings: Settings) -> Settings {
        settings.apply_managed(self.managed_settings.clone());
        settings
    }

    #[cfg(feature = "mcp")]
    pub fn with_mcp_policy(mut self, policy: mcp::policy::McpPolicyGate) -> Self {
        self.mcp_policy = Arc::new(policy);
        self.mcp_auth.set_policy(self.mcp_policy.clone());
        self
    }

    /// The enterprise IdP `"grant": "enterprise"` MCP servers exchange the
    /// user's identity at.
    #[cfg(feature = "mcp")]
    pub fn with_mcp_enterprise_idp(self, idp: Option<mcp::config::EnterpriseIdp>) -> Self {
        self.mcp_auth.set_enterprise_idp(idp);
        self
    }

    /// Override how long an unanswered permission prompt may block a turn
    /// before the daemon denies it on the client's behalf. `Duration::ZERO`
    /// means "wait forever" (the engine's own behavior).
    ///
    /// Builder-style rather than another `new()` parameter: `new()` already
    /// takes eleven arguments, and every existing caller (tests included)
    /// wants the default.
    /// Build every session engine with these Rust plugins.
    pub fn with_plugins(mut self, plugins: Vec<Arc<dyn runtime::plugin::Plugin>>) -> Self {
        self.rust_plugins = plugins;
        self
    }

    /// Hand every session engine this pool builds to `customize` last.
    pub fn with_session_customizer(mut self, customize: SessionCustomizer) -> Self {
        self.customize = Some(customize);
        self
    }

    pub fn with_permission_prompt_timeout(mut self, timeout: Duration) -> Self {
        self.permission_prompt_timeout = timeout;
        self
    }

    /// Subscribe to daemon-level async notifications (MCP connect
    /// outcomes, import auto-detection, ...) — see `daemon.subscribeEvents`.
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value> {
        self.events_tx.subscribe()
    }

    /// Publish a daemon-level notification to every current subscriber. A
    /// `send` error just means nobody's listening right now — not a real
    /// failure, so this is intentionally infallible from the caller's side.
    pub fn emit_event(&self, event: serde_json::Value) {
        self.record_daemon_event(&event);
        let _ = self.events_tx.send(event);
    }

    /// The `daemon.event`s that are also pool-level telemetry. Read off the
    /// event rather than recorded at each emission site, of which there are
    /// several per kind.
    fn record_daemon_event(&self, event: &serde_json::Value) {
        let text = |key: &str| event[key].as_str().unwrap_or_default().to_string();
        let recorded = match event["kind"].as_str() {
            Some("mcp_connected") => telemetry::TelemetryEvent::mcp_server_connected(
                "",
                0,
                None,
                telemetry::McpServerConnectedPayload {
                    server_name: text("server"),
                    transport: text("transport"),
                    tool_count: event["tool_count"].as_u64().unwrap_or(0) as usize,
                },
            ),
            Some("mcp_connect_failed") => telemetry::TelemetryEvent::mcp_connection_error(
                "",
                0,
                None,
                telemetry::McpConnectionErrorPayload {
                    server_name: text("server"),
                    error_message: text("error"),
                },
            ),
            _ => return,
        };
        let _ = self.pool_telemetry.record(recorded);
    }

    /// Record how long the daemon took to start, in the pool-level
    /// telemetry.
    pub fn record_startup(&self, timing: telemetry::StartupTimingPayload) {
        let _ = self
            .pool_telemetry
            .record(telemetry::TelemetryEvent::startup_timing(
                "", 0, None, timing,
            ));
    }

    /// A session came into or left memory, or was deleted — a `daemon.event`
    /// so a host learns of it without subscribing to every session.
    fn emit_session_event(&self, kind: &str, session_id: &str, extra: serde_json::Value) {
        let mut event = serde_json::json!({"kind": kind, "session_id": session_id});
        if let (Some(event), serde_json::Value::Object(extra)) = (event.as_object_mut(), extra) {
            event.extend(extra);
        }
        self.emit_event(event);
    }

    /// The project directory this pool's sessions run in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Full slash command catalog — built-in locals + skill-derived
    /// (see `build_shared_commands`) — for the
    /// `commands.list` RPC. Executing one is not a separate RPC: send
    /// `/name args` as the `message` of `session.run_turn`, which already
    /// intercepts and runs it (see `runtime::turn::process_turn`).
    pub async fn list_commands(&self) -> Vec<runtime::commands::CommandInfo> {
        self.default_binding().await.commands.list_detailed()
    }
}

/// A session's turn slot, taken from it.
struct HeldSlot {
    slot: Arc<AsyncMutex<Option<EventReceiver>>>,
    event_rx: EventReceiver,
}

/// Why [`SessionPool::rebuild_idle`] did not rebuild.
enum Rebuild {
    NotLive,
    /// Running the turn named, if one has started yet, or the background
    /// tasks named.
    Busy {
        current_turn: Option<String>,
        running_tasks: Vec<String>,
    },
    Failed(String),
}

impl From<Rebuild> for SetModelError {
    fn from(e: Rebuild) -> Self {
        match e {
            Rebuild::Busy {
                current_turn,
                running_tasks,
            } => SetModelError::Busy {
                current_turn_id: current_turn,
                running_task_ids: running_tasks,
            },
            Rebuild::NotLive => SetModelError::Rpc((
                codes::SESSION_NOT_FOUND,
                "session is no longer running".to_string(),
            )),
            Rebuild::Failed(e) => SetModelError::Rpc((codes::INTERNAL_ERROR, e)),
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

struct Discard;

#[async_trait::async_trait]
impl crate::rpc::FrameSink for Discard {
    async fn send_json(&self, _json: String) -> bool {
        true
    }
}
