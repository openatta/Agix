//! Tool trait. All tools implement this single trait.

use crate::context::config::NetworkModeConfig;
use crate::error::ToolError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Cross-crate, data-only view of the sandbox policy a tool should apply.
/// Mirrors `context::config::SandboxPolicyConfig` field-for-field; `tools`
/// converts it into its own `bash::sandbox::SandboxPolicy` at the call site.
#[derive(Debug, Clone, Default)]
pub struct SandboxSettings {
    pub allow_read: Vec<PathBuf>,
    pub deny_read: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
    pub network_mode: NetworkModeConfig,
    /// See `base::settings::SandboxConfig::deny_write`.
    pub deny_write: Vec<String>,
    /// See `base::settings::SandboxConfig::allow_write`.
    pub allow_write: Vec<PathBuf>,
    /// See `base::context::config::SandboxPolicyConfig::paths`.
    pub paths: Arc<crate::paths::SensitivePaths>,
    /// See `base::settings::SandboxConfig::require_enforcement`.
    pub require_enforcement: bool,
    /// See `base::settings::SandboxConfig::unattended_requires_network_policy`.
    pub unattended_requires_network_policy: bool,
}
pub use crate::context::config::PathStyle;
use crate::interface::permission::{ModeStance, PermissionFacts};
pub use crate::interface::permission::{PermissionContentKind, ToolPermissionSpec};
pub use crate::permission::PermissionMode;
#[derive(Debug, Clone)]
pub struct PromptContext {
    pub cwd: PathBuf,
    pub model: String,
    pub session_id: String,
    pub is_interactive: bool,
    pub all_tool_names: Vec<String>,
    pub allowed_agent_types: Vec<String>,
    pub path_style: PathStyle,
}
impl Default for PromptContext {
    fn default() -> Self {
        Self {
            cwd: PathBuf::new(),
            model: String::new(),
            session_id: String::new(),
            is_interactive: false,
            all_tool_names: vec![],
            allowed_agent_types: vec![],
            path_style: PathStyle::default(),
        }
    }
}
pub trait SnapshotFile: Send + Sync + std::fmt::Debug {
    fn record(&self, p: &std::path::Path, n: &str);
}
pub trait EffectsCallback: Send + Sync + std::fmt::Debug {
    fn append_system_message(&self, k: &str, c: &str);
    fn os_notify(&self, _m: &str, _k: &str) {}
}
pub trait RunningTasksCallback: Send + Sync + std::fmt::Debug {
    fn find(&self, tid: &str) -> Option<(String, Vec<String>, crate::context::RunningStatus)>;
    fn cancel(&self, tid: &str) -> bool;
}

/// One progress report of a running tool call. `progress` and `total` are in
/// whatever unit the tool counts (lines, bytes, steps); either may be absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgressUpdate {
    pub message: String,
    pub progress: Option<f64>,
    pub total: Option<f64>,
}

impl ProgressUpdate {
    pub fn text(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }
}

/// What a tool call reports progress through. The engine hands every call
/// one that reaches the session's event stream as `AgentEvent::ToolProgress`,
/// rate-limited per call; reports after the call returns are dropped.
#[derive(Clone)]
pub struct ProgressSender {
    tool_use_id: String,
    callback: Option<Arc<dyn ProgressCallback>>,
}
pub trait ProgressCallback: Send + Sync {
    fn on_progress(&self, tool_use_id: &str, data: &str);
    fn on_update(&self, tool_use_id: &str, update: ProgressUpdate) {
        self.on_progress(tool_use_id, &update.message);
    }
}
impl ProgressSender {
    pub fn noop(id: impl Into<String>) -> Self {
        Self {
            tool_use_id: id.into(),
            callback: None,
        }
    }
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            tool_use_id: id.into(),
            callback: None,
        }
    }
    pub fn with_callback(id: impl Into<String>, cb: Arc<dyn ProgressCallback>) -> Self {
        Self {
            tool_use_id: id.into(),
            callback: Some(cb),
        }
    }
    pub fn send(&self, data: &str) {
        if let Some(ref cb) = self.callback {
            cb.on_progress(&self.tool_use_id, data);
        }
    }
    pub fn update(&self, update: ProgressUpdate) {
        if let Some(ref cb) = self.callback {
            cb.on_update(&self.tool_use_id, update);
        }
    }
    pub fn tool_use_id(&self) -> &str {
        &self.tool_use_id
    }
}

