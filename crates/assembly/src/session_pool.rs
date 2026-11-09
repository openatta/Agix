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
    /// The settings files of this binding's tiers that did not exist when it
    /// was built, so are not watched — see [`ProjectBinding::appeared`].
    unwatched_settings: Vec<(crate::watch_hub::Tier, PathBuf)>,
    /// Set once a tier this binding was merged from changed. Every session
    /// built on it holds the same flag and rebuilds before its next turn.
    stale: Arc<std::sync::atomic::AtomicBool>,
}

impl ProjectBinding {
    fn build(
        settings: Arc<Settings>,
        plugin_agent_types: &[base::agent_type::AgentTypeDefinition],
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
        let modules = crate::modules::ProjectModules::build(&settings, plugin_agent_types);
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

    /// The tier of a settings file that has been created since this binding
    /// was built. Nothing watched it — a missing file is not subscribed, see
    /// `SessionPool::watch_tiers_of` — so this is the only way its first
    /// contents (a rule `permit_always` wrote to a new `settings.local.json`,
    /// say) reach the next session without a `config.reload`.
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

    /// Re-merge what the plugin set now contributes. Skill directories are
    /// not re-scanned: the catalog every session on the pair shares is
    /// live-reloaded, and `commands` is a view onto that same catalog.
    fn refresh_plugins(&self, plugin_agent_types: &[base::agent_type::AgentTypeDefinition]) {
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
    process_groups: Option<Arc<base::interface::exec::local::GroupLedger>>,
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

/// Whether `settings` came with a project tier of its own: one whose `.agix`
/// is neither the global nor the scene directory. A project rooted at
/// `$HOME` shares `~/.agix` with the global tier, and those files are the
/// user's.
#[cfg(feature = "mcp")]
fn has_own_project_tier(settings: &Settings) -> bool {
    is_own_project_dir(
        &settings.paths.local_data_dir,
        &settings.paths.global_data_dir,
        &settings.paths.user_data_dir,
    )
}

#[cfg(feature = "mcp")]
fn is_own_project_dir(project: &Path, global: &Path, scene: &Path) -> bool {
    let same = |a: &Path, b: &Path| {
        a == b
            || matches!(
                (a.canonicalize(), b.canonicalize()),
                (Ok(a), Ok(b)) if a == b
            )
    };
    !same(project, global) && !same(project, scene)
}

/// The `mcp_servers` keys the project tier's own files (`settings.json`,
/// `settings.local.json`) declare. A key a project sets at all, even one
/// field of a global server's, makes that server the project's.
#[cfg(feature = "mcp")]
fn project_tier_mcp_servers(settings: &Settings) -> std::collections::HashSet<String> {
    if !has_own_project_tier(settings) {
        return Default::default();
    }
    use base::interface::settings::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};
    [SETTINGS_FILE, SETTINGS_LOCAL_FILE]
        .iter()
        .filter_map(|file| std::fs::read_to_string(settings.paths.local_data_dir.join(file)).ok())
        .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter_map(|v| v.get("mcp_servers")?.as_object().cloned())
        .flat_map(|servers| servers.into_iter().map(|(name, _)| name))
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

    /// The MCP servers a `(project, scene)` with these settings connects:
    /// its `mcp_servers` and every active plugin's `[[mcp]]` declarations.
    #[cfg(feature = "mcp")]
    fn configured_mcp_servers(&self, settings: &Settings) -> HashMap<String, serde_json::Value> {
        let mut servers = settings.mcp_servers.clone();
        servers.extend(self.plugins.mcp_servers());
        servers
    }

    /// The one gate every MCP server passes before this pool connects it.
    #[cfg(feature = "mcp")]
    fn admit_mcp(&self, name: &str, cfg: &mcp::config::McpServerConfig) -> Result<(), String> {
        self.mcp_policy.admit(name, cfg).inspect_err(|reason| {
            tracing::warn!(server = %name, %reason, "MCP server not connected");
        })
    }

    /// Parse the servers `settings` configure and pass each through
    /// `admit_mcp`, a project's own after `confined_to_own_env`. Every entry
    /// that will not be connected gets a `mcp_connect_failed` event here.
    #[cfg(feature = "mcp")]
    fn admit_mcp_servers(&self, settings: &Settings) -> AdmittedMcp {
        let plugin_servers = self.plugins.mcp_servers();
        let projects_own: std::collections::HashSet<String> = project_tier_mcp_servers(settings)
            .into_iter()
            .filter(|name| !plugin_servers.contains_key(name))
            .collect();
        let mut admitted = AdmittedMcp::default();
        for (name, v) in &self.configured_mcp_servers(settings) {
            let parsed = serde_json::from_value::<mcp::config::McpServerConfig>(v.clone())
                .map_err(|e| {
                    tracing::warn!(server = %name, error = %e, "invalid mcp_servers config, skipping");
                    format!("invalid config: {e}")
                })
                .and_then(|cfg| {
                    if projects_own.contains(name) {
                        cfg.confined_to_own_env().inspect_err(|reason| {
                            tracing::warn!(server = %name, %reason, "MCP server not connected");
                        })
                    } else {
                        Ok(cfg)
                    }
                });
            let refusal = match parsed {
                Ok(cfg) => match self.admit_mcp(name, &cfg) {
                    Ok(()) => {
                        admitted.servers.insert(name.clone(), cfg);
                        continue;
                    }
                    Err(reason) => {
                        admitted.denied.push(mcp::manager::DeniedServer {
                            name: name.clone(),
                            transport: cfg.transport_kind(),
                            reason: reason.clone(),
                        });
                        reason
                    }
                },
                Err(reason) => reason,
            };
            self.emit_event(serde_json::json!({
                "kind": "mcp_connect_failed",
                "server": name,
                "error": refusal,
            }));
            admitted.refused.push(name.clone());
        }
        admitted
    }

    fn scene_registry(&self) -> Arc<scene::SceneRegistry> {
        self.scene_registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Take up the scenes installed packages own.
    ///
    /// **Every embedder that constructs a `SessionPool` must call this
    /// before serving**: `new` is synchronous and cannot, and a session
    /// created before it runs cannot enter a package's scene.
    pub async fn load_plugins(&self) {
        self.adopt_plugin_scenes().await;
    }

    /// Rebuild the scene registry from the built-ins plus whatever the
    /// current plugin set owns, and make those scenes servable.
    ///
    /// Plugin scenes are activated rather than merely registered: installing
    /// and enabling a plugin that owns a scene *is* the user's consent to
    /// serve it, and requiring a second `scene.activate` would be a step
    /// with no safety it buys — entering the scene is still an explicit
    /// `session.create`.
    ///
    /// Withdrawing one removes it from both maps, which stops new sessions
    /// from entering it. Sessions already inside keep the `Arc` they were
    /// built with and run to completion — the alternative would be pulling a
    /// session's system prompt out from under it mid-turn.
    async fn adopt_plugin_scenes(&self) {
        let plugin_scenes = self.plugins.scenes();

        let (registry, _) = self.scene_registry_now(&plugin_scenes);
        let plugin_ids: std::collections::HashSet<String> =
            plugin_scenes.iter().map(|s| s.id().to_string()).collect();
        *self
            .scene_registry
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);

        let mut active = self.active_scenes.write().await;
        active.retain(|id, _| !id.starts_with("plugin:") || plugin_ids.contains(id));
        for s in plugin_scenes {
            active.insert(s.id().to_string(), s);
        }
    }

    /// Whether this binary has the plugin subsystem, and whether it is on —
    /// see `crate::plugins::PluginStatus`. `daemon.doctor` reports it, and
    /// the `plugin.*` RPCs refuse with `PLUGINS_DISABLED` when it is not
    /// `Enabled`.
    pub fn plugin_status(&self) -> crate::plugins::PluginStatus {
        self.plugins.status()
    }

    /// Every installed plugin with its current enable state — for the
    /// `plugin.list` RPC. Includes disabled ones, since a management UI needs
    /// to show and re-enable them.
    pub async fn list_plugins(&self) -> Vec<serde_json::Value> {
        self.plugins.list()
    }

    /// Install a plugin from an explicit source. `scope` picks which tier's
    /// cache it lands in ("global", the default a caller should use unless it
    /// specifically wants a scene-only install, or "scene"). Refreshes the
    /// active set on success so sessions created after this call see it —
    /// already-running sessions are unaffected, same as
    /// `config.setProvider`/`mcp.addServer`.
    /// Re-read the plugin directories and reload what is there.
    ///
    /// Install/uninstall/enable/disable each refresh on their own, so this is
    /// for the case none of them covers: the files changed without going
    /// through an RPC — a plugin dropped into the directory by hand, or a
    /// rebuilt component swapped in during development. Without it the only
    /// way to pick that up is restarting the daemon.
    ///
    /// Returns the installed set afterwards, so a caller learns what the
    /// rescan actually found without a second call.
    pub async fn reload_plugins(&self) -> serde_json::Value {
        self.plugins.refresh().await;
        self.refresh_after_plugin_change().await;
        serde_json::json!({"plugins": self.list_plugins().await})
    }

    pub async fn install_plugin(
        &self,
        name: &str,
        version: &str,
        download_url: &str,
        checksum: Option<&str>,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let out = self
            .plugins
            .install(name, version, download_url, checksum, scope)
            .await?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Uninstall a plugin (all versions, or a specific one) from `scope`'s
    /// tier — see `install_plugin` for the refresh semantics.
    pub async fn uninstall_plugin(
        &self,
        name: &str,
        version: Option<&str>,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let out = self.plugins.uninstall(name, version, scope).await?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Enable or disable a plugin by name in `scope`'s tier — see
    /// `install_plugin` for the refresh semantics.
    pub async fn set_plugin_enabled(
        &self,
        name: &str,
        enabled: bool,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let out = self.plugins.set_enabled(name, enabled, scope).await?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Re-merge whatever the plugin set now contributes into the pool-wide
    /// catalogs. The subsystem has already re-read itself by this point; what
    /// is left is the state this pool derives from it. Skill directories are
    /// deliberately not re-scanned: `self.skill_catalog` is the one
    /// persistent, live-reloaded manager every session shares.
    async fn refresh_after_plugin_change(&self) {
        self.adopt_plugin_scenes().await;
        let plugin_agent_types = self.plugins.agent_types();
        for binding in self.bindings.lock().await.values() {
            binding.refresh_plugins(&plugin_agent_types);
        }
    }

    /// Current MCP connection status for this pool's *default* project —
    /// see `mcp::manager::McpManager::server_statuses`. A pure cache read:
    /// unlike `mcp_for_project`, this never triggers a connect, so an
    /// unconnected (or not-yet-connected) project just reports no servers
    /// rather than paying for a connect attempt on a status query.
    #[cfg(feature = "mcp")]
    pub async fn mcp_status(&self) -> Vec<serde_json::Value> {
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let manager = self
            .mcp_by_project
            .lock()
            .await
            .get(&(Some(self.cwd.clone()), startup_scene))
            .cloned()
            .unwrap_or_else(|| Arc::new(McpManager::empty()));
        mcp_status_entries(&manager)
    }

    /// Connect the default project's MCP servers (`configured_mcp_servers`)
    /// in the background — never blocks the caller. Each outcome is a
    /// `daemon.event`: `mcp_connected`, or `mcp_connect_failed` for a config
    /// that does not parse, a server the policy denies, or one that could
    /// not be reached, with the reason. This is the default project's first
    /// connect (see `first_mcp_connect`), so a session created meanwhile
    /// waits for it rather than connecting again.
    #[cfg(feature = "mcp")]
    pub fn connect_mcp_servers_in_background(self: &Arc<Self>) {
        let _ = self.this.set(Arc::downgrade(self));
        self.watch_mcp_auth();
        let pool = self.clone();
        tokio::spawn(async move {
            let settings = pool.settings.read().await.clone();
            if pool.configured_mcp_servers(&settings).is_empty() {
                return;
            }
            let key = (Some(pool.cwd.clone()), settings.paths.scope.clone());
            let manager = pool.first_mcp_connect(key, &settings).await;
            for status in manager.server_statuses() {
                pool.emit_event(serde_json::json!({
                    "kind": "mcp_connected",
                    "server": status.name,
                    "transport": status.transport,
                    "tool_count": status.tool_count,
                }));
            }
            let not_connected = manager.failed().iter().map(|f| &f.name);
            for name in not_connected.chain(manager.pending().iter().map(|p| &p.name)) {
                pool.emit_event(mcp_connect_failed_event(&manager, name));
            }
        });
    }

    /// The pair's MCP manager: the cached one, or a new connect of the
    /// servers `settings` declare, cached. The connect returns after
    /// [`MCP_SESSION_CONNECT_WAIT`]; a server still connecting then is
    /// finished by [`Self::finish_mcp_connects`].
    #[cfg(feature = "mcp")]
    async fn first_mcp_connect(
        &self,
        key: ProjectSceneKey,
        settings: &Settings,
    ) -> Arc<McpManager> {
        let _first = self.mcp_first_connect.lock().await;
        if let Some(existing) = self.mcp_by_project.lock().await.get(&key) {
            return existing.clone();
        }
        let AdmittedMcp {
            servers: admitted,
            denied,
            ..
        } = self.admit_mcp_servers(settings);
        let pool = self.this.get().and_then(std::sync::Weak::upgrade);
        let wait = pool.as_ref().map(|_| MCP_SESSION_CONNECT_WAIT);
        let (mut manager, late) = McpManager::connect_all_within(
            admitted,
            settings.sandbox.subprocess_env.clone(),
            Some(self.mcp_auth.clone()),
            wait,
        )
        .await;
        // A server may have announced a change while this manager was being
        // built, after its list was read.
        manager.refresh_if_changed().await;
        let manager = Arc::new(manager.with_denied(denied));
        self.mcp_by_project
            .lock()
            .await
            .insert(key.clone(), manager.clone());
        if let (Some(pool), Some(late)) = (pool, late) {
            tokio::spawn(Self::finish_mcp_connects(Arc::downgrade(&pool), key, late));
        }
        manager
    }

    /// Take each server that finishes after its pair's manager was handed
    /// out into that manager, announce it, and have the pair's sessions
    /// rebuilt on it before their next turn. Stops when the pair's manager
    /// has been replaced or dropped since — a reload connects anew.
    #[cfg(feature = "mcp")]
    async fn finish_mcp_connects(
        pool: std::sync::Weak<Self>,
        key: ProjectSceneKey,
        mut late: mcp::manager::LateConnects,
    ) {
        while let Some(outcome) = late.next().await {
            let Some(pool) = pool.upgrade() else {
                return;
            };
            let server = outcome.server().to_string();
            let connected = outcome.connected();
            let updated = {
                let mut cache = pool.mcp_by_project.lock().await;
                let Some(current) = cache.get(&key) else {
                    return;
                };
                let mut updated = current.share();
                if !updated.absorb(outcome) {
                    continue;
                }
                let updated = Arc::new(updated);
                cache.insert(key.clone(), updated.clone());
                updated
            };
            pool.emit_event(match connected {
                Some(s) => serde_json::json!({
                    "kind": "mcp_connected",
                    "server": s.name,
                    "transport": s.transport,
                    "tool_count": s.tool_count,
                }),
                None => mcp_connect_failed_event(&updated, &server),
            });
            pool.invalidate_mcp_keys(std::slice::from_ref(&key)).await;
        }
    }

    /// Reconnect MCP servers to match `mcp_servers` (from freshly-reloaded
    /// `Settings`) — the `config.reload`/`config.setProvider` counterpart to
    /// `connect_mcp_servers_in_background`, which runs once at daemon
    /// startup. `create()` sources MCP from the `mcp_by_project` cache, not
    /// from `Settings`, so a reloaded `mcp_servers` reaches sessions only
    /// through this.
    ///
    /// Connects the *complete* new config rather than diffing it against the
    /// old one: a config that removed every server produces an empty
    /// `McpManager`, which disconnects everything. Awaited inline, unlike the
    /// startup path, so the `config.reload` / `config.setProvider` response
    /// reports the connections as they are — but for at most
    /// [`MCP_SESSION_CONNECT_WAIT`]: a server still connecting then is
    /// reported as connecting and finished in the background like a
    /// session's (`finish_mcp_connects`).
    ///
    /// Only replaces `mcp_by_project`'s entry for `(project_root, scene_id)`;
    /// every other pair's cached connections are left untouched.
    ///
    /// Returns `(connected, failed, connecting)` server names for
    /// `ReloadReport`.
    #[cfg(feature = "mcp")]
    async fn reconcile_mcp_servers(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        settings: &Settings,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        let AdmittedMcp {
            servers: admitted,
            denied,
            refused,
        } = self.admit_mcp_servers(settings);
        let requested: std::collections::HashSet<String> = admitted.keys().cloned().collect();
        let pool = self.this.get().and_then(std::sync::Weak::upgrade);
        let wait = pool.as_ref().map(|_| MCP_SESSION_CONNECT_WAIT);
        let (manager, late) = McpManager::connect_all_within(
            admitted,
            settings.sandbox.subprocess_env.clone(),
            Some(self.mcp_auth.clone()),
            wait,
        )
        .await;
        let manager = manager.with_denied(denied);
        let statuses = manager.server_statuses();
        let connected: Vec<String> = statuses.iter().map(|s| s.name.clone()).collect();
        let mut connecting: Vec<String> = manager
            .connecting()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        connecting.sort();
        let settled: std::collections::HashSet<&String> =
            connected.iter().chain(&connecting).collect();
        let unreachable: Vec<String> = requested
            .iter()
            .filter(|name| !settled.contains(name))
            .cloned()
            .collect();

        for status in &statuses {
            self.emit_event(serde_json::json!({
                "kind": "mcp_connected",
                "server": status.name,
                "transport": status.transport,
                "tool_count": status.tool_count,
            }));
        }
        for name in &unreachable {
            self.emit_event(mcp_connect_failed_event(&manager, name));
        }
        let mut failed = unreachable;
        failed.extend(refused);

        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        self.mcp_by_project
            .lock()
            .await
            .insert(key.clone(), Arc::new(manager));
        if let (Some(pool), Some(late)) = (pool, late) {
            tokio::spawn(Self::finish_mcp_connects(Arc::downgrade(&pool), key, late));
        }
        (connected, failed, connecting)
    }

    /// Merge an MCP server config patch into the project-tier settings.json
    /// (same partial-patch semantics as `set_provider` — see its doc
    /// comment for why raw JSON, not a typed struct), then connect it live
    /// so it's usable immediately without a daemon restart.
    #[cfg(feature = "mcp")]
    pub async fn add_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        config: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        self.put_mcp_server(project_root, scene_id, name, config, false)
            .await
    }

    /// `mcp.updateServer` — replace the configuration of a server the
    /// project's settings already declare (a rotated token, a new header) and
    /// reconnect it. Sessions on the project pick the new connection up at
    /// their next turn.
    #[cfg(feature = "mcp")]
    pub async fn update_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        config: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        self.put_mcp_server(project_root, scene_id, name, config, true)
            .await
    }

    /// Refused before anything is written when the MCP policy denies it:
    /// a settings entry the pool would only ever deny is no use to anyone.
    #[cfg(feature = "mcp")]
    async fn put_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        config: serde_json::Value,
        must_exist: bool,
    ) -> Result<serde_json::Value, RpcError> {
        let invalid = |message: String| (crate::rpc::codes::INVALID_PARAMS, message);
        if name.trim().is_empty() {
            return Err(invalid("name must not be empty".into()));
        }
        if !config.is_object() {
            return Err(invalid("config must be a JSON object".into()));
        }
        let mut cfg: mcp::config::McpServerConfig = serde_json::from_value(config.clone())
            .map_err(|e| invalid(format!("invalid mcp server config: {e}")))?;
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let written_to = self.settings_path_of(project_root);
        if is_own_project_dir(
            written_to.parent().unwrap_or(&written_to),
            &self.paths.global_root(),
            &self.scene_root(scene_id, &startup_scene),
        ) {
            cfg = cfg.confined_to_own_env().map_err(invalid)?;
        }
        self.admit_mcp(name, &cfg)
            .map_err(|reason| (crate::rpc::codes::MCP_SERVER_DENIED, reason))?;
        self.write_mcp_server(project_root, scene_id, name, &cfg, config, must_exist)
            .await
            .map_err(invalid)
    }

    #[cfg(feature = "mcp")]
    async fn write_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        cfg: &mcp::config::McpServerConfig,
        config: serde_json::Value,
        must_exist: bool,
    ) -> Result<serde_json::Value, String> {
        let settings_path = self.settings_path_of(project_root);
        let mut project_json: serde_json::Value = if settings_path.exists() {
            let text = tokio::fs::read_to_string(&settings_path)
                .await
                .map_err(|e| format!("read {}: {e}", settings_path.display()))?;
            let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                format!(
                    "existing {} is not valid JSON, refusing to overwrite it: {e}",
                    settings_path.display()
                )
            })?;
            if !parsed.is_object() {
                return Err(format!(
                    "existing {} is not a JSON object at the top level, refusing to overwrite it",
                    settings_path.display()
                ));
            }
            parsed
        } else {
            serde_json::json!({})
        };
        let obj = project_json
            .as_object_mut()
            .expect("verified to be an object above");
        let mcp_servers = obj
            .entry("mcp_servers")
            .or_insert_with(|| serde_json::json!({}));
        if !mcp_servers.is_object() {
            *mcp_servers = serde_json::json!({});
        }
        if must_exist && mcp_servers.get(name).is_none() {
            return Err(format!(
                "no mcp server `{name}` in {}",
                settings_path.display()
            ));
        }
        mcp_servers
            .as_object_mut()
            .expect("just normalized to object above")
            .insert(name.to_string(), config);

        if let Some(parent) = settings_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let pretty = serde_json::to_string_pretty(&project_json).map_err(|e| e.to_string())?;
        tokio::fs::write(&settings_path, pretty)
            .await
            .map_err(|e| format!("write {}: {e}", settings_path.display()))?;

        // The written file and the cache entry are the same pair: a server
        // added to one project's settings connects for that project.
        let target = (
            Some(project_root.unwrap_or(self.cwd.as_path()).to_path_buf()),
            scene_id.to_string(),
        );
        let env = self
            .settings_for_project(target.0.as_deref(), scene_id)
            .await
            .sandbox
            .subprocess_env
            .clone();
        let (statuses, entries, failure) = {
            let mut cache = self.mcp_by_project.lock().await;
            let current = cache
                .get(&target)
                .cloned()
                .unwrap_or_else(|| Arc::new(McpManager::empty()));
            let still_denied = current
                .denied()
                .iter()
                .filter(|d| d.name != name)
                .cloned()
                .collect();
            let mut updated = current
                .share()
                .with_subprocess_env(env.clone())
                .with_auth(Some(self.mcp_auth.clone()))
                .with_denied(still_denied);
            updated.refresh_tools().await;
            updated.add_server(name, cfg).await;
            let statuses = updated.server_statuses();
            let entries = mcp_status_entries(&updated);
            let failure = mcp_connect_failed_event(&updated, name);
            cache.insert(target.clone(), Arc::new(updated));
            (statuses, entries, failure)
        };
        // A running session holds its own manager, cloned from the cache when
        // its engine was built. Marking its binding stale rebuilds it on the
        // new connection before its next turn; nothing in flight is touched.
        if let Some(root) = target.0 {
            self.invalidate_bindings(Some(&crate::watch_hub::Tier::Project(root)))
                .await;
        }

        // Same notification contract as the startup background connect —
        // see `connect_mcp_servers_in_background`.
        match statuses.iter().find(|s| s.name == name) {
            Some(s) => self.emit_event(serde_json::json!({
                "kind": "mcp_connected",
                "server": s.name,
                "transport": s.transport,
                "tool_count": s.tool_count,
            })),
            None => self.emit_event(failure),
        }

