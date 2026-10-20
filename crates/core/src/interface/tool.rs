//! Tool trait — v7 unified. All tools implement this single trait.
//! Types moved here from tools/src/legacy.rs. legacy.rs is now a pure re-export.

use crate::context::config::NetworkModeConfig;
use crate::error::ToolError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ── Shared types (used by ALL tools) ──

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
    /// See `base::context::config::SandboxPolicyConfig::state_root`.
    pub state_root: Option<PathBuf>,
    /// See `base::settings::SandboxConfig::require_enforcement`.
    pub require_enforcement: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptBehavior {
    Cancel,
    Block,
}
pub use crate::permission::PermissionMode;
#[derive(Debug, Clone)]
pub struct PromptContext {
    pub cwd: PathBuf,
    pub model: String,
    pub session_id: String,
    pub is_interactive: bool,
    pub all_tool_names: Vec<String>,
    pub allowed_agent_types: Vec<String>,
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

#[derive(Clone)]
pub struct ProgressSender {
    tool_use_id: String,
    callback: Option<Arc<dyn ProgressCallback>>,
}
pub trait ProgressCallback: Send + Sync {
    fn on_progress(&self, tool_use_id: &str, data: &str);
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
    pub fn send_blob(&self, _: &[u8]) {}
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

// ── Unified ToolContext (legacy + v2 fields) ──

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
    /// [`crate::interface::exec`]. The default is this machine; a deployment
    /// executing somewhere else replaces the set and no tool notices.
    pub exec: crate::interface::exec::ExecProviders,
}
impl ToolContext {
    pub fn for_test(cwd: PathBuf) -> Self {
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
            session: Arc::new(crate::context::SessionState::new(cwd)),
            tool_use_id: String::new(),
            agent: None,
            parent_messages: None,
            agent_depth: 0,
            events_tx: None,
            elicitation: None,
            exec: Default::default(),
        }
    }
    pub fn from_engine_ctx(cwd: PathBuf, cancel: CancellationToken) -> Self {
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
            session: Arc::new(crate::context::SessionState::new(cwd)),
            tool_use_id: String::new(),
            agent: None,
            parent_messages: None,
            agent_depth: 0,
            events_tx: None,
            elicitation: None,
            exec: Default::default(),
        }
    }
}

/// What a tool's permission match content is, which decides how rules are
/// matched against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionContentKind {
    /// Compared to rule patterns verbatim.
    Opaque,
    /// A path the tool reads. Resolved against the session's cwd before
    /// matching, and `Read(...)` rules apply to it as well as the tool's own.
    ReadPath,
    /// A path the tool writes. Resolved like `ReadPath`; `Edit(...)` rules
    /// apply to it as well as the tool's own.
    WritePath,
    /// A shell command line, matched one simple command at a time.
    ShellCommand,
}

// ── Unified Tool trait ──