#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Allow {
        decision_reason: Option<String>,
    },
    Deny {
        reason: Option<String>,
        decision_reason: Option<String>,
    },
    Ask {
        message: String,
        decision_reason: Option<String>,
    },
}
impl PermissionDecision {
    pub fn allow() -> Self {
        Self::Allow {
            decision_reason: None,
        }
    }
    pub fn deny(r: impl Into<String>) -> Self {
        Self::Deny {
            reason: Some(r.into()),
            decision_reason: None,
        }
    }
    pub fn ask(m: impl Into<String>) -> Self {
        Self::Ask {
            message: m.into(),
            decision_reason: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ValidationResult {
    Ok,
    Err(String, i32),
}
impl ValidationResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
    pub fn is_err(&self) -> bool {
        matches!(self, Self::Err(..))
    }
    pub fn err(m: impl Into<String>, c: i32) -> Self {
        Self::Err(m.into(), c)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolResult {
    pub content: ToolResultContent,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_meta: Option<McpMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_messages: Option<Vec<Value>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpMeta {
    #[serde(default)]
    pub server_name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}
impl Default for ToolResultContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: Option<String>,
    pub source: Option<Value>,
}

/// Largest image, decoded, that goes to the model as one image block. The API
/// refuses an image over 5 MB of base64 — 3.75 MB decoded — and the refused
/// request would carry it again on every later turn; rounded down to leave
/// room for the rest of the request.
pub const MAX_IMAGE_BYTES: u64 = 3_500_000;

/// Constructors/accessors for the two block shapes the model layer understands.
///
/// The `source` object of an image block is the Anthropic `image.source` shape
/// (`{type: "base64", media_type, data}`) verbatim — MCP servers already hand us
/// that shape, and the runtime's tool-result renderer reads it back out with
/// [`ToolResultBlock::as_image`]. Building it through these two functions keeps
/// the producer (`Read`, the MCP adapter) and the consumer from drifting apart.
impl ToolResultBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            block_type: "text".into(),
            text: Some(s.into()),
            source: None,
        }
    }
    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            block_type: "image".into(),
            text: None,
            source: Some(serde_json::json!({
                "type": "base64",
                "media_type": media_type.into(),
                "data": data.into(),
            })),
        }
    }
    /// `(media_type, base64_data)` when this is a well-formed image block.
    pub fn as_image(&self) -> Option<(&str, &str)> {
        if self.block_type != "image" {
            return None;
        }
        let source = self.source.as_ref()?;
        Some((
            source.get("media_type")?.as_str()?,
            source.get("data")?.as_str()?,
        ))
    }
}

impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(s.into()),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        }
    }
    pub fn error_text(s: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(s.into()),
            is_error: true,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        }
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub turn_no: u32,
    pub sandbox: SandboxSettings,
    pub cancel: CancellationToken,
    pub additional_writable_dirs: Vec<PathBuf>,
    pub snapshot_file: Option<Arc<dyn SnapshotFile>>,
    pub effects: Option<Arc<dyn EffectsCallback>>,
    pub running_tasks: Option<Arc<dyn RunningTasksCallback>>,
    pub dangerously_disable_sandbox: bool,
    pub max_file_read_bytes: usize,
    pub permission_mode: PermissionMode,
    pub config: Arc<crate::context::EngineConfig>,
    pub session: Arc<crate::context::SessionState>,
    pub tool_use_id: String,
    pub agent: Option<crate::session::AgentContext>,
    pub parent_messages: Option<Vec<crate::message::Message>>,
    pub agent_depth: u32,
    pub events_tx: Option<
        tokio::sync::mpsc::UnboundedSender<crate::context::task::BackgroundAgentProgressData>,
    >,
    /// How a tool asks the user something — see
    /// [`crate::interface::elicitation::Elicitation`]. `None` where nothing
    /// can reach a person (a test context, a background run), which every
    /// asking tool must read as a refusal rather than as permission to make
    /// the answer up.
    pub elicitation: Option<Arc<dyn crate::interface::elicitation::Elicitation>>,
    /// How this tool reaches the machine — see
    /// [`crate::interface::exec`]. In production this machine's
    /// (`platform`); a deployment executing somewhere else replaces the set
    /// and no tool notices.
    pub exec: crate::interface::exec::ExecProviders,
    /// How this tool calls other tools — set only for a tool whose
    /// [`ToolSpec::needs_dispatch`] says it does.
    pub dispatch: Option<Arc<dyn crate::interface::tool_dispatch::ToolDispatch>>,
    /// The call comes from a program (`RunCode`), not from the model: there
    /// is nobody to ask and no conversation to report back to mid-call.
    pub from_code: bool,
    /// What the session's sandbox holds a command to.
    pub confinement: crate::interface::exec::ConfinementReport,
    /// Whether someone can be asked about what this call does.
    pub attendance: crate::interface::permission::Attendance,
    pub turn: Arc<TurnScope>,
}

/// What belongs to the one turn a call is made in, and ends with it.
#[derive(Debug, Default)]
pub struct TurnScope {
    /// Who started the turn, which decides what goal operations it may perform.
    pub origin: crate::context::goal::TurnOrigin,
    structured_output: std::sync::Mutex<Option<Value>>,
}

impl TurnScope {
    pub fn new(origin: crate::context::goal::TurnOrigin) -> Self {
        Self {
            origin,
            structured_output: Default::default(),
        }
    }