        Ok(serde_json::json!({
            "written_to": settings_path.display().to_string(),
            "servers": entries,
        }))
    }

    /// Report this pool's MCP authorization transitions as `daemon.event`s,
    /// and take a server whose tokens stopped working off every connection
    /// that has it.
    #[cfg(feature = "mcp")]
    fn watch_mcp_auth(self: &Arc<Self>) {
        use mcp::oauth::AuthEvent;
        let pool = Arc::downgrade(self);
        self.mcp_auth.set_listener(Arc::new(move |event| {
            let Some(pool) = pool.upgrade() else {
                return;
            };
            pool.emit_event(match &event {
                AuthEvent::Required { server, scopes, .. }
                | AuthEvent::StepUp { server, scopes, .. } => serde_json::json!({
                    "kind": "mcp_auth_required", "server": server, "scopes": scopes,
                }),
                AuthEvent::Completed { server, .. } => serde_json::json!({
                    "kind": "mcp_auth_completed", "server": server,
                }),
                AuthEvent::Failed { server, error, .. } => serde_json::json!({
                    "kind": "mcp_auth_failed", "server": server, "error": error,
                }),
            });
            if let AuthEvent::Required { url, .. } = event {
                tokio::spawn(async move { pool.demote_mcp_servers(&url).await });
            }
        }));
    }

    #[cfg(feature = "mcp")]
    async fn invalidate_mcp_keys(&self, keys: &[ProjectSceneKey]) {
        use crate::watch_hub::Tier;
        for (root, scene) in keys {
            let tier = match root {
                Some(root) => Tier::Project(root.clone()),
                None => Tier::Scene(scene.clone()),
            };
            self.invalidate_bindings(Some(&tier)).await;
        }
    }

    /// Disconnect every server at `url` until someone signs in again.
    #[cfg(feature = "mcp")]
    async fn demote_mcp_servers(&self, url: &str) {
        let mut touched = Vec::new();
        {
            let mut cache = self.mcp_by_project.lock().await;
            for (key, manager) in cache.iter_mut() {
                let names: Vec<String> = manager
                    .servers_at(url)
                    .into_iter()
                    .filter(|n| manager.clients().iter().any(|c| c.server_name() == n))
                    .collect();
                if names.is_empty() {
                    continue;
                }
                let mut updated = manager.share();
                for name in &names {
                    updated.demote(name);
                }
                updated.refresh_tools().await;
                *manager = Arc::new(updated);
                touched.push(key.clone());
            }
        }
        self.invalidate_mcp_keys(&touched).await;
    }

    /// Connect again every server at `url` — and only those — wherever this
    /// pool has it, connected or waiting for a sign-in. Each outcome is a
    /// `mcp_connected` / `mcp_connect_failed` event.
    #[cfg(feature = "mcp")]
    async fn reconnect_mcp_servers(&self, url: &str) -> Vec<String> {
        self.reconnect_mcp_where(|manager| manager.servers_at(url))
            .await
    }

    /// [`Self::reconnect_mcp_servers`] for the servers `pick` names in each
    /// cached manager.
    #[cfg(feature = "mcp")]
    async fn reconnect_mcp_where(&self, pick: impl Fn(&McpManager) -> Vec<String>) -> Vec<String> {
        let mut touched = Vec::new();
        let mut outcomes: HashMap<String, Result<mcp::manager::ServerStatus, Option<String>>> =
            HashMap::new();
        {
            let mut cache = self.mcp_by_project.lock().await;
            for (key, manager) in cache.iter_mut() {
                let names = pick(manager);
                if names.is_empty() {
                    continue;
                }
                let mut updated = manager.share();
                for name in &names {
                    if let Some(cfg) = updated.config_of(name).cloned() {
                        updated.add_server(name, &cfg).await;
                    }
                }
                updated.refresh_tools().await;
                let statuses = updated.server_statuses();
                for name in names {
                    let outcome = match statuses.iter().find(|s| s.name == name) {
                        Some(status) => Ok(status.clone()),
                        None => Err(updated
                            .failed()
                            .iter()
                            .find(|f| f.name == name)
                            .map(|f| f.error.clone())),
                    };
                    let telling = |o: &Result<_, Option<String>>| match o {
                        Ok(_) => 2,
                        Err(Some(_)) => 1,
                        Err(None) => 0,
                    };
                    let seen = outcomes.entry(name).or_insert(Err(None));
                    if telling(&outcome) > telling(seen) {
                        *seen = outcome;
                    }
                }
                *manager = Arc::new(updated);
                touched.push(key.clone());
            }
        }
        self.invalidate_mcp_keys(&touched).await;
        let mut names: Vec<String> = outcomes.keys().cloned().collect();
        names.sort();
        for (name, outcome) in outcomes {
            self.emit_event(match outcome {
                Ok(s) => serde_json::json!({
                    "kind": "mcp_connected",
                    "server": s.name,
                    "transport": s.transport,
                    "tool_count": s.tool_count,
                }),
                Err(error) => serde_json::json!({
                    "kind": "mcp_connect_failed",
                    "server": name,
                    "error": match error {
                        Some(error) => format!("connect failed after authorizing: {error}"),
                        None => "connect failed after authorizing — see mcp.status and the daemon log".into(),
                    },
                }),
            });
        }
        names
    }

    /// `mcp.auth.setIdentity` — the user's enterprise identity, which every
    /// `"grant": "enterprise"` server waiting for one is then connected with.
    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_set_identity(
        &self,
        identity: mcp::oauth::EnterpriseIdentity,
    ) -> Result<serde_json::Value, RpcError> {
        if self.mcp_auth.enterprise_idp().is_none() {
            return Err((
                codes::INVALID_PARAMS,
                "no enterprise IdP is configured: the host's Assembly::mcp_enterprise_idp or \
                 the MCP policy's enterprise_idp names one"
                    .into(),
            ));
        }
        self.mcp_auth.set_identity(Some(identity));
        let reconnected = self
            .reconnect_mcp_where(|manager| {
                manager
                    .pending()
                    .iter()
                    .filter(|p| {
                        manager
                            .config_of(&p.name)
                            .and_then(|c| c.oauth())
                            .is_some_and(|o| o.grant == mcp::config::OAuthGrant::Enterprise)
                    })
                    .map(|p| p.name.clone())
                    .collect()
            })
            .await;
        Ok(serde_json::json!({"reconnected": reconnected}))
    }

    /// The configuration of the MCP server `name` on a project, for a
    /// sign-in. A server the policy denies never gets one.
    #[cfg(feature = "mcp")]
    async fn mcp_server_to_authorize(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
    ) -> Result<mcp::config::McpServerConfig, RpcError> {
        let root = project_root.unwrap_or(self.cwd.as_path());
        let manager = self.mcp_for_project(Some(root), scene_id).await;
        if let Some(cfg) = manager.config_of(name) {
            return Ok(cfg.clone());
        }
        if let Some(denied) = manager.denied().iter().find(|d| d.name == name) {
            return Err((codes::MCP_SERVER_DENIED, denied.reason.clone()));
        }
        Err((
            codes::INVALID_PARAMS,
            format!("no MCP server `{name}` is configured for this project"),
        ))
    }

    /// `mcp.auth.start` — begin a sign-in to an MCP server. A loopback flow
    /// completes itself when the browser comes back; an external one waits
    /// for `mcp.auth.complete`.
    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_start(
        self: &Arc<Self>,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        redirect: mcp::oauth::Redirect,
        scopes: Option<Vec<String>>,
    ) -> Result<serde_json::Value, RpcError> {
        let cfg = self
            .mcp_server_to_authorize(project_root, scene_id, name)
            .await?;
        let mut start = self
            .mcp_auth
            .start(name, &cfg, redirect, scopes)
            .await
            .map_err(auth_flow_error)?;
        if let Some(mut callbacks) = start.callbacks.take() {
            let pool = Arc::downgrade(self);
            let flow_id = start.flow_id.clone();
            tokio::spawn(async move {
                while let Some(callback) = callbacks.recv().await {
                    let Some(pool) = pool.upgrade() else {
                        return;
                    };
                    let outcome = pool.finish_mcp_auth(&flow_id, &callback.url).await;
                    let open = pool.mcp_auth.flow_server(&flow_id).await.is_some();
                    let _ = callback
                        .reply
                        .send(outcome.map(|done| done.server).map_err(|e| e.to_string()));
                    if !open {
                        return;
                    }
                }
            });
        }
        Ok(serde_json::json!({
            "flow_id": start.flow_id,
            "authorize_url": start.authorize_url,
            "redirect_uri": start.redirect_uri,
            "expires_in": start.expires_in,
        }))
    }

    #[cfg(feature = "mcp")]
    async fn finish_mcp_auth(
        &self,
        flow_id: &str,
        callback_url: &str,
    ) -> Result<mcp::oauth::FlowDone, mcp::oauth::AuthFlowError> {
        let done = self.mcp_auth.complete(flow_id, callback_url).await?;
        self.reconnect_mcp_servers(&done.url).await;
        Ok(done)
    }

    /// `mcp.auth.complete` — finish a sign-in with the URL the browser was
    /// sent back to, then reconnect that server.
    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_complete(
        &self,
        flow_id: &str,
        callback_url: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let done = self
            .finish_mcp_auth(flow_id, callback_url)
            .await
            .map_err(auth_flow_error)?;
        Ok(serde_json::json!({"server": done.server, "scopes": done.scopes}))
    }

    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_cancel(&self, flow_id: &str) -> Result<serde_json::Value, RpcError> {
        let server = self
            .mcp_auth
            .cancel(flow_id)
            .await
            .map_err(auth_flow_error)?;
        Ok(serde_json::json!({"server": server}))
    }

    /// `mcp.auth.logout` — revoke and forget a server's tokens, and
    /// disconnect it.
    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_logout(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let cfg = self
            .mcp_server_to_authorize(project_root, scene_id, name)
            .await?;
        self.mcp_auth
            .logout(name, &cfg)
            .await
            .map_err(auth_flow_error)?;
        if let Some(url) =
            mcp::oauth::server_url(&cfg).and_then(|u| mcp::oauth::canonical_server_url(&u).ok())
        {
            self.demote_mcp_servers(&url).await;
        }
        Ok(serde_json::json!({"server": name}))
    }

    /// Read back the current effective provider/task_models configuration —
    /// the read-side counterpart to `set_provider`. `api_key` is redacted
    /// (`***<last 4 chars>`) unless `include_secrets` is `true` — default
    /// to redacted so a caller that just wants to know *which* providers
    /// are configured doesn't accidentally end up with plaintext keys in
    /// its own logs/UI. See the trust-boundary note on `config.setProvider`
    /// in `crates/assembly/src/server.rs` — `include_secrets: true` is still gated
    /// by nothing more than "can reach this socket", same as every other
    /// method here.
    pub async fn get_providers(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        include_secrets: bool,
    ) -> serde_json::Value {
        let settings = self
            .binding_for(Some(project_root.unwrap_or(self.cwd.as_path())), scene_id)
            .await
            .settings
            .clone();
        let providers: serde_json::Map<String, serde_json::Value> = settings
            .providers
            .iter()
            .map(|(id, cfg)| {
                let api_key = match &cfg.api_key {
                    None => serde_json::Value::Null,
                    Some(k) if include_secrets => serde_json::Value::String(k.clone()),
                    Some(k) => serde_json::Value::String(redact_secret(k)),
                };
                (
                    id.clone(),
                    serde_json::json!({
                        "api_type": cfg.api_type,
                        "base_url": cfg.base_url,
                        "api_key": api_key,
                        "default_model": cfg.default_model,
                        "models": cfg.models,
                    }),
                )
            })
            .collect();
        serde_json::json!({
            "providers": providers,
            "default_provider": settings.default_provider,
            "task_models": settings.task_models,
        })
    }

    /// Detect importable cross-tool configuration sources (Claude Code/
    /// Codex/Cursor) in this pool's project directory — pure filesystem
    /// detection, no LLM turn needed. The manual `/import` path (the bundled
    /// `import` skill) does its own detection with the file tools.
    pub async fn list_import_sources(&self, project_root: Option<&Path>) -> Vec<serde_json::Value> {
        let project_root = project_root.unwrap_or(self.cwd.as_path());
        base::frozen::detect_import_sources(project_root)
            .await
            .iter()
            .map(|s| serde_json::json!({"source": s.kind().as_str(), "description": s.describe()}))
            .collect()
    }

    /// Execute an import for a previously-detected `source` (one of the
    /// `source` values `list_import_sources` returns) and record the
    /// decision so the automatic startup detection doesn't ask again.
    pub async fn run_import(
        &self,
        project_root: Option<&Path>,
        source: &str,
    ) -> Result<serde_json::Value, String> {
        let project_root = project_root.unwrap_or(self.cwd.as_path());
        let kind = base::frozen::ImportSourceKind::try_parse(source).ok_or_else(|| {
            format!("unknown source `{source}` — expected claude_code, codex, or cursor")
        })?;
        let sources = base::frozen::detect_import_sources(project_root).await;
        let Some(matched) = sources.iter().find(|s| s.kind() == kind) else {
            return Err(format!(
                "source `{source}` not currently detected in this project"
            ));
        };
        let summary = base::frozen::execute_import(project_root, matched)
            .await
            .map_err(|e| e.to_string())?;
        base::frozen::mark_imported(project_root, &sources, kind)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "source": summary.kind.as_str(),
            "actions": summary.actions,
        }))
    }

    /// Read-only config/wiring diagnostics — see `crate::doctor`.
    pub async fn doctor_report(&self) -> serde_json::Value {
        let settings = self.settings.read().await.clone();
        let mut checks: Vec<Arc<dyn base::interface::health::HealthCheck>> = Vec::new();
        #[cfg(feature = "mcp")]
        checks.push(self.mcp_policy.clone());
        checks.extend(self.plugins.health_check());
        checks.extend(
            self.health
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .checks()
                .iter()
                .cloned(),
        );
        crate::doctor::run_doctor(
            self.paths.as_ref(),
            self.scene.id(),
            settings,
            self.history_store.is_some(),
            self.plugins.status(),
            &base::interface::health::HealthChecks::from_vec(checks),
        )
    }

    /// Add a diagnostic of the host's own to what `daemon.doctor` reports.
    pub fn register_health_check(&self, check: Arc<dyn base::interface::health::HealthCheck>) {
        let mut health = self.health.write().unwrap_or_else(|e| e.into_inner());
        let existing = std::mem::take(&mut *health);
        *health = existing.with(check);
    }

    /// The currently-active settings snapshot (for config-inspection RPCs).
    pub async fn settings(&self) -> Arc<Settings> {
        self.settings.read().await.clone()
    }

    /// The project-tier `settings.json` of `project_root`, or of this
    /// daemon's own project when none is named.
    fn settings_path_of(&self, project_root: Option<&Path>) -> PathBuf {
        project_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.paths.project_root())
            .join(".agix")
            .join(base::interface::settings::SETTINGS_FILE)
    }

    /// Merge a provider config patch (and/or `default_provider` /
    /// `task_models` patches) into the project-tier settings.json — its
    /// credential fields into the uncommitted `settings.local.json` — then
    /// reload the full effective `Settings` (global → scene → project) so
    /// validation and any session created after this call see the change —
    /// see `crate::doctor` module docs for the same tri-tier layout.
    ///
    /// `config_patch`/`task_models_patch` are raw partial JSON objects
    /// merged key-by-key onto the existing project-tier file (see
    /// `base::interface::settings::merge_json_values`) — deliberately not
    /// typed `ProviderConfig` structs, since deserializing a partial patch
    /// into a `#[serde(default)]` struct would silently null out every field
    /// the caller didn't mention, clobbering unrelated existing values.
    /// `delete: true` removes the `providers.<id>` entry outright rather
    /// than merging; any `task_models` entry that referenced it degrades to
    /// `default_provider` automatically the next time routing is resolved
    /// (see `base::provider::resolve_task_models`'s soft-fallback) — no
    /// separate "downgrade" write is needed.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_provider(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        provider_id: &str,
        delete: bool,
        config_patch: Option<serde_json::Value>,
        default_provider: Option<String>,
        task_models_patch: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        if provider_id.trim().is_empty() {
            return Err("provider_id must not be empty".into());
        }

        if let Some(ref cfg) = config_patch {
            if !cfg.is_object() {
                return Err("config must be a JSON object".into());
            }
            serde_json::from_value::<base::provider::ProviderConfig>(cfg.clone())
                .map_err(|e| format!("invalid provider config: {e}"))?;
        }
        if let Some(ref tm) = task_models_patch {
            let obj = tm.as_object().ok_or("task_models must be a JSON object")?;
            for (task, ov) in obj {
                serde_json::from_value::<base::provider::TaskModelOverride>(ov.clone())
                    .map_err(|e| format!("invalid task_models.{task}: {e}"))?;
            }
        }

        let settings_path = self.settings_path_of(project_root);
        let local_path =
            settings_path.with_file_name(base::interface::settings::SETTINGS_LOCAL_FILE);
        let mut project_json = read_settings_object(&settings_path).await?;
        let mut local_json = read_settings_object(&local_path).await?;
        let local_before = local_json.clone();
        let obj = project_json
            .as_object_mut()
            .expect("verified to be an object above");
        let local = local_json
            .as_object_mut()
            .expect("verified to be an object above");

        if delete {
            for tier in [&mut *obj, &mut *local] {
                if let Some(providers) = tier.get_mut("providers").and_then(|v| v.as_object_mut()) {
                    providers.remove(provider_id);
                }
            }
        } else if let Some(mut cfg) = config_patch {
            let secrets = take_provider_secrets(&mut cfg);
            let existing = provider_entry(obj, provider_id);
            base::interface::settings::merge_json_values(existing, cfg);
            let mut moved = take_provider_secrets(existing);
            moved.extend(secrets);
            if !moved.is_empty() {
                let local_entry = provider_entry(local, provider_id);
                for (key, value) in moved {
                    local_entry[key] = value;
                }
            }
        }

        if let Some(dp) = default_provider {
            obj.insert(
                "default_provider".to_string(),
                serde_json::Value::String(dp),
            );
        }

        if let Some(tm) = task_models_patch {
            let task_models = obj
                .entry("task_models")
                .or_insert_with(|| serde_json::json!({}));
            if !task_models.is_object() {
                *task_models = serde_json::json!({});
            }
            base::interface::settings::merge_json_values(task_models, tm);
        }

        write_settings_file(&settings_path, &project_json, false).await?;
        let secrets_written_to = if local_json != local_before {
            write_settings_file(&local_path, &local_json, true).await?;
            Some(local_path.display().to_string())
        } else {
            None
        };

        let (reloaded, report) = self
            .apply_reloaded_settings(project_root, scene_id, None)
            .await;
        let mut result = serde_json::json!({
            "written_to": settings_path.display().to_string(),
            "providers": reloaded.providers.keys().collect::<Vec<_>>(),
            "default_provider": reloaded.default_provider,
            "task_models": reloaded.task_models.keys().collect::<Vec<_>>(),
            "routing": report.to_json(),
        });
        if let Some(path) = secrets_written_to {
            result["secrets_written_to"] = serde_json::Value::String(path);
        }

        Ok(result)
    }

    /// Re-read whatever is currently on disk across all three settings.json
    /// tiers and apply it — the `config.reload` RPC's backing implementation,
    /// used when a human (or another process) hand-edited a settings.json
    /// file directly instead of going through `config.setProvider`. Shares
    /// `apply_reloaded_settings()` with `set_provider()`, so both entry
    /// points converge on identical semantics (routing re-resolved,
    /// `task_router` rebuilt, bindings marked stale so already-running
    /// sessions recreate themselves lazily — see `run_turn`).
    pub async fn reload_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> serde_json::Value {
        self.reload_settings_from(project_root, scene_id, None)
            .await
    }

    async fn reload_settings_from(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        changed: Option<&crate::watch_hub::Tier>,
    ) -> serde_json::Value {
        let (reloaded, report) = self
            .apply_reloaded_settings(project_root, scene_id, changed)
            .await;
        serde_json::json!({
            "providers": reloaded.providers.keys().collect::<Vec<_>>(),
            "default_provider": reloaded.default_provider,
            "task_models": reloaded.task_models.keys().collect::<Vec<_>>(),
            "routing": report.to_json(),
        })
    }

    /// Re-read the three-tier merged settings.json from disk, re-resolve
    /// `providers`/`task_models`, rebuild `self.task_router` from the result,
    /// swap `self.settings`, and invalidate the bindings built from `changed`
    /// (every binding when `None`: an explicit reload does not know which tier
    /// moved) so their running sessions recreate themselves before their next
    /// turn (see `run_turn`). Shared by `set_provider()` (called after writing a
    /// patch) and `reload_settings()` (called directly, no patch — just
    /// "whatever's on disk right now").
    ///
    /// Never fails outright — misconfiguration degrades to "keep the
    /// previous `task_router`, report why in the returned `ReloadReport`";
    /// `resolve_task_models` is advisory.
    async fn apply_reloaded_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        changed: Option<&crate::watch_hub::Tier>,
    ) -> (Arc<Settings>, ReloadReport) {
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let project_root = project_root.unwrap_or(self.cwd.as_path());
        let is_default = project_root == self.cwd.as_path() && scene_id == startup_scene;

        // The daemon's own pair keeps its separate, hot-swappable copy in
        // `self.settings` — everything with no session to ask about reads it.
        // Every other pair lives only in its binding, which the invalidation
        // below rebuilds from disk.
        if is_default {
            let default_model = self.settings.read().await.model.model_name.clone();
            let reloaded = Arc::new(Settings::load(
                self.paths.global_root(),
                self.paths.config_root(),
                self.paths.project_root().join(".agix"),
                self.scene.id(),
                &default_model,
            ));
            *self.settings.write().await = reloaded;
        }

        // The cached bindings hold the settings that were just replaced, and
        // the skill/agent catalogs and router built from them. Dropping them
        // is what makes a reload reach the next session.
        self.invalidate_bindings(changed).await;

        let binding = self.binding_for(Some(project_root), scene_id).await;
        #[cfg_attr(not(feature = "mcp"), allow(unused_mut))]
        let mut report = binding.routing.clone();
        #[cfg(feature = "mcp")]
        {
            let (mcp_connected, mcp_failed, mcp_connecting) = self
                .reconcile_mcp_servers(Some(project_root), scene_id, &binding.settings)
                .await;
            report.mcp_connected = mcp_connected;
            report.mcp_failed = mcp_failed;
            report.mcp_connecting = mcp_connecting;
        }

        (binding.settings.clone(), report)
    }

    /// `session.create` — the explicit multi-project entry point, with an
    /// optional `scene` (resolved against `active_scenes` — see
    /// `resolve_scene`; omitted means the pool's default scene). Builds and
    /// starts a new primary session bound to `project_root`, or this pool's
    /// default project for `ProjectSelector::Default`. Unlike the implicit
    /// `run_turn(session_id: None)` path, this is the one place a caller
    /// can ask for a session bound to a project or scene other than the
    /// pool's own.
    pub async fn create_session(
        &self,
        scene_id: Option<&str>,
        project_root: ProjectSelector,
        options: Option<&SessionOptions>,
    ) -> Result<serde_json::Value, RpcError> {
        let Some(scene) = self.resolve_scene(scene_id).await else {
            return Err((
                codes::SCENE_NOT_FOUND,
                format!(
                    "scene is not active on this daemon: {}",
                    scene_id.unwrap_or("")
                ),
            ));
        };

        // Settings are resolved against *this session's* scene, not the
        // daemon's. The `Default` arm goes through the same call as the others
        // so that a non-startup scene on the default project still picks up
        // its own tier; `settings_for_project` short-circuits to the
        // hot-swappable `self.settings` for the one pair that is the pool's
        // own.
        // A scene that requires a project is refused here, not later: with no
        // project root there is nothing for its tools to read or write, and
        // the failure is far more legible at creation than at the first turn.
        if scene.requires_project() && matches!(project_root, ProjectSelector::NoProject) {
            return Err((
                codes::PROJECT_REQUIRED,
                format!(
                    "scene `{}` requires a project; pass project_root",
                    scene.id()
                ),
            ));
        }

        let session_scene_id = scene.id().to_string();
        let (binding, project_root_for_meta) = match project_root {
            ProjectSelector::Default => (
                self.binding_for(Some(&self.cwd.clone()), &session_scene_id)
                    .await,
                Some(self.cwd.clone()),
            ),
            ProjectSelector::NoProject => (self.binding_for(None, &session_scene_id).await, None),
            ProjectSelector::Path(p) => {
                if !p.is_dir() {
                    return Err((
                        codes::PROJECT_NOT_FOUND,
                        format!(
                            "project_root does not exist or is not a directory: {}",
                            p.display()
                        ),
                    ));
                }
                (self.binding_for(Some(&p), &session_scene_id).await, Some(p))
            }
        };

        binding
            .check_model_choice(
                options.and_then(|o| o.provider.as_deref()),
                options.and_then(|o| o.model.as_deref()),
            )
            .map_err(|e| (codes::INVALID_PARAMS, e))?;

        let scene_id_for_result = scene.id().to_string();
        let sid = Id::new().to_string();
        let created_id = self
            .create(
                sid,
                scene,
                binding,
                project_root_for_meta.clone(),
                options,
                false,
            )
            .await
            .map_err(|e| (codes::INTERNAL_ERROR, e))?;

        let created_at = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(&created_id)
                .map(|live| format_instant(live.created_at))
                .unwrap_or_default()
        };

        Ok(serde_json::json!({
            "session_id": created_id,
            "scene": scene_id_for_result,
            "session_kind": "primary",
            "project_root": project_root_for_meta.map(|p| p.display().to_string()),
            "created_at": created_at,
        }))
    }

    /// 创建新 session 并启动 Agent 后台 run loop。
    /// `options` 仅在新创建 session 时生效；已有 session 时忽略。
    ///
    /// `base_settings`/`project_root_for_meta` are the already-resolved
    /// settings for whichever project this session belongs to (see
    /// `settings_for_project`) and the value to stamp on `Meta.project_root`
    /// (`None` = no-project session) — callers that just want "this pool's
    /// default project" pass `self.settings.read().await.clone()` /
    /// `Some(self.cwd.clone())`.
    async fn create(
        &self,
        session_id: String,
        scene: Arc<dyn Scene>,
        binding: Arc<ProjectBinding>,
        project_root_for_meta: Option<PathBuf>,
        options: Option<&SessionOptions>,
        resume: bool,
    ) -> Result<String, String> {
        self.refuse_while_shutting_down()?;
        self.wait_until_stopped(&session_id).await;
        let claim = self.claim_log(&session_id).await?;
        let spec = self.spec_for(&session_id, resume, options).await;
        self.lock_kept_specs().retain(|k| k != &session_id);
        let options = spec.options.as_ref();
        self.note_served(project_root_for_meta.as_deref());
        let scene_id = scene.id().to_string();
        // The `Builder` takes ownership of the scene; the session keeps a
        // handle of its own because `run_turn` has to ask it things.
        let scene_for_session = scene.clone();
        let base_settings = binding.settings.clone();
        let task_router = binding.task_router.clone();

        // Wrap the model in a recorder if this session asked for one.
        //
        // Both the conversation's model *and* every routed provider get
        // wrapped, sharing one `Recorder` so they write into one recording.
        // Wrapping only the conversation's model left sub-agents, compaction
        // and memory extraction — everything reached through the router —
        // recording nothing whenever `providers` was configured.
        let recorder_config =
            options
                .and_then(|o| o.recorder.as_ref())
                .map(|recorder| RecorderConfig {
                    mode: match recorder.mode.as_str() {
                        "record" => RecorderMode::Record,
                        // `rerun` is driven from a recording, not from a live
                        // session, so a session asking for it just talks to the
                        // provider — see `telemetry::recorder::rerun`.
                        "rerun" => RecorderMode::Rerun,
                        _ => RecorderMode::Replay,
                    },
                    name: recorder.name.clone(),
                    root: recordings_root(&base_settings, recorder.dir.as_deref()),
                    on_divergence: if recorder.strict {
                        Divergence::Strict
                    } else {
                        Divergence::Warn
                    },
                });
        let clock = session_clock(options, recorder_config.as_ref(), &session_id);
        let provider_id = spec.provider();
        let conversation_model = match provider_id.as_deref() {
            None => binding.model.clone(),
            Some(id) => binding
                .provider_client(id)
                .ok_or_else(|| format!("provider `{id}` is not configured"))?,
        };
        let model_name = spec.model().or_else(|| {
            provider_id
                .as_ref()
                .and_then(|id| binding.settings.providers.get(id))
                .and_then(|config| config.default_model.clone())
        });
        let (model, task_router) = match recorder_config {
            None => (conversation_model, task_router),
            Some(config) => {
                let shared = spec
                    .recorder
                    .clone()
                    .unwrap_or_else(telemetry::recorder::Recorder::new);
                let wrap =
                    |provider_id: Option<&str>, inner: Arc<dyn base::interface::model::Model>| {
                        let mut m = RecorderModel::shared(
                            inner,
                            Some(config.clone()),
                            env!("CARGO_PKG_VERSION"),
                            shared.clone(),
                        );
                        if let Some(id) = provider_id {
                            m = m.with_provider_id(id);
                        }
                        Arc::new(m) as Arc<dyn base::interface::model::Model>
                    };
                (
                    wrap(provider_id.as_deref(), conversation_model),
                    task_router.map(|r| Arc::new(r.map_models(|id, inner| wrap(Some(id), inner)))),
                )
            }
        };

        // The prompt timeout is a daemon-level flag but the wait it bounds
        // lives in the engine, so it travels as a setting rather than as a
        // daemon-side wrapper around `Permission`. `Settings` is behind an
        // `Arc`, so override on a private clone rather than in place.
        // The mode is decided once and written into the snapshot as well:
        // the gate, the hook payloads and `Meta` all read it from there, and a
        // snapshot still holding the settings-file mode made every one of them
        // report a mode the session was not running in.
        let permission_mode = effective_permission_mode(&base_settings, options);
        let settings_snapshot = {
            let mut snap = (*base_settings).clone();
            snap.execution.permission_prompt_timeout_secs =
                self.permission_prompt_timeout.as_secs();
            snap.permission_mode = permission_mode;
            if let Some(name) = &model_name {
                snap.model.model_name = name.clone();
            }
            Arc::new(snap)
        };
        let namer = SessionNamer::of(
            &session_id,
            task_router.as_deref(),
            &model,
            &settings_snapshot.model.model_name,
        );
        let permission = resolve_session_permission(
            self.permission.as_ref(),
            &self.tools,
            &settings_snapshot,
            permission_mode,
            options,
        );

        let mut builder = Builder::new()
            .scene(scene)
            .model(model)
            .tools(self.tools.clone())
            .settings(settings_snapshot.clone())
            .permission(permission)
            .session_id(session_id.clone())
            .commands_override(binding.commands.clone())
            .cron_store(self.schedule.clone())
            .question_prompts(options.is_some_and(SessionOptions::answers_questions));
        if let Some(ref store) = self.history_store {
            builder = builder.history_store(store.clone());
        }
        if let Some(clock) = clock {
            builder = builder.environment(clock);
        }
        if let Some(router) = task_router {
            builder = builder.task_router(router);
        }
        builder = builder.scene_registry(self.scene_registry());
        // A project's scripts belong to that project: the paths in its
        // settings resolve against it, and so does the question of whether a
        // file is inside it — which is what decides whether the script may
        // rewrite the prompt or only add to it. Binding a session against the
        // pool's own directory instead gets both wrong for every session
        // created with a `project_root` of its own: the paths miss, so the
        // whole set is dropped, and had they hit, the project's own code
        // would have been judged to come from outside it.
        let script_root = project_root_for_meta
            .clone()
            .unwrap_or_else(|| self.cwd.clone());
        let package_scripts = self.plugins.script_bindings();
        #[cfg(feature = "scripts")]
        let script_ledger = {
            let (bound, faults) = bind_scripts(&settings_snapshot, &script_root, &package_scripts);
            self.plugins.note_script_faults(faults);
            match bound {
                Some(bound) => {
                    let ledger = bound.ledger.clone();
                    builder = builder.plugin(Arc::new(script_host::ScriptPlugin::new(bound)));
                    Some(ledger)
                }
                None => None,
            }
        };
        #[cfg(not(feature = "scripts"))]
        let script_ledger = {
            let (_, faults) = bind_scripts(&settings_snapshot, &script_root, &package_scripts);
            self.plugins.note_script_faults(faults);
            None
        };

        // Give this session its own owned `McpManager` built from this
        // project's centrally-connected client handles (cheap `Arc` clones
        // — not a reconnect) — see `SessionPool.mcp_by_project`'s doc
        // comment for why this isn't shared as a single `Arc<McpManager>`
        // directly (`McpManager` needs `&mut self` for `refresh_tools()`,
        // which a shared `Agent` field can't give it). Skipped entirely
        // when nothing's connected, so the common "no MCP servers
        // configured" case pays zero cost.
        #[cfg(not(feature = "mcp"))]
        let mcp_server_count = 0;
        #[cfg(feature = "mcp")]
        let mcp_server_count = {
            let central = self
                .mcp_for_project(project_root_for_meta.as_deref(), &scene_id)
                .await;
            let count = central.server_count();
            if count > 0 || !central.pending().is_empty() {
                let mut per_session = central.share();
                per_session.refresh_tools().await;
                builder = builder.module(Arc::new(mcp::McpModule::new(per_session)));
            }
            count
        };
        // After MCP, so the `Agent` tool sees the servers it may grant a
        // delegate; skills after agents, so forked skills can fork.
        builder = builder.modules(binding.modules.session_modules());

        // On by default: `Builder::build()`'s fallback handle drops its
        // receiver on the spot, so without a recorder every event is lost.
        // The caller's `output` only chooses *where*, and
        // `telemetry.mode: disabled` in settings.json opts out.
        //
        // `TELEMETRY_DIR/<session-id>.jsonl` under the local data dir keeps
        // one file per session, next to the transcripts, so a recording is
        // findable by session id without the caller having had to plan for
        // it in advance.
        // The default location is only used when `local_data_dir` is a real
        // absolute path. It is not always set (tests, embedders that never
        // configure `paths`), and joining onto an empty or relative value
        // would scatter `telemetry/` directories into whatever the process's
        // working directory happens to be — including a user's repository.
        // Recording nothing is much better than writing somewhere surprising.
        let telemetry_path = match options.and_then(|o| o.telemetry.as_ref()) {
            Some(t) => Some(std::path::PathBuf::from(&t.output)),
            None => {
                let dir = &settings_snapshot.paths.local_data_dir;
                dir.is_absolute()
                    .then(|| dir.join("telemetry").join(format!("{session_id}.jsonl")))
            }
        };
        if let (true, Some(telemetry_path)) = (settings_snapshot.telemetry_enabled, telemetry_path)
        {
            match FileRecorder::new(&telemetry_path) {
                Ok(rec) => {
                    // `telemetry` crate never touches the network itself —
                    // this is where the daemon decides where events go:
                    // always the per-session JSONL file, plus OTel when an
                    // endpoint is configured. `start_otel` installs the
                    // global meter/tracer providers, so that one is a
                    // process-level side effect done once, guarded by a
                    // `OnceLock` rather than per session (see `otel_sink`).
                    let mut recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
                        vec![std::sync::Arc::new(rec)];
                    if let Some(otel) = otel_sink(settings_snapshot.telemetry_url.as_deref()) {
                        recorders.push(otel);
                    }
                    let telemetry_config = telemetry::TelemetryConfig {
                        enabled: true,
                        mode: telemetry::TelemetryMode::Enabled,
                        default_event_enabled: true,
                        ..Default::default()
                    };
                    match telemetry::spawn(telemetry_config, recorders) {
                        Ok((handle, consumer)) => {
                            tokio::spawn(consumer);
                            builder = builder.telemetry_handle(handle);
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to start telemetry pipeline; this session records nothing");
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        path = %telemetry_path.display(),
                        error = %e,
                        "could not open the session telemetry file; this session records nothing"
                    );
                }
            }
        }

        if let Some(prompt) = spec.prompt() {
            builder = builder.session_prompt(prompt);
        }
        if let Some(schema) = options.and_then(|o| o.output_schema.clone()) {
            builder = builder.output_schema(schema);
        }
        // A rebuild keeps the ledger: the session goes on, and so do the
        // groups its earlier commands started.
        let process_groups = if base_settings.sandbox.reap_process_groups {
            let kept = self
                .sessions
                .lock()
                .await
                .get(&session_id)
                .and_then(|live| live.process_groups.clone());
            Some(kept.unwrap_or_else(base::interface::exec::local::GroupLedger::new))
        } else {
            None
        };
        if let Some(ledger) = &process_groups {
            builder = builder.process_groups(ledger.clone());
        }
        for plugin in &self.rust_plugins {
            builder = builder.plugin(plugin.clone());
        }
        if let Some(customize) = &self.customize {
            builder = customize(
                builder,
                &SessionCtx {
                    session_id: &session_id,
                    scene: &scene_id,
                    project_root: project_root_for_meta.as_deref(),
                    resumed: resume,
                },
            );
        }
        let (mut agent, event_rx, input_tx) = builder
            .goal_rounds_from_owner()
            .build()
            .map_err(|e| format!("build agent: {e}"))?;
        let state = agent.session_state();
        let spawner = agent.agent_spawner();
        let health = agent.health();
        self.forward_task_events(&session_id, &state);
        self.forward_goal_events(&session_id, &state);

        let _ = agent
            .telemetry()
            .record(telemetry::TelemetryEvent::session_start(
                &session_id,
                0,
                None,
                telemetry::SessionStartPayload {
                    permission_mode: serde_json::to_value(settings_snapshot.permission_mode)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "default".to_string()),
                    model: settings_snapshot.model.model_name.clone(),
                    max_tokens: settings_snapshot.model.max_tokens,
                    thinking_mode: !matches!(
                        settings_snapshot.model.thinking_mode,
                        base::interface::settings::ThinkingMode::Off
                    ),
                    sandbox_enabled: !settings_snapshot.sandbox.dangerously_disable_sandbox,
                    resume_from: resume.then(|| session_id.clone()),
                    auth_modes: Vec::new(),
                    mcp_server_count,
                    plugin_count: self.plugins.count(),
                    skill_count: binding.modules.skill_count(),
                    output_format: "jsonrpc".to_string(),
                    started_at_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                        / 1_000_000) as i64,
                },
            ));

        // First `Meta` line for a genuinely new primary session — resumed
        // sessions already have one from when they were first created, and
        // must not get a second (that would break the "Meta is always
        // entries[0]" assumption `SessionManager::resume`/`session.resume`
        // rely on). Best-effort: a write failure here doesn't fail session
        // creation — the session still works, it's just not identifiable by
        // scene/project on a later resume (falls back to inference, same as
        // any other pre-v2 file).
        if !resume {
            if let Some(ref store) = self.history_store {
                if let Ok(sid) = base::session::SessionId::parse(&session_id) {
                    let project_root_str = project_root_for_meta
                        .as_ref()
                        .map(|p| p.display().to_string());
                    let meta = history::entry::LogEntry::Meta {
                        cwd: project_root_str.clone().unwrap_or_else(|| {
                            settings_snapshot
                                .paths
                                .global_data_dir
                                .display()
                                .to_string()
                        }),
                        started_at: store.environment().now(),
                        model: settings_snapshot.model.model_name.clone(),
                        permission_mode: serde_json::to_value(settings_snapshot.permission_mode)
                            .ok()
                            .and_then(|v| v.as_str().map(|s| s.to_string()))
                            .unwrap_or_else(|| "default".to_string()),
                        engine_version: env!("CARGO_PKG_VERSION").to_string(),
                        agix_version: env!("CARGO_PKG_VERSION").to_string(),
                        parent_session_id: None,
                        scene: Some(scene_id.clone()),
                        project_root: project_root_str,
                        session_kind: history::entry::SessionKind::Primary,
                        schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
                    };
                    if let Err(e) = store.append(sid, meta).await {
                        warn!(%session_id, error = %e, "failed to write session Meta entry");
                    }
                }
            }
            if spec.options.is_some() {
                self.persist_spec(&session_id, &spec).await;
            }
        }

        // Load prior messages back into memory before the turn loop starts —
        // `resume_or_create` only decided *that* history exists on disk, the
        // actual projection back into `Agent.session.messages` happens here.
        // A failure here (corrupt entry, race with a concurrent delete, ...)
        // degrades to the same "id reused, empty context" behavior as before
        // this was wired up, rather than failing session creation outright.
        let mut stored_name = None;
        let mut has_user_turn = false;
        if resume {
            let previous_state = self
                .sessions
                .lock()
                .await
                .get(&session_id)
                .and_then(|live| live.state.clone());
            self.stop_live_agent(&session_id).await;
            let resume_start = std::time::Instant::now();
            let resume_result = agent.resume_session(&session_id).await;
            if let Some(previous) = &previous_state {
                state.carry_goal_arming_from(previous);
                state.carry_tasks_and_notices_from(previous);
            }
            let outcome = match &resume_result {
                Ok(()) => telemetry::ResumeOutcome::Succeeded,
                // Doesn't fail session creation — degrades to "id reused,
                // empty context" (see the warn! below), which is what
                // `Degraded` (not `Failed`) means here.
                Err(_) => telemetry::ResumeOutcome::Degraded,
            };
            if let Err(e) = &resume_result {
                warn!(%session_id, error = %e, "resume_session failed, continuing with empty context");
            }
            let (projected_message_count, entry_count) = self.history_counts(&session_id).await;
            let entries = self.load_entries(&session_id).await.unwrap_or_default();
            let compact_boundary_count = entries
                .iter()
                .filter(|e| matches!(e.entry, history::entry::LogEntry::Compact { .. }))
                .count();
            let sidechain_entry_count = entries.iter().filter(|e| e.is_sidechain).count();
            has_user_turn = entries.iter().any(|e| {
                !e.is_sidechain && matches!(e.entry, history::entry::LogEntry::User { .. })
            });
            stored_name = self.stats_of(&session_id).await.title;
            let _ = agent
                .telemetry()
                .record(telemetry::TelemetryEvent::resume_action(
                    &session_id,
                    0,
                    None,
                    telemetry::ResumeActionPayload {
                        outcome,
                        source: "jsonl".into(),
                        entry_count,
                        projected_message_count,
                        compact_boundary_count,
                        sidechain_entry_count,
                        warning_kind: resume_result
                            .err()
                            .map(|_| "resume_session_failed".to_string()),
                        latency_ms: resume_start.elapsed().as_millis() as u64,
                    },
                ));
        }

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let exited = CancellationToken::new();
        let exited_on_drop = exited.clone().drop_guard();

        tokio::spawn(async move {
            let _exited_on_drop = exited_on_drop;
            let _claim = claim;
            let mut agent = agent;
            let _ = agent.run(cancel_clone).await;
        });

        let mut sessions = self.sessions.lock().await;
        // Checked again under the lock `shutdown_all` drains the map with: a
        // session inserted after that would outlive the shutdown.
        if let Err(e) = self.refuse_while_shutting_down() {
            cancel.cancel();
            return Err(e);
        }

        // 容量检查：驱逐最久未活跃的 session。只有这次 insert 真的会让
        // session **总数**增加时才需要驱逐——`session_id` 已经在 map 里的话
        // （config-generation 触发的原地重建，见 `run_turn`），这次操作是
        // "替换同一个 key"，不是"新增一个"，`sessions.len()` 不会变，不该
        // 因为这次重建就凭空多驱逐一个无关的、真正最久未用的 session。
        if !sessions.contains_key(&session_id) && sessions.len() >= self.cap {
            if let Some((evicted, live)) = evict_lru(&mut sessions) {
                self.evict(&evicted, &live);
                info!(sid = %evicted, "session evicted (LRU, pool full)");
                self.emit_session_event(
                    "session_evicted",
                    &evicted,
                    serde_json::json!({"reason": "capacity"}),
                );
            }
        }

        let (event_rx, arrived) = relay_events(event_rx);
        let event_rx = Arc::new(AsyncMutex::new(Some(event_rx)));
        let idle = Arc::new(tokio::sync::Notify::new());
        self.forward_events_between_turns(&session_id, &event_rx, &arrived, &idle, &cancel);
        let live = LiveSession {
            input_tx,
            event_rx,
            cancel,
            exited,
            context: None,
            scene: scene_for_session,
            project: project_root_for_meta.clone(),
            named_by_user: stored_name.is_some(),
            name: stored_name,
            created_at: Instant::now(),
            last_active: Instant::now(),
            is_first_turn: !has_user_turn,
            namer,
            binding_stale: binding.stale.clone(),
            current_turn: None,
            turn_initiator: None,
            state: Some(state.clone()),
            spawner: Some(spawner),
            idle,
            scripts: script_ledger,
            health,
            subscribers: HashMap::new(),
            pending_prompts: HashMap::new(),
            process_groups,
        };
        // `insert` returning `Some` means a live entry already existed under
        // this exact sid — only an in-place rebuild (`rebuild_idle`) gets
        // here. Its `CancellationToken` isn't tied to `Drop`, so the old
        // Agent's background loop would otherwise keep running forever as a
        // leaked task if we didn't cancel it here. Who watches the session
        // and what it is still being asked go on with it; a turn waiting for
        // the old entry's slot is woken to find the new one.
        let rebuilt = match sessions.insert(session_id.clone(), live) {
            Some(old) => {
                old.cancel.cancel();
                if let Some(new) = sessions.get_mut(&session_id) {
                    new.subscribers.extend(old.subscribers);
                    new.pending_prompts.extend(old.pending_prompts);
                }
                old.idle.notify_waiters();
                true
            }
            None => false,
        };
        drop(sessions);
        let goal = state.goal_view();
        if resume && !goal.is_null() {
            let frame = StreamFrame::goal_changed(&session_id, goal);
            self.broadcast_to_session(&session_id, &frame).await;
        }
        if !rebuilt {
            self.emit_session_event(
                if resume {
                    "session_resumed"
                } else {
                    "session_created"
                },
                &session_id,
                serde_json::json!({
                    "scene": scene_id,
                    "project_root": project_root_for_meta,
                }),
            );
        }
        info!(%session_id, "session created");
        Ok(session_id)
    }

    /// This session's spec: the one this process already holds, else the one
    /// its log recorded (for a resume), else a new one from `options`.
    async fn spec_for(
        &self,
        session_id: &str,
        resume: bool,
        options: Option<&SessionOptions>,
    ) -> SessionSpec {
        if let Some(spec) = self.lock_specs().get(session_id) {
            return spec.clone();
        }
        let mut spec = if resume {
            self.recorded_spec(session_id).await
        } else {
            SessionSpec::default()
        };
        // A log written before specs were recorded has none; the options of
        // the call that brought the session back are the best there is, and
        // are recorded so the next resume finds them too.
        let adopted = resume && spec.options.is_none() && options.is_some();
        if spec.options.is_none() {
            spec.options = options.cloned();
        }
        // `recorder` is never recorded, so the call bringing the session back
        // is the only place it can come from.
        if let Some(recorder) = options.and_then(|o| o.recorder.clone()) {
            spec.options.get_or_insert_with(Default::default).recorder = Some(recorder);
        }
        if spec.options.as_ref().is_some_and(|o| o.recorder.is_some()) {
            spec.recorder = Some(telemetry::recorder::Recorder::new());
        }
        let spec = self
            .lock_specs()
            .entry(session_id.to_string())
            .or_insert(spec)
            .clone();
        if adopted {
            self.persist_spec(session_id, &spec).await;
        }
        spec
    }

    fn lock_specs(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionSpec>> {
        self.specs
            .lock()
            .expect("specs mutex is never held across a panic")
    }

    async fn recorded_spec(&self, session_id: &str) -> SessionSpec {
        let entries = self.load_entries(session_id).await.unwrap_or_default();
        let recorded = entries
            .iter()
            .rev()
            .find_map(|env| match &env.entry {
                history::entry::LogEntry::Extension { ns, event, payload }
                    if ns == SPEC_NS && event == SPEC_EVENT =>
                {
                    serde_json::from_value::<RecordedSpec>(payload.clone()).ok()
                }
                _ => None,
            })
            .unwrap_or_default();
        SessionSpec {
            options: recorded.options,
            model: recorded.model,
            provider: recorded.provider,
            prompt: recorded.prompt,
            recorder: None,
        }
    }

    async fn persist_spec(&self, session_id: &str, spec: &SessionSpec) {
        let Some(store) = self.history_store.as_ref() else {
            return;
        };
        let Ok(sid) = base::session::SessionId::parse(session_id) else {
            return;
        };
        let recorded = RecordedSpec {
            options: spec.options.clone().map(|mut o| {
                o.recorder = None;
                o
            }),
            model: spec.model.clone(),
            provider: spec.provider.clone(),
            prompt: spec.prompt.clone(),
        };
        let Ok(payload) = serde_json::to_value(&recorded) else {
            return;
        };
        let entry = history::entry::LogEntry::Extension {
            ns: SPEC_NS.to_string(),
            event: SPEC_EVENT.to_string(),
            payload,
        };
        if let Err(e) = store.append(sid, entry).await {
            warn!(%session_id, error = %e, "failed to record the session spec");
        }
    }

    /// Change this session's spec and record the change in its log.
    async fn update_spec(&self, session_id: &str, change: impl FnOnce(&mut SessionSpec)) {
        let spec = {
            let mut specs = self.lock_specs();
            let spec = specs.entry(session_id.to_string()).or_default();
            change(spec);
            spec.clone()
        };
        self.persist_spec(session_id, &spec).await;
    }

    /// Longest name `session.rename` accepts.
    ///
    /// A name is a label in somebody's session list, not a description. The
    /// cap is here rather than at the wire layer because it is the engine's
    /// answer, and a host that embeds the engine gets the same one.
    const MAX_SESSION_NAME: usize = 200;

    /// `session.rename` — what the person looking at this session calls it.
    ///
    /// `None` clears the name, putting the session back under whatever names
    /// it automatically. The name is written where the session's own facts
    /// live, so every host sharing `~/.agix` sees it — a name each host kept
    /// in an overlay of its own is a name that differs depending on which
    /// window you have open.
    ///
    /// Works on a session that is not in memory: naming is a fact about the
    /// session, not an operation on a running engine, so it does not resume
    /// anything to do it.
    pub async fn rename_session(
        &self,
        session_id: &str,
        name: Option<&str>,
    ) -> Result<serde_json::Value, RpcError> {
        let name = match name {
            None => None,
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err((
                        codes::INVALID_PARAMS,
                        "name must not be empty; pass null to clear it".into(),
                    ));
                }
                if trimmed.chars().count() > Self::MAX_SESSION_NAME {
                    return Err((
                        codes::INVALID_PARAMS,
                        format!("name must be at most {} characters", Self::MAX_SESSION_NAME),
                    ));
                }
                // A host renders this in a list. A newline or a control
                // character in it is not a name, it is a rendering bug
                // somebody else has to find.
                if trimmed.chars().any(char::is_control) {
                    return Err((
                        codes::INVALID_PARAMS,
                        "name must not contain control characters".into(),
                    ));
                }
                Some(trimmed.to_string())
            }
        };

        let store = self.require_history_store()?;
        let sid = base::session::SessionId::parse(session_id).map_err(|_| {
            (
                codes::INVALID_PARAMS,
                format!("invalid session_id: {session_id}"),
            )
        })?;
        // Naming something that does not exist would create a sidecar for a
        // session nothing else knows about — a row in every future listing
        // that nobody can open.
        let live = self.sessions.lock().await.contains_key(session_id);
        if !live
            && matches!(
                store.load(sid).await,
                Err(history::error::HistoryError::SessionNotFound(_))
            )
        {
            return Err((
                codes::SESSION_NOT_FOUND,
                format!("session not found: {session_id}"),
            ));
        }

        store
            .set_session_title(sid, name.clone())
            .await
            .map_err(|e| (codes::INTERNAL_ERROR, format!("could not rename: {e}")))?;

        {
            let mut sessions = self.sessions.lock().await;
            if let Some(live) = sessions.get_mut(session_id) {
                live.name = name.clone();
                live.named_by_user = name.is_some();
            }
        }

        Ok(serde_json::json!({ "session_id": session_id, "name": name }))
    }

    const MAX_ENTRY_NS: usize = 128;

    pub async fn append_entry(
        &self,
        session_id: &str,
        ns: &str,
        event: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        if ns.is_empty() || ns.len() > Self::MAX_ENTRY_NS || ns.chars().any(char::is_control) {
            return Err((
                codes::INVALID_PARAMS,
                format!(
                    "ns must be 1–{} characters with no control characters",
                    Self::MAX_ENTRY_NS
                ),
            ));
        }
        if ns.starts_with(ENGINE_NS_PREFIX) {
            return Err((
                codes::INVALID_PARAMS,
                format!("the `{ENGINE_NS_PREFIX}*` namespaces are the engine's own"),
            ));
        }
        if event.is_empty() || event.chars().any(char::is_control) {
            return Err((
                codes::INVALID_PARAMS,
                "event must be a non-empty name".into(),
            ));
        }
        let store = self.require_history_store()?;
        let sid = base::session::SessionId::parse(session_id).map_err(|_| {
            (
                codes::INVALID_PARAMS,
                format!("invalid session_id: {session_id}"),
            )
        })?;
        // Appending to a session that does not exist would create a log for it.
        self.load_entries(session_id).await?;
        let entry = history::entry::LogEntry::Extension {
            ns: ns.to_string(),
            event: event.to_string(),
            payload: payload.clone(),
        };
        store
            .append(sid, entry)
            .await
            .map_err(|e| (codes::INTERNAL_ERROR, format!("could not append: {e}")))?;
        if let Err(e) = store.sync(sid).await {
            warn!(%session_id, error = %e, "an appended entry was not synced");
        }

        let entries = self.load_entries(session_id).await?;
        let lengths = history::transcript::projected_lengths_with(&entries, store.projection());
        let written = entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, env)| match &env.entry {
                history::entry::LogEntry::Extension {
                    ns: n,
                    event: e,
                    payload: p,
                } if n == ns && e == event && *p == payload => Some(entry_view(i, env, &lengths)),
                _ => None,
            })
            .flatten()
            .ok_or_else(|| {
                (
                    codes::INTERNAL_ERROR,
                    "the entry was not found after writing it".to_string(),
                )
            })?;

        let frame = StreamFrame::extension_entry(session_id, written.clone());
        self.broadcast_to_session(session_id, &frame).await;
        Ok(written)
    }

    pub async fn list_entries(
        &self,
        session_id: &str,
        namespaces: Option<&[String]>,
    ) -> Result<serde_json::Value, RpcError> {
        let store = self.require_history_store()?;
        let entries = self.load_entries(session_id).await?;
        let lengths = history::transcript::projected_lengths_with(&entries, store.projection());
        let listed: Vec<serde_json::Value> = entries
            .iter()
            .enumerate()
            .filter(|(_, env)| match &env.entry {
                history::entry::LogEntry::Extension { ns, .. } => {
                    !ns.starts_with(ENGINE_NS_PREFIX)
                        && namespaces.map_or(true, |wanted| wanted.iter().any(|w| w == ns))
                }
                _ => false,
            })
            .filter_map(|(i, env)| entry_view(i, env, &lengths))
            .collect();
        Ok(serde_json::json!({ "entries": listed }))
    }

    /// `session.setModel` — what the next API call this session makes should
    /// ask for.
    ///
    /// The engine has been able to do this all along (`EngineCommand::
    /// UpdateModel`); what it had no way to hear was a client asking. The
    /// transcript is not touched: this is a change to what answers the next
    /// message, not a rewrite of the conversation that led to it.
    ///
    /// Refused while a turn is running, rather than queued behind it. The
    /// engine's main loop is inside `process_turn` for the duration and is
    /// not reading its input channel, so a command accepted here would take
    /// effect at some unannounced later moment — the caller would have been
    /// told the model changed and watched the old one answer.
    ///
    /// With `provider`, the conversation moves to that provider's client. An
    /// engine is built around its client, so the session is rebuilt on the
    /// new spec; it is idle, so nothing is cut off. A model is checked
    /// against the provider it will be sent to and refused when that
    /// provider's `models` list does not have it — sending a name to an
    /// endpoint that does not serve it fails only later, and less legibly.
    pub async fn set_session_model(
        &self,
        session_id: &str,
        model: Option<&str>,
        provider: Option<&str>,
    ) -> Result<serde_json::Value, SetModelError> {
        let (input_tx, project, scene) = {
            let mut sessions = self.sessions.lock().await;
            let Some(live) = sessions.get_mut(session_id) else {
                return Err(SetModelError::Rpc((
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {session_id}"),
                )));
            };
            if let Some(current_turn_id) = live.current_turn.clone() {
                return Err(SetModelError::Busy {
                    current_turn_id: Some(current_turn_id),
                    running_task_ids: Vec::new(),
                });
            }
            live.last_active = Instant::now();
            (
                live.input_tx.clone(),
                live.project.clone(),
                live.scene.id().to_string(),
            )
        };

        let current_provider = self
            .lock_specs()
            .get(session_id)
            .and_then(SessionSpec::provider);
        let binding = self.binding_for(project.as_deref(), &scene).await;
        binding
            .check_model_choice(provider.or(current_provider.as_deref()), model)
            .map_err(|e| SetModelError::Rpc((codes::INVALID_PARAMS, e)))?;

        let switching_provider = provider.is_some_and(|p| current_provider.as_deref() != Some(p));
        let record = self.update_spec(session_id, |spec| {
            if let Some(p) = provider {
                spec.provider = Some(p.to_string());
                // A model chosen for the old provider means nothing to the new
                // one; without a new choice the new provider's default applies.
                if switching_provider {
                    spec.model = None;
                }
            }
            if let Some(m) = model {
                spec.model = Some(m.to_string());
            }
        });

        // A provider is a different client, which an engine is built around
        // rather than handed mid-life: rebuild the session on the new spec.
        // The turn slot is taken before the change is recorded, so a turn
        // that started since the check above refuses both rather than being
        // cut off or finding the change applied by some later rebuild.
        if switching_provider {
            self.with_session_lock(session_id, async {
                let held = self.take_idle_slot(session_id).await?;
                record.await;
                self.rebuild_holding(session_id, held, None).await
            })
            .await
            .map_err(SetModelError::from)?;
        } else {
            record.await;
        }
        if let (false, Some(m)) = (switching_provider, model) {
            if input_tx
                .send(InputMessage::System {
                    kind: EngineCommand::UpdateModel,
                    content: m.to_string(),
                })
                .is_err()
            {
                return Err(SetModelError::Rpc((
                    codes::SESSION_NOT_FOUND,
                    format!("session is no longer running: {session_id}"),
                )));
            }
        }

        let spec = self
            .lock_specs()
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        Ok(serde_json::json!({
            "session_id": session_id,
            "model": spec.model(),
            "provider": spec.provider(),
            "applies": "next_turn",
        }))
    }

    /// `session.setPrompt` — this session's own system-prompt append and
    /// override from its next turn on. A string sets a field, `None` in
    /// `change` leaves it. Refused while a turn runs, like `setModel`; an
    /// idle session is rebuilt on the new prompt at once, its history kept.
    pub async fn set_session_prompt(
        &self,
        session_id: &str,
        change: impl FnOnce(&mut runtime::agent::SessionPrompt),
    ) -> Result<serde_json::Value, SetModelError> {
        {
            let sessions = self.sessions.lock().await;
            if let Some(current_turn_id) = sessions
                .get(session_id)
                .and_then(|l| l.current_turn.clone())
            {
                return Err(SetModelError::Busy {
                    current_turn_id: Some(current_turn_id),
                    running_task_ids: Vec::new(),
                });
            }
        }
        if !self.session_exists(session_id).await {
            return Err(SetModelError::Rpc((
                codes::SESSION_NOT_FOUND,
                format!("session not found: {session_id}"),
            )));
        }
        if !self.lock_specs().contains_key(session_id) {
            let recorded = self.recorded_spec(session_id).await;
            self.lock_specs().insert(session_id.to_string(), recorded);
        }
        let record = self.update_spec(session_id, |spec| {
            let mut prompt = spec.prompt().unwrap_or_default();
            change(&mut prompt);
            spec.prompt = Some(prompt);
        });
        let prompt = self
            .with_session_lock(session_id, async {
                let held = match self.take_idle_slot(session_id).await {
                    Ok(held) => Some(held),
                    Err(Rebuild::NotLive) => None,
                    Err(busy) => return Err(busy),
                };
                record.await;
                let prompt = self
                    .lock_specs()
                    .get(session_id)
                    .and_then(SessionSpec::prompt)
                    .unwrap_or_default();
                match held {
                    Some(held) => {
                        self.rebuild_holding(session_id, held, None).await?;
                    }
                    None => self.release_spec(session_id),
                }
                Ok(prompt)
            })
            .await
            .map_err(SetModelError::from)?;
        Ok(serde_json::json!({
            "session_id": session_id,
            "append": prompt.append,
            "override": prompt.override_text,
            "applies": "next_turn",
        }))
    }

    /// Forget a session's `session.setModel` choice — it is deleted or
    /// closed, and an id is never reused.
    fn forget_spec(&self, session_id: &str) {
        self.lock_specs().remove(session_id);
    }

    /// Stop `live`, just taken out of `sessions` under `sid`, without
    /// waiting for it: whoever brings `sid` back waits instead
    /// ([`Self::wait_until_stopped`]).
    fn stop_removed(&self, sid: &str, live: &LiveSession) {
        live.cancel.cancel();
        let mut stopping = self
            .stopping
            .lock()
            .expect("stopping is never held across a panic");
        stopping.retain(|_, exited| !exited.is_cancelled());
        stopping.insert(sid.to_string(), live.exited.clone());
    }

    /// Wait, within [`AGENT_EXIT_GRACE`], for the engine `sid` last ran on
    /// to stop, if it has not.
    async fn wait_until_stopped(&self, sid: &str) {
        let exited = self
            .stopping
            .lock()
            .expect("stopping is never held across a panic")
            .get(sid)
            .cloned();
        let Some(exited) = exited else { return };
        if tokio::time::timeout(AGENT_EXIT_GRACE, exited.cancelled())
            .await
            .is_err()
        {
            warn!(%sid, "the session's previous engine did not stop within the grace period");
        }
    }

    /// Take `live` out of memory as [`Self::stop_removed`] does, for a
    /// session that may be brought back.
    fn evict(&self, sid: &str, live: &LiveSession) {
        self.stop_removed(sid, live);
        reap_process_groups_later(live);
        self.release_spec(sid);
    }

    /// Drop the spec of `sid`, now out of memory, when its log gives it
    /// back; one the log cannot — a recorder's progress, a session with no
    /// log — is kept, for at most `cap` sessions.
    fn release_spec(&self, sid: &str) {
        let logged = self.history_store.is_some() && base::session::SessionId::parse(sid).is_ok();
        let records = self
            .lock_specs()
            .get(sid)
            .is_some_and(|spec| spec.recorder.is_some());
        if logged && !records {
            self.forget_spec(sid);
            return;
        }
        let oldest = {
            let mut kept = self.lock_kept_specs();
            kept.retain(|k| k != sid);
            kept.push_back(sid.to_string());
            if kept.len() > self.cap {
                kept.pop_front()
            } else {
                None
            }
        };
        if let Some(oldest) = oldest {
            self.forget_spec(&oldest);
        }
    }

    fn lock_kept_specs(&self) -> std::sync::MutexGuard<'_, std::collections::VecDeque<String>> {
        self.kept_specs
            .lock()
            .expect("kept_specs is never held across a panic")
    }

    /// Record a raised ask so late subscribers can be handed it.
    async fn remember_prompt<T: serde::Serialize>(
        &self,
        session_id: &str,
        (prompt_id, task_id): &Ask,
        frame: &T,
    ) {
        let Ok(frame) = serde_json::to_value(frame) else {
            return;
        };
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.get_mut(session_id) {
            live.pending_prompts.insert(
                prompt_id.clone(),
                PendingPrompt {
                    frame,
                    task_id: task_id.clone(),
                },
            );
        }
    }

    /// Forget an ask: answered, or the turn that raised it ended.
    async fn forget_prompt(&self, session_id: &str, prompt_id: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.get_mut(session_id) {
            live.pending_prompts.remove(prompt_id);
        }
    }

    /// Forget the asks of a background task that ended.
    async fn forget_prompts_of_task(&self, session_id: &str, task_id: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.get_mut(session_id) {
            live.pending_prompts
                .retain(|_, prompt| prompt.task_id.as_deref() != Some(task_id));
        }
    }

    /// Register `client` as a watcher of `session_id`.
    ///
    /// Returns the transcript watermark and any unanswered permission asks,
    /// which together are what a newly opened tab needs to catch up:
    /// subscribe first, then read history up to `last_seq`, then apply the
    /// frames that arrived meanwhile. Reading first and subscribing second
    /// loses everything produced in between — the reason this returns a
    /// watermark at all.
    pub async fn subscribe_session(
        &self,
        session_id: &str,
        client: Arc<crate::rpc::Client>,
    ) -> Result<serde_json::Value, RpcError> {
        let pending: Vec<serde_json::Value> = {
            let mut sessions = self.sessions.lock().await;
            let Some(live) = sessions.get_mut(session_id) else {
                return Err((
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {session_id}"),
                ));
            };
            live.subscribers.insert(client.id(), client);
            live.pending_prompts
                .values()
                .map(|prompt| prompt.frame.clone())
                .collect()
        };

        Ok(serde_json::json!({
            "session_id": session_id,
            "last_seq": self.transcript_len(session_id).await,
            "pending_prompts": pending,
        }))
    }

    /// Stop sending this session's frames to `connection`.
    pub async fn unsubscribe_session(
        &self,
        session_id: &str,
        connection: crate::rpc::ConnectionId,
    ) {
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.get_mut(session_id) {
            live.subscribers.remove(&connection);
        }
    }

    /// A connection went away: forget it everywhere.
    ///
    /// That is all a disconnect means: the turn keeps running, because
    /// another connection may be watching and a teardown would cascade into
    /// deleting the session's sidechain transcripts.
    pub async fn drop_connection(&self, connection: crate::rpc::ConnectionId) {
        let mut sessions = self.sessions.lock().await;
        for live in sessions.values_mut() {
            live.subscribers.remove(&connection);
        }
    }

    /// How many messages this session's transcript holds — its watermark, in
    /// the same positions `session.history` pages by and `step_end` /
    /// `turn_complete` report as `seq`.
    async fn transcript_len(&self, session_id: &str) -> usize {
        let Ok(store) = self.require_history_store() else {
            return 0;
        };
        self.load_entries(session_id)
            .await
            .map(|entries| {
                history::transcript::projected_lengths_with(&entries, store.projection())
                    .last()
                    .copied()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// Send one frame to every connection watching `session_id`.
    ///
    /// A subscriber that cannot take the frame — gone, or fallen far enough
    /// behind to fill its queue — is dropped here rather than waited on. One
    /// tab that stopped reading must not hold up the turn or the other tabs
    /// watching it. A subscriber dropped for falling behind is told so
    /// (`subscription_dropped`) and can subscribe again and catch up through
    /// `last_seq`, which is what makes dropping an acceptable answer.
    async fn broadcast_to_session<T: serde::Serialize>(&self, session_id: &str, frame: &T) {
        self.broadcast_to_session_except(session_id, frame, None)
            .await;
    }

    /// [`Self::broadcast_to_session`], skipping one connection — the caller
    /// that is about to be told the same thing in its own response.
    async fn broadcast_to_session_except<T: serde::Serialize>(
        &self,
        session_id: &str,
        frame: &T,
        except: Option<crate::rpc::ConnectionId>,
    ) {
        let json = match serde_json::to_string(frame) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(error = %e, "could not serialize a session frame; dropping it");
                return;
            }
        };
        let mut sessions = self.sessions.lock().await;
        let Some(live) = sessions.get_mut(session_id) else {
            return;
        };
        live.last_active = Instant::now();
        live.subscribers.retain(|id, client| {
            Some(*id) == except || client.try_send_for(session_id, json.clone())
        });
    }

    /// The turn currently running on `session_id`, if any. `session.get`
    /// reports it as `turn_state`; a missing or unknown session reads as idle,
    /// same as one sitting between turns.
    pub async fn current_turn_of(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.current_turn.clone())
    }

    /// Cancel the in-flight turn, keeping the session.
    ///
    /// `EngineCommand::CancelTurn` cancels the turn's own child token, not
    /// the session's — the agent loop keeps running and the next `run_turn`
    /// is served normally. Returns whether there was a turn to interrupt;
    /// there being none is an answer, not an error.
    pub async fn interrupt_session(&self, session_id: &str) -> Result<bool, RpcError> {
        let (input_tx, running) = {
            let sessions = self.sessions.lock().await;
            let Some(live) = sessions.get(session_id) else {
                return Err((
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {session_id}"),
                ));
            };
            (live.input_tx.clone(), live.current_turn.is_some())
        };
        if !running {
            return Ok(false);
        }
        let _ = input_tx.send(InputMessage::System {
            kind: EngineCommand::CancelTurn,
            content: String::new(),
        });
        Ok(true)
    }

    /// The goal round to start once a turn that ended with `stop_reason`
    /// gives its slot back, if it leaves `state`'s goal active and armed.
    /// Asked before the slot is given back, so a rebuild that takes the slot
    /// next carries the round over.
    async fn goal_round_after(
        &self,
        sid: &str,
        state: &base::context::SessionState,
        stop_reason: &str,
    ) -> Option<base::context::goal::GoalRound> {
        if !runtime::turn::CONTINUES_A_GOAL.contains(&stop_reason) {
            return None;
        }
        let round = state.goal_next_round();
        self.record_goal_changes(sid, state).await;
        round
    }

    fn start_goal_round(&self, sid: &str, round: base::context::goal::GoalRound) {
        if let Some(pool) = self.this.get().and_then(|p| p.upgrade()) {
            tokio::spawn(goal_round_task(pool, sid.to_string(), round));
        }
    }

    /// Start the next goal round of an idle session, if its goal wants one.
    async fn continue_goal(&self, sid: &str) {
        let Some(state) = self.live_state(sid).await else {
            return;
        };
        if let Some(round) = self.goal_round_after(sid, &state, "end_turn").await {
            self.start_goal_round(sid, round);
        }
    }

    async fn run_goal_round(&self, sid: String, round: base::context::goal::GoalRound) {
        let connection = crate::rpc::ConnectionId(
            self.internal_connections
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed),
        );
        let client = Arc::new(crate::rpc::Client::new(connection, Arc::new(Discard)));
        info!(%sid, turn_id = %round.turn_id, "goal round started");
        let response = self
            .run_turn(
                Some(sid.clone()),
                round.message,
                Vec::new(),
                round.turn_id.clone(),
                client,
                serde_json::Value::Null,
                None,
                OnBusy::Queue,
                "goal",
            )
            .await;
        self.drop_connection(connection).await;
        if let Some(state) = self.live_state(&sid).await {
            state.goal_abandon_round(&round.turn_id);
        }
        if let Some(error) = response.error {
            warn!(%sid, code = error.code, message = %error.message, "goal round failed");
        }
    }

    async fn live_state(&self, sid: &str) -> Option<Arc<base::context::SessionState>> {
        self.sessions
            .lock()
            .await
            .get(sid)
            .and_then(|live| live.state.clone())
    }

    /// Write the goal changes `sid`'s log does not have yet.
    async fn record_goal_changes(&self, sid: &str, state: &base::context::SessionState) {
        let changes = state.take_unrecorded_goal_changes();
        let (Some(store), Ok(parsed)) = (
            self.history_store.as_ref(),
            base::session::SessionId::parse(sid),
        ) else {
            return;
        };
        for goal in changes {
            let entry = history::entry::LogEntry::Extension {
                ns: base::context::goal::GOAL_NS.to_string(),
                event: base::context::goal::GOAL_EVENT.to_string(),
                payload: serde_json::json!({ "goal": goal }),
            };
            if let Err(e) = store.append(parsed, entry).await {
                warn!(%sid, error = %e, "failed to record a goal change");
            }
        }
    }

    /// `session.goal`: the session's goal, read from its log when the session
    /// is not in memory (and then reported disarmed).
    pub async fn session_goal(&self, sid: &str) -> Result<serde_json::Value, RpcError> {
        if let Some(state) = self.live_state(sid).await {
            return Ok(serde_json::json!({ "goal": state.goal_view() }));
        }
        let entries = self.load_entries(sid).await?;
        let goal: Option<base::context::goal::Goal> = entries
            .iter()
            .rev()
            .find_map(|env| match &env.entry {
                history::entry::LogEntry::Extension { ns, event, payload }
                    if ns == base::context::goal::GOAL_NS
                        && event == base::context::goal::GOAL_EVENT =>
                {
                    Some(payload["goal"].clone())
                }
                _ => None,
            })
            .and_then(|goal| serde_json::from_value(goal).ok());
        Ok(serde_json::json!({
            "goal": base::context::goal::GoalSlot::restored(goal).view()
        }))
    }

    /// `session.setGoal` / `session.updateGoal`: a host acts on the goal with
    /// a person's authority. A goal left active and armed on an idle session
    /// starts its next round at once.
    pub async fn change_session_goal(
        &self,
        sid: &str,
        change: GoalChange,
    ) -> Result<serde_json::Value, RpcError> {
        if self.live_state(sid).await.is_none() {
            self.resume_session(sid, false, None)
                .await
                .map_err(|e| match e {
                    ResumeError::Rpc(e) => e,
                    ResumeError::SidechainTerminal { session_id, .. } => (
                        codes::SIDECHAIN_TERMINAL,
                        format!("session {session_id} is a finished sub-agent session"),
                    ),
                })?;
        }
        let state = self.live_state(sid).await.ok_or_else(|| {
            (
                codes::SESSION_NOT_FOUND,
                format!("session not found: {sid}"),
            )
        })?;
        let human = base::context::goal::TurnOrigin::Human;
        let current = || {
            let view = state.goal_view();
            (
                view["id"].as_str().unwrap_or_default().to_string(),
                view["revision"].as_u64().unwrap_or_default(),
            )
        };
        let invalid = |e: base::context::goal::GoalError| (codes::INVALID_PARAMS, e.to_string());
        let pausing = matches!(change, GoalChange::Pause);
        match change {
            GoalChange::Set {
                objective,
                max_rounds,
            } => {
                state
                    .goal_create(objective, max_rounds, human)
                    .map_err(invalid)?;
            }
            GoalChange::Clear => state.goal_clear(human).map_err(invalid)?,
            GoalChange::Pause => {
                let (id, rev) = current();
                state
                    .goal_update(&id, rev, base::context::goal::GoalAction::Pause, human)
                    .map_err(invalid)?;
            }
            GoalChange::Resume => {
                let (id, rev) = current();
                state
                    .goal_update(&id, rev, base::context::goal::GoalAction::Resume, human)
                    .map_err(invalid)?;
            }
            GoalChange::Edit {
                objective,
                max_rounds,
            } => {
                let (id, rev) = current();
                state
                    .goal_update(
                        &id,
                        rev,
                        base::context::goal::GoalAction::Edit {
                            objective,
                            max_rounds,
                        },
                        human,
                    )
                    .map_err(invalid)?;
            }
        }
        self.record_goal_changes(sid, &state).await;

        let initiator = self
            .sessions
            .lock()
            .await
            .get(sid)
            .map(|live| (live.current_turn.is_some(), live.turn_initiator));
        match initiator {
            Some((true, Some("goal"))) if pausing => {
                self.interrupt_session(sid).await?;
            }
            Some((false, _)) => self.continue_goal(sid).await,
            _ => {}
        }
        Ok(serde_json::json!({ "goal": state.goal_view() }))
    }

    /// Push `session_id`'s goal changes to whoever watches the session.
    fn forward_goal_events(&self, session_id: &str, state: &Arc<base::context::SessionState>) {
        let Some(pool) = self.this.get().cloned() else {
            return;
        };
        let mut events = state.subscribe_goal();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(goal) => {
                        let Some(pool) = pool.upgrade() else { return };
                        let frame = StreamFrame::goal_changed(&session_id, goal);
                        pool.broadcast_to_session(&session_id, &frame).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Give the turn slot `slot` back and mark the session idle again, if
    /// `slot` is still the session's — a session closed or replaced while the
    /// turn ran is not this turn's to touch. Done under the sessions lock, so
    /// nothing sees the slot free while `turn_id` still reads as running.
    /// Whatever the turn's own engine was asking died with the turn;
    /// a background task's asks stay. Returns whether `slot` was the
    /// session's.
    async fn release_turn_slot(
        &self,
        sid: &str,
        slot: &Arc<AsyncMutex<Option<EventReceiver>>>,
        event_rx: EventReceiver,
        turn_id: &str,
    ) -> bool {
        let mut sessions = self.sessions.lock().await;
        *slot.lock().await = Some(event_rx);
        let Some(live) = sessions
            .get_mut(sid)
            .filter(|live| Arc::ptr_eq(&live.event_rx, slot))
        else {
            return false;
        };
        if live.current_turn.as_deref() == Some(turn_id) {
            live.current_turn = None;
            self.emit_session_event(
                "turn_completed",
                sid,
                serde_json::json!({
                    "turn_id": turn_id,
                    "initiator": live.turn_initiator.take(),
                }),
            );
            live.pending_prompts
                .retain(|_, prompt| prompt.task_id.is_some());
        }
        live.idle.notify_waiters();
        true
    }

    /// 执行一个 turn：发送消息 → 流式返回事件 → 返回结果。
    /// session_id 为 None 时自动创建新 session。
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn(
        &self,
        session_id: Option<String>,
        message: String,
        attachments: Vec<runtime::agent::Attachment>,
        turn_id: String,
        client: Arc<crate::rpc::Client>,
        id: serde_json::Value,
        options: Option<SessionOptions>,
        on_busy: OnBusy,
        initiator: &'static str,
    ) -> RpcResponse {
        let sid = match session_id {
            Some(ref sid) => {
                // ① Not in memory: resume or create. ② In memory on a binding
                // that is still current: use it. ③ In memory on a binding a
                // settings change invalidated: rebuild before this message, so
                // it is served on the new configuration.
                //
                // ③ only rebuilds an idle session. A rebuild replaces the
                // Agent, and replacing one mid-turn would cut that turn off,
                // as it would the background tasks running under it; a busy
                // session is served on the old configuration this once and
                // checked again next time — the flag stays set, so nothing
                // is missed.
                let stale = {
                    let sessions = self.sessions.lock().await;
                    sessions.get(sid).map(|live| {
                        live.binding_stale
                            .load(std::sync::atomic::Ordering::Relaxed)
                    })
                };
                let resolved = match stale {
                    None => self.ensure_live(sid, options.as_ref()).await,
                    Some(false) => Ok(sid.clone()),
                    Some(true) => match self.rebuild_idle(sid, options.as_ref()).await {
                        Ok(sid) => Ok(sid),
                        Err(Rebuild::Busy { .. }) => Ok(sid.clone()),
                        Err(Rebuild::NotLive) => self.ensure_live(sid, options.as_ref()).await,
                        Err(Rebuild::Failed(e)) => Err(e),
                    },
                };
                match resolved {
                    Ok(sid) => sid,
                    Err(e) => return RpcResponse::err(id, codes::INTERNAL_ERROR, e),
                }
            }
            None => {
                let sid = Id::new().to_string();
                match self
                    .create(
                        sid.clone(),
                        self.scene.clone(),
                        self.default_binding().await,
                        Some(self.cwd.clone()),
                        options.as_ref(),
                        false,
                    )
                    .await
                {
                    Ok(sid) => sid,
                    Err(e) => {
                        return RpcResponse::err(id, codes::INTERNAL_ERROR, e);
                    }
                }
            }
        };

        // 取走 `event_rx` 就是"一个会话同一时刻只能有一个 turn"的实现，而且
        // 必须**在发消息之前**取：消息一旦 send 进去就归正在跑的那个 Agent
        // 消费了，先发后取的话，调用方看到忙，消息却已经被别的 turn 吃掉。
        let mut steer_tried = false;
        let (input_tx, event_rx_mutex, mut event_rx) = loop {
            let (input_tx, event_rx_mutex, busy_with, idle) = {
                let mut sessions = self.sessions.lock().await;
                let Some(live) = sessions.get_mut(&sid) else {
                    return RpcResponse::err(
                        id,
                        codes::SESSION_NOT_FOUND,
                        format!("session not found: {sid}"),
                    );
                };
                live.last_active = Instant::now();
                (
                    live.input_tx.clone(),
                    live.event_rx.clone(),
                    live.current_turn.clone(),
                    live.idle.clone(),
                )
            };
            let became_idle = idle.notified();
            tokio::pin!(became_idle);
            became_idle.as_mut().enable();
            let taken = event_rx_mutex.lock().await.take();
            if let Some(rx) = taken {
                let still_this_session = self
                    .sessions
                    .lock()
                    .await
                    .get(&sid)
                    .is_some_and(|live| Arc::ptr_eq(&live.event_rx, &event_rx_mutex));
                if still_this_session {
                    break (input_tx, event_rx_mutex, rx);
                }
                *event_rx_mutex.lock().await = Some(rx);
                continue;
            }
            match on_busy {
                OnBusy::Reject => {
                    return RpcResponse::err_with_data(
                        id,
                        codes::SESSION_BUSY,
                        "session is busy",
                        serde_json::json!({
                            "session_id": sid,
                            "current_turn_id": busy_with,
                        }),
                    );
                }
                OnBusy::Steer if !steer_tried && attachments.is_empty() => {
                    steer_tried = true;
                    let (ack, acked) = tokio::sync::oneshot::channel();
                    let _ = input_tx.send(InputMessage::Steer {
                        content: message.clone(),
                        message_id: turn_id.clone(),
                        ack: Some(ack),
                    });
                    if let Ok(Ok(runtime::steering::SteerAck::Accepted)) =
                        tokio::time::timeout(Duration::from_secs(5), acked).await
                    {
                        if let Some(live) = self.sessions.lock().await.get_mut(&sid) {
                            live.subscribers.insert(client.id(), client.clone());
                        }
                        return RpcResponse::ok(
                            id,
                            serde_json::json!({
                                "session_id": sid,
                                "turn_id": turn_id,
                                "delivery": "steer",
                                "into_turn_id": busy_with,
                            }),
                        );
                    }
                    // The running turn is already past its last step: this
                    // message waits for the next turn instead.
                    continue;
                }
                OnBusy::Steer | OnBusy::Queue => {}
            }
            // The timeout covers a session that was replaced or dropped while
            // we waited: its `idle` is never notified again.
            tokio::select! {
                _ = became_idle => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        };

        // A goal round runs only if it is still the one its goal handed out:
        // a pause, clear or change since then, or another turn's round, means
        // nobody wants it any more.
        let started = {
            let mut sessions = self.sessions.lock().await;
            match sessions
                .get_mut(&sid)
                .filter(|live| Arc::ptr_eq(&live.event_rx, &event_rx_mutex))
            {
                None => None,
                Some(live) => {
                    let wanted = initiator != "goal"
                        || live
                            .state
                            .as_ref()
                            .is_some_and(|state| state.goal_claim_round(&turn_id));
                    if wanted {
                        live.current_turn = Some(turn_id.clone());
                        live.turn_initiator = Some(initiator);
                        self.emit_session_event(
                            "turn_started",
                            &sid,
                            serde_json::json!({"turn_id": turn_id, "initiator": initiator}),
                        );
                        // Whoever starts a turn watches it. Not a special role
                        // — it lands in the same subscriber set as any tab that
                        // called `session.subscribe`, and leaves the same way.
                        live.subscribers.insert(client.id(), client.clone());
                    }
                    Some((wanted, live.state.clone()))
                }
            }
        };
        let turn_state = match started {
            Some((true, state)) => state,
            Some((false, _)) => {
                self.release_turn_slot(&sid, &event_rx_mutex, event_rx, &turn_id)
                    .await;
                return RpcResponse::ok(
                    id,
                    serde_json::json!({"session_id": sid, "turn_id": turn_id, "skipped": true}),
                );
            }
            None => {
                *event_rx_mutex.lock().await = Some(event_rx);
                return RpcResponse::err(
                    id,
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {sid}"),
                );
            }
        };
        if let (Some(state), "goal") = (&turn_state, initiator) {
            self.record_goal_changes(&sid, state).await;
        }

        // 发送用户消息
        let _ = input_tx.send(InputMessage::User {
            content: message.clone(),
            attachments,
            turn_id: turn_id.clone(),
        });

        let api_calls;
        let ended_with;
        loop {
            let event = event_rx.recv().await;
            if let Some((frame, ask)) = event
                .as_ref()
                .and_then(|event| background_frame(&sid, Some(&turn_id), event))
            {
                // Registered before it is sent: a tab that subscribes between
                // the two must still be handed the question.
                if let Some(ask) = &ask {
                    self.remember_prompt(&sid, ask, &frame).await;
                }
                self.broadcast_to_session(&sid, &frame).await;
                continue;
            }
            match event {
                Some(AgentEvent::SystemInit { .. }) => continue,
                Some(AgentEvent::TurnStart { .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"turn_start"}),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::StepStart { step, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"step_start","step":step}),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::StepEnd {
                    step,
                    stop_reason,
                    usage,
                    tool_calls,
                    seq,
                    ..
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"step_end","step":step,"stop_reason":stop_reason,
                            "tool_calls":tool_calls,"seq":seq,
                            "usage":serde_json::to_value(usage).unwrap_or_else(|_| serde_json::json!({}))
                        }),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::TextDelta { text, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"text_delta","text":text}),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::ThinkingDelta { text, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"thinking_delta","text":text}),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::ToolUseStart { id: tid, name, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"tool_use_start","id":tid,"name":name}),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::ToolArgsDelta {
                    id: tid,
                    partial_json,
                    ..
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"tool_args_delta","id":tid,"partial_json":partial_json}),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::ToolUse {
                    id: tid,
                    name,
                    input,
                    ..
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"tool_use","id":tid,"name":name,"input":input}),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::ToolResult {
                    id: tid,
                    name,
                    content,
                    is_error,
                    ..
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"tool_result","id":tid,"name":name,"content":content,"is_error":is_error}),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                // Context was compacted mid-turn. The only moment this is
                // worth saying: the user sees the consequence (the model
                // stopped remembering something) and has no way back to the
                // cause once the turn is over.
                //
                // Destructured exhaustively on purpose — a field added to
                // `CompactAction` must fail this build rather than quietly
                // stop reaching the client, which is how `Usage`'s two cache
                // fields missed `turn_complete` for a release.
                Some(AgentEvent::CompactAction {
                    strategy,
                    messages_before,
                    messages_after,
                    turn_id: _,
                    dropped_rounds,
                    dropped_messages,
                    estimated_tokens_saved,
                    tokens_before,
                    tokens_after,
                }) => {
                    let mut event = serde_json::json!({
                        "kind":"compact","strategy":strategy,
                        "messages_before":messages_before,"messages_after":messages_after,
                        "tokens_before":tokens_before,"tokens_after":tokens_after
                    });
                    let map = event.as_object_mut().expect("json! built an object");
                    for (key, value) in [
                        ("dropped_rounds", dropped_rounds),
                        ("dropped_messages", dropped_messages),
                        ("estimated_tokens_saved", estimated_tokens_saved),
                    ] {
                        if let Some(v) = value {
                            map.insert(key.into(), serde_json::json!(v));
                        }
                    }
                    let f = StreamFrame::event(&sid, &turn_id, event);
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::UserMessageDelivered {
                    message_id,
                    content,
                    ..
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"user_message_delivered","message_id":message_id,
                            "content":content
                        }),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::UserMessageRefused {
                    message_id,
                    content,
                    message,
                    ..
                }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"user_message_refused","message_id":message_id,
                            "content":content,"message":message
                        }),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::UserMessagesReturned { messages, .. }) => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"user_messages_returned","messages":messages
                        }),
                    );
                    self.broadcast_to_session(&sid, &f).await;
                }
                Some(AgentEvent::TurnComplete {
                    stop_reason,
                    api_calls: ac,
                    tool_calls,
                    usage,
                    message,
                    last_call_usage,
                    context,
                    structured_output,
                    seq,
                    ..
                }) => {
                    api_calls = ac;
                    ended_with = stop_reason.clone();
                    if context.is_some() {
                        if let Some(live) = self.sessions.lock().await.get_mut(&sid) {
                            live.context = context;
                        }
                    }
                    let mut event = serde_json::json!({
                        "kind":"turn_complete","stop_reason":stop_reason,"api_calls":api_calls,
                        "tool_calls":tool_calls,"seq":seq,
                        "last_call_usage":serde_json::to_value(last_call_usage).unwrap_or_else(|_| serde_json::json!({})),
                        // Serialized whole rather than field by field: a
                        // hand-written projection is invisible to the
                        // compiler when `Usage` grows.
                        "usage":serde_json::to_value(usage).unwrap_or_else(|_| serde_json::json!({}))
                    });
                    if let Some(message) = message {
                        event["message"] = serde_json::json!(message);
                    }
                    if let Some(context) = context {
                        event["context"] = serde_json::json!(context);
                    }
                    if let Some(output) = structured_output {
                        event["structured_output"] = output;
                    }
                    let f = StreamFrame::event(&sid, &turn_id, event);
                    self.broadcast_to_session(&sid, &f).await;
                    break;
                }
                Some(AgentEvent::Error { code, message, .. }) => {
                    // The caller learns this from its error response; every
                    // other watcher would otherwise see a turn that never ends.
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({"kind":"error","code":code,"message":message}),
                    );
                    self.broadcast_to_session_except(&sid, &f, Some(client.id()))
                        .await;
                    self.release_turn_slot(&sid, &event_rx_mutex, event_rx, &turn_id)
                        .await;
                    return RpcResponse::err(id, codes::ENGINE_ERROR, format!("{code}: {message}"));
                }
                // The engine is gone. Dropping the entry is what lets the
                // next call on this id resume it from its transcript instead
                // of sending into a closed channel.
                None => {
                    let f = StreamFrame::event(
                        &sid,
                        &turn_id,
                        serde_json::json!({
                            "kind":"error","code":"engine_stopped",
                            "message":"engine stopped before the turn completed"
                        }),
                    );
                    self.broadcast_to_session_except(&sid, &f, Some(client.id()))
                        .await;
                    let dead = {
                        let mut sessions = self.sessions.lock().await;
                        let ours = sessions
                            .get(&sid)
                            .is_some_and(|live| Arc::ptr_eq(&live.event_rx, &event_rx_mutex));
                        if ours {
                            sessions.remove(&sid)
                        } else {
                            None
                        }
                    };
                    if let Some(dead) = dead {
                        self.evict(&sid, &dead);
                        dead.idle.notify_waiters();
                        self.emit_session_event(
                            "session_closed",
                            &sid,
                            serde_json::json!({"reason": "engine_stopped"}),
                        );
                    }
                    warn!(%sid, "engine stopped before the turn completed");
                    return RpcResponse::err(
                        id,
                        codes::ENGINE_ERROR,
                        "engine stopped before the turn completed",
                    );
                }
                _ => continue,
            }
        }

        // The turn is over. A client that vanished mid-stream is not part of
        // this: the turn ran to completion and landed in the transcript, and
        // whoever comes back reads it there. Cancelling on a broken writer
        // would cascade into deleting sidechain transcripts another tab is
        // still watching.

        let next_round = match &turn_state {
            Some(state) => self.goal_round_after(&sid, state, &ended_with).await,
            None => None,
        };
        let ours = self
            .release_turn_slot(&sid, &event_rx_mutex, event_rx, &turn_id)
            .await;
        if let (true, Some(round)) = (ours, next_round) {
            self.start_goal_round(&sid, round);
        }

        // Whether a session names itself is its *own* scene's call, not this
        // pool's default scene's: a `harnix` session created on a `--scene
        // code` daemon is still a harnix session, and the two scenes disagree
        // about this exactly as they disagree about the system prompt.
        //
        // The decision is taken under the lock and the naming call made
        // outside it — it is a model call, and the sessions map is the
        // pool's hottest mutex.
        let mut session_name = None;
        let naming = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get_mut(&sid) {
                // A slash command is not what the conversation is about.
                Some(live) if live.is_first_turn && !message.starts_with('/') => {
                    live.is_first_turn = false;
                    (live.scene.auto_name_session() && !live.named_by_user)
                        .then(|| live.scene.session_name_prompt(&message))
                        .flatten()
                        .map(|prompt| (prompt, live.namer.clone()))
                }
                Some(live) => {
                    session_name = live.name.clone();
                    None
                }
                None => None,
            }
        };
        if let Some((prompt, namer)) = naming {
            match namer.name(&prompt).await {
                Ok(Some(name)) => {
                    session_name = self.adopt_generated_name(&sid, name).await;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(%sid, error=%e, "session name generation failed");
                }
            }
        }

        RpcResponse::ok(
            id,
            serde_json::json!({
                "session_id": sid,
                "turn_id": turn_id,
                "delivery": "turn",
                "name": session_name,
                "api_calls": api_calls,
            }),
        )
    }

    /// A name somebody typed while the naming call was out wins over the
    /// guess; otherwise the guess is stored like any other name, so a resume
    /// or a daemon restart finds it.
    async fn adopt_generated_name(&self, sid: &str, name: String) -> Option<String> {
        {
            let mut sessions = self.sessions.lock().await;
            let live = sessions.get_mut(sid)?;
            if live.named_by_user {
                return live.name.clone();
            }
            live.name = Some(name.clone());
        }
        if let (Some(store), Ok(parsed)) = (
            self.history_store.as_ref(),
            base::session::SessionId::parse(sid),
        ) {
            if let Err(e) = store.set_session_title(parsed, Some(name.clone())).await {
                warn!(%sid, error=%e, "could not store the generated session name");
            }
        }
        Some(name)
    }

    /// A session's recorded `session_kind`/`parent_session_id`/`resumable`,
    /// read off its `Meta` line (and terminal marker, for `resumable`).
    /// Sessions with no `Meta` at all (files written before the daemon wrote
    /// one — see `create()`) default to `(Primary, None, true)`. `resumable` is `false` only for a sidechain that has a
    /// `LogEntry::SessionEnd` marker —
    /// primary sessions and still-running/externally-cut-off sidechains are
    /// always `true`. One `load_entries` scan covers both, since listing
    /// every session would otherwise pay for the same disk read twice.
    /// What the log says this session is — scene, project, kind, lineage.
    ///
    /// The empty set when there is no store, the id does not parse, or the
    /// log has no `Meta` line: none of those is a reason to fail a listing,
    /// and "we do not know" is the honest entry for a session that cannot say.
    async fn facts_of(&self, session_id: &str) -> history::store::SessionFacts {
        let Some(ref store) = self.history_store else {
            return history::store::SessionFacts::default();
        };
        let Ok(sid) = base::session::SessionId::parse(session_id) else {
            return history::store::SessionFacts::default();
        };
        store.session_facts(sid).await.unwrap_or_default()
    }

    /// What the store has recorded about this session — the name and the
    /// counters. The empty record when there is no store or the id does not
    /// parse, for the same reason `facts_of` answers that way: neither is a
    /// reason to fail a listing.
    async fn stats_of(&self, session_id: &str) -> history::store::SessionStats {
        let Some(ref store) = self.history_store else {
            return history::store::SessionStats::default();
        };
        let Ok(sid) = base::session::SessionId::parse(session_id) else {
            return history::store::SessionStats::default();
        };
        store.session_stats(sid).await.unwrap_or_default()
    }

    async fn session_facts_and_resumable(
        &self,
        session_id: &str,
    ) -> (history::store::SessionFacts, bool) {
        let facts = self.facts_of(session_id).await;
        // Only a sidechain can be non-resumable, and only the whole log can
        // say whether it ended — so only a sidechain pays for reading it.
        if !matches!(facts.session_kind, history::entry::SessionKind::Sidechain) {
            return (facts, true);
        }
        let ended = self
            .load_entries(session_id)
            .await
            .map(|entries| {
                entries
                    .iter()
                    .any(|env| matches!(env.entry, history::entry::LogEntry::SessionEnd { .. }))
            })
            .unwrap_or(false);
        (facts, !ended)
    }

    /// `scene.describe {scene, project_root, include_secrets}` — what this
    /// scene is and what a session in it would run with.
    ///
    /// Reports what the daemon can actually answer: the scene's capability
    /// bits, its tool surface, and the settings a session created here would
    /// resolve to. It deliberately does **not** report which tier each field
    /// came from — settings are merged tier by tier and the provenance is not
    /// retained, so a `sources` map would have to be reconstructed by
    /// re-reading and re-merging every tier. That is a feature, not a field.
    pub async fn describe_scene(
        &self,
        scene_id: &str,
        project_root: Option<&Path>,
        include_secrets: bool,
    ) -> Result<serde_json::Value, RpcError> {
        let Some(scene) = self.scene_registry().resolve(scene_id) else {
            return Err((codes::SCENE_NOT_FOUND, format!("unknown scene: {scene_id}")));
        };
        let settings = self.settings_for_project(project_root, scene_id).await;
        let mut settings_json =
            serde_json::to_value(settings.as_ref()).unwrap_or_else(|_| serde_json::json!({}));
        if !include_secrets {
            redact_settings_secrets(&mut settings_json);
        }

        Ok(serde_json::json!({
            "scene": scene.id(),
            "name": scene.name(),
            "description": scene.description(),
            "project_root": project_root.map(|p| p.display().to_string()),
            "active": self.active_scenes.read().await.contains_key(scene_id),
            "capabilities": {
                "requires_project": scene.requires_project(),
                "supports_team": scene.supports_team(),
            },
            "tools": {
                // `Scene::tools()` is a whitelist, and empty means "no
                // whitelist — every registered tool". Reporting that as `[]`
                // would read to a client as "no tools at all", the opposite;
                // `null` says there is no restriction.
                "allowed": if scene.tools().is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(scene.tools())
                },
                "disallowed": scene.disallowed_tools(),
                "deferred": scene.deferred_tools(),
            },
            "settings": settings_json,
        }))
    }

    /// `config.get {scene, project_root, tier}` — one settings tier as it
    /// sits on disk, or `effective` for the merged result.
    ///
    /// A tier that has no file reads as `null`, which is the honest answer:
    /// "nothing at this layer" and "an empty object here" are different
    /// states and a client showing the config should be able to tell them
    /// apart.
    pub async fn config_tier(
        &self,
        scene_id: &str,
        project_root: Option<&Path>,
        tier: &str,
        include_secrets: bool,
    ) -> Result<serde_json::Value, RpcError> {
        if self.scene_registry().resolve(scene_id).is_none() {
            return Err((codes::SCENE_NOT_FOUND, format!("unknown scene: {scene_id}")));
        }

        let paths = self.paths.as_ref();
        let path = match tier {
            "global" => Some(paths.global_root().join("settings.json")),
            "scene" => Some(
                paths
                    .global_root()
                    .join("scenes")
                    .join(scene_id)
                    .join("settings.json"),
            ),
            "project" => project_root.map(|p| p.join(".agix").join("settings.json")),
            "effective" => None,
            other => {
                return Err((
                    codes::INVALID_PARAMS,
                    format!("tier must be global|scene|project|effective, got `{other}`"),
                ))
            }
        };

        let mut value = match tier {
            "effective" => {
                let settings = self.settings_for_project(project_root, scene_id).await;
                serde_json::to_value(settings.as_ref()).unwrap_or_else(|_| serde_json::json!({}))
            }
            _ => match path {
                Some(p) => tokio::fs::read_to_string(&p)
                    .await
                    .ok()
                    .and_then(|raw| serde_json::from_str(&raw).ok())
                    .unwrap_or(serde_json::Value::Null),
                // `project` with no project: there is no such tier to read.
                None => serde_json::Value::Null,
            },
        };
        if !include_secrets {
            redact_settings_secrets(&mut value);
        }

        Ok(serde_json::json!({
            "scene": scene_id,
            "project_root": project_root.map(|p| p.display().to_string()),
            "tier": tier,
            "settings": value,
        }))
    }

    /// One session's summary plus what only the live pool knows.
    ///
    /// Built from `list_all` rather than a second, parallel assembly: a
    /// `session.get` that disagreed with the `session.list` entry for the
    /// same session would be worse than not having it.
    pub async fn session_detail(&self, session_id: &str) -> Result<serde_json::Value, RpcError> {
        let listed = self
            .list_all(true, None)
            .await
            .into_iter()
            .find(|s| s.session_id == session_id);
        let info = match listed {
            Some(info) => info,
            None => self.cold_session_info(session_id).await?,
        };

        // The entry's own `scene`, not a second lookup: `session.get`
        // disagreeing with the `session.list` entry for the same session is
        // the failure this whole assembly is arranged to prevent.
        let scene_active = match &info.scene {
            Some(id) => self.active_scenes.read().await.contains_key(id),
            None => false,
        };
        let current_turn = self.current_turn_of(session_id).await;

        let mut detail = serde_json::to_value(&info).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = detail.as_object_mut() {
            obj.insert("scene_active".into(), serde_json::json!(scene_active));
            // `turn_state` is the pair a client actually acts on: whether to
            // enable its send button, and which turn to interrupt if not.
            obj.insert(
                "turn_state".into(),
                serde_json::json!(if current_turn.is_some() {
                    "running"
                } else {
                    "idle"
                }),
            );
            obj.insert("current_turn_id".into(), serde_json::json!(current_turn));
            let context = self
                .sessions
                .lock()
                .await
                .get(session_id)
                .and_then(|live| live.context);
            if let Some(context) = context {
                obj.insert("context".into(), serde_json::json!(context));
            }
            let health = self
                .sessions
                .lock()
                .await
                .get(session_id)
                .map(|live| live.health.clone())
                .filter(|health| !health.is_empty());
            if let Some(health) = health {
                obj.insert("health".into(), health.report().to_json());
            }
            if let Some(scripts) = self.script_report(session_id).await {
                obj.insert("scripts".into(), scripts);
            }
        }
        Ok(detail)
    }

    /// What this session's scripts have done, for `session.get`.
    ///
    /// `None` when the session bound none — a key that is absent says "no
    /// scripts here", which is a different answer from one that is present and
    /// empty, and the difference is the first thing somebody debugging a
    /// script needs.
    ///
    /// The counts answer "is it running at all"; `recent` answers "and why
    /// not". A failed call carries its reason, because "the script threw",
    /// "it ran out of time" and "it answered in a shape this point cannot use"
    /// send a reader to three different places.
    async fn script_report(&self, session_id: &str) -> Option<serde_json::Value> {
        use base::interface::script::ScriptOutcome;

        let ledger = {
            let sessions = self.sessions.lock().await;
            sessions.get(session_id)?.scripts.clone()?
        };
        let records = ledger.records();

        let (mut applied, mut no_change, mut failed, mut refused) = (0, 0, 0, 0);
        for r in &records {
            match r.outcome {
                ScriptOutcome::Applied => applied += 1,
                ScriptOutcome::NoChange { .. } => no_change += 1,
                ScriptOutcome::Failed { .. } => failed += 1,
                ScriptOutcome::Refused { .. } => refused += 1,
            }
        }

        /// Enough to see a pattern, few enough to read.
        const RECENT: usize = 20;
        let recent: Vec<serde_json::Value> = records
            .iter()
            .rev()
            .take(RECENT)
            .map(|r| {
                let (outcome, detail) = match &r.outcome {
                    ScriptOutcome::Applied => ("applied", None),
                    ScriptOutcome::NoChange { detail } => ("no_change", detail.clone()),
                    ScriptOutcome::Failed { error } => ("failed", Some(error.to_string())),
                    ScriptOutcome::Refused { detail } => ("refused", Some(detail.clone())),
                };
                serde_json::json!({
                    "point": r.point,
                    "script": r.script,
                    "entry": r.entry,
                    "turn": r.turn,
                    "outcome": outcome,
                    "detail": detail,
                })
            })
            .collect();

        Some(serde_json::json!({
            "calls": records.len(),
            "applied": applied,
            "no_change": no_change,
            "failed": failed,
            "refused": refused,
            // Non-zero means `recent` is a tail of a longer story.
            "dropped": ledger.dropped(),
            "recent": recent,
        }))
    }

    /// 列出所有 session（活跃的 + 历史的），合并去重。
    ///
    /// The historical half is a `HistoryStore::find_sessions` answer rather
    /// than a directory walk of the pool's own, so a store that keeps an
    /// index over its sessions answers this RPC from the index. Only the ids
    /// and their order are taken from it: the fields a summary could fill in
    /// (`preview`, `message_count`, timestamps) are what `session.list` has
    /// always reported empty for an inactive session, and filling them here
    /// would change the RPC.
    ///
    /// `include_children == false` (the default) hides sidechain sessions —
    /// they're sub-agent/team-member transcripts, not something a user
    /// browsing their conversation list should see. `parent_session_id`,
    /// when given, ignores `include_children` entirely and instead returns
    /// every session recording that one as its parent (delegating to
    /// `HistoryStore::child_sessions`, the same path `agent.list`'s
    /// post-restart fallback uses).
    ///
    /// Each child reports its own recorded `session_kind`. Both a sidechain
    /// and a fork name a parent, and answering `sidechain` for all of them
    /// makes a fork look like something the parent's `session.close` is about
    /// to delete.
    pub async fn list_all(
        &self,
        include_children: bool,
        parent_session_id: Option<&str>,
    ) -> Vec<SessionInfo> {
        if let Some(parent) = parent_session_id {
            let Some(ref store) = self.history_store else {
                return Vec::new();
            };
            let Ok(children) = store.child_sessions(parent).await else {
                return Vec::new();
            };
            let active_ids: std::collections::HashSet<String> = {
                let sessions = self.sessions.lock().await;
                sessions.keys().cloned().collect()
            };
            let mut out = Vec::with_capacity(children.len());
            for sid in children {
                let sid_str = sid.to_string();
                let active = active_ids.contains(&sid_str);
                let (facts, resumable) = self.session_facts_and_resumable(&sid_str).await;
                let stats = store.session_stats(sid).await.unwrap_or_default();
                out.push(SessionInfo {
                    session_id: sid_str,
                    name: stats.title,
                    preview: None,
                    message_count: 0,
                    created_at: String::new(),
                    last_active: String::new(),
                    status: if active {
                        SessionStatus::Active
                    } else {
                        SessionStatus::Inactive
                    },
                    session_kind: facts.session_kind,
                    scene: facts.scene,
                    project_root: facts.project_root,
                    compact_count: stats.compact_count,
                    parent_session_id: Some(parent.to_string()),
                    resumable,
                });
            }
            return out;
        }

        // Snapshot active sessions and release the lock before the disk
        // reads below (`session_kind_and_parent` per session, plus
        // `store.find_sessions()`) — holding `self.sessions` across that
        // much I/O would block every other pool operation for the duration.
        let active_snapshot: Vec<(String, Option<String>, Instant, Instant)> = {
            let sessions = self.sessions.lock().await;
            sessions
                .iter()
                .map(|(sid, live)| {
                    (
                        sid.clone(),
                        live.name.clone(),
                        live.created_at,
                        live.last_active,
                    )
                })
                .collect()
        };

        let mut out: Vec<SessionInfo> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for (sid, name, created_at, last_active) in active_snapshot {
            let (facts, resumable) = self.session_facts_and_resumable(&sid).await;
            // From the same place a cold session's do: the store counts as it
            // writes, so a live session and one on disk are answered alike
            // instead of from two accountings that can disagree.
            let stats = self.stats_of(&sid).await;
            seen.insert(sid.clone());
            if !include_children
                && matches!(facts.session_kind, history::entry::SessionKind::Sidechain)
            {
                continue;
            }
            out.push(SessionInfo {
                session_id: sid,
                name,
                preview: None,
                message_count: 0,
                created_at: format_instant(created_at),
                last_active: format_instant(last_active),
                status: SessionStatus::Active,
                session_kind: facts.session_kind,
                scene: facts.scene,
                project_root: facts.project_root,
                compact_count: stats.compact_count,
                parent_session_id: facts.parent_session_id,
                resumable,
            });
        }

        // 从 HistoryStore 查历史（inactive sessions）
        if let Some(ref store) = self.history_store {
            let mut summaries = Vec::new();
            for home in self.served_projects().await {
                let query = history::query::SessionQuery::recent(usize::MAX)
                    .ids_only()
                    .within(history::query::SessionScope::Project(home));
                if let Ok(found) = store.find_sessions(&query).await {
                    summaries.extend(found);
                }
            }
            {
                for summary in summaries {
                    let sid_str = summary.session_id.to_string();
                    if !seen.insert(sid_str.clone()) {
                        continue;
                    }
                    // The name somebody gave it. A cold session has no live
                    // entry to carry one, which is why `session.rename`
                    // writes it beside the log rather than into the pool.
                    let name = summary.title.clone();
                    let (facts, resumable) = self.session_facts_and_resumable(&sid_str).await;
                    if !include_children
                        && matches!(facts.session_kind, history::entry::SessionKind::Sidechain)
                    {
                        continue;
                    }
                    out.push(SessionInfo {
                        session_id: sid_str,
                        name,
                        preview: None,
                        message_count: 0,
                        created_at: String::new(),
                        last_active: String::new(),
                        status: SessionStatus::Inactive,
                        session_kind: facts.session_kind,
                        scene: facts.scene,
                        project_root: facts.project_root,
                        compact_count: summary.compact_count,
                        parent_session_id: facts.parent_session_id,
                        resumable,
                    });
                }
            }
        }

        out
    }

    /// A session that is not in memory and not under any project `session.list`
    /// enumerates — one whose project no session in this process has touched
    /// yet. `session.resume` finds those by id, so `session.get` must too.
    async fn cold_session_info(&self, session_id: &str) -> Result<SessionInfo, RpcError> {
        let not_found = || {
            (
                codes::SESSION_NOT_FOUND,
                format!("session not found: {session_id}"),
            )
        };
        let store = self.history_store.as_ref().ok_or_else(not_found)?;
        let sid = base::session::SessionId::parse(session_id).map_err(|_| not_found())?;
        match store.load(sid).await {
            Ok(entries) if !entries.is_empty() => {}
            _ => return Err(not_found()),
        }
        let (facts, resumable) = self.session_facts_and_resumable(session_id).await;
        let stats = self.stats_of(session_id).await;
        Ok(SessionInfo {
            session_id: session_id.to_string(),
            name: stats.title,
            preview: None,
            message_count: 0,
            created_at: String::new(),
            last_active: String::new(),
            status: SessionStatus::Inactive,
            session_kind: facts.session_kind,
            scene: facts.scene,
            project_root: facts.project_root,
            compact_count: stats.compact_count,
            parent_session_id: facts.parent_session_id,
            resumable,
        })
    }

    /// Every project this daemon has served a session for: its own, plus each
    /// one a session was created or resumed under in this process. `None` is
    /// the no-project tier.
    ///
    /// This is what `session.list` enumerates cold sessions over. Not every
    /// project on disk (`SessionScope::AllProjects`), which would report
    /// sessions belonging to daemons and working directories this one has
    /// nothing to do with; and not just the default project, which is what it
    /// listed back when every session's transcript landed there regardless of
    /// which project the session was actually bound to.
    ///
    /// Held apart from the binding cache, which a settings change empties.
    /// Not persisted: after a restart a project no session has touched since
    /// is not listed until one does — the alternative is a daemon that
    /// remembers projects it no longer serves. `session.get` still finds such
    /// a session by id.
    ///
    /// Compared resolved, as the store resolves a project before reading it:
    /// one folder named two ways (`/var/…` and `/private/var/…` on macOS) is
    /// one project.
    async fn served_projects(&self) -> Vec<Option<PathBuf>> {
        let noted: Vec<Option<PathBuf>> = self.lock_served().clone();
        let mut out = Vec::with_capacity(noted.len() + 1);
        for project in std::iter::once(Some(self.cwd.clone())).chain(noted) {
            let project = match project {
                Some(p) => Some(history::path::canonicalize_cwd(&p).await.unwrap_or(p)),
                None => None,
            };
            if !out.contains(&project) {
                out.push(project);
            }
        }
        out
    }

    fn lock_served(&self) -> std::sync::MutexGuard<'_, Vec<Option<PathBuf>>> {
        self.served
            .lock()
            .expect("served mutex is never held across a panic")
    }

    fn note_served(&self, project: Option<&Path>) {
        let project = project.map(Path::to_path_buf);
        let mut served = self.lock_served();
        if !served.contains(&project) {
            served.push(project);
        }
    }

    /// 获取活跃 session 数量。
    pub async fn active_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Run `fut` while holding this pool's per-`session_id` lock — see the
    /// `session_locks` field doc comment. Closes the race between a
    /// sidechain's cascade delete (`delete_sidechain_list`, `delete_session`)
    /// and a concurrent `session.resume(that_same_id, create_if_missing:
    /// true)`: whichever side gets here first runs to completion before
    /// the other starts, so a resume can't observe the id mid-cascade (files
    /// half gone) and a completed cascade can't delete out from under a
    /// resume that just finished recreating the id. It does *not* stop the
    /// id from being reused afterward — recreating a just-deleted sidechain
    /// as a fresh orphan session is still possible; the two operations
    /// serialize instead of interleaving.
    async fn with_session_lock<T>(
        &self,
        session_id: &str,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        let lock = {
            let mut locks = self.session_locks.lock().await;
            locks
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let result = {
            let _guard = lock.lock().await;
            fut.await
        };
        drop(lock);
        let mut locks = self.session_locks.lock().await;
        if locks
            .get(session_id)
            .is_some_and(|entry| Arc::strong_count(entry) == 1)
        {
            locks.remove(session_id);
        }
        result
    }

    /// `parent_session_id`'s **sidechains**, or `[]` if there's no history
    /// store or the scan itself fails — logged either way so
    /// `session.close`/`.delete` reporting "0 sidechains" is distinguishable
    /// in the daemon log from "there genuinely were none" instead of silently
    /// looking the same as the empty case.
    ///
    /// Children are filtered by `session_kind`, not taken whole.
    /// `parent_session_id` records two different relationships: a sidechain,
    /// which the parent spawned and which cannot outlive it, and a fork,
    /// which merely records where it was copied from. Cascading over the raw
    /// child list deletes the forks too, which is the opposite of what a fork
    /// is for.
    async fn fetch_child_sidechains(
        &self,
        parent_session_id: &str,
    ) -> Vec<base::session::SessionId> {
        let Some(ref store) = self.history_store else {
            return Vec::new();
        };
        // `HistoryStore::child_sessions`' default impl is O(n) in the total
        // number of sessions this project has ever recorded (full
        // load-and-parse per file — see its doc comment); there's no cheap
        // index to check "does this session have sidechains" first, so
        // every `session.close`/`.delete` pays this once regardless of
        // whether the session actually has any. `delete_session` (this
        // function's other caller) shares this one scan rather than running
        // it twice.
        let children = match store.child_sessions(parent_session_id).await {
            Ok(children) => children,
            Err(e) => {
                warn!(
                    parent_session_id,
                    error = %e,
                    "failed to scan for child sessions; reporting no sidechains"
                );
                return Vec::new();
            }
        };
        let mut sidechains = Vec::with_capacity(children.len());
        for child in children {
            let facts = self.facts_of(&child.to_string()).await;
            if matches!(facts.session_kind, history::entry::SessionKind::Sidechain) {
                sidechains.push(child);
            }
        }
        sidechains
    }

    /// Cancel (if live) and delete each of `children`, one `with_session_lock`
    /// critical section per child. Best-effort throughout — see
    /// `shutdown_session`'s doc comment.
    async fn delete_sidechain_list(
        &self,
        parent_session_id: &str,
        children: Vec<base::session::SessionId>,
    ) -> usize {
        let Some(ref store) = self.history_store else {
            return 0;
        };
        let mut deleted = 0;
        for child in children {
            let child_str = child.to_string();
            let ok = self
                .with_session_lock(&child_str, async {
                    {
                        let mut sessions = self.sessions.lock().await;
                        if let Some(live) = sessions.remove(&child_str) {
                            self.stop_removed(&child_str, &live);
                            reap_process_groups_later(&live);
                        }
                    }
                    match store.delete(child).await {
                        Ok(()) => {
                            self.schedule.remove_session(&child_str);
                            self.emit_session_event(
                                "session_deleted",
                                &child_str,
                                serde_json::json!({"parent_session_id": parent_session_id}),
                            );
                            true
                        }
                        Err(e) => {
                            warn!(
                                session_id = %child_str,
                                parent_session_id,
                                error = %e,
                                "failed to delete sidechain during cascade; leaving for startup GC"
                            );
                            false
                        }
                    }
                })
                .await;
            if ok {
                deleted += 1;
            }
        }
        deleted
    }

    /// Remove `session_id` from the live map (if present) and cascade-delete
    /// `children` — the shared body of `shutdown_session` (which scans for
    /// its own children) and `delete_session` (which reuses a scan it
    /// already did for its response body).
    async fn shutdown_session_with_children(
        &self,
        session_id: &str,
        children: Vec<base::session::SessionId>,
    ) -> usize {
        let process_groups = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(session_id).and_then(|live| {
                self.stop_removed(session_id, &live);
                info!(%session_id, "session removed");
                self.emit_session_event(
                    "session_closed",
                    session_id,
                    serde_json::json!({"reason": "closed"}),
                );
                live.process_groups
            })
        };
        if let Some(ledger) = process_groups {
            ledger.reap(PROCESS_GROUP_GRACE).await;
        }
        // The in-memory copy only: a session resumed later reads its spec back
        // from its log.
        self.forget_spec(session_id);
        self.delete_sidechain_list(session_id, children).await
    }

    /// `session.close` — destroys the runtime entity; the parent's
    /// transcript stays on disk and remains resumable. This
    /// **cascades**: every sidechain of this session (sub-agent / team
    /// member) is deleted too — a closed parent has no path left to reach
    /// them (`session.list {parent_session_id}` only makes sense while the
    /// parent might still resume that context; once it's gone, so are they).
    /// Returns how many sidechains were
    /// actually deleted.
    ///
    /// A sidechain still mid-run (background / team member) is cancelled
    /// before its transcript is deleted, but this doesn't wait for that
    /// cancellation to finish landing on disk (N2's suggested grace window
    /// is intentionally not implemented here — it would make an interactive
    /// `session.close` call noticeably slower for what's normally a rare
    /// race). A delete that loses that race, or fails for any other reason,
    /// is logged and left for the startup GC to clean up later (N1) — it
    /// never fails `session.close` itself.
    pub async fn shutdown_session(&self, session_id: &str) -> usize {
        let children = self.fetch_child_sidechains(session_id).await;
        self.shutdown_session_with_children(session_id, children)
            .await
    }

    /// `session.delete` — irreversibly deletes the parent's own transcript
    /// too (not just the runtime entity `session.close` destroys), cascading
    /// sidechains the same way. `dry_run: true` reports what *would* be
    /// deleted (sidechain count) without deleting anything, so a host can
    /// show a confirmation dialog first.
    pub async fn delete_session(
        &self,
        session_id: &str,
        dry_run: bool,
    ) -> Result<serde_json::Value, RpcError> {
        let store = self.require_history_store()?;
        let sid = base::session::SessionId::parse(session_id).map_err(|_| {
            (
                codes::INVALID_PARAMS,
                format!("invalid session_id: {session_id}"),
            )
        })?;

        // One scan shared by both the (possibly dry-run) report below and
        // the actual cascade delete — `shutdown_session_with_children`
        // takes this same `Vec` instead of re-scanning, so the two can't
        // disagree.
        let children = self.fetch_child_sidechains(session_id).await;
        let sidechain_ids: Vec<String> = children.iter().map(|id| id.to_string()).collect();

        if dry_run {
            return Ok(serde_json::json!({
                "session_id": session_id,
                "deleted": false,
                "sidechains_deleted": sidechain_ids.len(),
                "sidechain_ids": sidechain_ids,
            }));
        }

        // Cascade and the primary delete share one `with_session_lock`
        // critical section — see that method's doc comment for the resume
        // race this closes.
        let (sidechains_deleted, delete_result) = self
            .with_session_lock(session_id, async {
                let deleted = self
                    .shutdown_session_with_children(session_id, children)
                    .await;
                let result = store.delete(sid).await;
                (deleted, result)
            })
            .await;
        delete_result.map_err(|e| (codes::INTERNAL_ERROR, e.to_string()))?;
        self.schedule.remove_session(session_id);
        self.emit_session_event("session_deleted", session_id, serde_json::json!({}));

        Ok(serde_json::json!({
            "session_id": session_id,
            "deleted": true,
            "sidechains_deleted": sidechains_deleted,
            "sidechain_ids": sidechain_ids,
        }))
    }

    /// `session.respondToPrompt` — deliver the host's answer to a pending
    /// `prompt` frame (see `run_turn`'s event loop). Unlike `shutdown_session`,
    /// an unknown `session_id` here is a real caller error (answering a prompt
    /// for a session that isn't running can't silently no-op the way closing
    /// an already-closed session can). An answer of the other `prompt_type`
    /// than the open prompt it names is refused rather than dropped, so a
    /// client that confused the two learns of it.
    pub async fn respond_to_prompt(
        &self,
        session_id: &str,
        prompt_id: String,
        answer: PromptAnswer,
    ) -> Result<(), RpcError> {
        let input_tx = {
            let sessions = self.sessions.lock().await;
            let Some(live) = sessions.get(session_id) else {
                return Err((
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {session_id}"),
                ));
            };
            let asked = live.pending_prompts.get(&prompt_id).and_then(|prompt| {
                prompt
                    .frame
                    .pointer("/params/event/prompt_type")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            });
            if let Some(asked) = asked.filter(|asked| asked != answer.prompt_type()) {
                return Err((
                    codes::INVALID_PARAMS,
                    format!(
                        "prompt `{prompt_id}` is a {asked} prompt, not a {}",
                        answer.prompt_type()
                    ),
                ));
            }
            live.input_tx.clone()
        };

        // Straight to the engine, whose input demultiplexer wakes the blocked
        // tool call. A `prompt_id` nobody is waiting on (already answered,
        // timed out, stale) is a silent no-op — the documented behaviour.
        let _ = input_tx.send(match answer {
            PromptAnswer::Permission(decision) => InputMessage::PermissionResponse {
                prompt_id: prompt_id.clone(),
                decision,
            },
            PromptAnswer::Question(answer) => InputMessage::QuestionResponse {
                prompt_id: prompt_id.clone(),
                answer,
            },
        });
        self.forget_prompt(session_id, &prompt_id).await;
        Ok(())
    }

    /// The `HistoryStore` these three methods read and write, or a ready-made
    /// RPC error explaining that this daemon has no session persistence at
    /// all (`JsonlHistoryStore::with_root` failed at startup — see
    /// `daemon/src/main.rs`; `daemon.doctor`'s
    /// `session_persistence.history_store_wired` reports the same fact).
    fn require_history_store(&self) -> Result<&Arc<dyn history::store::HistoryStore>, RpcError> {
        self.history_store.as_ref().ok_or_else(|| {
            (
                codes::INTERNAL_ERROR,
                "session history persistence is not enabled on this daemon \
                 (see daemon.doctor's session_persistence.history_store_wired)"
                    .to_string(),
            )
        })
    }

    /// Parse + load one session's raw log, mapping both failure kinds onto
    /// the error codes `docs/daemon_rpc_protocol.md` documents.
    async fn load_entries(
        &self,
        session_id: &str,
    ) -> Result<Vec<history::entry::EnvelopedEntry>, RpcError> {
        let store = self.require_history_store()?;
        let sid = base::session::SessionId::parse(session_id).map_err(|e| {
            (
                codes::INVALID_PARAMS,
                format!("invalid session_id `{session_id}`: {e}"),
            )
        })?;
        store.load(sid).await.map_err(|e| match e {
            history::error::HistoryError::SessionNotFound(_) => (
                codes::SESSION_NOT_FOUND,
                format!("session not found: {session_id}"),
            ),
            other => (
                codes::INTERNAL_ERROR,
                format!("failed to read session history: {other}"),
            ),
        })
    }

    /// `session.history` — a bounded page of a session's projected
    /// transcript.
    ///
    /// Reads the on-disk log, not the live `Agent`'s in-memory message list:
    /// the store is the only view that exists for an *inactive* session
    /// (the whole point — an IDE that restarted has nothing in memory), and
    /// for an active one it's the same content up to the last completed step
    /// (the transcript is written at every step boundary). A step currently in
    /// flight therefore isn't visible here until it completes; `active` in
    /// the response tells a caller when that caveat applies.
    ///
    /// Paging is `offset`/`limit` over **projected messages** (what
    /// `history::transcript::project_messages` produces — compaction
    /// boundaries already applied, metadata/sidechain entries already
    /// dropped), not raw log entries, so the indices line up with what the
    /// client renders. `total` always reports the full count regardless of
    /// the page, so a client wanting the tail of a long session can ask for
    /// `limit: 0` first (cheap, no message payload) and then request
    /// `offset: total - n`.
    pub async fn session_history(
        &self,
        session_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<serde_json::Value, RpcError> {
        let entries = self.load_entries(session_id).await?;
        let messages = history::transcript::project_messages_sourced(
            &entries,
            self.require_history_store()?.projection(),
        );
        let total = messages.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        let page = messages[start..end]
            .iter()
            .enumerate()
            .map(|(i, m)| history_item(start + i, m))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                (
                    codes::INTERNAL_ERROR,
                    format!("failed to serialize transcript: {e}"),
                )
            })?;
        let active = self.sessions.lock().await.contains_key(session_id);

        Ok(serde_json::json!({
            "session_id": session_id,
            "messages": page,
            "total": total,
            "offset": start,
            "limit": limit,
            "has_more": end < total,
            "entry_count": entries.len(),
            "active": active,
        }))
    }

    /// `session.export` — the session, its sidechains and what they refer to,
    /// as one archive at `request.path` (see `crate::export`).
    ///
    /// Read-only: nothing is resumed or created, and a session in another
    /// scene exports like any other. The logs are cut at one moment, under
    /// this session's lock so a cascade delete cannot run through the middle
    /// of the cut; a turn running meanwhile carries on, and the archive holds
    /// what it had written by its last completed step. The recording, when
    /// asked for, is measured in the same moment.
    pub async fn export_session(
        &self,
        request: crate::export::ExportRequest,
    ) -> Result<serde_json::Value, RpcError> {
        let store = self.require_history_store()?.clone();
        let root = base::session::SessionId::parse(&request.session_id).map_err(|e| {
            (
                codes::INVALID_PARAMS,
                format!("invalid session_id `{}`: {e}", request.session_id),
            )
        })?;
        let settings = self.settings.read().await.clone();
        let roots = history::path::HistoryRoots::under(&settings.paths.global_data_dir);
        crate::export::check_target(&request.path, &[roots.projects, roots.sessions.clone()])?;

        let include = request.include;
        let (lineage, cut_at, live, recordings) = self
            .with_session_lock(&request.session_id, async {
                let mut lineage = store.snapshot_lineage(root).await.map_err(|e| match e {
                    history::error::HistoryError::SessionNotFound(_) => (
                        codes::SESSION_NOT_FOUND,
                        format!("session not found: {}", request.session_id),
                    ),
                    other => (
                        codes::INTERNAL_ERROR,
                        format!("failed to read session history: {other}"),
                    ),
                })?;
                let cut_at = store.environment().now();
                if !include.sidechains {
                    lineage.sessions.truncate(1);
                }
                let live: HashMap<
                    String,
                    (Option<String>, Option<Arc<base::context::SessionState>>),
                > = {
                    let sessions = self.sessions.lock().await;
                    lineage
                        .sessions
                        .iter()
                        .filter_map(|(sid, _)| {
                            let sid = sid.to_string();
                            let live = sessions.get(&sid)?;
                            Some((sid, (live.current_turn.clone(), live.state.clone())))
                        })
                        .collect()
                };
                let recordings = if include.recording {
                    self.measure_recordings(&settings, &request.session_id, &lineage)
                        .await
                } else {
                    Vec::new()
                };
                Ok::<_, RpcError>((lineage, cut_at, live, recordings))
            })
            .await?;

        let mut warnings = Vec::new();
        if include.recording && recordings.is_empty() {
            warnings.push("no recording was found for these sessions".to_string());
        }
        let mut sessions = Vec::with_capacity(lineage.sessions.len());
        for (sid, entries) in lineage.sessions {
            let id = sid.to_string();
            let (history, extension_entries) =
                export_views(&entries, store.projection()).map_err(|e| {
                    (
                        codes::INTERNAL_ERROR,
                        format!("failed to serialize transcript: {e}"),
                    )
                })?;
            let stats = store.session_stats(sid).await.unwrap_or_default();
            let metadata: Option<history::project::SessionMetadata> = tokio::fs::read_to_string(
                history::path::session_metadata_file(&roots.sessions, &sid),
            )
            .await
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
            let memory = tokio::fs::read_to_string(history::path::session_memory_file(
                &roots.sessions,
                &sid,
            ))
            .await
            .ok();
            let (current_turn, state) = live.get(&id).cloned().unwrap_or((None, None));
            let runtime = state.map(|state| {
                serde_json::json!({
                    "durable": false,
                    "permission_mode": state.permission_mode(),
                    "todos": state.todos(),
                    "goal": state.goal_view(),
                })
            });
            sessions.push(crate::export::SessionPart {
                session_id: sid,
                entries,
                history,
                extension_entries,
                title: stats.title,
                tags: metadata
                    .as_ref()
                    .map(|m| m.tags.clone())
                    .unwrap_or_default(),
                total_input_tokens: metadata.as_ref().and_then(|m| m.total_input_tokens),
                total_output_tokens: metadata.as_ref().and_then(|m| m.total_output_tokens),
                compact_count: stats.compact_count,
                memory,
                active: live.contains_key(&id),
                current_turn,
                runtime,
            });
        }
        let snapshot = crate::export::ExportSnapshot {
            root,
            cut_at,
            sessions,
            forks: lineage.forks,
            recordings,
            warnings,
        };
        tokio::task::spawn_blocking(move || crate::export::write(&request, snapshot))
            .await
            .map_err(|e| (codes::INTERNAL_ERROR, format!("export did not finish: {e}")))?
            .map_err(RpcError::from)
    }

    /// The recordings these sessions write to, with how long each one's
    /// `calls.jsonl` is right now.
    ///
    /// A recording is named by its `recorder.name`, or else by the session
    /// whose calls it holds, under the root the session's recorder named or
    /// the daemon's own. `recorder` is never written to the log, so for a
    /// session this process did not build only the daemon's root and the
    /// session ids are known.
    async fn measure_recordings(
        &self,
        settings: &Settings,
        root_session: &str,
        lineage: &history::store::LineageSnapshot,
    ) -> Vec<crate::export::RecordingPart> {
        let recorder = self
            .lock_specs()
            .get(root_session)
            .and_then(|spec| spec.options.as_ref()?.recorder.clone());
        let mut dirs = vec![recordings_root(
            settings,
            recorder.as_ref().and_then(|r| r.dir.as_deref()),
        )];
        let default_root = recordings_root(settings, None);
        if !dirs.contains(&default_root) {
            dirs.push(default_root);
        }
        let ids: Vec<String> = lineage
            .sessions
            .iter()
            .map(|(sid, _)| sid.to_string())
            .collect();
        let mut names: Vec<(String, Vec<String>)> = Vec::new();
        if let Some(name) = recorder.as_ref().and_then(|r| r.name.clone()) {
            names.push((name, ids.clone()));
        }
        names.extend(ids.iter().map(|id| (id.clone(), vec![id.clone()])));

        let mut found: Vec<crate::export::RecordingPart> = Vec::new();
        for (name, sessions) in names {
            let one_component = matches!(
                Path::new(&name).components().collect::<Vec<_>>().as_slice(),
                [std::path::Component::Normal(_)]
            );
            if !one_component || found.iter().any(|r| r.name == name) {
                continue;
            }
            for root in &dirs {
                let dir = root.join(&name);
                if let Ok(meta) = tokio::fs::metadata(dir.join("calls.jsonl")).await {
                    found.push(crate::export::RecordingPart {
                        name: name.clone(),
                        dir,
                        calls_len: meta.len(),
                        sessions: sessions.clone(),
                    });
                    break;
                }
            }
        }
        found
    }

    /// `session.fork` — branch a session at a message boundary into a new
    /// session id whose history is a copy of everything up to that point.
    ///
    /// `at_message` counts **projected messages** (same coordinates
    /// `session.history` returns); `None` forks the whole transcript. The cut
    /// is resolved to the shortest *entry* prefix whose projection already
    /// holds that many messages (see `history::transcript::projected_lengths`
    /// — a `Compact` entry can shrink the running count, so this is a scan,
    /// not arithmetic on entry kinds).
    ///
    /// The fork is written to disk and is **not** made live in the pool:
    /// nothing about the parent is touched, no engine is spawned, and the
    /// new id behaves exactly like any other id with history — the next
    /// `session.resume`/`session.run_turn` carrying it resumes the copy.
    /// That's what makes the two independent: writes to the fork append to a
    /// different jsonl file.
    ///
    /// Two deliberate lossy points, both invisible to the projected view:
    /// sidechain entries (sub-agent side conversations) are dropped, because
    /// `HistoryStore::append` takes a `LogEntry` and can't carry the
    /// envelope's `is_sidechain`/`parent_id` flags through; and the source's
    /// `Meta` line is replaced with a fresh one recording
    /// `parent_session_id`, which is what makes the lineage queryable via
    /// `HistoryStore::child_sessions`.
    ///
    /// The fork records `session_kind: Primary`, and everything that walks
    /// the child list has to honour that. `parent_session_id` answers "where
    /// did this come from", which a sidechain and a fork answer the same way
    /// while meaning opposite things about who may delete whom.
    pub async fn fork_session(
        &self,
        session_id: &str,
        at_message: Option<usize>,
    ) -> Result<serde_json::Value, RpcError> {
        let entries = self.load_entries(session_id).await?;
        let store = self.require_history_store()?;

        let lengths = history::transcript::projected_lengths_with(&entries, store.projection());
        let total_messages = lengths.last().copied().unwrap_or(0);
        let target = at_message.unwrap_or(total_messages).min(total_messages);
        // Shortest prefix already holding `target` messages. `target == 0`
        // keeps nothing (a fork of the bare session start), which is a legal
        // request: "give me this conversation's setup with none of its turns".
        // A fork of the whole conversation keeps what follows its last message
        // too: a host's entries written after the final turn.
        let cut = if target >= total_messages {
            entries.len()
        } else if target == 0 {
            0
        } else {
            lengths
                .iter()
                .position(|&len| len >= target)
                .map(|i| i + 1)
                .unwrap_or(entries.len())
        };

        let new_sid = base::session::SessionId::new();
        // Fork inherits the source session's own recorded scene and project
        // when known (pre-v2 sources have neither) rather than the pool's
        // defaults, since a source session may live under a different
        // project or scene.
        let (source_scene, source_project_root, source_schema_version) = entries
            .iter()
            .find_map(|env| match &env.entry {
                history::entry::LogEntry::Meta {
                    scene,
                    project_root,
                    schema_version,
                    ..
                } => Some((scene.clone(), project_root.clone(), *schema_version)),
                _ => None,
            })
            .unwrap_or((None, None, 1));
        let project_root_for_fork = if source_schema_version >= 2 {
            source_project_root.map(PathBuf::from)
        } else {
            Some(self.cwd.clone())
        };
        // A fork inherits the source session's scene, so its settings must be
        // resolved against that scene's tier — not the daemon's startup one.
        let fork_scene = match source_scene.clone() {
            Some(s) => s,
            None => self.settings.read().await.paths.scope.clone(),
        };
        let settings = match &project_root_for_fork {
            Some(p) => self.settings_for_project(Some(p), &fork_scene).await,
            None => self.settings_for_project(None, &fork_scene).await,
        };
        let meta = history::entry::LogEntry::Meta {
            cwd: project_root_for_fork
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| settings.paths.global_data_dir.display().to_string()),
            started_at: store.environment().now(),
            model: settings.model.model_name.clone(),
            permission_mode: serde_json::to_value(settings.permission_mode)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "default".to_string()),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            agix_version: env!("CARGO_PKG_VERSION").to_string(),
            parent_session_id: Some(session_id.to_string()),
            scene: Some(source_scene.unwrap_or_else(|| self.scene.id().to_string())),
            project_root: project_root_for_fork.map(|p| p.display().to_string()),
            session_kind: history::entry::SessionKind::Primary,
            schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
        };

        // Best-effort cleanup on a partial copy: a half-written fork that a
        // client then resumes would silently present a truncated
        // conversation as if it were complete. Deleting is safer than
        // leaving it, and the id was minted here so nothing else can be
        // referencing it yet.
        let mut copied = 0usize;
        if let Err(e) = store.append(new_sid, meta).await {
            let _ = store.delete(new_sid).await;
            return Err((
                codes::INTERNAL_ERROR,
                format!("failed to write forked session metadata: {e}"),
            ));
        }
        for env in entries.iter().take(cut) {
            if env.is_sidechain || matches!(env.entry, history::entry::LogEntry::Meta { .. }) {
                continue;
            }
            let tags = history::entry::EntryTags {
                turn_id: env.turn_id.clone(),
                origin: env.origin,
            };
            if let Err(e) = store.append_tagged(new_sid, env.entry.clone(), tags).await {
                let _ = store.delete(new_sid).await;
                return Err((
                    codes::INTERNAL_ERROR,
                    format!("failed to copy session history into the fork: {e}"),
                ));
            }
            copied += 1;
        }

        let message_count =
            history::transcript::project_messages_with(&entries[..cut], store.projection()).len();
        info!(
            source = %session_id,
            fork = %new_sid,
            at_message = message_count,
            "session forked"
        );
        Ok(serde_json::json!({
            "session_id": new_sid.to_string(),
            "parent_session_id": session_id,
            "forked_at_message": message_count,
            "message_count": message_count,
            "entry_count": copied,
            "source_message_count": total_messages,
            "active": false,
        }))
    }

    /// This daemon instance's *default* scene id — see the `scene` field.
    pub fn scene_id(&self) -> &str {
        self.scene.id()
    }

    /// Resolve `scene_id` against the currently-active scenes, or `None`
    /// (the pool default) against `self.scene` directly. `session.create`'s
    /// resolution rule: omitted `scene` param always means the default,
    /// regardless of what else is active.
    async fn resolve_scene(&self, scene_id: Option<&str>) -> Option<Arc<dyn Scene>> {
        match scene_id {
            None => Some(self.scene.clone()),
            Some(id) => self.active_scenes.read().await.get(id).cloned(),
        }
    }

    /// `scene.list` — every scene this binary registers, each flagged with
    /// whether it's currently active. `sessions` counts *in-memory* live
    /// sessions recorded under that scene (an approximation — matches what
    /// `session.list`'s own active/inactive split already accepts; a
    /// disk-wide count would need scanning every session's `Meta`, not
    /// worth it just for this summary view).
    pub async fn list_scenes(&self) -> Vec<serde_json::Value> {
        let active = self.active_scenes.read().await;
        let mut counts: HashMap<String, usize> = HashMap::new();
        {
            let sessions = self.sessions.lock().await;
            for sid in sessions.keys() {
                if let Some(scene) = self.facts_of(sid).await.scene {
                    *counts.entry(scene).or_insert(0) += 1;
                }
            }
        }
        self.scene_registry()
            .list_all()
            .into_iter()
            .map(|info| {
                serde_json::json!({
                    "scene": info.id,
                    "name": info.name,
                    "description": info.description,
                    "active": active.contains_key(&info.id),
                    "sessions": counts.get(&info.id).copied().unwrap_or(0),
                    "requires_project": info.requires_project,
                    "supports_team": info.supports_team,
                })
            })
            .collect()
    }

    /// `scene.activate {scene}` — idempotent. `Err` when `scene_id` isn't
    /// one this binary registers at all (`scene.list` would never show it).
    /// Make a scene the host defines known to this pool, so `scene.list`
    /// shows it and `scene.activate` (or [`Assembly::extra_scenes`]) can turn
    /// it on. Registering does not activate. A scene with the id of a built-in
    /// one replaces it.
    ///
    /// [`Assembly::extra_scenes`]: crate::assemble::Assembly::extra_scenes
    pub fn register_scene(&self, scene: Arc<dyn Scene>) {
        {
            let mut host = self.host_scenes.write().unwrap_or_else(|e| e.into_inner());
            host.retain(|s| s.id() != scene.id());
            host.push(scene);
        }
        let (registry, _) = self.scene_registry_now(&self.plugins.scenes());
        *self
            .scene_registry
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);
    }

    fn scene_registry_now(
        &self,
        plugin_scenes: &[Arc<dyn Scene>],
    ) -> (scene::SceneRegistry, Vec<String>) {
        registry_of(
            &self.host_scenes.read().unwrap_or_else(|e| e.into_inner()),
            plugin_scenes,
            &self.paths.global_root().join("scenes"),
            &self
                .registered_scenes
                .read()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Rebuild the scene table from everything that contributes to it, and
    /// point active scenes that are still there at their current definition —
    /// a declarative scene edited on disk is served as edited to the sessions
    /// created from now on. Sessions already running keep the definition they
    /// were built with.
    async fn refresh_scene_table(&self) -> SceneTableChange {
        let before: std::collections::BTreeSet<String> =
            self.scene_registry().ids().into_iter().collect();
        let (registry, refused) = self.scene_registry_now(&self.plugins.scenes());
        let after: std::collections::BTreeSet<String> = registry.ids().into_iter().collect();
        {
            let mut active = self.active_scenes.write().await;
            for (id, scene) in active.iter_mut() {
                if let Some(current) = registry.resolve(id) {
                    *scene = current;
                }
            }
        }
        *self
            .scene_registry
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);
        SceneTableChange {
            added: after.difference(&before).cloned().collect(),
            removed: before.difference(&after).cloned().collect(),
            refused,
        }
    }

    /// `scene.reload`: read the declarative scenes on disk again.
    pub async fn reload_scenes(&self) -> serde_json::Value {
        let change = self.refresh_scene_table().await;
        let mut scenes = self.scene_registry().ids();
        scenes.sort();
        serde_json::json!({
            "scenes": scenes,
            "added": change.added,
            "removed": change.removed,
            "errors": change.refused,
        })
    }

    /// `scene.register`: add a declarative scene from its `scene.json` and
    /// `prompt.md` text, in memory only. It is not activated.
    pub async fn register_scene_from_sources(
        &self,
        scene_json: &str,
        prompt_md: &str,
    ) -> Result<String, RpcError> {
        let scene = scene::DeclarativeScene::from_sources(
            std::path::Path::new("scene.register"),
            scene_json,
            prompt_md,
        )
        .map_err(|e| (codes::INVALID_PARAMS, e.to_string()))?;
        let id = scene.id().to_string();
        if self.scene_registry().resolve(&id).is_some() {
            return Err((
                codes::INVALID_PARAMS,
                format!("scene `{id}` is already registered"),
            ));
        }
        self.registered_scenes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(scene));
        self.refresh_scene_table().await;
        Ok(id)
    }

    /// `scene.unregister`: withdraw a scene `scene.register` added. Refused
    /// while sessions in memory run in it, as `scene.deactivate` is.
    pub async fn unregister_scene(&self, scene_id: &str) -> Result<(), RpcError> {
        let known = self
            .registered_scenes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|s| s.id() == scene_id);
        if !known {
            return Err((
                codes::SCENE_NOT_FOUND,
                format!("scene `{scene_id}` was not added by scene.register"),
            ));
        }
        self.deactivate_scene(scene_id).await?;
        self.registered_scenes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|s| s.id() != scene_id);
        self.refresh_scene_table().await;
        Ok(())
    }

    pub async fn activate_scene(&self, scene_id: &str) -> Result<(), RpcError> {
        if self.active_scenes.read().await.contains_key(scene_id) {
            return Ok(());
        }
        let Some(resolved) = self.scene_registry().resolve(scene_id) else {
            return Err((codes::SCENE_NOT_FOUND, format!("unknown scene: {scene_id}")));
        };
        self.active_scenes
            .write()
            .await
            .insert(scene_id.to_string(), resolved);
        Ok(())
    }

    /// `scene.deactivate {scene}` — refuses while any *in-memory* session is
    /// recorded under this scene (`SCENE_HAS_ACTIVE_SESSIONS`, with the
    /// blocking session ids so the caller can close them and retry).
    /// Deactivating only removes it from `active_scenes` — no session
    /// history is touched, and a deactivated scene's sessions can still be
    /// listed (`session.list`) and resumed once it's reactivated.
    pub async fn deactivate_scene(&self, scene_id: &str) -> Result<(), RpcError> {
        let blocking: Vec<String> = {
            let sessions = self.sessions.lock().await;
            let mut out = Vec::new();
            for sid in sessions.keys() {
                if let Some(scene) = self.facts_of(sid).await.scene {
                    if scene == scene_id {
                        out.push(sid.clone());
                    }
                }
            }
            out
        };
        if !blocking.is_empty() {
            return Err((
                codes::SCENE_HAS_ACTIVE_SESSIONS,
                format!(
                    "scene {scene_id} has active sessions: {}",
                    blocking.join(", ")
                ),
            ));
        }
        self.active_scenes.write().await.remove(scene_id);
        Ok(())
    }

    /// Like `session_kind_and_parent`, but returns the recorded scene
    /// instead of the parent — `list_scenes`/`deactivate_scene`'s shared
    /// lookup. `None` for the scene half covers "no Meta at all" the same
    /// way `session_kind_and_parent` does for kind/parent.
    /// Merged settings for `project_root` (global → scene → project tiers),
    /// built once and cached — see the `projects` field doc comment.
    /// `None` is the global no-project tier (docs/ARCHITECTURE.md §3.2):
    /// `local_data_dir` becomes
    /// `global_data_dir` itself rather than `<project>/.agix`, so project-
    /// layer settings are skipped entirely, same as the design's "no
    /// specific project" case.
    ///
    /// Does not validate that `project_root` exists — `Settings::load`
    /// already degrades a missing/unreadable `settings.json` to defaults
    /// (soft-degrade, same as every other tier). Callers that need to
    /// reject a nonexistent project outright (`session.create`) check that
    /// separately, since resume callers deliberately do **not**: a project
    /// directory that moved after the session was created should still
    /// resume (`docs/daemon_rpc_protocol.md` §3.4), not fail.
    async fn settings_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<Settings> {
        self.binding_for(project_root, scene_id)
            .await
            .settings
            .clone()
    }

    /// The [`ProjectBinding`] for this pair, built on first use.
    ///
    /// The pool's own `(cwd, startup scene)` pair takes its settings from the
    /// hot-swappable `self.settings` that `config.reload` writes, rather than
    /// loading them again — but it is cached here like every other pair, and
    /// `apply_reloaded_settings` drops it so the next session picks the new
    /// ones up.
    async fn binding_for(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<ProjectBinding> {
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        let is_default_pair =
            key.0.as_deref() == Some(self.cwd.as_path()) && scene_id == startup_scene;
        let cached = self.bindings.lock().await.get(&key).cloned();
        if let Some(existing) = cached {
            let Some(tier) = existing.appeared().cloned() else {
                return existing;
            };
            info!(tier = ?tier, "a settings file was created; reloading");
            if is_default_pair {
                Box::pin(self.reload_settings_from(None, scene_id, Some(&tier))).await;
            } else {
                self.invalidate_bindings(Some(&tier)).await;
            }
            if let Some(rebuilt) = self.bindings.lock().await.get(&key) {
                return rebuilt.clone();
            }
        }

        let settings = if is_default_pair {
            self.settings.read().await.clone()
        } else {
            let default_model = self.settings.read().await.model.model_name.clone();
            let local_dir = match &key.0 {
                Some(root) => root.join(".agix"),
                None => self.paths.global_root(),
            };
            let (global_root, scene_root, scene_id) = (
                self.paths.global_root(),
                self.scene_root(scene_id, &startup_scene),
                scene_id.to_string(),
            );
            Arc::new(
                off_the_runtime(move || {
                    Settings::load(
                        global_root,
                        scene_root,
                        local_dir,
                        &scene_id,
                        &default_model,
                    )
                })
                .await,
            )
        };

        let (watch, unwatched_settings) = self.watch_tiers_of(&settings, &key);
        let last_good = self.last_good_routers.lock().await.get(&key).cloned();
        let (agent_types, client, credentials) = (
            self.plugins.agent_types(),
            self.client.clone(),
            self.credentials.clone(),
        );
        let built = Arc::new(
            off_the_runtime(move || ProjectBinding {
                unwatched_settings,
                ..ProjectBinding::build(
                    settings,
                    &agent_types,
                    watch,
                    &client,
                    last_good,
                    &credentials,
                )
            })
            .await,
        );
        if let Some(router) = built.task_router.clone() {
            self.last_good_routers
                .lock()
                .await
                .insert(key.clone(), router);
        }
        // Built outside the lock — scanning skill and agent directories is
        // disk work, and holding the map across it would serialize the first
        // session of every project behind the first session of any of them.
        // Whoever got there first wins; a duplicate build is wasted work, not
        // a second live catalog.
        let mut bindings = self.bindings.lock().await;
        bindings.entry(key).or_insert(built).clone()
    }

    /// Watch everything this binding was built from — its settings files,
    /// and the skill and agent-type directories of each of its three tiers —
    /// so a change to any of them invalidates it (see
    /// `start_config_watcher`).
    ///
    /// One hub, so one `notify` stream for the whole process no matter how
    /// many projects it serves, and paths several projects share are
    /// registered once.
    ///
    /// Only paths that exist are subscribed. `WatchHub::subscribe` falls back
    /// to the nearest existing ancestor of a path that isn't there, and the
    /// ancestor of `~/.agix/settings.json` is `~/.agix` — which also holds
    /// every transcript this daemon writes. Watching that would turn each
    /// turn's own log writes into a settings-change notification. The
    /// settings files left out are returned alongside the subscriptions: one
    /// created later is noticed the next time the binding is asked for
    /// (`ProjectBinding::appeared`). A skill or agent directory created later
    /// is picked up the next time this binding is built, or by an explicit
    /// `config.reload`.
    #[allow(clippy::type_complexity)]
    fn watch_tiers_of(
        &self,
        settings: &Settings,
        key: &ProjectSceneKey,
    ) -> (
        Vec<crate::watch_hub::WatchToken>,
        Vec<(crate::watch_hub::Tier, PathBuf)>,
    ) {
        use crate::watch_hub::{Tier, WatchKey, WatchTarget};
        let Some(hub) = self.watch.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        // The project tier's skills sit beside the project (`.agents/skills`)
        // rather than under `.agix`, which is why this is two paths and the
        // others are one — see `skills::module::build_default_skill_manager`.
        let project_root = key.0.clone();
        let mut tiers = vec![(
            Tier::Global,
            settings.paths.global_data_dir.clone(),
            settings.paths.global_data_dir.join("skills"),
        )];
        tiers.push((
            Tier::Scene(key.1.clone()),
            settings.paths.user_data_dir.clone(),
            settings.paths.user_data_dir.join("skills"),
        ));
        if let Some(root) = project_root {
            tiers.push((
                Tier::Project(root.clone()),
                settings.paths.local_data_dir.clone(),
                root.join(".agents").join("skills"),
            ));
        }

        let mut tokens = Vec::new();
        let mut unwatched = Vec::new();
        let mut subscribe = |tier: &Tier, target: WatchTarget, path: PathBuf| {
            if path.exists() {
                tokens.push(hub.subscribe(
                    WatchKey {
                        tier: tier.clone(),
                        target,
                    },
                    &path,
                ));
            } else if target == WatchTarget::Settings {
                unwatched.push((tier.clone(), path));
            }
        };
        for (tier, config_dir, skills_dir) in tiers {
            for file in [
                base::interface::settings::SETTINGS_FILE,
                base::interface::settings::SETTINGS_LOCAL_FILE,
            ] {
                subscribe(&tier, WatchTarget::Settings, config_dir.join(file));
            }
            subscribe(&tier, WatchTarget::Skills, skills_dir);
            subscribe(&tier, WatchTarget::Agents, config_dir.join("agents"));
        }
        (tokens, unwatched)
    }

    /// Drop the cached bindings merged from `changed` — every binding when
    /// `None` — and mark them stale, so the next session on those pairs is
    /// built from what is on disk now and the running ones rebuild before
    /// their next turn. A project tier sits under only its own pairs; the
    /// scene and global tiers under more.
    async fn invalidate_bindings(&self, changed: Option<&crate::watch_hub::Tier>) {
        self.bindings.lock().await.retain(|key, binding| {
            let hit = changed.map_or(true, |tier| tier_covers(tier, key));
            if hit {
                binding.mark_stale();
            }
            !hit
        });
    }

    /// Re-read the skill or agent-type directories into the catalogs of the
    /// bindings merged from `tier`, in place. Sessions share those catalogs,
    /// so they see the change at once and nothing is rebuilt.
    async fn refresh_catalogs(
        &self,
        tier: &crate::watch_hub::Tier,
        target: crate::watch_hub::WatchTarget,
    ) {
        use crate::watch_hub::WatchTarget;
        let plugin_agent_types = self.plugins.agent_types();
        for (key, binding) in self.bindings.lock().await.iter() {
            if !tier_covers(tier, key) {
                continue;
            }
            match target {
                WatchTarget::Skills => binding.reload_skills(),
                WatchTarget::Agents => binding.refresh_plugins(&plugin_agent_types),
                _ => {}
            }
        }
    }

    /// The binding this daemon's own project and startup scene resolve to —
    /// what a pool-level question with no session to ask about is answered
    /// from (`commands.list`, `daemon.doctor`'s skill count).
    async fn default_binding(&self) -> Arc<ProjectBinding> {
        let scene = self.settings.read().await.paths.scope.clone();
        let cwd = self.cwd.clone();
        self.binding_for(Some(&cwd), &scene).await
    }

    /// The scene tier directory for `scene_id`.
    ///
    /// The daemon's *own* scene keeps whatever `DaemonPaths::config_root()`
    /// resolves to — tests inject a `StaticDaemonPaths` whose config root is a
    /// tempdir bearing no relation to `global_root()/scenes/<id>`, and
    /// recomputing it here would move that scene's settings out from under
    /// them. Any other scene has no such injected path, so it is derived the
    /// way `DefaultDaemonPaths::from_env` derives the first one.
    fn scene_root(&self, scene_id: &str, startup_scene: &str) -> PathBuf {
        if scene_id == startup_scene {
            self.paths.config_root()
        } else {
            self.paths.global_root().join("scenes").join(scene_id)
        }
    }

    /// This project's centrally-connected `McpManager` — the MCP
    /// counterpart to `settings_for_project`, same `None`/`Some(root)` key
    /// convention. Connects lazily and caches the result on first use
    /// rather than eagerly for every project this pool has ever seen a
    /// session for: most daemons only ever serve their own default project,
    /// and eagerly connecting every project mentioned in a `session.create`
    /// call would pay for MCP servers nobody's session ever actually needs.
    /// The default project is the one exception — it's still connected
    /// eagerly at startup by `connect_mcp_servers_in_background`, so by the
    /// time this is first called for it the cache is typically already warm.
    ///
    /// Not deduplicated across projects with an identical `mcp_servers`
    /// config — two projects that happen to declare the same server each
    /// get their own connection. Sharing a handle across projects is future
    /// work; this only needed to stop the wrong thing from happening (a
    /// non-default project silently inheriting the default project's
    /// connections, or never connecting its own at all).
    /// Drop the cached connections whose `mcp_servers` came from `tier`, so the
    /// next session or rebuild on those keys connects with the changed config
    /// (a new `Authorization`, say) instead of reusing the old connection.
    /// `keep` is a key the caller reconnects itself.
    #[cfg(feature = "mcp")]
    async fn forget_mcp_built_from(
        &self,
        tier: &crate::watch_hub::Tier,
        keep: Option<&ProjectSceneKey>,
    ) {
        self.mcp_by_project
            .lock()
            .await
            .retain(|key, _| !tier_covers(tier, key) || keep == Some(key));
    }

    #[cfg(feature = "mcp")]
    async fn mcp_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<McpManager> {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        if let Some(existing) = self.mcp_by_project.lock().await.get(&key) {
            return existing.clone();
        }
        let settings = self.settings_for_project(project_root, scene_id).await;
        self.first_mcp_connect(key, &settings).await
    }

    /// A session's own recorded `(schema_version, project_root)`, read off
    /// its `Meta` line — used by `resume_or_create` so resuming a session
    /// rebuilds it against *its own* project, not this pool's default one.
    /// `None` covers both "no `Meta` at all" (a session that predates this
    /// daemon writing one) and "pre-v2 `Meta`" (no `project_root` field
    /// existed yet) — both mean "nothing recorded, infer the pool default",
    /// same spirit as `SceneCheck::Inferred`. Distinguishing those from a
    /// v2 `Meta` that explicitly recorded `project_root: null` (a genuine
    /// no-project session) is exactly why this returns the raw
    /// `schema_version` instead of collapsing straight to `Option<PathBuf>`.
    /// The recorded `scene` rides along because it comes from the same
    /// `Meta` and reattaching needs both: a session's settings are resolved
    /// against its own project *and* its own scene tier.
    async fn recorded_project(
        &self,
        session_id: &str,
    ) -> Option<(u32, Option<String>, Option<String>)> {
        let entries = self.load_entries(session_id).await.ok()?;
        let history::entry::LogEntry::Meta {
            schema_version,
            project_root,
            scene,
            ..
        } = find_meta(&entries)?
        else {
            return None;
        };
        Some((*schema_version, project_root.clone(), scene.clone()))
    }

    /// See [`SceneCheck`]. Safe to call unconditionally, including for a
    /// session id with no on-disk history at all (degrades to `Inferred`).
    ///
    /// Matches against any *currently active* scene (`self.active_scenes`),
    /// not just this pool's default — a session legitimately created via
    /// `session.create {scene: "harnix"}` after `scene.activate {scene:
    /// "harnix"}` is exactly as resumable/closeable/forkable as one under the
    /// default scene; only a scene this daemon isn't currently serving at
    /// all counts as a mismatch.
    pub async fn check_scene(&self, session_id: &str) -> SceneCheck {
        match self.facts_of(session_id).await.scene {
            None => SceneCheck::Inferred,
            Some(s) if self.active_scenes.read().await.contains_key(&s) => SceneCheck::Matches,
            Some(s) => SceneCheck::Mismatch(s),
        }
    }

    /// A sidechain (sub-agent/team-member) session that has a
    /// `LogEntry::SessionEnd` marker has run its one-shot task to
    /// conclusion and has no continuation semantics — `session.resume`
    /// must reject it with `SIDECHAIN_TERMINAL` instead of silently
    /// reattaching to a "finished" transcript. Primary sessions and
    /// sidechains with no marker (still running, or cut off externally)
    /// return `None` and proceed normally.
    pub async fn sidechain_terminal_state(
        &self,
        session_id: &str,
    ) -> Option<(Option<String>, history::entry::SessionEndState)> {
        let entries = self.load_entries(session_id).await.ok()?;
        let history::entry::LogEntry::Meta {
            session_kind,
            parent_session_id,
            ..
        } = find_meta(&entries)?
        else {
            return None;
        };
        if !matches!(session_kind, history::entry::SessionKind::Sidechain) {
            return None;
        }
        entries.iter().rev().find_map(|env| match &env.entry {
            history::entry::LogEntry::SessionEnd { state } => {
                Some((parent_session_id.clone(), *state))
            }
            _ => None,
        })
    }

    /// `session.resume` — the explicit, inspectable form of what
    /// `session.run_turn` has always done implicitly when handed a known
    /// `session_id`.
    ///
    /// Reattaches (loading the transcript back into the engine's context via
    /// the same `resume_or_create` → `Agent::resume_session` path
    /// `run_turn` uses) and reports what was reattached to, so a restarted
    /// client can tell "I'm continuing a 40-message conversation" from "this
    /// id was never used".
    ///
    /// `create_if_missing` is the one place this deliberately differs from
    /// `run_turn`: an id with no history is an error here by default
    /// (`SESSION_NOT_FOUND`), because an explicit "resume" that silently
    /// invents an empty session is how a client ends up rendering a blank
    /// window and calling it a restored one. `run_turn` keeps its
    /// create-on-miss behavior unchanged — sending a message to an unknown
    /// id is a different intent.
    ///
    /// An already-live session is reported as `already_active` and is *not*
    /// recreated: recreating swaps the `Agent` out from under any turn
    /// currently running on it (see `create()`'s same-sid replace path).
    /// This is also this method's fast path: an already-active id returns
    /// straight from the in-memory `sessions` map, never acquiring
    /// `with_session_lock` or reading `sidechain_terminal_state`'s full
    /// transcript off disk — both only matter for an id this pool isn't
    /// already actively serving.
    pub async fn resume_session(
        &self,
        session_id: &str,
        create_if_missing: bool,
        options: Option<&SessionOptions>,
    ) -> Result<serde_json::Value, ResumeError> {
        let resume_start = Instant::now();
        let settings = self.settings.read().await.clone();
        let mode = effective_permission_mode(&settings, options);

        // Already live: report state, touch nothing. The lock is released
        // before the (disk-reading) history lookup — `sessions` is the
        // pool's hottest mutex and every other holder keeps it CPU-only.
        let already_active = {
            let mut sessions = self.sessions.lock().await;
            sessions.get_mut(session_id).map(|live| {
                live.last_active = Instant::now();
                (
                    live.name.clone(),
                    format_instant(live.created_at),
                    format_instant(live.last_active),
                )
            })
        };
        if let Some((name, created_at, last_active)) = already_active {
            let (message_count, entry_count) = self.history_counts(session_id).await;
            let _ = self
                .pool_telemetry
                .record(telemetry::TelemetryEvent::resume_action(
                    session_id,
                    0,
                    None,
                    telemetry::ResumeActionPayload {
                        outcome: telemetry::ResumeOutcome::Succeeded,
                        source: "already_active".into(),
                        entry_count,
                        projected_message_count: message_count,
                        compact_boundary_count: 0,
                        sidechain_entry_count: 0,
                        warning_kind: None,
                        latency_ms: resume_start.elapsed().as_millis() as u64,
                    },
                ));
            return Ok(self
                .resume_report(
                    session_id,
                    "already_active",
                    message_count,
                    entry_count,
                    name,
                    created_at,
                    last_active,
                    mode,
                )
                .await);
        }

        // Not already active — serialize against a concurrent cascade
        // delete of this same id (see `with_session_lock`'s doc comment):
        // the terminal-state check and `resume_or_create` below must both
        // run as one atomic step relative to `delete_sidechain_list`/
        // `delete_session`, not race them.
        self.with_session_lock(session_id, async {
            // `docs/daemon_rpc_protocol.md` §3.3: a sidechain that already reached a terminal state has
            // no continuation semantics — reject before touching engine
            // state.
            if let Some((parent_session_id, state)) =
                self.sidechain_terminal_state(session_id).await
            {
                let _ = self
                    .pool_telemetry
                    .record(telemetry::TelemetryEvent::resume_action(
                        session_id,
                        0,
                        None,
                        telemetry::ResumeActionPayload {
                            outcome: telemetry::ResumeOutcome::Failed,
                            source: "sidechain_terminal".into(),
                            entry_count: 0,
                            projected_message_count: 0,
                            compact_boundary_count: 0,
                            sidechain_entry_count: 0,
                            warning_kind: Some("sidechain_terminal".into()),
                            latency_ms: resume_start.elapsed().as_millis() as u64,
                        },
                    ));
                return Err(ResumeError::SidechainTerminal {
                    session_id: session_id.to_string(),
                    parent_session_id,
                    final_state: state,
                });
            }

            let (message_count, entry_count) = self.history_counts(session_id).await;
            let has_history = entry_count > 0;
            if !has_history && !create_if_missing {
                let _ = self
                    .pool_telemetry
                    .record(telemetry::TelemetryEvent::resume_action(
                        session_id,
                        0,
                        None,
                        telemetry::ResumeActionPayload {
                            outcome: telemetry::ResumeOutcome::Failed,
                            source: "not_found".into(),
                            entry_count: 0,
                            projected_message_count: 0,
                            compact_boundary_count: 0,
                            sidechain_entry_count: 0,
                            warning_kind: Some("not_found".into()),
                            latency_ms: resume_start.elapsed().as_millis() as u64,
                        },
                    ));
                return Err(ResumeError::Rpc((
                    codes::SESSION_NOT_FOUND,
                    format!(
                        "session not found: {session_id} \
                         (pass create_if_missing: true to start a new session under this id)"
                    ),
                )));
            }

            let raced = self.sessions.lock().await.get(session_id).map(|live| {
                (
                    live.name.clone(),
                    format_instant(live.created_at),
                    format_instant(live.last_active),
                )
            });
            if let Some((name, created_at, last_active)) = raced {
                return Ok(self
                    .resume_report(
                        session_id,
                        "already_active",
                        message_count,
                        entry_count,
                        name,
                        created_at,
                        last_active,
                        mode,
                    )
                    .await);
            }
            let sid = self
                .resume_or_create(session_id.to_string(), options)
                .await
                .map_err(|e| ResumeError::Rpc((codes::INTERNAL_ERROR, e)))?;

            let (created_at, last_active) = {
                let sessions = self.sessions.lock().await;
                match sessions.get(&sid) {
                    Some(live) => (
                        format_instant(live.created_at),
                        format_instant(live.last_active),
                    ),
                    None => (String::new(), String::new()),
                }
            };
            let status = if has_history { "resumed" } else { "created" };

            Ok(self
                .resume_report(
                    &sid,
                    status,
                    message_count,
                    entry_count,
                    None,
                    created_at,
                    last_active,
                    mode,
                )
                .await)
        })
        .await
    }

    /// Projected message count + raw entry count for a session id, or
    /// `(0, 0)` when there's no store, the id doesn't parse, or nothing is
    /// on disk — all three mean the same thing to `session.resume`'s caller
    /// ("no prior conversation"), so they don't need distinguishing here.
    async fn history_counts(&self, session_id: &str) -> (usize, usize) {
        match (
            self.load_entries(session_id).await,
            self.history_store.as_ref(),
        ) {
            (Ok(entries), Some(store)) => (
                history::transcript::project_messages_with(&entries, store.projection()).len(),
                entries.len(),
            ),
            _ => (0, 0),
        }
    }

    /// `mode` is what the session was created under; a live session reports
    /// the mode it is in now, which `EnterPlanMode` / `ExitPlanMode` move.
    #[allow(clippy::too_many_arguments)]
    async fn resume_report(
        &self,
        session_id: &str,
        status: &str,
        message_count: usize,
        entry_count: usize,
        name: Option<String>,
        created_at: String,
        last_active: String,
        mode: base::interface::settings::PermissionMode,
    ) -> serde_json::Value {
        let mode = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.state.as_ref())
            .map_or(mode, |state| state.permission_mode().into());
        let prompts_enabled = !matches!(
            mode,
            base::interface::settings::PermissionMode::BypassPermissions
        );
        serde_json::json!({
            "session_id": session_id,
            "status": status,
            "active": true,
            "message_count": message_count,
            "entry_count": entry_count,
            "name": name,
            "created_at": created_at,
            "last_active": last_active,
            "scene": self.scene.id(),
            "permission": {
                "mode": serde_json::to_value(mode).unwrap_or(serde_json::Value::Null),
                "prompts": prompts_enabled,
                "prompt_timeout_secs": self.permission_prompt_timeout.as_secs(),
            },
        })
    }

    /// 关闭所有 session，等每个 Agent 把手上的一轮收尾落盘再返回。From the
    /// moment this starts no session is created; a second call waits for the
    /// first to finish.
    pub async fn shutdown_all(&self) {
        if self
            .shutting_down
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.shut_down.cancelled().await;
            return;
        }
        let _done = self.shut_down.clone().drop_guard();
        let (exits, ledgers): (Vec<CancellationToken>, Vec<_>) = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .drain()
                .map(|(sid, live)| {
                    live.cancel.cancel();
                    info!(%sid, "session removed (shutdown)");
                    (live.exited, live.process_groups)
                })
                .unzip()
        };
        let all = futures::future::join_all(exits.iter().map(|e| e.cancelled()));
        if tokio::time::timeout(AGENT_EXIT_GRACE, all).await.is_err() {
            warn!("some sessions did not stop within the grace period");
        }
        futures::future::join_all(
            ledgers
                .iter()
                .flatten()
                .map(|ledger| ledger.reap(PROCESS_GROUP_GRACE)),
        )
        .await;
    }

    fn refuse_while_shutting_down(&self) -> Result<(), String> {
        if self.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("the daemon is shutting down".to_string());
        }
        Ok(())
    }

    /// Stops the Agent running under `session_id`, if any, and waits for it
    /// to finish — so whatever reads its transcript next reads all of it.
    async fn stop_live_agent(&self, session_id: &str) {
        let exited = {
            let sessions = self.sessions.lock().await;
            let Some(live) = sessions.get(session_id) else {
                return;
            };
            live.cancel.cancel();
            live.exited.clone()
        };
        if tokio::time::timeout(AGENT_EXIT_GRACE, exited.cancelled())
            .await
            .is_err()
        {
            warn!(%session_id, "previous agent did not stop within the grace period");
        }
    }

    /// Some change notifications were lost, so any tier may have changed:
    /// reload this pool's own pair, and drop every other pair's MCP
    /// connections so none outlives the settings it was built from.
    async fn reload_after_lost_changes(&self) {
        #[cfg(feature = "mcp")]
        {
            let own = (Some(self.cwd.clone()), self.scene_id().to_string());
            self.forget_mcp_built_from(&crate::watch_hub::Tier::Global, Some(&own))
                .await;
        }
        self.reload_settings(None, self.scene_id()).await;
    }

    /// React to what a binding is built from changing on disk, for **every**
    /// project this daemon serves rather than only its own.
    ///
    /// A settings change invalidates exactly the bindings merged from the tier
    /// that changed; a change to this daemon's own tiers also goes through the
    /// full reload, since the merged settings, the task router and the MCP
    /// reconcile all hang off it. A skill or agent-type directory change is
    /// re-read into the catalogs in place — sessions share them, so nothing
    /// needs rebuilding.
    ///
    /// The pool's own tiers are watched for as long as the pool lives, not
    /// only while some binding on them happens to exist.
    pub fn start_config_watcher(self: &Arc<Self>) {
        let Some(mut changes) = self.watch.as_ref().map(|hub| hub.changes()) else {
            return;
        };
        let pool = self.clone();
        tokio::spawn(async move {
            use crate::watch_hub::{Tier, WatchTarget};
            {
                let settings = pool.settings.read().await.clone();
                let own = (Some(pool.cwd.clone()), settings.paths.scope.clone());
                let (tokens, _) = pool.watch_tiers_of(&settings, &own);
                *pool
                    .pool_watch
                    .lock()
                    .expect("pool_watch is never held across a panic") = tokens;
            }
            loop {
                let key = match changes.recv().await {
                    Ok(key) => key,
                    // Lagged: some notifications were dropped, which means
                    // something changed — reload rather than skip it.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        pool.reload_after_lost_changes().await;
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                match key.target {
                    WatchTarget::Settings => {}
                    WatchTarget::Skills | WatchTarget::Agents => {
                        info!(tier = ?key.tier, target = ?key.target, "a catalog changed on disk");
                        pool.refresh_catalogs(&key.tier, key.target).await;
                        continue;
                    }
                    // Plugins are a global/scene concern with a refresh of
                    // their own (`plugin.reload`).
                    _ => continue,
                }
                let ours = match &key.tier {
                    Tier::Project(root) => root == &pool.cwd,
                    Tier::Global | Tier::Scene(_) => true,
                };
                if ours {
                    info!(tier = ?key.tier, "settings changed on disk; reloading");
                    let scene = match &key.tier {
                        Tier::Scene(scene) => scene.clone(),
                        _ => pool.scene_id().to_string(),
                    };
                    #[cfg(feature = "mcp")]
                    let reconciled = (Some(pool.cwd.clone()), scene.clone());
                    #[cfg(feature = "mcp")]
                    pool.forget_mcp_built_from(&key.tier, Some(&reconciled))
                        .await;
                    pool.reload_settings_from(None, &scene, Some(&key.tier))
                        .await;
                } else {
                    info!(tier = ?key.tier, "settings changed on disk");
                    #[cfg(feature = "mcp")]
                    pool.forget_mcp_built_from(&key.tier, None).await;
                    pool.invalidate_bindings(Some(&key.tier)).await;
                }
            }
        });
    }

    /// Fire scheduled jobs as they come due, for as long as the pool lives.
    ///
    /// Checks at the top of every minute — a 5-field cron expression's
    /// resolution. A due job runs as a turn of its session through
    /// [`Self::run_turn`], the same path a client's message takes, so it
    /// queues behind a running turn, is announced as `turn_started` /
    /// `turn_completed` with initiator `"schedule"`, and its frames reach
    /// whoever watches the session. A session that is not in memory is
    /// resumed for it. Jobs missed while the daemon was down are not caught up.
    pub fn start_scheduler(self: &Arc<Self>) {
        let _ = self.this.set(Arc::downgrade(self));
        let pool = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let into_minute = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() % 60_000)
                    .unwrap_or(0) as u64;
                tokio::time::sleep(Duration::from_millis(60_000 - into_minute + 50)).await;
                let Some(pool) = pool.upgrade() else {
                    return;
                };
                tokio::spawn(async move {
                    pool.run_due_jobs(time::OffsetDateTime::now_utc()).await;
                });
            }
        });
    }

    /// Fire every job due at `now`, each as its session's turn, and return
    /// once those turns have ended. The scheduler calls this every minute.
    pub async fn run_due_jobs(&self, now: time::OffsetDateTime) {
        let turns: Vec<_> = self
            .schedule
            .pop_due_at(now)
            .into_iter()
            .map(|job| self.fire_scheduled(job))
            .collect();
        futures::future::join_all(turns).await;
    }

    /// Run one scheduled job as a turn of its session.
    async fn fire_scheduled(&self, job: tools::cron::CronJob) {
        let Some(session_id) = job.session_id.clone() else {
            warn!(job_id = %job.id, "scheduled job belongs to no session; dropping it");
            self.schedule.remove(&job.id);
            return;
        };
        let connection = crate::rpc::ConnectionId(
            self.internal_connections
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed),
        );
        let client = Arc::new(crate::rpc::Client::new(connection, Arc::new(Discard)));
        info!(job_id = %job.id, %session_id, "scheduled job fired");
        let response = self
            .run_turn(
                Some(session_id.clone()),
                job.prompt.clone(),
                Vec::new(),
                format!("schedule-{}-{}", job.id, Id::new()),
                client,
                serde_json::Value::Null,
                None,
                OnBusy::Queue,
                "schedule",
            )
            .await;
        self.drop_connection(connection).await;
        if let Some(error) = response.error {
            warn!(job_id = %job.id, %session_id, code = error.code, message = %error.message,
                  "scheduled job's turn failed");
        }
    }

    /// While no turn holds `event_rx`, pass on what the engine still says: a
    /// background sub-agent's question and progress, a team stage. Woken by
    /// `arrived` when the engine sends something; when a turn holds the
    /// receiver, waits for `idle` and looks again, since what arrived as the
    /// turn ended is still there. A turn that starts takes the receiver as
    /// before; this only ever tries the lock, so it never makes a turn wait.
    fn forward_events_between_turns(
        &self,
        session_id: &str,
        event_rx: &Arc<AsyncMutex<Option<EventReceiver>>>,
        arrived: &Arc<tokio::sync::Notify>,
        idle: &Arc<tokio::sync::Notify>,
        cancel: &CancellationToken,
    ) {
        let Some(pool) = self.this.get().cloned() else {
            return;
        };
        let (event_rx, arrived, idle, cancel) = (
            event_rx.clone(),
            arrived.clone(),
            idle.clone(),
            cancel.clone(),
        );
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = arrived.notified() => {}
                }
                let waiting = loop {
                    let returned = idle.notified();
                    tokio::pin!(returned);
                    returned.as_mut().enable();
                    if let Ok(mut slot) = event_rx.try_lock() {
                        if let Some(rx) = slot.as_mut() {
                            let mut waiting = Vec::new();
                            while let Ok(event) = rx.try_recv() {
                                waiting.push(event);
                            }
                            break waiting;
                        }
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = returned => {}
                    }
                };
                if waiting.is_empty() {
                    continue;
                }
                let Some(pool) = pool.upgrade() else { return };
                for event in waiting {
                    if let Some((frame, ask)) = background_frame(&session_id, None, &event) {
                        if let Some(ask) = &ask {
                            pool.remember_prompt(&session_id, ask, &frame).await;
                        }
                        pool.broadcast_to_session(&session_id, &frame).await;
                    }
                }
            }
        });
    }

    /// Push `session_id`'s task events to whoever watches the session, for as
    /// long as its engine lives. Outside any turn, so `turn_id` is `null`.
    fn forward_task_events(&self, session_id: &str, state: &Arc<base::context::SessionState>) {
        let Some(pool) = self.this.get().cloned() else {
            return;
        };
        let mut events = state.subscribe_tasks();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(pool) = pool.upgrade() else { return };
                        if event["kind"] == "task_finished" {
                            if let Some(task_id) = event["task"]["task_id"].as_str() {
                                pool.forget_prompts_of_task(&session_id, task_id).await;
                            }
                        }
                        if let Some(frame) = StreamFrame::task_event(&session_id, &event) {
                            pool.broadcast_to_session(&session_id, &frame).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// `session.tasks`: the work this session has running — sub-agents, team
    /// members, shell commands, monitors, and the jobs scheduled for it.
    /// `active` is whether its engine is in memory; a session that is not has
    /// only its scheduled jobs.
    pub async fn session_tasks(&self, session_id: &str) -> serde_json::Value {
        let state = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.state.clone());
        let mut tasks = state.as_ref().map(|s| s.task_infos()).unwrap_or_default();
        tasks.extend(self.schedule.list_for(session_id).into_iter().map(|job| {
            serde_json::json!({
                "task_id": job.id,
                "kind": "cron",
                "status": "waiting",
                "started_ms": job.created_ms,
                "ended_ms": null,
                "detail": {
                    "cron": job.cron,
                    "prompt": job.prompt,
                    "recurring": job.recurring,
                    "durable": job.durable,
                    "expires_ms": job.expires_ms,
                },
            })
        }));
        serde_json::json!({"active": state.is_some(), "tasks": tasks})
    }

    /// `session.taskOutput`: part of what one of `session.tasks`' entries has
    /// produced — what the model's `TaskOutput` reads. `None` if the
    /// session's engine does not know the task; `Some(Err(next_byte))` if
    /// `read` starts past the end of the output.
    pub async fn task_output(
        &self,
        session_id: &str,
        task_id: &str,
        read: TaskOutputRead,
    ) -> Option<Result<serde_json::Value, u64>> {
        let state = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.state.clone())?;
        let task = state.find_running_task(task_id)?;
        let (slice, truncated) = match read {
            TaskOutputRead::Tail(tail_bytes) => {
                let slice = task.output_tail(tail_bytes);
                let truncated = slice.from_byte > 0;
                (slice, truncated)
            }
            TaskOutputRead::From(from_byte) => match task.output_from(from_byte) {
                Ok(slice) => {
                    let truncated = slice.dropped > 0;
                    (slice, truncated)
                }
                Err(next_byte) => return Some(Err(next_byte)),
            },
        };
        Some(Ok(serde_json::json!({
            "output": slice.text,
            "truncated": truncated,
            "status": task.info()["status"],
            "from_byte": slice.from_byte,
            "next_byte": slice.next_byte,
            "dropped": slice.dropped,
            "final": slice.closed,
        })))
    }

    /// `session.stopTask`: stop one of `session.tasks`' entries. A scheduled
    /// job is cancelled; running work is stopped the way the model's
    /// `TaskStop` stops it — a shell command's whole process group goes.
    /// `false` if there is no such running task.
    pub async fn stop_task(&self, session_id: &str, task_id: &str) -> bool {
        if self.schedule.remove_for(session_id, task_id) {
            return true;
        }
        let state = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.state.clone());
        state.is_some_and(|s| s.cancel_running_task(task_id))
    }

    /// `session.messageTask`: hand a message to a running background
    /// sub-agent, the way the model's `TaskMessage` does. `None` when the
    /// session is not in memory; otherwise why the task did not take it.
    pub async fn message_task(
        &self,
        session_id: &str,
        task_id: &str,
        message: String,
    ) -> Option<Result<(), String>> {
        let state = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.state.clone())?;
        Some(tools::task_message::deliver(&state, task_id, message))
    }

    /// `session.startAgent`: a sub-agent the host starts under `session_id`,
    /// in the background, whether or not a turn is running. A session not in
    /// memory is resumed for it.
    pub async fn start_agent(
        &self,
        session_id: &str,
        request: base::interface::agent_spawner::AgentRequest,
    ) -> Result<serde_json::Value, RpcError> {
        use base::interface::agent_spawner::StartAgentError;
        let not_found = || {
            (
                codes::SESSION_NOT_FOUND,
                format!("no session `{session_id}`"),
            )
        };
        if !self.session_exists(session_id).await {
            return Err(not_found());
        }
        self.ensure_live(session_id, None)
            .await
            .map_err(|e| (codes::INTERNAL_ERROR, e))?;
        let (state, spawner, cancel) = {
            let sessions = self.sessions.lock().await;
            let live = sessions.get(session_id).ok_or_else(not_found)?;
            (
                live.state.clone(),
                live.spawner.clone(),
                live.cancel.child_token(),
            )
        };
        let (Some(state), Some(spawner)) = (state, spawner) else {
            return Err(not_found());
        };
        let started = spawner
            .start_agent(request, state, cancel)
            .await
            .map_err(|e| match e {
                StartAgentError::Limit(m) => (codes::TASK_LIMIT, m),
                StartAgentError::Unsupported(m) => (codes::INVALID_REQUEST, m),
            })?;
        Ok(serde_json::json!({
            "task_id": started.task_id,
            "agent_session_id": started.agent_session_id,
        }))
    }

    /// Ask a session's engine about, or change, its context
    /// (`session.inspectContext`, `session.dropMessages`,
    /// `session.editMessage`). A session not in memory is resumed for it; one
    /// running a turn answers `SESSION_BUSY` — its context is moving under
    /// the question. A change is applied to the engine first and then written
    /// to the log as an `agix.context` entry, so a resume reproduces it.
    pub async fn context_request(
        &self,
        session_id: &str,
        request: runtime::agent::ContextRequest,
    ) -> Result<serde_json::Value, RpcError> {
        if !self.session_exists(session_id).await {
            return Err((
                codes::SESSION_NOT_FOUND,
                format!("no session `{session_id}`"),
            ));
        }
        self.ensure_live(session_id, None)
            .await
            .map_err(|e| (codes::INTERNAL_ERROR, e))?;
        let input_tx = {
            let sessions = self.sessions.lock().await;
            let live = sessions.get(session_id).ok_or_else(|| {
                (
                    codes::SESSION_NOT_FOUND,
                    format!("no session `{session_id}`"),
                )
            })?;
            if let Some(turn) = &live.current_turn {
                return Err((
                    codes::SESSION_BUSY,
                    format!("session `{session_id}` is running turn `{turn}`"),
                ));
            }
            live.input_tx.clone()
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        input_tx
            .send(InputMessage::Context {
                request: request.clone(),
                reply,
            })
            .map_err(|_| {
                (
                    codes::ENGINE_ERROR,
                    "the session's engine has stopped".to_string(),
                )
            })?;
        let result = tokio::time::timeout(Duration::from_secs(30), answer)
            .await
            .map_err(|_| (codes::ENGINE_ERROR, "the engine did not answer".to_string()))?
            .map_err(|_| (codes::ENGINE_ERROR, "the engine went away".to_string()))?
            .map_err(|e| (codes::INVALID_PARAMS, e))?;

        let edits: Vec<(&str, serde_json::Value)> = match &request {
            runtime::agent::ContextRequest::Inspect => Vec::new(),
            runtime::agent::ContextRequest::Drop { at, .. } => at
                .iter()
                .map(|i| ("drop", serde_json::json!({"at": i})))
                .collect(),
            runtime::agent::ContextRequest::Edit { at, text, .. } => {
                vec![("edit", serde_json::json!({"at": at, "text": text}))]
            }
        };
        if let (false, Some(store)) = (edits.is_empty(), self.history_store.as_ref()) {
            let sid = base::session::SessionId::parse(session_id)
                .map_err(|e| (codes::INVALID_PARAMS, e.to_string()))?;
            for (event, mut payload) in edits {
                payload["at_ms"] = serde_json::json!(std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0));
                let entry = history::entry::LogEntry::Extension {
                    ns: history::transcript::CONTEXT_NS.to_string(),
                    event: event.to_string(),
                    payload,
                };
                store.append(sid, entry).await.map_err(|e| {
                    (
                        codes::INTERNAL_ERROR,
                        format!("the change was made but not logged: {e}"),
                    )
                })?;
            }
        }
        Ok(result)
    }

    /// Schedule `prompt` to run as a turn of `session_id` on `cron`.
    pub fn schedule_add(&self, new: tools::cron::NewCronJob) -> tools::cron::CronJob {
        self.schedule.add_job(new)
    }

    /// Whether `session_id` names a session this pool has — in memory, or a
    /// transcript it can resume.
    pub async fn session_exists(&self, session_id: &str) -> bool {
        if self.sessions.lock().await.contains_key(session_id) {
            return true;
        }
        self.load_entries(session_id).await.is_ok()
    }

    /// Scheduled jobs, all of them or one session's.
    pub fn schedule_list(&self, session_id: Option<&str>) -> Vec<tools::cron::CronJob> {
        match session_id {
            Some(sid) => self.schedule.list_for(sid),
            None => self.schedule.list(),
        }
    }

    pub fn schedule_remove(&self, id: &str) -> bool {
        self.schedule.remove(id)
    }

    pub fn start_janitor(self: &Arc<Self>) {
        let pool = self.clone();
        // Sweep interval scales with `idle_timeout` so a short configured
        // timeout is actually honored. Clamped to [30s, 300s] so a very short
        // timeout doesn't spin the loop and a very long one doesn't sweep
        // needlessly often.
        let sweep_interval =
            (pool.idle_timeout / 4).clamp(Duration::from_secs(30), Duration::from_secs(300));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(sweep_interval).await;
                let mut sessions = pool.sessions.lock().await;
                let to_evict = idle_session_ids(&sessions, Instant::now(), pool.idle_timeout);
                for sid in &to_evict {
                    if let Some(live) = sessions.remove(sid) {
                        pool.evict(sid, &live);
                        info!(%sid, "session evicted (idle timeout)");
                        pool.emit_session_event(
                            "session_evicted",
                            sid,
                            serde_json::json!({"reason": "idle"}),
                        );
                    }
                }
                if !to_evict.is_empty() {
                    debug!(
                        evicted = to_evict.len(),
                        remaining = sessions.len(),
                        "janitor run"
                    );
                }
            }
        });
    }

    /// Hold `session_id`'s log against other processes for as long as the
    /// returned claim lives — `Err` when another daemon has it open. An id
    /// that is not a session id has no log to hold.
    async fn claim_log(
        &self,
        session_id: &str,
    ) -> Result<Option<history::store::SessionClaim>, String> {
        let (Some(store), Ok(sid)) = (
            self.history_store.as_ref(),
            base::session::SessionId::parse(session_id),
        ) else {
            return Ok(None);
        };
        match store.claim(sid).await {
            Ok(claim) => Ok(Some(claim)),
            Err(e @ history::error::HistoryError::Locked(_)) => Err(e.to_string()),
            Err(e) => {
                warn!(%session_id, error = %e, "could not take the session log's writer lock");
                Ok(None)
            }
        }
    }

    /// Bring `sid` into memory under that id: resumed from its log when it
    /// has one, created empty otherwise. A failure is the caller's error —
    /// never a different session under a fresh id, which would carry the
    /// conversation on somewhere the host is not looking.
    async fn resume_or_create(
        &self,
        sid: String,
        options: Option<&SessionOptions>,
    ) -> Result<String, String> {
        let _claim = self.claim_log(&sid).await?;
        // 尝试从 HistoryStore 加载历史消息。`sid` 来自 RPC 调用方（`session.run_turn`
        // 的 `session_id` 参数），不保证是合法的 BASE58 ID——解析失败当作"没有历史"
        // 处理（等价于查不到），而不是 unwrap 崩溃：一个格式错误的 session_id 不该
        // 让整条连接的处理任务 panic。
        let has_history = if let Some(ref store) = self.history_store {
            match base::session::SessionId::parse(&sid) {
                Ok(parsed) => match store.load(parsed).await {
                    Ok(entries) => !entries.is_empty(),
                    Err(_) => false,
                },
                Err(_) => false,
            }
        } else {
            false
        };

        // Rebuild against *this session's own* recorded project, not
        // always the pool default — otherwise a session created under
        // `session.create {project_root: X}` would silently come back with
        // project Y's settings the moment it needed reattaching (idle
        // eviction, daemon restart, explicit `session.resume`).
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let (binding, project_root_for_meta, scene_id) = match self.recorded_project(&sid).await {
            Some((schema_version, project_root, scene)) if schema_version >= 2 => {
                let scene = scene.unwrap_or_else(|| startup_scene.clone());
                match project_root {
                    Some(p) => {
                        let path = PathBuf::from(p);
                        (
                            self.binding_for(Some(&path), &scene).await,
                            Some(path),
                            scene,
                        )
                    }
                    None => (self.binding_for(None, &scene).await, None, scene),
                }
            }
            _ => (
                self.default_binding().await,
                Some(self.cwd.clone()),
                startup_scene.clone(),
            ),
        };

        // The scene this session recorded, not this pool's default. Settings
        // were already resolved against the recorded scene above; handing the
        // default scene to `create()` anyway gave the rebuilt session that
        // scene's system prompt and tool surface while it read the other
        // one's settings — two halves of one session disagreeing about which
        // scene it is. Every caller is scene-guarded (`check_scene`) before
        // reaching here, so an inactive scene at this point is a race, and
        // refusing is the only answer that doesn't silently change what the
        // session is.
        let Some(scene) = self.resolve_scene(Some(&scene_id)).await else {
            return Err(format!(
                "session `{sid}` was recorded under scene `{scene_id}`, which is not active on this daemon"
            ));
        };

        self.create(
            sid.clone(),
            scene,
            binding,
            project_root_for_meta,
            options,
            has_history,
        )
        .await
        .map_err(|e| format!("session `{sid}` could not be brought back: {e}"))
    }

    /// `sid` in memory: resumed from its log, or created, unless it already
    /// is. Serialized per id, so two callers racing on one cold session get
    /// one engine rather than the second replacing the first's.
    async fn ensure_live(
        &self,
        sid: &str,
        options: Option<&SessionOptions>,
    ) -> Result<String, String> {
        self.with_session_lock(sid, async {
            if self.sessions.lock().await.contains_key(sid) {
                return Ok(sid.to_string());
            }
            self.resume_or_create(sid.to_string(), options).await
        })
        .await
    }

    /// Rebuild a live, idle session in place from its spec and log. Its
    /// turn slot is held from before the old engine stops until the new one
    /// is in the map, so no turn can start on, or be cut off by, the
    /// replacement; a session running a turn or background tasks is refused.
    async fn rebuild_idle(
        &self,
        sid: &str,
        options: Option<&SessionOptions>,
    ) -> Result<String, Rebuild> {
        self.with_session_lock(sid, async {
            let held = self.take_idle_slot(sid).await?;
            self.rebuild_holding(sid, held, options).await
        })
        .await
    }

    /// Take the turn slot of live, idle `sid`, so no turn starts until it
    /// is given back or the session rebuilt. A session whose background
    /// tasks still run is not idle: they run under its engine and would be
    /// stopped with it. Callers hold `sid`'s session lock.
    async fn take_idle_slot(&self, sid: &str) -> Result<HeldSlot, Rebuild> {
        let (slot, current_turn, state, idle) = {
            let sessions = self.sessions.lock().await;
            let live = sessions.get(sid).ok_or(Rebuild::NotLive)?;
            (
                live.event_rx.clone(),
                live.current_turn.clone(),
                live.state.clone(),
                live.idle.clone(),
            )
        };
        let mut taken = slot.lock().await;
        let Some(event_rx) = taken.take() else {
            return Err(Rebuild::Busy {
                current_turn,
                running_tasks: Vec::new(),
            });
        };
        let running_tasks = state
            .map(|state| state.running_background_task_ids())
            .unwrap_or_default();
        if !running_tasks.is_empty() {
            *taken = Some(event_rx);
            drop(taken);
            idle.notify_waiters();
            return Err(Rebuild::Busy {
                current_turn: None,
                running_tasks,
            });
        }
        drop(taken);
        Ok(HeldSlot { slot, event_rx })
    }

    /// Rebuild `sid` while `held` keeps turns off it. A failed rebuild gives
    /// the slot back to the session that is still there.
    async fn rebuild_holding(
        &self,
        sid: &str,
        held: HeldSlot,
        options: Option<&SessionOptions>,
    ) -> Result<String, Rebuild> {
        let rebuilt = self.resume_or_create(sid.to_string(), options).await;
        if rebuilt.is_err() {
            *held.slot.lock().await = Some(held.event_rx);
            if let Some(live) = self.sessions.lock().await.get(sid) {
                live.idle.notify_waiters();
            }
        }
        rebuilt.map_err(Rebuild::Failed)
    }
}