#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value;

    /// Where this tool came from, for [`crate::interface::model::ToolDef::source`].
    ///
    /// The name prefixes (`mcp__`, `plugin__`) already imply the answer, but a
    /// naming convention is not a declaration: it holds only for as long as
    /// nobody registers a builtin whose name happens to start that way. An
    /// adapter that knows which server or plugin it is fronting says so here.
    fn source(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("builtin")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.prompt_fragment()
    }
    fn prompt_fragment(&self) -> String {
        self.description().to_string()
    }
    /// This tool's detailed usage guide, when it has one that says more than
    /// `description()` already does — `None` when it doesn't.
    ///
    /// `prompt()` has always existed and ~35 tools implement it with a
    /// multi-KB markdown guide (`agent_tool.prompt.md`, `team/tool.prompt.md`,
    /// `prompts/coding/*.prompt.md`), but until now nothing put those guides
    /// in front of the model at all: `build_tool_defs` builds every `ToolDef`
    /// from `description()`, so `prompt()` had zero production call sites.
    /// Inlining them is not an option either — they total ~85 KB (~21k tokens)
    /// across the built-in set, paid on *every* API call. They are instead
    /// fetched on demand via `ToolSearch{query: "select:<name>"}`, and this
    /// method is the predicate deciding which tools have anything to fetch.
    ///
    /// The default implementation *derives* the answer rather than making
    /// every tool declare it (which would mean editing 35 files and leaving
    /// the next tool author a flag to forget): `prompt()` falls back to
    /// `prompt_fragment()`, which falls back to `description()`, so a tool
    /// overriding neither necessarily produces text equal to its description
    /// and is correctly reported as undocumented.
    /// Does this tool's own `Allow` from [`Tool::check_permissions`] survive a
    /// *mode-level* denial (`Plan`, `DontAsk`)?
    ///
    /// Default `false`, which is what makes plan mode mean anything: `Write`
    /// and `Edit` allow themselves for any in-project path, and that verdict
    /// is a statement about *path safety*, not a claim that writing files is
    /// fine while the user asked for a plan. Letting it outrank the mode is
    /// how plan mode came to be decorative.
    ///
    /// A tool returns `true` only when its self-`Allow` is a statement about
    /// the tool's own nature rather than about its arguments — a coordination
    /// primitive that touches no file, no shell and no network, and so is not
    /// what any of these modes exist to restrain. The team mailbox's
    /// `SendMessage` is the case in point: team members run under `Plan` by
    /// default, and a plan-mode member that cannot talk to its coordinator is
    /// not a plan-mode member, it is a broken one.
    ///
    /// Rule-based denials and the bypass-immune path list are **not**
    /// affected by this: an explicit `deny` always wins, whatever the tool
    /// says about itself.
    fn self_allow_overrides_mode(&self) -> bool {
        false
    }

    async fn detailed_prompt(&self, ctx: &PromptContext) -> Option<String> {
        let body = self.prompt(ctx).await;
        let trimmed = body.trim();
        if trimmed.is_empty()
            || trimmed == self.description().trim()
            || trimmed == self.prompt_fragment().trim()
        {
            return None;
        }
        Some(body)
    }
    fn is_enabled(&self) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn strict(&self) -> bool {
        false
    }
    fn is_deferred(&self) -> bool {
        false
    }
    fn is_dynamic(&self) -> bool {
        false
    }
    fn is_direct(&self) -> bool {
        false
    }
    fn short_description(&self) -> Option<String> {
        None
    }
    fn permission_match_content(&self, _: &Value) -> Option<String> {
        None
    }
    /// How the permission engine reads [`permission_match_content`](Self::permission_match_content).
    fn permission_content_kind(&self) -> PermissionContentKind {
        PermissionContentKind::Opaque
    }
    fn affected_paths(&self, _: &Value) -> Vec<PathBuf> {
        vec![]
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Cancel
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
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
    fn all(&self) -> Vec<Arc<dyn Tool>>;
    fn find(&self, n: &str) -> Option<Arc<dyn Tool>>;

    /// Add a tool, returning a handle that can withdraw exactly this
    /// registration.
    fn register(&self, t: Arc<dyn Tool>) -> Disposer;

    /// Substitute the entry for `t.name()`, or append when there is none.
    ///
    /// Distinct from [`register`](Self::register) because that one appends and
    /// [`find`](Self::find) returns the first name match, so re-registering an
    /// existing name leaves the original in front and the new instance
    /// unreachable.
    fn replace(&self, t: Arc<dyn Tool>) -> Disposer;

    /// Withdraw every registration under `name`. Returns how many went.
    ///
    /// Present so a contributor can be unloaded cleanly. Masking a tool with a
    /// deny rule leaves it in the registry, still visible to anything that
    /// enumerates it; removal is what actually takes it out.
    fn remove(&self, name: &str) -> usize;

    // The rest are conveniences the engine reaches for constantly. They are
    // defaulted in terms of `all`/`find` so a second implementation owes only
    // the five methods above — an implementor that can answer them more
    // cheaply than by materializing every tool is free to override.

    fn get(&self, n: &str) -> Option<Arc<dyn Tool>> {
        self.find(n)
    }
    fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.all()
    }
    fn names(&self) -> Vec<String> {
        self.all().iter().map(|t| t.name().to_string()).collect()
    }
    fn len(&self) -> usize {
        self.all().len()
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn has_direct_tool(&self) -> bool {
        self.all().iter().any(|t| t.is_direct())
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
    pub fn all(&self) -> Vec<Arc<dyn Tool>> {
        self.list()
    }
    pub fn find(&self, n: &str) -> Option<Arc<dyn Tool>> {
        self.get(n)
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
    pub fn has_direct_tool(&self) -> bool {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|e| e.tool.is_direct())
    }
}
impl ToolRegistry for InMemoryToolRegistry {
    fn all(&self) -> Vec<Arc<dyn Tool>> {
        self.list()
    }
    fn find(&self, n: &str) -> Option<Arc<dyn Tool>> {
        self.get(n)
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
pub fn extract_tool_name(i: &Value) -> Option<&str> {
    i.get("tool").and_then(|v| v.as_str())
}

#[async_trait]
pub trait SecondaryLlm: Send + Sync {
    async fn extract_with_prompt(&self, p: &str, c: &str) -> Result<String, String>;
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
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        fn is_read_only(&self, _: &Value) -> bool {
            true
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            true
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
        assert!(InMemoryToolRegistry::new().all().is_empty());
    }
    #[test]
    fn find() {
        let r = InMemoryToolRegistry::new();
        r.register(Arc::new(F));
        assert!(r.find("f").is_some());
    }

    /// A tool that says nothing about where it came from is a builtin. This is
    /// the default every registered tool inherits, so the ones that override it
    /// (MCP, plugins) are the exceptions rather than the other way round — a
    /// newly written builtin cannot forget to declare itself.
    #[test]
    fn a_tool_that_says_nothing_is_a_builtin() {
        assert_eq!(F.source(), "builtin");
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
}

impl LayeredToolRegistry {
    pub fn new(base: Arc<dyn ToolRegistry>) -> Self {
        Self {
            base,
            overlay: InMemoryToolRegistry::new(),
        }
    }

    /// Names hidden from `all`/`find` without touching the base registry.
    ///
    /// The layer's reason for existing: an embedder that wants a narrower tool
    /// surface than the one it was handed, without the right to modify it.
    pub fn hide(&self, name: &str) {
        self.overlay.register(Arc::new(Tombstone {
            name: name.to_string(),
        }));
    }
}

/// Marks a name as hidden. Never dispatched — [`LayeredToolRegistry`] filters
/// it out of every read — so its `call` is unreachable rather than merely
/// unused.
#[derive(Debug)]
struct Tombstone {
    name: String,
}

#[async_trait::async_trait]
impl Tool for Tombstone {
    fn name(&self) -> &str {
        &self.name
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn is_enabled(&self) -> bool {
        false
    }
    async fn call(
        &self,
        _i: Value,
        _c: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::Execution(anyhow::anyhow!(
            "`{}` is hidden in this registry layer and should never be dispatched",
            self.name
        )))
    }
}

fn is_tombstone(t: &Arc<dyn Tool>) -> bool {
    !t.is_enabled() && t.description().is_empty()
}

impl ToolRegistry for LayeredToolRegistry {
    fn all(&self) -> Vec<Arc<dyn Tool>> {
        let overlay = self.overlay.list();
        let hidden: std::collections::HashSet<&str> = overlay
            .iter()
            .filter(|t| is_tombstone(t))
            .map(|t| t.name())
            .collect();

        let mut out: Vec<Arc<dyn Tool>> = overlay
            .iter()
            .filter(|t| !is_tombstone(t))
            .cloned()
            .collect();
        let shadowed: std::collections::HashSet<String> =
            out.iter().map(|t| t.name().to_string()).collect();

        for t in self.base.all() {
            if !hidden.contains(t.name()) && !shadowed.contains(t.name()) {
                out.push(t);
            }
        }
        out
    }

    fn find(&self, n: &str) -> Option<Arc<dyn Tool>> {
        match self.overlay.get(n) {
            Some(t) if is_tombstone(&t) => None,
            Some(t) => Some(t),
            None => self.base.find(n),
        }
    }

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
        if self.base.find(name).is_some() {
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
        fn input_schema(&self) -> Value {
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
        assert!(reg.find("Alpha").is_some());
        assert!(reg.names().contains(&"Alpha".to_string()));

        // A disposer withdraws its own registration and nothing else.
        let keep = reg.register(stub("Beta"));
        d.dispose();
        assert!(reg.find("Alpha").is_none(), "disposed tool still reachable");
        assert!(
            reg.find("Beta").is_some(),
            "disposer took an unrelated entry"
        );
        let _ = keep;

        // `replace` substitutes rather than shadowing.
        reg.register(stub("Gamma"));
        reg.replace(stub("Gamma"));
        assert_eq!(
            reg.all().iter().filter(|t| t.name() == "Gamma").count(),
            1,
            "replace left the original in place"
        );

        assert_eq!(reg.remove("Gamma"), 1);
        assert!(reg.find("Gamma").is_none());
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
        assert!(reg.find("Alpha").is_some());
    }

    /// Two registrations of one name are distinct, and a disposer takes only
    /// the one it was handed back for.
    #[test]
    fn a_disposer_targets_its_own_registration_not_the_name() {
        let reg = InMemoryToolRegistry::new();
        let first = reg.register(stub("Dup"));
        reg.register(stub("Dup"));
        assert_eq!(reg.all().iter().filter(|t| t.name() == "Dup").count(), 2);
        first.dispose();
        assert_eq!(
            reg.all().iter().filter(|t| t.name() == "Dup").count(),
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

        assert!(layer.find("FromBase").is_some(), "base entry not visible");
        assert_eq!(layer.remove("FromBase"), 1);
        assert!(layer.find("FromBase").is_none(), "removal did not hide it");
        assert!(
            base.find("FromBase").is_some(),
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
            layer.all().iter().filter(|t| t.name() == "Shared").count(),
            1
        );
    }
}