    /// Record what `StructuredOutput` accepted; a later call replaces it.
    pub fn set_structured_output(&self, value: Value) {
        *self
            .structured_output
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(value);
    }

    pub fn take_structured_output(&self) -> Option<Value> {
        self.structured_output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

impl ToolContext {
    pub fn for_test_with(cwd: PathBuf, exec: crate::interface::exec::ExecProviders) -> Self {
        Self {
            cwd: cwd.clone(),
            session_id: "test".into(),
            turn_no: 0,
            sandbox: Default::default(),
            cancel: CancellationToken::new(),
            additional_writable_dirs: vec![],
            snapshot_file: None,
            effects: None,
            running_tasks: None,
            dangerously_disable_sandbox: true,
            max_file_read_bytes: 0,
            permission_mode: PermissionMode::default(),
            config: Arc::new(crate::context::EngineConfig::defaults_for("test")),
            session: Arc::new(crate::context::SessionState::for_tests(cwd)),
            tool_use_id: String::new(),
            agent: None,
            parent_messages: None,
            agent_depth: 0,
            events_tx: None,
            elicitation: None,
            exec,
            dispatch: None,
            from_code: false,
            confinement: Default::default(),
            attendance: crate::interface::permission::Attendance::Attended,
            turn: Arc::default(),
        }
    }
    pub fn from_engine_ctx_with(
        cwd: PathBuf,
        cancel: CancellationToken,
        exec: crate::interface::exec::ExecProviders,
    ) -> Self {
        Self {
            cwd: cwd.clone(),
            session_id: String::new(),
            turn_no: 0,
            sandbox: SandboxSettings::default(),
            cancel,
            additional_writable_dirs: vec![],
            snapshot_file: None,
            effects: None,
            running_tasks: None,
            dangerously_disable_sandbox: false,
            max_file_read_bytes: 10 * 1024 * 1024,
            permission_mode: PermissionMode::default(),
            config: Arc::new(crate::context::EngineConfig::defaults_for("unknown")),
            session: Arc::new(crate::context::SessionState::for_tests(cwd)),
            tool_use_id: String::new(),
            agent: None,
            parent_messages: None,
            agent_depth: 0,
            events_tx: None,
            elicitation: None,
            exec,
            dispatch: None,
            from_code: false,
            confinement: Default::default(),
            attendance: crate::interface::permission::Attendance::Attended,
            turn: Arc::default(),
        }
    }
}

/// Where a tool came from, for [`crate::interface::model::ToolDef::source`].
///
/// A name prefix (`mcp__`) already implies the answer, but a naming
/// convention is not a declaration: it holds only for as long as nobody
/// registers a builtin whose name happens to start that way. An adapter that
/// knows which server or plugin it is fronting says so here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ToolOrigin {
    #[default]
    Builtin,
    Mcp {
        server: String,
    },
    Plugin {
        name: String,
    },
}

impl ToolOrigin {
    pub fn mcp(server: impl Into<String>) -> Self {
        Self::Mcp {
            server: server.into(),
        }
    }

    pub fn plugin(name: impl Into<String>) -> Self {
        Self::Plugin { name: name.into() }
    }

    pub fn mcp_server(&self) -> Option<&str> {
        match self {
            Self::Mcp { server } => Some(server),
            Self::Builtin | Self::Plugin { .. } => None,
        }
    }

    pub fn is_mcp(&self) -> bool {
        self.mcp_server().is_some()
    }

    /// `builtin`, `mcp:<server>` or `plugin:<name>`.
    pub fn wire(&self) -> String {
        match self {
            Self::Builtin => "builtin".to_string(),
            Self::Mcp { server } => format!("mcp:{server}"),
            Self::Plugin { name } => format!("plugin:{name}"),
        }
    }
}

/// What a tool says about one call: answered by [`Tool::call_traits`], which
/// is handed the call's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CallTraits {
    pub read_only: bool,
    pub concurrency_safe: bool,
    pub destructive: bool,
}

impl CallTraits {
    pub fn read_only(mut self, yes: bool) -> Self {
        self.read_only = yes;
        self
    }
    pub fn concurrency_safe(mut self, yes: bool) -> Self {
        self.concurrency_safe = yes;
        self
    }
    pub fn destructive(mut self, yes: bool) -> Self {
        self.destructive = yes;
        self
    }
}

/// What a tool says about itself that holds for every call. Built with
/// [`ToolSpec::new`] and the setters: `ToolSpec::new().read_only(true)`.
///
/// `traits` is the answer when the input is not looked at. A tool whose answer
/// depends on the input overrides [`Tool::call_traits`] too, and puts its
/// conservative answer here.
///
/// The input schema is not here: it is built on every call and the spec is
/// asked for far more often than the schema.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct ToolSpec {
    /// What `ToolSearch` lists next to the name; the description when unset.
    pub short_description: Option<String>,
    pub origin: ToolOrigin,
    pub traits: CallTraits,
    /// Whether a program the model writes (`RunCode`) may call this tool.
    /// Tools that do work and return data say yes; tools that talk to the
    /// conversation or a person do not.
    pub callable_from_code: bool,
    /// What a program gets back from this tool, when that is more than the
    /// text the model sees — the shape of its structured content.
    pub code_result: Option<&'static str>,
    /// Whether this tool calls other tools, and so needs
    /// [`ToolContext::dispatch`].
    pub needs_dispatch: bool,
    pub permission: ToolPermissionSpec,
    /// How the permission modes treat this tool's calls.
    pub stance: ModeStance,
}