/// LRU 驱逐：从 `sessions` 里移除最久未活跃的一个（若非空）。不依赖 `SessionPool`
/// 的任何字段，拆成自由函数而非 `&self` 方法——一是本来就没用到 `self`，二是
/// 让测试不用搭一整个 `SessionPool` 就能直接验证驱逐选中的是最旧的那个。
/// The scenes a pool can serve: built-ins, then the host's, then the
/// declarative scenes under `user_scenes` (`<state root>/scenes/<id>/`), then
/// those `scene.register` added, then the plugins'. A declarative or
/// registered scene cannot take an id already there; one that does not load
/// is left out, named in the returned list, and the rest load.
fn registry_of(
    host_scenes: &[Arc<dyn Scene>],
    plugin_scenes: &[Arc<dyn Scene>],
    user_scenes: &std::path::Path,
    registered: &[Arc<dyn Scene>],
) -> (scene::SceneRegistry, Vec<String>) {
    let mut registry = scene::SceneRegistry::new();
    let mut refused = Vec::new();
    registry.register_builtin();
    for s in host_scenes {
        registry.register(s.clone());
    }
    for e in registry.register_declarative_dir(user_scenes) {
        warn!(error = %e, "a declarative scene was not loaded");
        refused.push(e.to_string());
    }
    for s in registered {
        if registry.resolve(s.id()).is_some() {
            let e = format!(
                "scene `{}` (scene.register): the id is already registered",
                s.id()
            );
            warn!(error = %e, "a registered scene was not loaded");
            refused.push(e);
        } else {
            registry.register(s.clone());
        }
    }
    for s in plugin_scenes {
        registry.register(s.clone());
    }
    (registry, refused)
}