impl ToolSpec {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn short_description(mut self, text: impl Into<String>) -> Self {
        self.short_description = Some(text.into());
        self
    }
    pub fn origin(mut self, origin: ToolOrigin) -> Self {
        self.origin = origin;
        self
    }
    pub fn traits(mut self, traits: CallTraits) -> Self {
        self.traits = traits;
        self
    }
    pub fn read_only(mut self, yes: bool) -> Self {
        self.traits.read_only = yes;
        self
    }
    pub fn concurrency_safe(mut self, yes: bool) -> Self {
        self.traits.concurrency_safe = yes;
        self
    }
    pub fn destructive(mut self, yes: bool) -> Self {
        self.traits.destructive = yes;
        self
    }
    pub fn callable_from_code(mut self, yes: bool) -> Self {
        self.callable_from_code = yes;
        self
    }
    pub fn code_result(mut self, shape: &'static str) -> Self {
        self.code_result = Some(shape);
        self
    }
    pub fn needs_dispatch(mut self, yes: bool) -> Self {
        self.needs_dispatch = yes;
        self
    }
    pub fn permission_content(mut self, kind: PermissionContentKind) -> Self {
        self.permission.content_kind = kind;
        self
    }
    pub fn self_allow_overrides_mode(mut self, yes: bool) -> Self {
        self.permission.self_allow_overrides_mode = yes;
        self.stance = self.permission.stance();
        self
    }
    pub fn leaves_plan_mode(mut self, yes: bool) -> Self {
        self.permission.leaves_plan_mode = yes;
        self.stance = self.permission.stance();
        self
    }
    pub fn stance(mut self, stance: ModeStance) -> Self {
        self.stance = stance;
        self.permission.self_allow_overrides_mode = stance == ModeStance::Coordination;
        self.permission.leaves_plan_mode = stance == ModeStance::LeavesPlan;
        self
    }
}

/// A tool the model can call.
///
/// What holds for every call is one [`spec`](Self::spec); what depends on the
/// call's input is [`call_traits`](Self::call_traits), which defaults to the
/// spec's answer. [`permission_facts`](Self::permission_facts) is the
/// permission engine's view of the call; by default it is read off
/// `check_permissions`, `permission_match_content` and `accept_edits_allows`.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// Everything about this tool that holds for every call — see
    /// [`ToolSpec`].
    fn spec(&self) -> ToolSpec;
    /// The input schema as worded for a session's [`PathStyle`]; only tools
    /// whose argument docs mention paths read `style`.
    fn input_schema(&self, style: PathStyle) -> Value;

    /// This tool's detailed usage guide, when it has one that says more than
    /// `description()` already does.
    ///
    /// `ToolDef`s carry only `description()`; the full guides total ~85 KB
    /// across the built-in set, too much to pay on every API call, so the model
    /// fetches them on demand via `ToolSearch{query: "select:<name>"}`.
    async fn guide(&self, _ctx: &PromptContext) -> Option<String> {
        None
    }

    /// What this call reads, writes and may run beside; `spec().traits` unless
    /// the answer depends on the input.
    fn call_traits(&self, _input: &Value) -> CallTraits {
        self.spec().traits
    }

    fn permission_match_content(&self, _: &Value) -> Option<String> {
        None
    }

    /// Whether `acceptEdits` lets this call through when neither the tool
    /// itself nor a rule did. A read does, by default. A tool whose read
    /// reaches further than reading — a file outside the directories the
    /// session may read, a request to a host the model chose, a program a
    /// repository's configuration names — says no here, and the call is asked
    /// about as under `default`.
    fn accept_edits_allows(&self, input: &Value, _: &ToolContext) -> bool {
        self.call_traits(input).read_only
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn permission_facts(&self, input: &Value, ctx: &ToolContext) -> PermissionFacts {
        PermissionFacts::bridged(self, input, ctx).await
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn call(
        &self,
        i: Value,
        c: ToolContext,
        p: ProgressSender,
    ) -> Result<ToolResult, ToolError>;
}

/// Identifies one registration, so it can be withdrawn without disturbing
/// anything else that happens to share the tool's name.
///
/// A name is not enough. `register` appends, so a name can legitimately be
/// present more than once, and "remove the tool called X" would then withdraw
/// whichever copy happened to be first — including one that a different
/// contributor added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegistrationId(u64);

impl RegistrationId {
    /// Mint an id. Public because every registry in the engine hands out
    /// disposers keyed by one, and they do not all live in this module.
    pub fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A handle that withdraws one registration.
///
/// **Dropping it does nothing.** Undoing on drop would be the more idiomatic
/// Rust shape, but it makes the overwhelmingly common call —
/// `registry.register(tool);` with the result ignored — silently unregister
/// the tool it just added. Withdrawal is therefore explicit: hold the handle
/// if you will need it, ignore it if you will not.
/// Not `#[must_use]`: the overwhelmingly common case is registering something
/// for the lifetime of the process and never withdrawing it, and a lint on
/// every one of those would be noise that teaches people to silence lints.
pub struct Disposer {
    id: RegistrationId,
    undo: Option<Box<dyn FnOnce(RegistrationId) + Send + Sync>>,
}

impl std::fmt::Debug for Disposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disposer").field("id", &self.id).finish()
    }
}