/// Where a connection the pool opens for itself writes: nowhere. Its turn's
/// frames still reach every real subscriber of the session.
/// A goal round as its own boxed future: `run_turn` starts rounds and rounds
/// run `run_turn`, so the `Send` of one cannot be inferred from the other.
fn goal_round_task(
    pool: Arc<SessionPool>,
    sid: String,
    round: base::context::goal::GoalRound,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move { pool.run_goal_round(sid, round).await })
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

/// Whether `live` is doing anything eviction would cut off: running a turn,
/// holding its turn slot (a turn about to start, a rebuild), or running
/// background tasks.
fn is_busy(live: &LiveSession) -> bool {
    if live.current_turn.is_some() {
        return true;
    }
    let slot_free = live.event_rx.try_lock().is_ok_and(|slot| slot.is_some());
    if !slot_free {
        return true;
    }
    live.state.as_ref().is_some_and(|state| {
        state
            .list_running_tasks()
            .iter()
            .any(|(_, status)| matches!(status, base::context::RunningStatus::Running))
    })
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

/// The engine's events passed through to a receiver of their own, with a
/// notice each time one arrives — what lets the forwarder between turns sleep
/// until there is something to forward.
fn relay_events(mut from_engine: EventReceiver) -> (EventReceiver, Arc<tokio::sync::Notify>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let arrived = Arc::new(tokio::sync::Notify::new());
    let notify = arrived.clone();
    tokio::spawn(async move {
        while let Some(event) = from_engine.recv().await {
            if tx.send(event).is_err() {
                return;
            }
            notify.notify_one();
        }
    });
    (rx, arrived)
}

/// Take the least recently active idle session out of `sessions`, or `None`
/// when every session is busy — the pool then runs over its cap rather than
/// cut one off.
fn evict_lru(sessions: &mut HashMap<String, LiveSession>) -> Option<(String, LiveSession)> {
    let sid = sessions
        .iter()
        .filter(|(_, live)| !is_busy(live))
        .min_by_key(|(_, live)| live.last_active)
        .map(|(k, _)| k.clone())?;
    sessions.remove_entry(&sid)
}

/// The frame for an event the engine sends whether or not a turn is running
/// — a question, a sub-agent's or a team's progress, changed skills — with
/// the id of the question and of the background task asking it when it is
/// one; `None` for any other event.
///
/// A question is not terminal: the tool call that asked waits on
/// `session.respondToPrompt`. A sub-agent's activity arrives under its own
/// `kind`, so a client that does not know it ignores it and one that does
/// can show the sub-agent's tool calls under its `agent_label` instead of a
/// stalled `Agent` call.
fn background_frame(
    sid: &str,
    turn_id: Option<&str>,
    event: &AgentEvent,
) -> Option<(StreamFrame, Option<Ask>)> {
    let (body, prompt_id) = match event {
        AgentEvent::PermissionPrompt {
            prompt_id,
            tool_name,
            message,
            paths,
            tool_use_id,
            task_id,
            ..
        } => {
            let mut body = serde_json::json!({
                "kind":"prompt","prompt_type":crate::rpc::PROMPT_TYPE_PERMISSION,"prompt_id":prompt_id,
                "tool_name":tool_name,"message":message,
                "paths":paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
            });
            if let Some(tool_use_id) = tool_use_id {
                body["tool_use_id"] = serde_json::json!(tool_use_id);
            }
            (body, Some((prompt_id.clone(), task_id.clone())))
        }
        AgentEvent::QuestionPrompt {
            prompt_id,
            question,
            header,
            options,
            tool_use_id,
            task_id,
            ..
        } => (
            serde_json::json!({
                "kind":"prompt","prompt_type":crate::rpc::PROMPT_TYPE_QUESTION,
                "prompt_id":prompt_id,"tool_use_id":tool_use_id,
                "question":question,"header":header,"options":options
            }),
            Some((prompt_id.clone(), task_id.clone())),
        ),
        AgentEvent::SubagentProgress {
            agent_label,
            agent_session_id,
            agent_type,
            parent_turn,
            parent_tool_use_id,
            task_id,
            event,
            ..
        } => (
            serde_json::json!({
                "kind":"subagent_progress","agent_label":agent_label,
                "agent_session_id":agent_session_id,"agent_type":agent_type,
                "parent_turn":parent_turn,"parent_tool_use_id":parent_tool_use_id,
                "task_id":task_id,
                "event":serde_json::to_value(&**event).unwrap_or(serde_json::Value::Null)
            }),
            None,
        ),
        AgentEvent::SkillsChanged { added, removed, .. } => (
            serde_json::json!({"kind":"skills_changed","added":added,"removed":removed}),
            None,
        ),
        AgentEvent::Notification {
            message, status, ..
        } => (
            serde_json::json!({"kind":"notification","message":message,"status":status}),
            None,
        ),
        AgentEvent::TeamProgress {
            team,
            team_id,
            stage,
            stage_index,
            stage_count,
            status,
            members,
            failed,
        } => (
            serde_json::json!({
                "kind":"team_progress","team":team,"team_id":team_id,"stage":stage,
                "stage_index":stage_index,"stage_count":stage_count,
                "status":status,"members":members,"failed":failed
            }),
            None,
        ),
        _ => return None,
    };
    Some((StreamFrame::event_in(sid, turn_id, body), prompt_id))
}

fn reap_process_groups_later(live: &LiveSession) {
    if let Some(ledger) = live.process_groups.clone() {
        tokio::spawn(async move { ledger.reap(PROCESS_GROUP_GRACE).await });
    }
}

/// Session id 列表：`last_active` 距 `now` 超过 `timeout`、且手上没有活的那些。拆成纯函数
/// （不做任何驱逐动作，只挑出 id）——原逻辑内嵌在 `start_janitor` 真实跑
/// 300 秒一次的循环体里，没法在测试里等 5 分钟去验证；抽出来之后可以直接用
/// 人为构造的" last_active 已经很旧"的 session 测，不用真的睡觉。
fn idle_session_ids(
    sessions: &HashMap<String, LiveSession>,
    now: Instant,
    timeout: Duration,
) -> Vec<String> {
    sessions
        .iter()
        .filter(|(_, live)| now.duration_since(live.last_active) > timeout && !is_busy(live))
        .map(|(sid, _)| sid.clone())
        .collect()
}

fn format_instant(t: Instant) -> String {
    let ago = Instant::now().duration_since(t);
    let secs_ago = ago.as_secs();
    let now = time::OffsetDateTime::now_utc();
    let abs = now - time::Duration::seconds(secs_ago as i64);
    abs.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

/// Build the pool-level telemetry handle — see `SessionPool::pool_telemetry`'s
/// doc comment for what this is for. Same shape as the per-session pipeline
/// `create()` builds (`FileRecorder` + optional OTel sink via
/// `telemetry::spawn`), just writing to one shared `pool.jsonl` instead of
/// per-session files, since these events happen before any session exists.
fn build_pool_telemetry(settings: &Settings) -> telemetry::TelemetryHandle {
    if !settings.telemetry_enabled {
        return telemetry::TelemetryHandle::noop();
    }
    let dir = &settings.paths.local_data_dir;
    if !dir.is_absolute() {
        // Same reasoning as `create()`'s per-session path: recording
        // nothing is better than scattering a `telemetry/` directory into
        // whatever the process's working directory happens to be.
        return telemetry::TelemetryHandle::noop();
    }
    let path = dir.join("telemetry").join("pool.jsonl");
    let rec = match FileRecorder::new(&path) {
        Ok(rec) => rec,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "could not open pool-level telemetry file; pool-level events record nothing");
            return telemetry::TelemetryHandle::noop();
        }
    };
    let mut recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
        vec![std::sync::Arc::new(rec)];
    if let Some(otel) = otel_sink(settings.telemetry_url.as_deref()) {
        recorders.push(otel);
    }
    let config = telemetry::TelemetryConfig {
        enabled: true,
        mode: telemetry::TelemetryMode::Enabled,
        default_event_enabled: true,
        ..Default::default()
    };
    match telemetry::spawn(config, recorders) {
        Ok((handle, consumer)) => {
            tokio::spawn(consumer);
            handle
        }
        Err(e) => {
            warn!(error = %e, "failed to start pool-level telemetry pipeline");
            telemetry::TelemetryHandle::noop()
        }
    }
}