impl Disposer {
    pub fn new(
        id: RegistrationId,
        undo: impl FnOnce(RegistrationId) + Send + Sync + 'static,
    ) -> Self {
        Self {
            id,
            undo: Some(Box::new(undo)),
        }
    }

    /// A handle over a registration that cannot be withdrawn — for an
    /// implementation whose contents are fixed after construction.
    pub fn inert(id: RegistrationId) -> Self {
        Self { id, undo: None }
    }

    pub fn id(&self) -> RegistrationId {
        self.id
    }

    /// Withdraw the registration this handle stands for.
    pub fn dispose(mut self) {
        if let Some(undo) = self.undo.take() {
            undo(self.id);
        }
    }
}

pub trait ToolRegistry: Send + Sync + 'static {
    fn list(&self) -> Vec<Arc<dyn Tool>>;
    fn get(&self, n: &str) -> Option<Arc<dyn Tool>>;

    /// Add a tool, returning a handle that can withdraw exactly this
    /// registration.
    fn register(&self, t: Arc<dyn Tool>) -> Disposer;

    /// Substitute the entry for `t.name()`, or append when there is none.
    ///
    /// Distinct from [`register`](Self::register) because that one appends and
    /// [`get`](Self::get) returns the first name match, so re-registering an
    /// existing name leaves the original in front and the new instance
    /// unreachable.
    fn replace(&self, t: Arc<dyn Tool>) -> Disposer;

    /// Withdraw every registration under `name`. Returns how many went.
    ///
    /// Present so a contributor can be unloaded cleanly. Masking a tool with a
    /// deny rule leaves it in the registry, still visible to anything that
    /// enumerates it; removal is what actually takes it out.
    fn remove(&self, name: &str) -> usize;

    fn names(&self) -> Vec<String> {
        self.list().iter().map(|t| t.name().to_string()).collect()
    }
    fn len(&self) -> usize {
        self.list().len()
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone)]
pub struct InMemoryToolRegistry {
    pub(crate) tools: Arc<std::sync::RwLock<Vec<Entry>>>,
}