/// Where recordings go: what the caller asked for, else what this deployment
/// configured, else the conventional user-level directory.
///
/// The per-call override stays because a test harness points recordings at a
/// fixture directory; a host that simply wants recording on should not have to
/// invent a path, so `settings.recorder` is consulted next.
fn recordings_root(settings: &Settings, requested: Option<&str>) -> PathBuf {
    if let Some(dir) = requested {
        return PathBuf::from(dir);
    }
    if let Some(recorder) = &settings.recorder {
        return recorder.root.clone();
    }
    base::paths::ConfigPaths::from_settings(&settings.paths).global_recordings_dir()
}

/// A `TelemetryRecorder` that forwards turn/tool events to OpenTelemetry, or
/// `None` when no OTLP endpoint is configured.
///
/// Every session gets the same process-wide exporter
/// (`crate::telemetry_otel::process_exporter`); a session's pipeline ending
/// flushes it, and only the process's own shutdown stops it.
///
/// The `telemetry` crate itself never talks to the network — this recorder
/// is how the daemon opts in to sending data anywhere. Compiled out unless
/// the daemon's own `otel` feature is on — the OTLP SDK is a heavy
/// dependency tree and the JSONL recorder is what makes a session debuggable
/// by default. Build the daemon with `--features otel` to get this.
#[cfg(feature = "otel")]
fn otel_sink(endpoint: Option<&str>) -> Option<std::sync::Arc<dyn telemetry::TelemetryRecorder>> {
    let endpoint = endpoint
        .map(str::to_string)
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok())
        .filter(|e| !e.trim().is_empty())?;
    crate::telemetry_otel::process_exporter(&endpoint)
        .map(|e| e as std::sync::Arc<dyn telemetry::TelemetryRecorder>)
}