/// A registered tool plus the id its `Disposer` refers to.
#[derive(Clone)]
pub struct Entry {
    pub id: RegistrationId,
    pub tool: Arc<dyn Tool>,
}
impl InMemoryToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Arc::new(std::sync::RwLock::new(Vec::new())),
        }
    }
    pub fn register(&self, t: Arc<dyn Tool>) -> Disposer {
        let id = RegistrationId::next();
        self.tools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(Entry { id, tool: t });
        self.disposer_for(id)
    }

    fn disposer_for(&self, id: RegistrationId) -> Disposer {
        let tools = self.tools.clone();
        Disposer::new(id, move |id| {
            tools
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|e| e.id != id);
        })
    }

    /// Swap the entry for `t.name()`, or append when there is none.
    ///
    /// [`register`](Self::register) appends unconditionally while
    /// [`get`](Self::get) returns the *first* name match, so re-registering an
    /// existing name leaves the original in front and the new instance
    /// unreachable. Use this wherever the intent is to substitute a tool that
    /// is already present — wrapping one in a decorator, for instance.
    pub fn replace(&self, t: Arc<dyn Tool>) -> Disposer {
        let id = RegistrationId::next();
        {
            let mut w = self.tools.write().unwrap_or_else(|e| e.into_inner());
            match w.iter().position(|x| x.tool.name() == t.name()) {
                Some(i) => w[i] = Entry { id, tool: t },
                None => w.push(Entry { id, tool: t }),
            }
        }
        self.disposer_for(id)
    }

    /// Withdraw every registration.
    pub fn clear(&self) {
        self.tools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Withdraw every registration under `name`; returns how many went.
    pub fn remove(&self, name: &str) -> usize {
        let mut w = self.tools.write().unwrap_or_else(|e| e.into_inner());
        let before = w.len();
        w.retain(|e| e.tool.name() != name);
        before - w.len()
    }
    pub fn get(&self, n: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|e| e.tool.name() == n)
            .map(|e| e.tool.clone())
    }
    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| e.tool.clone())
            .collect()
    }
    pub fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| e.tool.name().to_string())
            .collect()
    }
    pub fn len(&self) -> usize {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
impl ToolRegistry for InMemoryToolRegistry {
    fn list(&self) -> Vec<Arc<dyn Tool>> {
        InMemoryToolRegistry::list(self)
    }
    fn get(&self, n: &str) -> Option<Arc<dyn Tool>> {
        InMemoryToolRegistry::get(self, n)
    }
    fn register(&self, t: Arc<dyn Tool>) -> Disposer {
        InMemoryToolRegistry::register(self, t)
    }
    fn replace(&self, t: Arc<dyn Tool>) -> Disposer {
        InMemoryToolRegistry::replace(self, t)
    }
    fn remove(&self, name: &str) -> usize {
        InMemoryToolRegistry::remove(self, name)
    }
}
impl Default for InMemoryToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// `schema` as sent to the model: the same parameters, without what schema
/// generators add for other readers — the root `$schema` and `title`,
/// `"default": null`, Rust integer `format`s, and per-variant descriptions on
/// an enum of string constants (which becomes a plain `enum`).
///
/// Only what the model reads changes; inputs are still parsed against the
/// tool's own types.
pub fn compact_schema(mut schema: Value) -> Value {
    if let Some(root) = schema.as_object_mut() {
        root.remove("$schema");
        root.remove("title");
    }
    compact_node(&mut schema);
    schema
}

fn compact_node(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if map.get("default").is_some_and(Value::is_null) {
                map.remove("default");
            }
            if map
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(is_numeric_format)
            {
                map.remove("format");
            }
            if let Some(constants) = map.get("oneOf").and_then(string_constants) {
                map.remove("oneOf");
                map.insert("type".into(), Value::String("string".into()));
                map.insert("enum".into(), Value::Array(constants));
            }
            for child in map.values_mut() {
                compact_node(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(compact_node),
        _ => {}
    }
}

fn is_numeric_format(format: &str) -> bool {
    matches!(
        format,
        "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "float"
            | "double"
    )
}

/// The constants of a `oneOf` whose every branch is a string `const`,
/// optionally typed and described — nothing else may be lost by flattening.
fn string_constants(one_of: &Value) -> Option<Vec<Value>> {
    let branches = one_of.as_array()?;
    if branches.is_empty() {
        return None;
    }
    branches
        .iter()
        .map(|branch| {
            let branch = branch.as_object()?;
            let constant = branch.get("const").filter(|c| c.is_string())?;
            let only_known_keys = branch.keys().all(|k| {
                k == "const" || k == "description" || (k == "type" && branch[k] == "string")
            });
            only_known_keys.then(|| constant.clone())
        })
        .collect()
}

#[cfg(test)]
mod compact_schema_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drops_generator_noise_and_keeps_every_parameter() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "BashInput",
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {"type": "string", "description": "The command."},
                "timeout_ms": {"type": ["integer", "null"], "default": null,
                               "format": "uint64", "minimum": 0},
                "url": {"type": "string", "format": "uri"},
                "flag": {"type": "boolean", "default": false}
            }
        });
        assert_eq!(
            compact_schema(schema),
            json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {"type": "string", "description": "The command."},
                    "timeout_ms": {"type": ["integer", "null"], "minimum": 0},
                    "url": {"type": "string", "format": "uri"},
                    "flag": {"type": "boolean", "default": false}
                }
            })
        );
    }

    #[test]
    fn an_enum_of_described_constants_becomes_a_plain_enum() {
        let schema = json!({"$defs": {"Mode": {
            "description": "Permission mode.",
            "oneOf": [
                {"const": "default", "type": "string", "description": "Ask."},
                {"const": "plan", "type": "string", "description": "Read-only."}
            ]
        }}});
        assert_eq!(
            compact_schema(schema),
            json!({"$defs": {"Mode": {
                "description": "Permission mode.",
                "type": "string",
                "enum": ["default", "plan"]
            }}})
        );
    }

    #[test]
    fn a_oneof_that_is_not_only_constants_is_left_alone() {
        let one_of = json!([
            {"const": "auto", "type": "string"},
            {"type": "object", "properties": {"n": {"type": "integer"}}}
        ]);
        let compacted = compact_schema(json!({"oneOf": one_of.clone()}));
        assert_eq!(compacted["oneOf"], one_of);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct F;
    #[async_trait]
    impl Tool for F {
        fn name(&self) -> &str {
            "f"
        }
        fn description(&self) -> &str {
            "f"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new().read_only(true).concurrency_safe(true)
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            serde_json::json!({})
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }
    #[test]
    fn empty() {
        assert!(InMemoryToolRegistry::new().list().is_empty());
    }
    #[test]
    fn find() {
        let r = InMemoryToolRegistry::new();
        r.register(Arc::new(F));
        assert!(r.get("f").is_some());
    }

    /// A tool that says nothing about where it came from is a builtin, so the
    /// ones that say otherwise (MCP) are the exceptions — a newly written
    /// builtin cannot forget to declare itself.
    #[test]
    fn a_tool_that_says_nothing_is_a_builtin() {
        assert_eq!(F.spec().origin, ToolOrigin::Builtin);
        assert_eq!(ToolOrigin::Builtin.wire(), "builtin");
        assert_eq!(ToolOrigin::mcp("github").wire(), "mcp:github");
    }
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    async fn unreachable_call() -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("ok"))
    }

    struct Plain;
    #[async_trait]
    impl Tool for Plain {
        fn name(&self) -> &str {
            "Plain"
        }
        fn description(&self) -> &str {
            "says nothing about itself"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new()
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            unreachable_call().await
        }
    }

    struct Declared;
    #[async_trait]
    impl Tool for Declared {
        fn name(&self) -> &str {
            "Declared"
        }
        fn description(&self) -> &str {
            "declares a spec"
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new()
                .origin(ToolOrigin::mcp("x"))
                .concurrency_safe(true)
                .destructive(true)
                .needs_dispatch(true)
                .leaves_plan_mode(true)
                .short_description("short")
        }
        fn call_traits(&self, input: &Value) -> CallTraits {
            self.spec().traits.read_only(input.get("peek").is_some())
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            unreachable_call().await
        }
    }

    #[tokio::test]
    async fn a_tool_that_declares_nothing_gets_the_defaults() {
        assert_eq!(Plain.spec(), ToolSpec::new());
        assert_eq!(Plain.call_traits(&Value::Null), CallTraits::default());
        assert_eq!(Plain.spec().origin, ToolOrigin::Builtin);
        assert_eq!(
            Plain.spec().permission.content_kind,
            PermissionContentKind::Opaque
        );
        assert!(Plain.guide(&PromptContext::default()).await.is_none());
    }

    #[test]
    fn a_declared_spec_is_what_the_engine_reads() {
        let spec = Declared.spec();
        assert_eq!(spec.origin.mcp_server(), Some("x"));
        assert!(spec.traits.concurrency_safe && spec.traits.destructive);
        assert!(spec.needs_dispatch && spec.permission.leaves_plan_mode);
        assert!(!spec.callable_from_code);
        assert_eq!(spec.short_description.as_deref(), Some("short"));
    }

    #[test]
    fn call_traits_that_read_the_input_win_over_the_spec() {
        assert!(!Declared.spec().traits.read_only);
        assert!(
            Declared
                .call_traits(&serde_json::json!({"peek": 1}))
                .read_only
        );
        assert!(!Declared.call_traits(&serde_json::json!({})).read_only);
        let ctx = ToolContext::for_test_with(
            PathBuf::from("/tmp"),
            crate::interface::exec::ExecProviders::in_process(),
        );
        assert!(Declared.accept_edits_allows(&serde_json::json!({"peek": 1}), &ctx));
        assert!(!Declared.accept_edits_allows(&serde_json::json!({}), &ctx));
    }
}

/// A registry that layers one set of tools over another.
///
/// Exists to answer a question a single implementation cannot: is
/// [`ToolRegistry`] actually a contract, or just the shape of
/// [`InMemoryToolRegistry`]? Writing a second implementation is what finds the
/// methods that leaked an assumption about the first — and this one does not
/// store `Vec<Entry>` at all, so anything that only worked because the backing
/// store was a vector shows up here.
///
/// Reads consult the overlay first, then the base, so an overlay entry shadows
/// a base entry of the same name. Writes only ever touch the overlay: the base
/// is borrowed, and silently mutating something you were handed is how a
/// "layer" becomes a leak.
pub struct LayeredToolRegistry {
    base: Arc<dyn ToolRegistry>,
    overlay: InMemoryToolRegistry,
    hidden: std::sync::RwLock<std::collections::HashSet<String>>,
}

impl LayeredToolRegistry {
    pub fn new(base: Arc<dyn ToolRegistry>) -> Self {
        Self {
            base,
            overlay: InMemoryToolRegistry::new(),
            hidden: Default::default(),
        }
    }

    /// Names hidden from `list`/`get` without touching the base registry.
    ///
    /// The layer's reason for existing: an embedder that wants a narrower tool
    /// surface than the one it was handed, without the right to modify it.
    pub fn hide(&self, name: &str) {
        self.hidden
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string());
    }

    fn is_hidden(&self, name: &str) -> bool {
        self.hidden
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }
}

impl ToolRegistry for LayeredToolRegistry {
    fn list(&self) -> Vec<Arc<dyn Tool>> {
        let mut out = self.overlay.list();
        let shadowed: std::collections::HashSet<String> =
            out.iter().map(|t| t.name().to_string()).collect();
        for t in self.base.list() {
            if !self.is_hidden(t.name()) && !shadowed.contains(t.name()) {
                out.push(t);
            }
        }
        out
    }