/// No-op stand-in when the `otel` feature is off.
#[cfg(not(feature = "otel"))]
fn otel_sink(endpoint: Option<&str>) -> Option<std::sync::Arc<dyn telemetry::TelemetryRecorder>> {
    if endpoint.is_some_and(|e| !e.trim().is_empty()) {
        warn!(
            "an OTLP endpoint is configured but this build has no OpenTelemetry support; \
             rebuild with `--features otel`"
        );
    }
    None
}

/// The clock a session's kept time comes from, when it is not the machine's: the start the
/// host asked for, else — replaying — the moment the recording began, so the date in the
/// prompt is the one the recording's prompt carried.
fn session_clock(
    options: Option<&SessionOptions>,
    recorder: Option<&RecorderConfig>,
    session_id: &str,
) -> Option<Arc<dyn base::interface::environment::Environment>> {
    use base::interface::environment::ShiftedEnvironment;
    if let Some(clock) = options.and_then(|o| o.clock.as_ref()) {
        match time::OffsetDateTime::parse(
            &clock.start,
            &time::format_description::well_known::Rfc3339,
        ) {
            Ok(start) => return Some(Arc::new(ShiftedEnvironment::starting_at(start))),
            Err(e) => {
                tracing::warn!(start = %clock.start, error = %e, "clock.start is not RFC 3339; using the machine's clock")
            }
        }
    }
    let recorder = recorder.filter(|r| r.mode == RecorderMode::Replay)?;
    let name = recorder.name.as_deref().unwrap_or(session_id);
    let started = telemetry::recorder::recorded_at(&recorder.root, name)?;
    ShiftedEnvironment::starting_at_ms(started)
        .map(|e| Arc::new(e) as Arc<dyn base::interface::environment::Environment>)
}