    fn get(&self, n: &str) -> Option<Arc<dyn Tool>> {
        match self.overlay.get(n) {
            Some(t) => Some(t),
            None if self.is_hidden(n) => None,
            None => self.base.get(n),
        }
    }

    /// Registering a name the layer hid makes the new entry visible; the base
    /// entry stays hidden behind it.
    fn register(&self, t: Arc<dyn Tool>) -> Disposer {
        self.overlay.register(t)
    }

    fn replace(&self, t: Arc<dyn Tool>) -> Disposer {
        self.overlay.replace(t)
    }

    /// Removes from the overlay, then hides whatever the base still exposes.
    /// "Removed" has to mean "not visible through this registry" — leaving a
    /// base entry reachable after a `remove` would be the exact leak this
    /// layer exists to avoid.
    fn remove(&self, name: &str) -> usize {
        let removed = self.overlay.remove(name);
        if !self.is_hidden(name) && self.base.get(name).is_some() {
            self.hide(name);
            return removed + 1;
        }
        removed
    }
}

#[cfg(test)]
mod registry_contract_tests {
    use super::*;

    #[derive(Debug)]
    struct Stub(&'static str);

    #[async_trait::async_trait]
    impl Tool for Stub {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new()
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _i: Value,
            _c: ToolContext,
            _p: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    fn stub(n: &'static str) -> Arc<dyn Tool> {
        Arc::new(Stub(n))
    }

    /// The same assertions against every implementation. A contract only one
    /// type satisfies is not a contract.
    fn contract(reg: &dyn ToolRegistry) {
        let d = reg.register(stub("Alpha"));
        assert!(reg.get("Alpha").is_some());
        assert!(reg.names().contains(&"Alpha".to_string()));

        // A disposer withdraws its own registration and nothing else.
        let keep = reg.register(stub("Beta"));
        d.dispose();
        assert!(reg.get("Alpha").is_none(), "disposed tool still reachable");
        assert!(
            reg.get("Beta").is_some(),
            "disposer took an unrelated entry"
        );
        let _ = keep;

        // `replace` substitutes rather than shadowing.
        reg.register(stub("Gamma"));
        reg.replace(stub("Gamma"));
        assert_eq!(
            reg.list().iter().filter(|t| t.name() == "Gamma").count(),
            1,
            "replace left the original in place"
        );

        assert_eq!(reg.remove("Gamma"), 1);
        assert!(reg.get("Gamma").is_none());
        assert_eq!(reg.remove("NeverRegistered"), 0);
    }

    #[test]
    fn in_memory_registry_satisfies_the_contract() {
        contract(&InMemoryToolRegistry::new());
    }

    #[test]
    fn layered_registry_satisfies_the_contract() {
        contract(&LayeredToolRegistry::new(Arc::new(
            InMemoryToolRegistry::new(),
        )));
    }

    /// Dropping a disposer must *not* withdraw — the common call ignores the
    /// return value, and undoing on drop would silently unregister everything.
    #[test]
    fn dropping_a_disposer_keeps_the_registration() {
        let reg = InMemoryToolRegistry::new();
        drop(reg.register(stub("Alpha")));
        assert!(reg.get("Alpha").is_some());
    }

    /// Two registrations of one name are distinct, and a disposer takes only
    /// the one it was handed back for.
    #[test]
    fn a_disposer_targets_its_own_registration_not_the_name() {
        let reg = InMemoryToolRegistry::new();
        let first = reg.register(stub("Dup"));
        reg.register(stub("Dup"));
        assert_eq!(reg.list().iter().filter(|t| t.name() == "Dup").count(), 2);
        first.dispose();
        assert_eq!(
            reg.list().iter().filter(|t| t.name() == "Dup").count(),
            1,
            "disposing one registration took both"
        );
    }

    /// The layer must not mutate what it was handed.
    #[test]
    fn the_layer_never_writes_through_to_its_base() {
        let base = Arc::new(InMemoryToolRegistry::new());
        base.register(stub("FromBase"));
        let layer = LayeredToolRegistry::new(base.clone());

        layer.register(stub("FromLayer"));
        assert_eq!(base.len(), 1, "layer wrote into its base");

        assert!(layer.get("FromBase").is_some(), "base entry not visible");
        assert_eq!(layer.remove("FromBase"), 1);
        assert!(layer.get("FromBase").is_none(), "removal did not hide it");
        assert!(
            base.get("FromBase").is_some(),
            "removal reached through into the base"
        );
    }

    /// An overlay entry shadows a base entry of the same name.
    #[test]
    fn overlay_shadows_the_base_without_duplicating() {
        let base = Arc::new(InMemoryToolRegistry::new());
        base.register(stub("Shared"));
        let layer = LayeredToolRegistry::new(base);
        layer.register(stub("Shared"));
        assert_eq!(
            layer.list().iter().filter(|t| t.name() == "Shared").count(),
            1
        );
    }
}