/// The permission mode a session will actually run under.
///
/// The caller's `options.permission_mode` is honored, but **clamped** to the
/// daemon's configured `settings.permission_mode`: a client may tighten the
/// mode for its own session and may not loosen it, since there is no
/// per-method authorization on this socket (see `server.rs`).
///
/// `settings.allow_client_permission_override` (default `false`) lets the
/// client win outright, for deployments where every client is as trusted as
/// the daemon.
///
/// With nothing configured anywhere the result is `PermissionMode::Default`
/// — i.e. *ask*.
fn effective_permission_mode(
    settings: &Settings,
    options: Option<&SessionOptions>,
) -> base::interface::settings::PermissionMode {
    let Some(requested) = options.and_then(|o| o.permission_mode) else {
        return settings.permission_mode;
    };
    if settings.allow_client_permission_override {
        return requested;
    }
    let clamped = requested.min_permissive(settings.permission_mode);
    if clamped != requested {
        warn!(
            requested = ?requested,
            configured = ?settings.permission_mode,
            effective = ?clamped,
            "session requested a more permissive mode than this daemon allows; clamped \
             (set `allow_client_permission_override: true` to permit it)"
        );
    }
    clamped
}

/// Which `Permission` a new session gets.
///
/// Resolves a mode (`effective_permission_mode`) and builds a
/// `RuleSetPermission` for it; with nothing configured that mode is
/// `Default`, so an unlisted tool call raises a `kind:"prompt"` event and
/// waits for `session.respondToPrompt`.
///
/// `bypassPermissions` — from settings.json (daemon-wide) or
/// `options.permission_mode` (one session) — is the explicit opt-out, and it
/// returns the *literal same* shared `Arc` the pool was built with, not an
/// equivalent instance: a host that sandboxes the daemon itself gets a
/// zero-overhead path, and the `Arc::ptr_eq` assertion in the
/// unit test below keeps it that way.
///
/// Rules come from every tier, settings first — `settings.permission_rules`
/// and the `settings.local.json` overlay, each tagged with its own
/// `RuleSource` (see `RuleSetPermission::from_settings`) — then
/// `options.permission_rules` appended, so a session-scoped rule is
/// evaluated alongside the configured ones rather than replacing them.
///
/// An `ask` verdict from the rule engine becomes `PermissionOutcome::Prompt`;
/// the engine raises `AgentEvent::PermissionPrompt`, `run_turn` broadcasts it
/// to the session's subscribers, and `session.respondToPrompt` feeds the
/// answer back in.
///
/// Free function (not inlined into `create()`) specifically so this decision
/// is unit-testable without spinning up a real session.
#[allow(clippy::too_many_arguments)]
/// Turn `settings.scripts` into a prompt registry, or nothing.
///
/// `None` when no scripts are configured, which is the overwhelming case and
/// costs the session nothing — the builder then uses its own default and no
/// registry is allocated.
///
/// A binding that cannot be honored is logged and the whole set is dropped,
/// rather than half of it being applied. That is louder than it looks on
/// purpose: a script that silently never runs sends its author looking for a
/// bug in their JavaScript.
#[cfg(feature = "scripts")]
fn bind_scripts(
    settings: &Arc<base::interface::settings::Settings>,
    project_root: &std::path::Path,
    packages: &[crate::plugins::PackageScripts],
) -> (
    Option<script_host::bindings::BoundScripts>,
    Vec<(String, String)>,
) {
    use script_host::bindings::{bind_into, bind_lenient, BindingSource, BoundScripts};

    if settings.scripts.is_empty() && packages.is_empty() {
        return (None, Vec::new());
    }

    // One value, so one ledger: the operator's scripts and the packages'
    // share the record a turn resets and `session.scripts` reads. Two would
    // have left whichever the session did not hold running on a per-turn
    // budget nothing ever reset.
    let mut bound = BoundScripts::default();
    let engine = script_host::bindings::engine();

    if let Err(e) = bind_into(
        &mut bound,
        engine.clone(),
        &settings.scripts,
        &BindingSource::Project { root: project_root },
    ) {
        // All-or-nothing, and `bind_into` already left `bound` untouched.
        // Louder than it looks on purpose: a script that silently never runs
        // sends its author looking for a bug in their JavaScript.
        error!(error = %e, "a script binding is invalid; no scripts from settings were bound");
    }

    let mut faults = Vec::new();
    for package in packages {
        for e in bind_lenient(
            &mut bound,
            engine.clone(),
            &package.bindings,
            &BindingSource::Package {
                name: &package.name,
                root: &package.root,
            },
        ) {
            // One binding at a time, and the failure is recorded against the
            // package that shipped it: a set assembled from several packages
            // has no single author, so dropping it whole would let one bad
            // package take away every other package's contributions.
            warn!(plugin = %package.name, error = %e, "a plugin script binding was not honored");
            faults.push((package.name.clone(), e.to_string()));
        }
    }

    if bound.carriers.is_empty() {
        return (None, faults);
    }
    info!(
        scripts = bound.carriers.len(),
        "bound scripts to extension points"
    );
    (Some(bound), faults)
}

/// Without the script carrier compiled in, a `scripts` section is refused
/// loudly rather than ignored — and so is a package that ships one.
#[cfg(not(feature = "scripts"))]
fn bind_scripts(
    settings: &Arc<base::interface::settings::Settings>,
    _project_root: &std::path::Path,
    packages: &[crate::plugins::PackageScripts],
) -> (Option<()>, Vec<(String, String)>) {
    const NO_ENGINE: &str = "this build carries no script engine";

    if !settings.scripts.is_empty() {
        error!(
            scripts = settings.scripts.len(),
            "this build carries no script engine, so a `scripts` section cannot be honored; \
             rebuild with the `scripts` feature or remove the section"
        );
    }
    let faults = packages
        .iter()
        .flat_map(|p| {
            p.bindings
                .iter()
                .map(|b| {
                    (
                        p.name.clone(),
                        format!("{NO_ENGINE}, so `{}` is not bound", b.point),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    (None, faults)
}

fn resolve_session_permission(
    host: Option<&Arc<dyn Permission>>,
    tools: &Arc<base::tool::InMemoryToolRegistry>,
    settings: &Settings,
    mode: base::interface::settings::PermissionMode,
    options: Option<&SessionOptions>,
) -> Arc<dyn Permission> {
    if let (Some(host), base::interface::settings::PermissionMode::BypassPermissions) = (host, mode)
    {
        return host.clone();
    }

    let extra = options
        .map(|o| o.permission_rules.clone())
        .unwrap_or_default();

    let rule_set: Arc<dyn Permission> = Arc::new(
        permissions::rule_set_permission::RuleSetPermission::from_settings(
            settings,
            mode.into(),
            tools.clone(),
            extra,
        ),
    );

    rule_set
}

/// The provider fields that are credentials, which `config.setProvider` keeps
/// out of the committed `settings.json`.
const PROVIDER_SECRET_KEYS: &[&str] = &["api_key"];

/// Remove the credential fields from one provider's JSON, returning them.
fn take_provider_secrets(provider: &mut serde_json::Value) -> Vec<(String, serde_json::Value)> {
    let Some(fields) = provider.as_object_mut() else {
        return Vec::new();
    };
    PROVIDER_SECRET_KEYS
        .iter()
        .filter_map(|key| fields.remove(*key).map(|value| (key.to_string(), value)))
        .collect()
}

/// `providers.<id>` of a settings object, created as `{}` when it is missing
/// or not an object.
fn provider_entry<'a>(
    settings: &'a mut serde_json::Map<String, serde_json::Value>,
    provider_id: &str,
) -> &'a mut serde_json::Value {
    let providers = settings
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}));
    if !providers.is_object() {
        *providers = serde_json::json!({});
    }
    let entry = providers
        .as_object_mut()
        .expect("just normalized to an object")
        .entry(provider_id.to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !entry.is_object() {
        *entry = serde_json::json!({});
    }
    entry
}

/// A settings file's top-level object, `{}` when it does not exist. One that
/// is not a JSON object is refused rather than replaced: writing back over it
/// would discard every other field configured there.
async fn read_settings_object(path: &Path) -> Result<serde_json::Value, String> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(serde_json::json!({})),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        format!(
            "existing {} is not valid JSON, refusing to overwrite it: {e}",
            path.display()
        )
    })?;
    if !parsed.is_object() {
        return Err(format!(
            "existing {} is not a JSON object at the top level, refusing to overwrite it",
            path.display()
        ));
    }
    Ok(parsed)
}

/// Write `value` to `path`, readable by this user alone when `private`.
async fn write_settings_file(
    path: &Path,
    value: &serde_json::Value,
    private: bool,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let pretty = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .await
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    use tokio::io::AsyncWriteExt;
    file.write_all(pretty.as_bytes())
        .await
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    file.flush()
        .await
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// Redact every credential-shaped field in a settings blob.
///
/// Walks by key name rather than by type, because the same secrets appear at
/// several depths (`providers.<id>.api_key`, `model.auth_token`) and a
/// settings shape that grows a new provider field should not silently start
/// returning it in the clear. Key names match case-insensitively, and every
/// string under a `headers` or `env` object is redacted whatever its key: an
/// MCP server's `Authorization` header and the variables a stdio server is
/// started with are credentials as often as not, under names nobody can list.
fn redact_settings_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let key = k.to_ascii_lowercase();
                if matches!(key.as_str(), "headers" | "env") && v.is_object() {
                    redact_every_string(v);
                    continue;
                }
                match v {
                    serde_json::Value::String(s) if is_secret_key(&key) => {
                        if !s.is_empty() {
                            *v = serde_json::Value::String(redact_secret(s));
                        }
                    }
                    other => redact_settings_secrets(other),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_settings_secrets(item);
            }
        }
        _ => {}
    }
}

fn is_secret_key(lowercase_key: &str) -> bool {
    const SECRET_KEYS: &[&str] = &[
        "api_key",
        "auth_token",
        "token",
        "secret",
        "password",
        "authorization",
    ];
    SECRET_KEYS.contains(&lowercase_key)
        || ["_token", "_secret", "_password"]
            .iter()
            .any(|suffix| lowercase_key.ends_with(suffix))
}

fn redact_every_string(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => {
            *value = serde_json::Value::String(redact_secret(s));
        }
        serde_json::Value::Object(map) => map.values_mut().for_each(redact_every_string),
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_every_string),
        _ => {}
    }
}

/// Redact a secret to `***<last 4 chars>` (char-boundary safe — slices by
/// `char`, not byte, so this never panics on non-ASCII input); too-short
/// secrets (≤4 chars) redact to a bare `***` rather than revealing the
/// whole thing.
fn redact_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return "***".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("***{tail}")
}

/// One extension entry as `session.entries` and the `extension_entry` frame
/// show it. `at` counts the messages before it, in `session.history`'s
/// coordinates.
fn entry_view(
    index: usize,
    env: &history::entry::EnvelopedEntry,
    lengths: &[usize],
) -> Option<serde_json::Value> {
    if env.is_sidechain {
        return None;
    }
    let history::entry::LogEntry::Extension { ns, event, payload } = &env.entry else {
        return None;
    };
    let at = index
        .checked_sub(1)
        .map_or(0, |i| lengths.get(i).copied().unwrap_or(0));
    let ts = env
        .ts
        .format(&time::format_description::well_known::Rfc3339)
        .ok()?;
    Some(serde_json::json!({
        "id": env.id,
        "ts": ts,
        "ns": ns,
        "event": event,
        "payload": payload,
        "at": at,
    }))
}

/// A whole transcript as `session.history` pages it and the rows
/// `session.entries` lists, for an export to carry beside the raw log.
fn export_views(
    entries: &[history::entry::EnvelopedEntry],
    projection: &dyn history::transcript::TranscriptProjection,
) -> Result<(Vec<serde_json::Value>, Vec<serde_json::Value>), serde_json::Error> {
    let history = history::transcript::project_messages_sourced(entries, projection)
        .iter()
        .enumerate()
        .map(|(seq, m)| history_item(seq, m))
        .collect::<Result<Vec<_>, _>>()?;
    let lengths = history::transcript::projected_lengths_with(entries, projection);
    let rows = entries
        .iter()
        .enumerate()
        .filter(|(_, env)| {
            matches!(&env.entry, history::entry::LogEntry::Extension { ns, .. }
                if !ns.starts_with(ENGINE_NS_PREFIX))
        })
        .filter_map(|(i, env)| entry_view(i, env, &lengths))
        .collect();
    Ok((history, rows))
}

/// One `session.history` item: the projected message as it always was
/// (`role`, `content`, …) plus where it sits and where it came from.
fn history_item(
    seq: usize,
    sourced: &history::transcript::SourcedMessage<'_>,
) -> Result<serde_json::Value, serde_json::Error> {
    use history::entry::LogEntry;
    let env = sourced.entry;
    let mut item = serde_json::to_value(&sourced.message)?;
    let id = match sourced.part {
        Some(part) => serde_json::json!(format!(
            "{}#{part}",
            serde_json::to_value(env.id)?.as_str().unwrap_or_default()
        )),
        None => serde_json::to_value(env.id)?,
    };
    let origin = match (&sourced.message, sourced.part, &env.entry) {
        (base::message::Message::Assistant { .. }, _, _) => serde_json::json!("assistant"),
        (_, Some(_), _) => serde_json::json!("compact"),
        (_, None, LogEntry::ToolResult { .. }) => serde_json::json!("tool_result"),
        (_, None, LogEntry::Extension { .. }) => serde_json::json!("extension"),
        _ => serde_json::to_value(env.origin)?,
    };
    let usage = match (&env.entry, sourced.part) {
        (LogEntry::Assistant { usage: Some(u), .. }, None) => serde_json::to_value(u)?,
        _ => serde_json::Value::Null,
    };
    let ts = env
        .ts
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    if let Some(map) = item.as_object_mut() {
        map.insert("seq".into(), serde_json::json!(seq));
        map.insert("id".into(), id);
        map.insert("ts".into(), serde_json::json!(ts));
        map.insert("turn_id".into(), serde_json::json!(env.turn_id));
        map.insert("origin".into(), origin);
        map.insert("usage".into(), usage);
    }
    Ok(item)
}

#[cfg(test)]
mod pool_telemetry_tests {
    use super::*;

    #[tokio::test]
    async fn relative_local_data_dir_yields_a_noop_handle() {
        // `Settings::defaults_for` leaves `local_data_dir` at "." (relative)
        // — the same "don't scatter a telemetry/ dir into the working
        // directory" guard `create()`'s per-session path uses.
        let settings = Settings::defaults_for("claude-sonnet-4-6");
        let handle = build_pool_telemetry(&settings);
        // A noop handle's `record` always succeeds and writes nothing
        // anywhere observable; the real assertion is in the next test,
        // which proves the non-noop path actually persists.
        assert!(handle
            .record(telemetry::TelemetryEvent::resume_action(
                "sess",
                0,
                None,
                telemetry::ResumeActionPayload {
                    outcome: telemetry::ResumeOutcome::Failed,
                    source: "not_found".into(),
                    entry_count: 0,
                    projected_message_count: 0,
                    compact_boundary_count: 0,
                    sidechain_entry_count: 0,
                    warning_kind: Some("not_found".into()),
                    latency_ms: 0,
                },
            ))
            .is_ok());
    }

    #[tokio::test]
    async fn telemetry_disabled_yields_a_noop_handle_even_with_an_absolute_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::defaults_for("claude-sonnet-4-6");
        settings.telemetry_enabled = false;
        settings.paths.local_data_dir = dir.path().to_path_buf();
        let _handle = build_pool_telemetry(&settings);
        assert!(!dir.path().join("telemetry").join("pool.jsonl").exists());
    }

    #[tokio::test]
    async fn absolute_dir_and_telemetry_enabled_persists_events_to_pool_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::defaults_for("claude-sonnet-4-6");
        settings.telemetry_enabled = true;
        settings.paths.local_data_dir = dir.path().to_path_buf();
        let handle = build_pool_telemetry(&settings);

        handle
            .record(telemetry::TelemetryEvent::resume_action(
                "sess-1",
                0,
                None,
                telemetry::ResumeActionPayload {
                    outcome: telemetry::ResumeOutcome::Failed,
                    source: "not_found".into(),
                    entry_count: 0,
                    projected_message_count: 0,
                    compact_boundary_count: 0,
                    sidechain_entry_count: 0,
                    warning_kind: Some("not_found".into()),
                    latency_ms: 5,
                },
            ))
            .expect("record should succeed");

        // `FileRecorder` writes synchronously, but the event still has to
        // cross the channel to the fan-out task first — poll briefly rather
        // than assume it has landed the instant `record()` returns.
        let path = dir.path().join("telemetry").join("pool.jsonl");
        let content = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(content) = tokio::fs::read_to_string(&path).await {
                    if !content.is_empty() {
                        return content;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("pool.jsonl should have received the event");

        assert!(content.contains("resume_action"));
        assert!(content.contains("not_found"));
    }
}

#[cfg(test)]
mod redact_tests {
    use super::{redact_secret, redact_settings_secrets};

    #[test]
    fn mcp_headers_and_env_are_redacted_whatever_their_keys() {
        let mut settings = serde_json::json!({
            "mcp_servers": {
                "remote": {"type": "http", "url": "https://x",
                           "headers": {"Authorization": "Bearer sk-live-9999", "X-Trace": "on-1234"}},
                "local": {"type": "stdio", "command": "srv", "env": {"GITHUB_PAT": "ghp_abcd5678"}}
            },
            "providers": {"p": {"API_KEY": "sk-upper-4321", "Client_Secret": "cs-0000"}},
            "model": {"model_name": "keep-me"}
        });
        redact_settings_secrets(&mut settings);
        let dumped = settings.to_string();
        for leaked in ["sk-live", "on-1", "ghp_", "sk-upper", "cs-"] {
            assert!(!dumped.contains(leaked), "{leaked} in {dumped}");
        }
        assert_eq!(settings["model"]["model_name"], "keep-me");
        assert_eq!(settings["mcp_servers"]["remote"]["url"], "https://x");
        assert_eq!(settings["mcp_servers"]["local"]["command"], "srv");
    }

    #[test]
    fn redacts_long_secret_keeping_last_four() {
        assert_eq!(redact_secret("sk-ant-abcdef1234"), "***1234");
    }

    #[test]
    fn short_secret_fully_redacted() {
        assert_eq!(redact_secret("abcd"), "***");
        assert_eq!(redact_secret(""), "***");
    }

    #[test]
    fn handles_multibyte_chars_without_panicking() {
        // 4-byte-per-char emoji tail — a byte-slice `&s[len-4..]` would
        // panic here; char-based slicing must not.
        let s = "sk-😀😀😀😀";
        assert_eq!(redact_secret(s), "***😀😀😀😀");
    }
}

#[cfg(test)]
mod resolve_session_permission_tests {
    use super::*;

    struct AllowAllTestPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for AllowAllTestPermission {
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

    fn defaults() -> Settings {
        Settings::defaults_for("claude-sonnet-4-6")
    }

    /// Everything the resolver needs beyond the settings/options under test,
    /// with the host having supplied its own `Permission`.
    fn resolve(
        settings: &Settings,
        options: Option<&SessionOptions>,
    ) -> (Arc<dyn Permission>, Arc<dyn Permission>) {
        let host: Arc<dyn Permission> = Arc::new(AllowAllTestPermission);
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        let mode = effective_permission_mode(settings, options);
        let resolved = resolve_session_permission(Some(&host), &tools, settings, mode, options);
        (host, resolved)
    }

    /// With nothing configured, a session does not get the allow-all
    /// instance.
    #[test]
    fn nothing_configured_builds_a_real_asking_permission() {
        let (bypass, resolved) = resolve(&defaults(), None);
        assert!(
            !Arc::ptr_eq(&bypass, &resolved),
            "the daemon default is no longer allow-all — it must build a real asking permission"
        );

        let opts = SessionOptions::default();
        let (bypass, resolved) = resolve(&defaults(), Some(&opts));
        assert!(
            !Arc::ptr_eq(&bypass, &resolved),
            "an options object that simply omits permission_mode must not re-enable allow-all"
        );
    }

    #[tokio::test]
    async fn a_bypass_session_gets_the_hosts_permission_or_else_a_gate_that_only_validates() {
        use base::interface::permission::PermissionOutcome;
        let mut settings = defaults();
        settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;
        settings.permission_rules = vec![base::interface::settings::PermissionRule {
            tool: "Bash".into(),
            action: base::interface::settings::PermissionAction::Deny,
            tier: None,
        }];

        let (host, resolved) = resolve(&settings, None);
        assert!(
            Arc::ptr_eq(&host, &resolved),
            "a host that supplied its own permission gets that very instance"
        );

        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools::register_builtin_tools(&tools);
        let mode = effective_permission_mode(&settings, None);
        let gate = resolve_session_permission(None, &tools, &settings, mode, None);
        let cwd = std::env::temp_dir();
        assert!(matches!(
            gate.check("Bash", &serde_json::json!({"command": "ls"}), &cwd, "s")
                .await,
            PermissionOutcome::Permit
        ));
        assert!(
            matches!(
                gate.check("Bash", &serde_json::json!({"command": "  "}), &cwd, "s")
                    .await,
                PermissionOutcome::Deny { .. }
            ),
            "input validation still runs under bypass"
        );
    }

    #[test]
    fn a_session_clock_is_the_hosts_start_else_the_recordings_else_the_machines() {
        let date = |env: Option<Arc<dyn base::interface::environment::Environment>>| {
            env.map(|e| e.now().date().to_string())
        };
        let asked = SessionOptions {
            clock: Some(crate::rpc::ClockOptions {
                start: "2026-10-28T09:00:00Z".into(),
            }),
            ..Default::default()
        };
        assert_eq!(
            date(session_clock(Some(&asked), None, "s")),
            Some("2026-10-28".into())
        );

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("rec")).unwrap();
        std::fs::write(
            root.path().join("rec/calls.jsonl"),
            "{\"type\":\"recording\",\"version\":1,\"name\":\"rec\",\"session_id\":\"s0\",\
             \"created_at\":1761955200000,\"engine_version\":\"x\"}\n",
        )
        .unwrap();
        let recorder = |mode, name: Option<&str>| RecorderConfig {
            mode,
            name: name.map(str::to_string),
            root: root.path().to_path_buf(),
            on_divergence: Divergence::Warn,
        };
        let replay = recorder(RecorderMode::Replay, Some("rec"));
        assert_eq!(
            date(session_clock(None, Some(&replay), "s")),
            Some("2025-11-01".into())
        );
        assert_eq!(
            date(session_clock(Some(&asked), Some(&replay), "s")),
            Some("2026-10-28".into()),
            "what the host asked for wins"
        );
        assert!(session_clock(
            None,
            Some(&recorder(RecorderMode::Record, Some("rec"))),
            "s"
        )
        .is_none());
        assert!(
            session_clock(None, Some(&recorder(RecorderMode::Replay, None)), "no-such").is_none(),
            "no recording, no shift"
        );
        let garbled = SessionOptions {
            clock: Some(crate::rpc::ClockOptions {
                start: "yesterday".into(),
            }),
            ..Default::default()
        };
        assert!(session_clock(Some(&garbled), None, "s").is_none());
    }

    /// A client asking for a *more permissive* mode than the daemon is
    /// configured with gets clamped, not obeyed.
    #[test]
    fn session_options_cannot_loosen_an_asking_settings_default() {
        let opts = SessionOptions {
            permission_mode: Some(base::interface::settings::PermissionMode::BypassPermissions),
            ..Default::default()
        };
        let (bypass, resolved) = resolve(&defaults(), Some(&opts));
        assert!(
            !Arc::ptr_eq(&bypass, &resolved),
            "a client must not be able to talk the daemon into allow-all"
        );
        assert_eq!(
            effective_permission_mode(&defaults(), Some(&opts)),
            base::interface::settings::PermissionMode::Default,
            "the request should clamp down to the configured mode"
        );
    }

    /// ...unless the operator explicitly says clients are as trusted as the
    /// daemon itself.
    #[test]
    fn allow_client_permission_override_restores_client_wins() {
        let mut settings = defaults();
        settings.allow_client_permission_override = true;
        let opts = SessionOptions {
            permission_mode: Some(base::interface::settings::PermissionMode::BypassPermissions),
            ..Default::default()
        };
        let (bypass, resolved) = resolve(&settings, Some(&opts));
        assert!(Arc::ptr_eq(&bypass, &resolved));
    }

    /// Tightening is always allowed, in both directions of the comparison:
    /// a `bypassPermissions` daemon still honors a client that wants to be
    /// asked.
    #[test]
    fn a_client_may_always_tighten() {
        let mut settings = defaults();
        settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;
        let opts = SessionOptions {
            permission_mode: Some(base::interface::settings::PermissionMode::Plan),
            ..Default::default()
        };
        assert_eq!(
            effective_permission_mode(&settings, Some(&opts)),
            base::interface::settings::PermissionMode::Plan
        );
    }

    #[test]
    fn auto_and_yolo_in_session_options_clamp_and_resolve_as_bypass() {
        use base::interface::settings::PermissionMode;
        for spelling in ["auto", "yolo"] {
            let opts: SessionOptions =
                serde_json::from_value(serde_json::json!({ "permission_mode": spelling })).unwrap();
            assert_eq!(
                opts.permission_mode,
                Some(PermissionMode::BypassPermissions)
            );

            assert_eq!(
                effective_permission_mode(&defaults(), Some(&opts)),
                PermissionMode::Default,
                "{spelling} must clamp like bypassPermissions"
            );
            let mut accepts_edits = defaults();
            accepts_edits.permission_mode = PermissionMode::AcceptEdits;
            assert_eq!(
                effective_permission_mode(&accepts_edits, Some(&opts)),
                PermissionMode::AcceptEdits
            );

            let mut bypass = defaults();
            bypass.permission_mode = PermissionMode::BypassPermissions;
            let (host, resolved) = resolve(&bypass, Some(&opts));
            assert!(Arc::ptr_eq(&host, &resolved), "{spelling}");

            let mut overridable = defaults();
            overridable.allow_client_permission_override = true;
            let (host, resolved) = resolve(&overridable, Some(&opts));
            assert!(Arc::ptr_eq(&host, &resolved), "{spelling}");
        }
    }

    #[test]
    fn settings_ask_default_is_overridable_per_session_without_becoming_allow_all() {
        let opts = SessionOptions {
            permission_mode: Some(base::interface::settings::PermissionMode::DontAsk),
            ..Default::default()
        };
        let (bypass, resolved) = resolve(&defaults(), Some(&opts));
        assert!(!Arc::ptr_eq(&bypass, &resolved));
    }

    #[test]
    fn effective_mode_prefers_session_options_then_settings_then_ask() {
        use base::interface::settings::PermissionMode;
        let mut settings = defaults();
        assert_eq!(
            effective_permission_mode(&settings, None),
            PermissionMode::Default,
            "an unconfigured daemon asks"
        );

        settings.permission_mode = PermissionMode::BypassPermissions;
        assert_eq!(
            effective_permission_mode(&settings, None),
            PermissionMode::BypassPermissions
        );

        let opts = SessionOptions {
            permission_mode: Some(PermissionMode::Plan),
            ..Default::default()
        };
        assert_eq!(
            effective_permission_mode(&settings, Some(&opts)),
            PermissionMode::Plan,
            "a per-session mode wins over the settings default"
        );
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::*;

    /// A `LiveSession` with no real agent behind it — only `last_active`
    /// (and `cancel`, so eviction's `.cancel()` call doesn't panic) matter
    /// for these tests.
    fn dummy_live_session(last_active: Instant) -> LiveSession {
        let (input_tx, _input_rx): (InputSender, _) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx): (_, EventReceiver) = tokio::sync::mpsc::unbounded_channel();
        LiveSession {
            input_tx,
            event_rx: Arc::new(AsyncMutex::new(Some(event_rx))),
            cancel: CancellationToken::new(),
            exited: CancellationToken::new(),
            context: None,
            scene: Arc::new(scene::code::CodeScene::default()),
            project: None,
            name: None,
            created_at: last_active,
            last_active,
            is_first_turn: true,
            named_by_user: false,
            namer: SessionNamer::of(
                "dummy",
                None,
                &(Arc::new(AnthropicModel::new(Arc::new(
                    model::mock::MockAnthropicClient::new(),
                ))) as Arc<dyn base::interface::model::Model>),
                "claude-sonnet-4-6",
            ),
            binding_stale: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            current_turn: None,
            turn_initiator: None,
            state: None,
            spawner: None,
            idle: Arc::new(tokio::sync::Notify::new()),
            scripts: None,
            health: Default::default(),
            subscribers: HashMap::new(),
            pending_prompts: HashMap::new(),
            process_groups: None,
        }
    }

    #[test]
    fn evict_lru_removes_the_least_recently_active_session() {
        let now = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert(
            "oldest".to_string(),
            dummy_live_session(now.checked_sub(Duration::from_secs(60)).unwrap()),
        );
        sessions.insert(
            "middle".to_string(),
            dummy_live_session(now.checked_sub(Duration::from_secs(30)).unwrap()),
        );
        sessions.insert("newest".to_string(), dummy_live_session(now));

        evict_lru(&mut sessions);

        assert_eq!(
            sessions.len(),
            2,
            "sessions: {:?}",
            sessions.keys().collect::<Vec<_>>()
        );
        assert!(!sessions.contains_key("oldest"));
        assert!(sessions.contains_key("middle"));
        assert!(sessions.contains_key("newest"));
    }

    #[test]
    fn inserting_over_an_existing_sid_must_cancel_the_replaced_sessions_token() {
        // Mirrors the pattern `create()` uses: `sessions.insert(sid, new_live)`
        // returning `Some(old)` means a live entry already existed under that
        // exact sid — reachable via the config-generation recreate path in
        // `run_turn`. `CancellationToken` isn't tied to `Drop`, so failing to
        // call `.cancel()` on the returned old value would leak the old
        // Agent's background `tokio::spawn` loop forever. This test isolates
        // just that HashMap-replace-then-cancel pattern (not the full
        // `create()`/`SessionPool`, which needs a live client/settings/etc.).
        let mut sessions = HashMap::new();
        let old_session = dummy_live_session(Instant::now());
        let old_cancel = old_session.cancel.clone();
        sessions.insert("sid".to_string(), old_session);
        assert!(!old_cancel.is_cancelled());

        let new_session = dummy_live_session(Instant::now());
        if let Some(old) = sessions.insert("sid".to_string(), new_session) {
            old.cancel.cancel();
        }

        assert!(
            old_cancel.is_cancelled(),
            "replaced session's token must be cancelled"
        );
        assert_eq!(sessions.len(), 1);
    }

    fn running_a_task() -> Arc<base::context::SessionState> {
        let state = Arc::new(base::context::SessionState::new("/tmp".into()));
        state.register_task(
            "task-1",
            base::context::TaskType::Agent,
            CancellationToken::new(),
            serde_json::json!({}),
        );
        state
    }

    #[test]
    fn eviction_passes_over_a_session_that_is_doing_something() {
        let now = Instant::now();
        let long_ago = now.checked_sub(Duration::from_secs(600)).unwrap();
        let mut sessions = HashMap::new();
        let mut in_a_turn = dummy_live_session(long_ago);
        in_a_turn.current_turn = Some("t1".into());
        sessions.insert("in_a_turn".to_string(), in_a_turn);
        let slot_taken = dummy_live_session(long_ago);
        slot_taken.event_rx.try_lock().unwrap().take();
        sessions.insert("slot_taken".to_string(), slot_taken);
        let mut with_a_task = dummy_live_session(long_ago);
        with_a_task.state = Some(running_a_task());
        sessions.insert("with_a_task".to_string(), with_a_task);

        assert!(idle_session_ids(&sessions, now, Duration::from_secs(300)).is_empty());
        assert!(evict_lru(&mut sessions).is_none(), "every session is busy");
        assert_eq!(sessions.len(), 3);

        sessions.insert("idle".to_string(), dummy_live_session(now));
        assert_eq!(
            evict_lru(&mut sessions).map(|(sid, _)| sid).as_deref(),
            Some("idle")
        );
    }

    #[test]
    fn evict_lru_on_empty_map_is_a_noop() {
        let mut sessions: HashMap<String, LiveSession> = HashMap::new();
        evict_lru(&mut sessions); // must not panic
        assert!(sessions.is_empty());
    }

    #[test]
    fn idle_session_ids_selects_only_sessions_past_timeout() {
        let now = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert(
            "stale".to_string(),
            dummy_live_session(now.checked_sub(Duration::from_secs(600)).unwrap()),
        );
        sessions.insert("fresh".to_string(), dummy_live_session(now));

        let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
        assert_eq!(idle, vec!["stale".to_string()]);
    }

    #[test]
    fn idle_session_ids_empty_when_nothing_past_timeout() {
        let now = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert("fresh".to_string(), dummy_live_session(now));

        let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
        assert!(idle.is_empty());
    }

    #[test]
    fn idle_session_ids_exactly_at_timeout_boundary_is_not_evicted() {
        // 边界用 `>` 不是 `>=`（见 idle_session_ids 实现）——正好等于超时
        // 阈值的 session 这一轮还不驱逐，下一轮才会（因为到那时肯定 `>` 了）。
        let now = Instant::now();
        let mut sessions = HashMap::new();
        sessions.insert(
            "exactly_at_boundary".to_string(),
            dummy_live_session(now.checked_sub(Duration::from_secs(300)).unwrap()),
        );

        let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
        assert!(idle.is_empty(), "idle: {idle:?}");
    }
}

#[cfg(test)]
mod mcp_for_project_tests {
    use super::*;

    struct AllowAllTestPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for AllowAllTestPermission {
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

    /// A `SessionPool` with no MCP servers configured anywhere — every
    /// project's `mcp_for_project` connect resolves to an empty
    /// `McpManager` immediately (no subprocess, no network), which is all
    /// these tests need: they're about the *cache* around the connect, not
    /// the connect itself (that's `mcp_toy_server_smoke.rs`'s job).
    pub(super) async fn test_pool() -> (SessionPool, tempfile::TempDir) {
        test_pool_with_store(None).await
    }

    pub(super) async fn test_pool_with_store(
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
    ) -> (SessionPool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());
        let permission: Option<Arc<dyn Permission>> = Some(Arc::new(AllowAllTestPermission));
        let paths: Arc<dyn crate::config::DaemonPaths> = Arc::new(
            crate::config::StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        let client: Arc<dyn AnthropicClient> = Arc::new(model::mock::MockAnthropicClient::new());

        let pool = SessionPool::new(
            8,
            3600,
            client,
            Arc::new(Settings::defaults_for("claude-sonnet-4-6")),
            scene,
            permission,
            cwd,
            history_store,
            paths,
        );
        (pool, dir)
    }

    /// The watcher budget `crate::watch_hub` states: watched paths are
    /// `1 + M + P` — one global tier, one per scene, one per project — and
    /// not a multiple of the number of bindings.
    ///
    /// What this pins is the sharing: the tiers several projects have in
    /// common are registered once, so a binding only ever costs the paths
    /// that are its own. That a binding brings no `notify` streams of its
    /// own besides the hub's is not visible from here — the backend
    /// offers no count — and is held by construction instead: the catalogs a
    /// binding builds are the `unwatched` ones.
    #[tokio::test]
    async fn bindings_do_not_multiply_watchers() {
        let (pool, dir) = test_pool().await;
        let watched = |pool: &SessionPool| {
            pool.watch
                .as_ref()
                .expect("a hub in a test process")
                .watched_path_count()
        };

        // The tiers of the pool's own project: whatever of them exist on
        // disk are registered once each.
        pool.binding_for(Some(pool.cwd()), "code").await;
        let baseline = watched(&pool);

        // Three more projects, each with a settings file, a skills directory
        // and an agents directory of its own — nine more paths that exist,
        // and the shared global and scene tiers again.
        for name in ["one", "two", "three"] {
            let root = dir.path().join(name);
            std::fs::create_dir_all(root.join(".agix").join("agents")).unwrap();
            std::fs::create_dir_all(root.join(".agents").join("skills")).unwrap();
            std::fs::write(root.join(".agix").join("settings.json"), "{}").unwrap();
            pool.binding_for(Some(&root), "code").await;
        }

        let grown = watched(&pool) - baseline;
        assert_eq!(
            grown, 9,
            "three projects should add their own three paths each and re-register no shared one"
        );
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn cache_hit_returns_the_same_arc_without_reconnecting() {
        let (pool, _dir) = test_pool().await;
        let first = pool.mcp_for_project(Some(pool.cwd()), "code").await;
        let second = pool.mcp_for_project(Some(pool.cwd()), "code").await;
        assert!(
            Arc::ptr_eq(&first, &second),
            "a cache hit must return the exact cached Arc, not a freshly connected one"
        );
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn different_projects_get_independent_cache_entries() {
        let (pool, dir) = test_pool().await;
        let other_project = dir.path().join("other");
        std::fs::create_dir_all(&other_project).unwrap();

        let default_manager = pool.mcp_for_project(Some(pool.cwd()), "code").await;
        let other_manager = pool.mcp_for_project(Some(&other_project), "code").await;
        assert!(
            !Arc::ptr_eq(&default_manager, &other_manager),
            "a project seen for the first time must get its own cache miss and connect, \
             not silently inherit another project's connections"
        );

        let other_again = pool.mcp_for_project(Some(&other_project), "code").await;
        assert!(
            Arc::ptr_eq(&other_manager, &other_again),
            "the second lookup for the same non-default project must be a cache hit"
        );
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn no_project_key_is_cached_independently_of_the_default_project() {
        let (pool, _dir) = test_pool().await;
        let none_manager = pool.mcp_for_project(None, "code").await;
        let default_manager = pool.mcp_for_project(Some(pool.cwd()), "code").await;
        assert!(
            !Arc::ptr_eq(&none_manager, &default_manager),
            "the no-project tier (`None`) must not share a cache slot with the default project"
        );
    }

    /// A changed `Authorization` has to reach the next connection; reusing the
    /// cached one would keep sending the old token until a daemon restart.
    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn a_settings_change_drops_exactly_the_connections_built_from_it() {
        use crate::watch_hub::Tier;
        let (pool, dir) = test_pool().await;
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let default_coding = pool.mcp_for_project(Some(pool.cwd()), "code").await;
        let default_harnix = pool.mcp_for_project(Some(pool.cwd()), "harnix").await;
        let other_coding = pool.mcp_for_project(Some(&other), "code").await;

        pool.forget_mcp_built_from(&Tier::Project(other.clone()), None)
            .await;
        assert!(!Arc::ptr_eq(
            &other_coding,
            &pool.mcp_for_project(Some(&other), "code").await
        ));
        assert!(Arc::ptr_eq(
            &default_coding,
            &pool.mcp_for_project(Some(pool.cwd()), "code").await
        ));

        pool.forget_mcp_built_from(&Tier::Scene("harnix".into()), None)
            .await;
        assert!(!Arc::ptr_eq(
            &default_harnix,
            &pool.mcp_for_project(Some(pool.cwd()), "harnix").await
        ));
        assert!(Arc::ptr_eq(
            &default_coding,
            &pool.mcp_for_project(Some(pool.cwd()), "code").await
        ));

        let kept = (Some(pool.cwd().to_path_buf()), "code".to_string());
        pool.forget_mcp_built_from(&Tier::Global, Some(&kept)).await;
        assert!(Arc::ptr_eq(
            &default_coding,
            &pool.mcp_for_project(Some(pool.cwd()), "code").await
        ));
        assert_eq!(pool.mcp_by_project.lock().await.len(), 1);
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn lost_change_notifications_drop_every_other_pairs_connections() {
        let (pool, dir) = test_pool().await;
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other_coding = pool.mcp_for_project(Some(&other), "code").await;
        let default_harnix = pool.mcp_for_project(Some(pool.cwd()), "harnix").await;

        pool.reload_after_lost_changes().await;

        assert!(!Arc::ptr_eq(
            &other_coding,
            &pool.mcp_for_project(Some(&other), "code").await
        ));
        assert!(!Arc::ptr_eq(
            &default_harnix,
            &pool.mcp_for_project(Some(pool.cwd()), "harnix").await
        ));
    }

    fn is_stale(binding: &ProjectBinding) -> bool {
        binding.stale.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A project's settings changing is that project's business: the other
    /// projects' bindings, and the sessions built on them, stay as they are.
    #[tokio::test]
    async fn a_tier_change_invalidates_only_the_bindings_merged_from_it() {
        use crate::watch_hub::Tier;
        let (pool, dir) = test_pool().await;
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let own_coding = pool.binding_for(Some(pool.cwd()), "code").await;
        let own_harnix = pool.binding_for(Some(pool.cwd()), "harnix").await;
        let other_coding = pool.binding_for(Some(&other), "code").await;

        pool.invalidate_bindings(Some(&Tier::Project(other.clone())))
            .await;
        assert!(is_stale(&other_coding));
        assert!(!is_stale(&own_coding) && !is_stale(&own_harnix));
        assert!(Arc::ptr_eq(
            &own_coding,
            &pool.binding_for(Some(pool.cwd()), "code").await
        ));

        pool.invalidate_bindings(Some(&Tier::Scene("harnix".into())))
            .await;
        assert!(is_stale(&own_harnix));
        assert!(!is_stale(&own_coding));

        pool.invalidate_bindings(Some(&Tier::Global)).await;
        assert!(is_stale(&own_coding));
        assert!(pool.bindings.lock().await.is_empty());
    }

    /// A skill file added after a binding was built shows up in the catalog
    /// its sessions already hold, and nothing is marked for rebuilding.
    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn a_skill_change_reloads_the_shared_catalog_in_place() {
        use crate::watch_hub::{Tier, WatchTarget};
        let (pool, _dir) = test_pool().await;
        // Not the pool's startup scene: `test_pool`'s startup scene tier is
        // the tempdir root, which also holds the project.
        let binding = pool.binding_for(Some(pool.cwd()), "mini").await;
        let skill_dir = pool.cwd().join(".agents").join("skills").join("late");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: late\ndescription: added after the binding was built\n---\nbody\n",
        )
        .unwrap();
        assert!(binding.modules.skills().get("late").is_none());

        pool.refresh_catalogs(
            &Tier::Project(pool.cwd().to_path_buf()),
            WatchTarget::Skills,
        )
        .await;

        assert!(binding.modules.skills().get("late").is_some());
        assert!(!is_stale(&binding));
        assert!(Arc::ptr_eq(
            &binding,
            &pool.binding_for(Some(pool.cwd()), "mini").await
        ));
    }
}

/// Multiple scenes coexisting in one daemon process.
#[cfg(test)]
mod multi_scene_tests {
    use super::mcp_for_project_tests::test_pool;
    use super::*;

    /// Two scenes on one daemon must each read their own settings tier: a
    /// `harnix` session on a daemon started as `--scene code` runs on
    /// harnix's tier, not code's.
    #[tokio::test]
    async fn each_scene_resolves_its_own_settings_tier() {
        let (pool, dir) = test_pool().await;

        // `scenes/harnix/settings.json` sets a model the pool's own tier never
        // mentions — if the harnix session picks it up, it read harnix's tier.
        let harnix_tier = dir.path().join("scenes").join("harnix");
        std::fs::create_dir_all(&harnix_tier).unwrap();
        std::fs::write(
            harnix_tier.join("settings.json"),
            r#"{"model": {"model_name": "model-from-harnix-tier"}}"#,
        )
        .unwrap();

        let code = pool.settings_for_project(None, "code").await;
        let harnix = pool.settings_for_project(None, "harnix").await;

        assert_eq!(harnix.model.model_name, "model-from-harnix-tier");
        assert_ne!(
            code.model.model_name, harnix.model.model_name,
            "two scenes must not share one settings entry"
        );
        assert_eq!(
            harnix.paths.scope, "harnix",
            "scope must be the session's scene"
        );
        assert_eq!(code.paths.scope, "code");
    }

    /// The same project under two scenes gets two cache entries, and each is
    /// stable across calls.
    #[tokio::test]
    async fn settings_cache_is_keyed_by_project_and_scene_together() {
        let (pool, dir) = test_pool().await;
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();

        let a1 = pool.settings_for_project(Some(&other), "code").await;
        let a2 = pool.settings_for_project(Some(&other), "code").await;
        let b = pool.settings_for_project(Some(&other), "harnix").await;

        assert!(
            Arc::ptr_eq(&a1, &a2),
            "same (project, scene) must be cached"
        );
        assert!(
            !Arc::ptr_eq(&a1, &b),
            "one project under two scenes must not collapse to one entry"
        );
        assert_eq!(a1.paths.scope, "code");
        assert_eq!(b.paths.scope, "harnix");
    }

    /// The daemon's own scene keeps the injected `config_root()`; any other
    /// scene is derived under `global_root()/scenes/`.
    ///
    /// Tests inject a `StaticDaemonPaths` whose config root is a tempdir that
    /// bears no relation to `global_root()/scenes/<id>`, so recomputing the
    /// startup scene's directory would move its settings out from under them.
    #[tokio::test]
    async fn the_startup_scene_keeps_its_injected_config_root() {
        let (pool, _dir) = test_pool().await;

        assert_eq!(
            pool.scene_root("code", "code"),
            pool.paths.config_root(),
            "the startup scene must not be recomputed"
        );
        assert_eq!(
            pool.scene_root("harnix", "code"),
            pool.paths.global_root().join("scenes").join("harnix"),
            "another scene is derived the way DefaultDaemonPaths derives one"
        );
    }

    /// A scene that was never activated must be refused, not silently served
    /// by the daemon's default.
    #[tokio::test]
    async fn creating_a_session_in_an_inactive_scene_is_refused() {
        let (pool, _dir) = test_pool().await;
        assert!(
            pool.resolve_scene(Some("mini")).await.is_none(),
            "mini was never activated on this pool"
        );
        assert!(
            pool.resolve_scene(Some("harnix")).await.is_none(),
            "harnix was never activated either"
        );
        assert!(
            pool.resolve_scene(None).await.is_some(),
            "no scene named falls back to the daemon's own"
        );
    }

    /// `scene.activate` makes a second scene usable in the same process, and
    /// sessions created under each are recorded under their own scene.
    #[tokio::test]
    async fn two_scenes_can_be_active_in_one_process() {
        let (pool, _dir) = test_pool().await;

        pool.activate_scene("harnix")
            .await
            .expect("activate harnix");

        let code = pool.resolve_scene(Some("code")).await.expect("code");
        let harnix = pool.resolve_scene(Some("harnix")).await.expect("harnix");
        assert_eq!(code.id(), "code");
        assert_eq!(harnix.id(), "harnix");

        // Distinct scenes, so distinct tool surfaces — this is what makes
        // running both in one process meaningful rather than cosmetic.
        assert!(code.tools().is_empty(), "code allows every registered tool");
        assert!(
            !harnix.tools().is_empty(),
            "harnix restricts its tools to a whitelist"
        );
    }
}

#[cfg(test)]
mod set_model_tests {
    use super::mcp_for_project_tests::test_pool;
    use super::*;

    /// The engine's main loop is inside `process_turn` while a turn runs and
    /// is not reading its input channel. A command accepted here would sit in
    /// the queue and take effect at a moment nobody announced — the caller
    /// told the model had changed, watching the old one answer.
    #[tokio::test]
    async fn a_session_with_a_turn_in_flight_refuses_rather_than_queues() {
        let (pool, _dir) = test_pool().await;
        let session_id = pool
            .create_session(None, ProjectSelector::Default, None)
            .await
            .expect("a session")["session_id"]
            .as_str()
            .expect("a session id")
            .to_string();

        {
            let mut sessions = pool.sessions.lock().await;
            sessions
                .get_mut(&session_id)
                .expect("the session is live")
                .current_turn = Some("t1".into());
        }

        match pool
            .set_session_model(&session_id, Some("claude-opus-4-6"), None)
            .await
        {
            Err(SetModelError::Busy {
                current_turn_id, ..
            }) => {
                assert_eq!(
                    current_turn_id.as_deref(),
                    Some("t1"),
                    "the refusal names the turn holding it"
                )
            }
            Err(SetModelError::Rpc((code, message))) => {
                panic!("expected a busy refusal, got {code}: {message}")
            }
            Ok(v) => panic!("a busy session accepted a model change: {v}"),
        }

        assert!(
            pool.lock_specs()
                .get(&session_id)
                .and_then(|spec| spec.model.clone())
                .is_none(),
            "a refused change must not be recorded — a later rebuild would apply it"
        );
    }
}

#[cfg(test)]
mod with_session_lock_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct AllowAllTestPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for AllowAllTestPermission {
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

    async fn test_pool() -> (Arc<SessionPool>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());
        let permission: Option<Arc<dyn Permission>> = Some(Arc::new(AllowAllTestPermission));
        let paths: Arc<dyn crate::config::DaemonPaths> = Arc::new(
            crate::config::StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        let client: Arc<dyn AnthropicClient> = Arc::new(model::mock::MockAnthropicClient::new());

        let pool = Arc::new(SessionPool::new(
            8,
            3600,
            client,
            Arc::new(Settings::defaults_for("claude-sonnet-4-6")),
            scene,
            permission,
            cwd,
            None,
            paths,
        ));
        (pool, dir)
    }

    /// `session.close`'s cascade delete and a concurrent
    /// `session.resume(..., create_if_missing: true)` on the same sidechain id
    /// must not interleave. This drives many concurrent critical sections for
    /// the *same* id and asserts they never overlap.
    #[tokio::test]
    async fn concurrent_calls_for_the_same_id_never_overlap() {
        let (pool, _dir) = test_pool().await;
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            let concurrent = concurrent.clone();
            let max_concurrent = max_concurrent.clone();
            tasks.push(tokio::spawn(async move {
                pool.with_session_lock("shared-id", async {
                    let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "critical sections for the same session_id must never overlap"
        );
    }

    /// The flip side: locking must be per-id, not a single pool-wide mutex —
    /// otherwise every unrelated `session.close`/`.resume` pair would
    /// serialize against each other too.
    #[tokio::test]
    async fn distinct_ids_run_concurrently() {
        let (pool, _dir) = test_pool().await;
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(4));

        let mut tasks = Vec::new();
        for i in 0..4 {
            let pool = pool.clone();
            let concurrent = concurrent.clone();
            let max_concurrent = max_concurrent.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                pool.with_session_lock(&format!("id-{i}"), async {
                    barrier.wait().await;
                    let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        assert!(
            max_concurrent.load(Ordering::SeqCst) > 1,
            "distinct session ids must not serialize against each other"
        );
    }

    #[tokio::test]
    async fn registry_entry_is_cleaned_up_after_use() {
        let (pool, _dir) = test_pool().await;
        pool.with_session_lock("temp-id", async {}).await;
        assert!(
            pool.session_locks.lock().await.is_empty(),
            "a lock nothing else references must not linger in the registry forever"
        );
    }
}

/// `session.list`'s historical half is a `HistoryStore` query, not a walk the
/// pool does itself.
#[cfg(test)]
mod session_listing_tests {
    use super::mcp_for_project_tests::test_pool_with_store;
    use super::*;
    use history::entry::{EnvelopedEntry, LogEntry};
    use history::error::HistoryError;
    use history::query::SessionQuery;
    use history::store::{HistoryStore, InMemoryHistoryStore, SessionSummary};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A store that answers "which sessions are there" from an index it keeps
    /// as entries arrive, and counts every fall-through to the enumeration
    /// the default would use.
    struct IndexedStore {
        inner: InMemoryHistoryStore,
        index: std::sync::Mutex<Vec<base::session::SessionId>>,
        enumerations: AtomicUsize,
    }

    impl IndexedStore {
        fn new() -> Self {
            Self {
                inner: InMemoryHistoryStore::new(),
                index: std::sync::Mutex::new(Vec::new()),
                enumerations: AtomicUsize::new(0),
            }
        }

        fn indexed_order(&self) -> Vec<base::session::SessionId> {
            self.index.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HistoryStore for IndexedStore {
        async fn append(
            &self,
            session: base::session::SessionId,
            entry: LogEntry,
        ) -> Result<(), HistoryError> {
            {
                let mut index = self.index.lock().unwrap();
                index.retain(|s| *s != session);
                index.insert(0, session);
            }
            self.inner.append(session, entry).await
        }

        async fn load(
            &self,
            session: base::session::SessionId,
        ) -> Result<Vec<EnvelopedEntry>, HistoryError> {
            self.inner.load(session).await
        }

        async fn list_sessions(&self) -> Result<Vec<base::session::SessionId>, HistoryError> {
            self.enumerations.fetch_add(1, Ordering::SeqCst);
            self.inner.list_sessions().await
        }

        async fn delete(&self, session: base::session::SessionId) -> Result<(), HistoryError> {
            self.inner.delete(session).await
        }

        async fn find_sessions(
            &self,
            query: &SessionQuery,
        ) -> Result<Vec<SessionSummary>, HistoryError> {
            Ok(self
                .indexed_order()
                .into_iter()
                .take(query.limit)
                .map(|session_id| SessionSummary {
                    session_id,
                    last_modified: "1970-01-01T00:00:00Z".into(),
                    entry_count: 0,
                    message_count: 0,
                    preview: String::new(),
                    canonical_cwd: None,
                    title: None,
                    total_input_tokens: None,
                    total_output_tokens: None,
                    compact_count: 0,
                })
                .collect())
        }
    }

    async fn seed(store: &IndexedStore, count: usize) -> Vec<base::session::SessionId> {
        let mut ids = Vec::new();
        for i in 0..count {
            let sid = base::session::SessionId::new();
            store
                .append(
                    sid,
                    LogEntry::User {
                        content: vec![base::message::ContentBlock::Text {
                            text: format!("session {i}"),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
            ids.push(sid);
        }
        ids
    }

    /// A session on disk under a project no session in this process has
    /// touched: `session.resume` finds it by id, and so must `session.get`.
    #[tokio::test]
    async fn session_get_finds_a_cold_session_of_a_project_not_served_yet() {
        let store: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
        let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
        let sid = base::session::SessionId::new();
        store
            .append(
                sid,
                LogEntry::Meta {
                    cwd: "/elsewhere/proj".into(),
                    started_at: time::OffsetDateTime::now_utc(),
                    model: "m".into(),
                    permission_mode: "default".into(),
                    engine_version: "0".into(),
                    agix_version: "0".into(),
                    parent_session_id: None,
                    scene: Some("code".into()),
                    project_root: Some("/elsewhere/proj".into()),
                    session_kind: history::entry::SessionKind::Primary,
                    schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
                },
            )
            .await
            .unwrap();

        let detail = pool.session_detail(&sid.to_string()).await.unwrap();
        assert_eq!(detail["project_root"], "/elsewhere/proj");
        assert_eq!(detail["status"], "inactive");

        let missing = base::session::SessionId::new().to_string();
        assert_eq!(
            pool.session_detail(&missing).await.unwrap_err().0,
            codes::SESSION_NOT_FOUND
        );
    }

    /// The projects `session.list` covers are the ones sessions were built
    /// under, and a settings change does not make the pool forget them.
    #[tokio::test]
    async fn served_projects_outlive_a_settings_change() {
        let (pool, dir) = test_pool_with_store(None).await;
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        pool.note_served(Some(&other));
        pool.invalidate_bindings(None).await;
        let other = std::fs::canonicalize(&other).unwrap();
        assert!(pool.served_projects().await.contains(&Some(other)));
    }

    /// A project served under two spellings of its path — a symlink, or
    /// macOS's `/var` against `/private/var` — is one project, and each of
    /// its cold sessions is listed once.
    #[tokio::test]
    async fn a_project_served_under_two_spellings_lists_each_session_once() {
        let state = tempfile::tempdir().unwrap();
        let store: Arc<dyn HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                state.path(),
                history::path::HistoryRoots::under(state.path()),
            )
            .await
            .unwrap(),
        );
        let (pool, dir) = test_pool_with_store(Some(store.clone())).await;
        let work = dir.path().join("work");
        let alias = dir.path().join("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&work, &alias).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&work, &alias).unwrap();

        let sid = base::session::SessionId::new();
        store
            .append(
                sid,
                LogEntry::Meta {
                    cwd: alias.display().to_string(),
                    started_at: time::OffsetDateTime::now_utc(),
                    model: "m".into(),
                    permission_mode: "default".into(),
                    engine_version: "0".into(),
                    agix_version: "0".into(),
                    parent_session_id: None,
                    scene: Some("code".into()),
                    project_root: Some(alias.display().to_string()),
                    session_kind: history::entry::SessionKind::Primary,
                    schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
                },
            )
            .await
            .unwrap();
        pool.note_served(Some(&alias));

        let listed: Vec<String> = pool
            .list_all(false, None)
            .await
            .into_iter()
            .map(|info| info.session_id)
            .collect();
        assert_eq!(listed, vec![sid.to_string()]);
    }

    #[tokio::test]
    async fn a_store_with_an_index_answers_session_list_without_being_enumerated() {
        let store = Arc::new(IndexedStore::new());
        seed(&store, 3).await;
        let expected: Vec<String> = store
            .indexed_order()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
        let listed: Vec<String> = pool
            .list_all(false, None)
            .await
            .into_iter()
            .map(|info| info.session_id)
            .collect();

        assert_eq!(
            listed, expected,
            "the index's answer, in the index's order, is what the RPC reports"
        );
        assert_eq!(
            store.enumerations.load(Ordering::SeqCst),
            0,
            "a backend that answered `find_sessions` must not also be walked session by session"
        );
    }

    /// The fields a `SessionSummary` could fill in stay empty for a session
    /// that is only on disk — clients have always read them that way. The
    /// identity fields are a different question and answered separately (see
    /// `SessionFacts`); this seeded session has no `Meta` line at all, so
    /// they are `null` rather than invented.
    #[tokio::test]
    async fn an_inactive_session_reports_the_same_empty_summary_it_always_has() {
        let store = Arc::new(IndexedStore::new());
        let ids = seed(&store, 1).await;

        let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
        let listed = pool.list_all(false, None).await;
        assert_eq!(listed.len(), 1);

        assert_eq!(
            serde_json::to_value(&listed[0]).unwrap(),
            serde_json::json!({
                "session_id": ids[0].to_string(),
                "name": null,
                "preview": null,
                "message_count": 0,
                "created_at": "",
                "last_active": "",
                "status": "inactive",
                "session_kind": "primary",
                "scene": null,
                "project_root": null,
                "compact_count": 0,
                "resumable": true,
            })
        );
    }
}

#[cfg(test)]
mod rebuild_tests {
    use super::mcp_for_project_tests::test_pool_with_store;
    use super::*;
    use base::context::goal::TurnOrigin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn pool_with_history() -> (SessionPool, tempfile::TempDir, tempfile::TempDir) {
        let store_dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn history::store::HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                store_dir.path(),
                history::path::HistoryRoots::under(store_dir.path()),
            )
            .await
            .unwrap(),
        );
        let (pool, dir) = test_pool_with_store(Some(store)).await;
        (pool, dir, store_dir)
    }

    async fn new_session(pool: &SessionPool) -> String {
        pool.create_session(None, ProjectSelector::Default, None)
            .await
            .expect("a session")["session_id"]
            .as_str()
            .expect("a session id")
            .to_string()
    }

    async fn slot_and_state(
        pool: &SessionPool,
        sid: &str,
    ) -> (
        Arc<AsyncMutex<Option<EventReceiver>>>,
        Arc<base::context::SessionState>,
    ) {
        let sessions = pool.sessions.lock().await;
        let live = sessions.get(sid).expect("the session is live");
        (live.event_rx.clone(), live.state.clone().expect("a state"))
    }

    fn append(prompt: &mut runtime::agent::SessionPrompt) {
        prompt.append = Some("Answer briefly.".into());
    }

    #[tokio::test]
    async fn a_prompt_change_is_refused_once_a_turn_holds_the_slot_even_before_it_is_named() {
        let (pool, _dir, _store) = pool_with_history().await;
        let sid = new_session(&pool).await;
        let (slot, _) = slot_and_state(&pool, &sid).await;
        let held = slot.lock().await.take().expect("an idle session");

        match pool.set_session_prompt(&sid, append).await {
            Err(SetModelError::Busy {
                current_turn_id, ..
            }) => assert_eq!(current_turn_id, None),
            Err(SetModelError::Rpc((code, message))) => panic!("{code}: {message}"),
            Ok(v) => panic!("a rebuild cut off a starting turn: {v}"),
        }
        let (still, _) = slot_and_state(&pool, &sid).await;
        assert!(
            Arc::ptr_eq(&slot, &still),
            "the engine was replaced under the turn"
        );

        *slot.lock().await = Some(held);
        pool.set_session_prompt(&sid, append)
            .await
            .unwrap_or_else(|_| panic!("an idle session takes the change"));
        let (rebuilt, _) = slot_and_state(&pool, &sid).await;
        assert!(!Arc::ptr_eq(&slot, &rebuilt), "an idle session is rebuilt");
    }

    #[tokio::test]
    async fn a_prompt_set_on_a_session_out_of_memory_leaves_no_spec_behind() {
        let (pool, _dir, _store) = pool_with_history().await;
        let sid = new_session(&pool).await;
        let live = pool.sessions.lock().await.remove(&sid).unwrap();
        pool.evict(&sid, &live);
        assert!(!pool.lock_specs().contains_key(&sid));

        let answer = pool
            .set_session_prompt(&sid, append)
            .await
            .unwrap_or_else(|_| panic!("a session out of memory takes the change"));
        assert_eq!(answer["append"], "Answer briefly.");
        assert!(!pool.lock_specs().contains_key(&sid));
        assert_eq!(
            pool.recorded_spec(&sid).await.prompt.and_then(|p| p.append),
            Some("Answer briefly.".to_string()),
            "the log gives the change back"
        );
    }

    #[tokio::test]
    async fn a_rebuild_keeps_the_goal_armed_and_the_round_it_was_about_to_start() {
        let (pool, _dir, _store) = pool_with_history().await;
        let sid = new_session(&pool).await;
        let (_, before) = slot_and_state(&pool, &sid).await;
        before
            .goal_create("keep going".into(), Some(3), TurnOrigin::Human)
            .unwrap();
        pool.record_goal_changes(&sid, &before).await;
        let round = before.goal_next_round().expect("a round");

        pool.set_session_prompt(&sid, append)
            .await
            .unwrap_or_else(|_| panic!("an idle session takes the change"));
        let (_, after) = slot_and_state(&pool, &sid).await;
        assert!(!Arc::ptr_eq(&before, &after), "the session was rebuilt");
        assert_eq!(after.goal_view()["armed"], true, "{}", after.goal_view());
        assert!(after.goal_claim_round(&round.turn_id));
    }

    #[tokio::test]
    async fn a_rebuild_moves_the_sessions_watchers_and_open_questions_along() {
        let (pool, _dir, _store) = pool_with_history().await;
        let sid = new_session(&pool).await;
        let watcher = Arc::new(crate::rpc::Client::new(
            crate::rpc::ConnectionId(u64::MAX - 7),
            Arc::new(Discard),
        ));
        pool.subscribe_session(&sid, watcher.clone()).await.unwrap();
        pool.remember_prompt(
            &sid,
            &("p1".to_string(), Some("task-1".to_string())),
            &serde_json::json!({"kind": "prompt"}),
        )
        .await;

        pool.set_session_prompt(&sid, append)
            .await
            .unwrap_or_else(|_| panic!("an idle session takes the change"));
        let sessions = pool.sessions.lock().await;
        let live = sessions.get(&sid).unwrap();
        assert!(live.subscribers.contains_key(&watcher.id()));
        assert!(live.pending_prompts.contains_key("p1"));
    }

    #[tokio::test]
    async fn eviction_keeps_only_the_specs_a_log_cannot_give_back() {
        let (pool, _dir, _store) = pool_with_history().await;
        let logged = new_session(&pool).await;
        let recording = new_session(&pool).await;
        pool.lock_specs().get_mut(&recording).unwrap().recorder =
            Some(telemetry::recorder::Recorder::new());
        for sid in [&logged, &recording] {
            let live = pool.sessions.lock().await.remove(sid).unwrap();
            pool.evict(sid, &live);
        }
        assert!(!pool.lock_specs().contains_key(&logged));
        assert!(pool.lock_specs().contains_key(&recording));

        let unlogged: Vec<String> = (0..pool.cap).map(|i| format!("no log {i}")).collect();
        for sid in &unlogged {
            pool.lock_specs()
                .insert(sid.clone(), SessionSpec::default());
            pool.release_spec(sid);
        }
        assert!(
            !pool.lock_specs().contains_key(&recording),
            "the oldest kept spec makes room"
        );
        assert!(unlogged
            .iter()
            .all(|sid| pool.lock_specs().contains_key(sid)));
        assert_eq!(pool.lock_kept_specs().len(), pool.cap);
    }

    #[tokio::test]
    async fn two_callers_reviving_one_cold_session_build_one_engine() {
        let (pool, _dir, _store) = pool_with_history().await;
        let builds = Arc::new(AtomicUsize::new(0));
        let counted = builds.clone();
        let pool = pool.with_session_customizer(Arc::new(move |builder, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            builder
        }));
        let sid = new_session(&pool).await;
        pool.shutdown_session(&sid).await;
        assert!(!pool.sessions.lock().await.contains_key(&sid));

        let (a, b) = tokio::join!(pool.ensure_live(&sid, None), pool.ensure_live(&sid, None));
        assert_eq!((a.unwrap(), b.unwrap()), (sid.clone(), sid.clone()));
        assert_eq!(
            builds.load(Ordering::SeqCst),
            2,
            "one build to create, one to revive"
        );
        let (_, state) = slot_and_state(&pool, &sid).await;
        assert!(state.goal_view().is_null());
    }

    #[tokio::test]
    async fn a_background_tasks_question_outlives_the_turn_and_goes_with_its_task() {
        let (pool, _dir, _store) = pool_with_history().await;
        let sid = new_session(&pool).await;
        let (slot, _) = slot_and_state(&pool, &sid).await;
        let rx = slot.lock().await.take().unwrap();
        pool.sessions
            .lock()
            .await
            .get_mut(&sid)
            .unwrap()
            .current_turn = Some("t1".into());
        pool.remember_prompt(&sid, &("own".into(), None), &serde_json::json!({}))
            .await;
        pool.remember_prompt(
            &sid,
            &("background".into(), Some("task-1".into())),
            &serde_json::json!({}),
        )
        .await;

        assert!(pool.release_turn_slot(&sid, &slot, rx, "t1").await);
        assert_eq!(
            pending_ids(&pool, &sid).await,
            vec!["background".to_string()]
        );
        pool.forget_prompts_of_task(&sid, "task-1").await;
        assert!(pending_ids(&pool, &sid).await.is_empty());
    }

    async fn pending_ids(pool: &SessionPool, sid: &str) -> Vec<String> {
        let sessions = pool.sessions.lock().await;
        sessions[sid].pending_prompts.keys().cloned().collect()
    }
}

#[cfg(test)]
mod auto_name_tests {
    use super::mcp_for_project_tests::test_pool;
    use super::*;
    use base::interface::model::{
        call_purpose, Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams,
        ToolDef, Usage,
    };
    use base::interface::prompt::PromptBlock;
    use base::provider::{ApiType, ResolvedModel, TaskRouter};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers every call: a naming call with `title`, anything else with a
    /// line of text. Remembers what each call was for and which model it
    /// asked for.
    struct Answering {
        title: &'static str,
        calls: std::sync::Mutex<Vec<(Option<String>, String)>>,
    }

    impl Answering {
        fn new(title: &'static str) -> Arc<Self> {
            Arc::new(Self {
                title,
                calls: Default::default(),
            })
        }

        fn naming_calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(purpose, _)| purpose.as_deref() == Some(call_purpose::TITLE))
                .map(|(_, model)| model.clone())
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl Model for Answering {
        fn api_type(&self) -> ApiType {
            ApiType::Anthropic
        }

        async fn stream(
            &self,
            _: Vec<PromptBlock>,
            _: Vec<ToolDef>,
            _: Vec<ModelMessage>,
            params: StreamParams,
            _: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            let purpose = params
                .origin
                .as_ref()
                .and_then(|o| o.purpose())
                .map(str::to_string);
            let text = if purpose.as_deref() == Some(call_purpose::TITLE) {
                self.title
            } else {
                "好的，这就整理。"
            };
            self.calls.lock().unwrap().push((purpose, params.model));
            let events = vec![
                Ok(ModelEvent::TextDelta { text: text.into() }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Usage::default(),
                }),
            ];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// What a replay must never reach: fails every call, and counts them.
    #[derive(Default)]
    struct Unreachable {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Model for Unreachable {
        fn api_type(&self) -> ApiType {
            ApiType::Anthropic
        }

        async fn stream(
            &self,
            _: Vec<PromptBlock>,
            _: Vec<ToolDef>,
            _: Vec<ModelMessage>,
            _: StreamParams,
            _: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ModelError::Auth("invalid x-api-key".into()))
        }
    }

    /// Serve `project` on `scene` with `model`, and `task_router` when given,
    /// in place of what its settings would build.
    async fn bind(
        pool: &SessionPool,
        project: &Path,
        scene: &str,
        model: Arc<dyn Model>,
        task_router: Option<TaskRouter>,
    ) {
        let mut settings = (**pool.settings.read().await).clone();
        settings.memory_enabled = false;
        let mut binding = ProjectBinding::build(
            Arc::new(settings),
            &[],
            vec![],
            &pool.client,
            None,
            &pool.credentials,
        );
        binding.model = model;
        binding.task_router = task_router.map(Arc::new);
        pool.bindings.lock().await.insert(
            (Some(project.to_path_buf()), scene.to_string()),
            Arc::new(binding),
        );
    }

    fn recorder(mode: &str, root: &Path) -> Option<crate::rpc::RecorderOptions> {
        Some(crate::rpc::RecorderOptions {
            mode: mode.into(),
            name: Some("naming".into()),
            dir: Some(root.display().to_string()),
            strict: mode == "replay",
        })
    }

    async fn harnix_session(
        pool: &SessionPool,
        project: &Path,
        recorder: Option<crate::rpc::RecorderOptions>,
    ) -> String {
        pool.activate_scene("harnix")
            .await
            .expect("harnix is built in");
        let options = SessionOptions {
            recorder,
            ..Default::default()
        };
        pool.create_session(
            Some("harnix"),
            ProjectSelector::Path(project.to_path_buf()),
            Some(&options),
        )
        .await
        .expect("a harnix session")["session_id"]
            .as_str()
            .expect("a session id")
            .to_string()
    }

    /// The first turn's `session.send` result.
    async fn first_turn(pool: &SessionPool, sid: &str) -> serde_json::Value {
        let client = Arc::new(crate::rpc::Client::new(
            crate::rpc::ConnectionId(u64::MAX - 11),
            Arc::new(Discard),
        ));
        let response = pool
            .run_turn(
                Some(sid.to_string()),
                "把发布流程整理成一份清单".into(),
                vec![],
                "t-1".into(),
                client,
                serde_json::json!(1),
                None,
                OnBusy::Reject,
                "client",
            )
            .await;
        response
            .result
            .unwrap_or_else(|| panic!("the turn failed: {:?}", response.error))
    }

    fn naming_calls_in(root: &Path) -> Vec<String> {
        telemetry::recorder::reader::load(&root.join("naming"))
            .expect("a recording")
            .calls
            .into_iter()
            .filter(|c| c.request.purpose.as_deref() == Some(call_purpose::TITLE))
            .map(|c| c.request.params.model)
            .collect()
    }

    #[tokio::test]
    async fn a_replay_whose_recording_never_named_leaves_the_session_unnamed_without_calling_out() {
        let (pool, dir) = test_pool().await;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let recordings = dir.path().join("recordings");

        bind(&pool, &project, "harnix", Answering::new("发布清单"), None).await;
        let recorded = harnix_session(&pool, &project, recorder("record", &recordings)).await;
        pool.sessions
            .lock()
            .await
            .get_mut(&recorded)
            .unwrap()
            .named_by_user = true;
        first_turn(&pool, &recorded).await;
        assert!(naming_calls_in(&recordings).is_empty());

        let unreachable = Arc::new(Unreachable::default());
        bind(&pool, &project, "harnix", unreachable.clone(), None).await;
        bind(&pool, pool.cwd(), "code", unreachable.clone(), None).await;
        let replayed = harnix_session(&pool, &project, recorder("replay", &recordings)).await;
        let result = first_turn(&pool, &replayed).await;

        assert_eq!(result["name"], serde_json::Value::Null, "{result}");
        assert_eq!(unreachable.calls.load(Ordering::SeqCst), 0);
        let namer = pool.sessions.lock().await[&replayed].namer.clone();
        assert_eq!(
            namer.name("把发布流程整理成一份清单").await,
            Ok(None),
            "a naming call the recording lacks is skipped, not reported as a failure"
        );
        assert_eq!(unreachable.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_recorded_name_comes_back_on_replay_without_calling_out() {
        let (pool, dir) = test_pool().await;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let recordings = dir.path().join("recordings");

        bind(&pool, &project, "harnix", Answering::new("发布清单"), None).await;
        let recorded = harnix_session(&pool, &project, recorder("record", &recordings)).await;
        let result = first_turn(&pool, &recorded).await;
        assert_eq!(result["name"], "发布清单", "{result}");
        assert_eq!(naming_calls_in(&recordings).len(), 1);

        let unreachable = Arc::new(Unreachable::default());
        bind(&pool, &project, "harnix", unreachable.clone(), None).await;
        bind(&pool, pool.cwd(), "code", unreachable.clone(), None).await;
        let replayed = harnix_session(&pool, &project, recorder("replay", &recordings)).await;
        let result = first_turn(&pool, &replayed).await;

        assert_eq!(result["name"], "发布清单", "{result}");
        assert_eq!(unreachable.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_session_is_named_through_its_own_projects_title_route() {
        let (pool, dir) = test_pool().await;
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let main = Answering::new("不该用这个");
        let titler = Answering::new("发布清单");
        let router = TaskRouter::new(
            HashMap::from([
                ("main".to_string(), main.clone() as Arc<dyn Model>),
                ("titler".to_string(), titler.clone() as Arc<dyn Model>),
            ]),
            HashMap::from([(
                call_purpose::TITLE.to_string(),
                ResolvedModel {
                    provider_id: "titler".into(),
                    model: "title-model".into(),
                },
            )]),
            main.clone(),
        );
        bind(&pool, &project, "harnix", main.clone(), Some(router)).await;
        let sid = harnix_session(&pool, &project, None).await;
        let result = first_turn(&pool, &sid).await;

        assert_eq!(result["name"], "发布清单", "{result}");
        assert_eq!(titler.naming_calls(), ["title-model"]);
        assert!(main.naming_calls().is_empty());
    }
}
