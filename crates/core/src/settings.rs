//! Configuration injected by the application layer.
//!
//! `Settings::load()` is the single canonical settings.json loader (global →
//! scene → project, generic recursive JSON merge — see its doc comment).
//! Everything downstream (the AGENT itself, `Builder`) receives the already-
//! merged result; it does not perform its own multi-layer config merging.
//!
//! Where the layers come from is a separate question from what is done with
//! them: `load()` reads the six files on disk,
//! [`Settings::load_from`] takes them from any
//! [`ConfigSource`](crate::interface::config_source::ConfigSource), and both
//! run the same merge.

use crate::provider::{ApiType, ProviderConfig, TaskModelOverride};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// The shared, committed settings file present in every tier.
pub const SETTINGS_FILE: &str = "settings.json";

/// The gitignored per-machine overlay sitting beside `SETTINGS_FILE` in the
/// same tier, overriding it.
pub const SETTINGS_LOCAL_FILE: &str = "settings.local.json";

/// Complete AGENT configuration. Merged by `Settings::load()` before injection.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Settings {
    pub model: ModelSettings,
    pub paths: PathSettings,
    #[serde(default)]
    pub execution: ExecutionSettings,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Scripts bound to extension points — see [`ScriptBinding`]. Empty by
    /// default, and inert in a build without a script engine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<ScriptBinding>,

    /// Path to an instruction file (e.g. AGENTS.md, CLAUDE.md).
    /// The AGENT reads the file at its discretion (every turn, on change, etc.).
    #[serde(default)]
    pub instruction_file: Option<PathBuf>,

    /// Appended to the end of the system prompt.
    #[serde(default)]
    pub prompt_append: Option<String>,
    /// Overrides the entire system prompt if set.
    #[serde(default)]
    pub prompt_override: Option<String>,

    // ── Internal component configuration ──
    /// LLM interaction recording. None = pass-through.
    #[serde(default)]
    pub recorder: Option<RecorderConfig>,
    /// Telemetry endpoint URL. None = noop.
    #[serde(default)]
    pub telemetry_url: Option<String>,
    /// Session persistence directory. None = no persistence.
    /// Default: `Some(user_data_dir/sessions/)`.
    #[serde(default)]
    pub session_dir: Option<PathBuf>,

    /// Enable the file-based memory system (MEMORY.md + .md files).
    /// Default: true. Set to false to disable memory prompt injection and file-based memory.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,

    /// Disable skill dynamic-content-injection shell commands (`` !`cmd` ``
    /// and fenced ` ```! ` blocks in `SKILL.md` bodies). Default: false
    /// (enabled). When true, each such placeholder is replaced with
    /// `[shell command execution disabled by policy]` instead of running.
    /// Most useful set in managed/org-wide settings where individual users
    /// can't override it.
    #[serde(default)]
    pub disable_skill_shell_execution: bool,

    /// Permission mode for tool execution.
    #[serde(default)]
    pub permission_mode: PermissionMode,

    /// Allow/deny/ask rules for specific tools.
    ///
    /// Every tier's rules apply side by side: a project's rules are added to
    /// the global and scene ones, never substituted for them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<PermissionRule>,

    /// Rules contributed by the `settings.local.json` overlays, kept apart
    /// from `permission_rules` so they reach the rule engine as
    /// `RuleSource::LocalSettings`. Populated by `Settings::load`, never read
    /// from a settings file — hence `serde(skip)`.
    #[serde(skip)]
    pub local_permission_rules: Vec<PermissionRule>,

    /// Whether an RPC client may request a *more permissive* mode than
    /// `permission_mode` above.
    ///
    /// The daemon's `session.create` takes an optional `permission_mode` in
    /// its options. That used to override `permission_mode` outright in
    /// either direction, which made this setting a default rather than a
    /// policy: any client that could authenticate could open a
    /// `bypassPermissions` session no matter how the daemon was configured
    /// (and `daemon/src/server.rs` notes there is no per-method
    /// authorization on that socket). With this `false` — the default — a
    /// client can still *tighten* the mode for its own session, but a
    /// request to loosen it is clamped to what is configured here.
    ///
    /// Set it to `true` only when every client of this daemon is as trusted
    /// as the daemon itself.
    #[serde(default)]
    pub allow_client_permission_override: bool,

    /// Whether sessions record a telemetry transcript.
    ///
    /// Defaults to **on**: a session writes one JSONL file per session under
    /// `<local_data_dir>/telemetry/`, which is what makes a run debuggable
    /// after the fact (tool calls, permission decisions, turn costs). This
    /// used to be effectively off — the daemon attached a recorder only when
    /// a caller passed an explicit output path, and the no-recorder fallback
    /// dropped every event on the floor — so nothing was ever recorded
    /// unless someone had planned for it in advance.
    #[serde(default = "default_true")]
    pub telemetry_enabled: bool,

    /// Hooks configuration (merged from global/scene/project layers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_config: Option<serde_json::Value>,

    /// MCP server configurations, keyed by server name (merged from
    /// global/scene/project layers). Kept untyped (`serde_json::Value`
    /// rather than `mcp::config::McpServerConfig`) — `core` cannot depend on
    /// `mcp` (which already depends on `core`, for `base::paths::ConfigPaths`)
    /// without a circular dependency; downstream consumers deserialize each
    /// value into `McpServerConfig` themselves.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_servers: HashMap<String, serde_json::Value>,

    /// Multi-provider LLM registry, keyed by provider id.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub providers: HashMap<String, ProviderConfig>,
    /// Provider id used for any task with no `task_models` entry, and as the
    /// fallback target when an entry's provider/model turns out invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Per-task-type provider/model routing — see `base::provider::resolve_task_models`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub task_models: HashMap<String, TaskModelOverride>,

    /// User language preference (e.g. "zh-CN", "ja").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    /// Feature flags — compile-time + runtime gate for experimental features.
    #[serde(default)]
    pub feature_flags: crate::features::FeatureFlags,
}

fn default_memory_enabled() -> bool {
    true
}

/// Permission mode for tool execution.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Prompt user for each tool call that isn't explicitly allowed.
    #[default]
    Default,
    /// Auto-accept edits to files (Write/Edit), prompt for others.
    AcceptEdits,
    /// Bypass all permission checks.
    BypassPermissions,
    /// Plan mode — only allow read-only tools.
    Plan,
    /// Auto mode — skip prompts for known-safe operations.
    /// **Program-only**: requires transcript classifier; cannot be set by user.
    Auto,
    /// Don't ask — deny any tool not explicitly allowed (no prompt).
    DontAsk,
    /// Bubble mode — forward permission requests up to a parent agent.
    /// **Program-only**: set by team/coordinator runtime; cannot be set by user.
    Bubble,
    /// YOLO mode — aggressive auto-approval for power users.
    Yolo,
}

impl PermissionMode {
    /// Whether the user can set this mode via settings.json / CLI.
    /// `Auto` and `Bubble` are program-only — they are set by the runtime
    /// (classifier activation, team coordinator) and rejected from user config.
    pub fn is_user_settable(self) -> bool {
        !matches!(self, Self::Auto | Self::Bubble)
    }

    /// How permissive this mode is, on a total order — higher grants more
    /// without asking.
    ///
    /// Exists so a host can *clamp* a mode it did not choose: the daemon's
    /// `session.create` RPC accepts a caller-supplied `permission_mode`, and
    /// without an ordering there was no way to express "a client may tighten
    /// this, but may not loosen it past what `settings.json` configured".
    /// Anyone who could reach the socket could simply ask for
    /// `bypassPermissions` and get it, whatever the daemon was configured
    /// with — see `daemon::session_pool::effective_permission_mode`.
    ///
    /// The ranking:
    /// - `DontAsk` refuses anything not explicitly allowed — nothing is more
    ///   restrictive.
    /// - `Plan` refuses every non-read-only tool.
    /// - `Default` asks about anything not explicitly allowed.
    /// - `Bubble` is `Default` with the question routed to a parent agent
    ///   rather than a human; the same set of calls still needs approval.
    /// - `AcceptEdits` additionally waves through `Write`/`Edit`.
    /// - `Auto` then `Yolo` hand progressively more of the decision to a
    ///   classifier.
    /// - `BypassPermissions` asks nothing at all.
    pub fn permissiveness(self) -> u8 {
        match self {
            Self::DontAsk => 0,
            Self::Plan => 1,
            Self::Default => 2,
            Self::Bubble => 3,
            Self::AcceptEdits => 4,
            Self::Auto => 5,
            Self::Yolo => 6,
            Self::BypassPermissions => 7,
        }
    }

    /// The stricter (less permissive) of two modes. Ties keep `self`.
    pub fn min_permissive(self, other: Self) -> Self {
        if other.permissiveness() < self.permissiveness() {
            other
        } else {
            self
        }
    }
}

/// A single permission rule: allow/deny/ask a tool matching a pattern.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionRule {
    /// Tool name pattern (e.g. "Bash", "Bash(git push:*)", "FileWrite").
    pub tool: String,
    /// Action: "allow", "deny", or "ask".
    pub action: PermissionAction,
    /// The tier whose settings file this rule came from. Set by
    /// `Settings::load`; `None` for rules built in code.
    #[serde(skip)]
    pub tier: Option<crate::interface::config_source::Tier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Deny,
    Ask,
}

/// LLM model configuration for a single provider.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelSettings {
    pub api_type: ApiType,
    pub base_url: String,
    pub auth_token: String,
    /// Resolved model name (upper layer already resolved slot → name).
    pub model_name: String,
    pub max_tokens: u32,
    pub thinking_mode: ThinkingMode,
    /// Fallback model for persistent Overloaded/529 errors (e.g. Opus → Sonnet).
    /// None = no fallback.
    #[serde(default)]
    pub fallback_model: Option<String>,
}

/// Path configuration for data directories.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PathSettings {
    /// User-level, scene-specific override root (e.g. `~/.atta/scenes/<scope>/`).
    pub user_data_dir: PathBuf,
    /// User-level, cross-scene global root — flat, shared by every scene
    /// (e.g. `~/.atta/`). Used by resources that don't have a scene tier
    /// (`memory`/`sessions`/`recordings`/`mcp`) and as the base layer for resources
    /// that do (`settings.json`/`skills`/`plugins`/`agents`/`rules`/`hooks`).
    /// See `base::paths::ConfigPaths` module docs for the full breakdown.
    #[serde(default)]
    pub global_data_dir: PathBuf,
    /// Local/project data root (e.g. `<cwd>/.atta/`)
    pub local_data_dir: PathBuf,
    /// Which product instance's user-level state `user_data_dir` was built
    /// for (see `base::paths::ConfigPaths`). Carried alongside the resolved
    /// dirs so downstream code (e.g. `FrozenContext::collect`) doesn't have
    /// to re-derive it from the path. Defaults to `"code"` on deserialize
    /// for settings.json files predating this field — `Settings::merge`
    /// never copies a deserialized `paths` value over the one `Settings::load`
    /// sets explicitly, so this default only matters for standalone
    /// (de)serialization, not for the daemon's actual startup path.
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "code".to_string()
}

impl PathSettings {
    /// The actual project working directory — **not** `local_data_dir`
    /// itself, which is `<project_root>/.atta` (flat, no scope segment; see
    /// `base::paths::ConfigPaths`). Several call sites used to pass
    /// `local_data_dir` straight to `FrozenContext::collect` as if it were
    /// the project root, which meant `AGENTS.md`/git-status discovery was
    /// scanning the `.atta/` directory instead of the real project — this is
    /// the fixed, single place that derives it correctly (falls back to
    /// `local_data_dir` itself if it has no parent, e.g. `.atta` sitting at
    /// filesystem root — not expected in practice).
    pub fn project_root(&self) -> PathBuf {
        self.local_data_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.local_data_dir.clone())
    }

    /// The project tier's `settings.local.json` — the file the interactive
    /// permission prompt's "always allow, this project" writes to.
    ///
    /// Inside `local_data_dir` (`<project_root>/.atta/`), not beside it: the
    /// Bash sandbox's deny-file-write rules (`tools::bash::sandbox`) protect
    /// exactly this path to stop a sandboxed command from granting itself
    /// permissions, and a copy written anywhere else would sit outside that
    /// protection while still being loaded by `Settings::load`.
    pub fn local_settings_file(&self) -> PathBuf {
        self.local_data_dir.join(SETTINGS_LOCAL_FILE)
    }
}

/// Execution constraints.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExecutionSettings {
    pub max_parallelism: usize,
    /// Ceiling on LLM round trips in one turn, before the turn ends with
    /// `stop_reason: "max_turns"`.
    ///
    /// This is a runaway guard, not a task budget. Every comparable agent
    /// runtime treats it that way and lands in the same range — the OpenAI
    /// Agents SDK defaults `max_turns` to 10, LangGraph's `recursion_limit`
    /// to 25, LangChain's `max_iterations` to 15. A long task is meant to be
    /// served by compaction, delegation and resuming, not by a high ceiling:
    /// raising this does not rescue an agent stuck in a loop, it only buys
    /// the loop more iterations before it stops.
    ///
    /// Raise it deliberately for a genuinely long single turn. Nothing here
    /// detects a *lack of progress* — a model repeating the same failing
    /// tool call burns the whole allowance either way.
    pub max_api_calls_per_turn: u32,
    /// Maximum cumulative tokens (input + output, summed across every API
    /// call in a turn) before the turn is aborted with `budget_exceeded`.
    /// `None` = unlimited.
    ///
    /// Deliberately token-based, not a dollar figure: provider prices change
    /// over time and per contract, so enforcing a dollar budget needs a
    /// per-model price table that can silently drift out of date — a stale
    /// or missing price makes the hard cap wrong in either direction. Token
    /// counts come straight from the provider's own usage accounting on
    /// every response, so there is nothing to keep in sync.
    pub max_budget_tokens: Option<u64>,
    /// How long a tool call waits for a host to answer a permission prompt
    /// before the call is denied. `0` waits indefinitely (only session
    /// cancellation interrupts it).
    ///
    /// The wait itself lives in the engine (`execute_tool_inner`), so the
    /// bound has to be reachable from `EngineConfig` — it previously existed
    /// only as a daemon-side flag, which meant any host driving the engine
    /// directly had no timeout at all.
    ///
    /// Failing closed (deny, not permit) on expiry is deliberate: a prompt
    /// that went unanswered is not consent.
    #[serde(default = "default_permission_prompt_timeout_secs")]
    pub permission_prompt_timeout_secs: u64,
    /// Maximum number of `TeamCreate` sub-agents dispatched concurrently
    /// within a single stage. Was a hardcoded constant in
    /// `crates/team/src/coordinator.rs`; default 6 matches the old value.
    #[serde(default = "default_team_stage_concurrency")]
    pub team_stage_concurrency: usize,
    /// Gates whether `TeamCreate`/`TeamDelete` are registered at all for a
    /// session (`runtime::agent::Builder::build()`). Default `false`: most
    /// deployments never use multi-agent team coordination, and an
    /// always-registered tool means its `tool.prompt.md` "When to Use"
    /// section is always in the system prompt and its `team`/`name` input
    /// fields are always in the `Agent` tool's schema, whether or not the
    /// operator ever intends to allow it. Flipping this only affects
    /// sessions built *after* the change (existing sessions keep whatever
    /// tool set they were built with — same as every other setting here);
    /// there is no separate restart requirement.
    #[serde(default)]
    pub team_enabled: bool,
    /// Reclaim policy for persistent team members (`Agent` tool's
    /// `team_name`+`name` mode): total pool size across a session. When the
    /// pool exceeds this, the oldest-idle members are stopped (never an
    /// `Active` one) until it's back at or under the cap — reclaims just
    /// enough, not everything.
    #[serde(default = "default_team_max_persistent_members")]
    pub team_max_persistent_members: usize,
    /// Reclaim policy for persistent team members: how long a member may
    /// sit idle (no message since its last turn finished) before it's
    /// stopped on its own, independent of the total-count cap above. Either
    /// condition triggers a reclaim sweep; they're independent, not
    /// combined.
    #[serde(default = "default_team_member_idle_timeout_secs")]
    pub team_member_idle_timeout_secs: u64,
}

fn default_permission_prompt_timeout_secs() -> u64 {
    300
}

fn default_team_stage_concurrency() -> usize {
    6
}

fn default_team_max_persistent_members() -> usize {
    20
}

fn default_team_member_idle_timeout_secs() -> u64 {
    1800 // 30 minutes
}

impl Default for ExecutionSettings {
    fn default() -> Self {
        Self {
            max_parallelism: 10,
            max_api_calls_per_turn: 25,
            max_budget_tokens: None,
            permission_prompt_timeout_secs: default_permission_prompt_timeout_secs(),
            team_stage_concurrency: default_team_stage_concurrency(),
            team_enabled: false,
            team_max_persistent_members: default_team_max_persistent_members(),
            team_member_idle_timeout_secs: default_team_member_idle_timeout_secs(),
        }
    }
}

/// Context compaction configuration.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompactionConfig {
    pub threshold_tokens: usize,
    pub keep_recent: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold_tokens: 150_000,
            keep_recent: 20,
        }
    }
}

/// Runtime control over the plugin subsystem, for a build that has it
/// compiled in.
///
/// The compile-time feature decides whether plugins *can* exist; this
/// decides whether they may. A deployment that wants none should prefer the
/// feature, since that leaves nothing to switch back on — this is for the
/// case where the same binary serves several deployments.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PluginsConfig {
    /// Load installed plugins at all.
    pub enabled: bool,
    /// When non-empty, only these plugin names load. An allow-list, so a
    /// name that is not on it is refused rather than merely unmentioned.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow: Vec::new(),
        }
    }
}

impl PluginsConfig {
    /// May `name` load under this configuration?
    pub fn permits(&self, name: &str) -> bool {
        self.enabled && (self.allow.is_empty() || self.allow.iter().any(|a| a == name))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
/// Sandbox/security configuration.
pub struct SandboxConfig {
    pub deny_read: Vec<PathBuf>,
    /// Paths to re-allow reading, on top of the built-in credential deny
    /// defaults (`~/.ssh`, `~/.aws`, `~/.npmrc`, ...). Most specific wins.
    ///
    /// Exists because those defaults are now actually applied to `Bash` (they
    /// used to be defined and never reached production), and some of them
    /// sit on legitimate workflows — `npm install` against a private registry
    /// reads `~/.npmrc`, `docker build` reads `~/.docker/config.json`.
    /// Without this there was no way to say "yes, that one is fine here".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_read: Vec<PathBuf>,
    /// Extra filename segments a tool may never write, on top of the built-in
    /// credential defaults (`.env`, `id_rsa`, `.ssh`, `.atta`, ...). A path is
    /// refused if *any* of its components matches one, or begins with one
    /// followed by a dot — so `.env` also covers `.env.production`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_write: Vec<String>,
    /// Paths to re-allow writing, on top of the built-in deny defaults. Most
    /// specific wins.
    ///
    /// The write-side counterpart of [`allow_read`](Self::allow_read), and it
    /// exists for the same reason: the defaults sit on real workflows. A
    /// project that keeps a checked-in `.env.example`, or a deployment whose
    /// agent is meant to maintain a private-registry `.npmrc`, has to be able
    /// to say "that one is fine here".
    ///
    /// This exempts a path from the *deny* rules only. Where a tool may write
    /// at all is a different question, answered by `additional_directories` —
    /// keeping them apart is what stops one setting quietly widening the
    /// project boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_write: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
    /// Outbound network policy for sandboxed `Bash` commands.
    #[serde(default)]
    pub network_mode: crate::context::config::NetworkModeConfig,
    /// Bypass the sandbox entirely, in a build that has one.
    ///
    /// Default: false. Note what that does *not* mean: the backend is behind
    /// the `sandbox` compile-time feature, which is off by default, so an
    /// ordinary build has nothing to bypass and this setting changes nothing
    /// in it. It is the runtime half of a two-part answer — the build decides
    /// whether a sandbox exists, this decides whether to use the one it has.
    #[serde(default)]
    pub dangerously_disable_sandbox: bool,
    /// Refuse to run a sandboxed command when this build on this platform
    /// cannot actually enforce the policy, instead of running it
    /// unconstrained.
    ///
    /// Two ways to be unable: the `sandbox` compile-time feature is off, so no
    /// backend was compiled at all — the default — or it is on and the
    /// platform's tool is missing. Both refuse here, and the message says
    /// which.
    ///
    /// Off by default, which is the historical behavior: on Linux without
    /// `bwrap`, and on platforms with no backend at all, a command whose
    /// policy cannot be honored runs anyway with a warning. That is a silent
    /// downgrade from "constrained" to "unconstrained" — fine for a laptop,
    /// not fine for a host that treats the sandbox as a boundary.
    ///
    /// Turning it on makes such a command fail with an error naming the
    /// missing backend. The default is `false` rather than `true` only
    /// because flipping it would break every Linux box without bubblewrap
    /// installed, and every Windows host, on upgrade.
    #[serde(default)]
    pub require_enforcement: bool,
    /// The environment `Bash` / `Monitor` commands and command hooks start
    /// from, sandboxed or not. By default the host's environment minus the
    /// engine's own credentials (`ANTHROPIC_*`, `ATTACORE_DAEMON_TOKEN`);
    /// `"mode": "allowlist"` keeps only `PATH`, `HOME`, the locale and a few
    /// others, plus whatever `allow` names. MCP stdio servers always start from
    /// the default, with each server's own `env` added on top.
    #[serde(default)]
    pub subprocess_env: crate::interface::exec::local::SubprocessEnv,
}

/// One script, bound to one extension point.
///
/// ```jsonc
/// "scripts": [
///   { "path": ".atta/scripts/prompt.js", "point": "prompt.assemble", "entry": "onAssemble" }
/// ]
/// ```
///
/// The point is a catalog id — see `base::interface::catalog` and
/// `docs/extension_points.md`. A binding naming a point the engine does not
/// have, or one scripts may not use, is refused at startup with a message
/// saying which; it is not silently ignored, because a script that quietly
/// never runs is worse than one that fails to load.
///
/// Authority follows the file's location, not the configuration: a script
/// inside the project is the operator's own and may rewrite anything, while
/// one that arrived with a plugin may add. Nothing in this struct can change
/// that, which is deliberate — it would otherwise be a field a plugin could
/// set for itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScriptBinding {
    /// Path to the script, relative to the project root or absolute.
    pub path: PathBuf,
    /// Which extension point, by its catalog id.
    pub point: String,
    /// The function the script exports.
    pub entry: String,
    /// Wall clock for one call. Defaults to the carrier's own budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// How many times it may run in a turn. Defaults to the carrier's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls_per_turn: Option<u32>,
}

/// Recorder (record/replay) configuration. `None` on [`Settings`] leaves the
/// model wrapper a pass-through.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecorderConfig {
    pub mode: RecorderMode,
    /// Directory name for this recording under `root`. `None` uses the session
    /// id, which is what production recording wants; tests name theirs so a
    /// replay can find it again.
    #[serde(default)]
    pub name: Option<String>,
    /// Where recordings live. Each one is a self-contained directory beneath
    /// it, so a host retires a recording by deleting that directory.
    pub root: PathBuf,
    #[serde(default)]
    pub on_divergence: Divergence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecorderMode {
    Record,
    Replay,
    /// Re-issue a recording's requests against a real provider.
    ///
    /// The third mode exists because the other two cannot answer "what would
    /// the model have said if the prompt were different". `Replay` never reads
    /// a request's content — it compares blob *ids* and returns the recorded
    /// response — so editing a blob changes nothing it observes. `Rerun` loads
    /// the request back from the blobs (picking up any edit), sends it, and
    /// reports how the new response differs from the recorded one.
    Rerun,
}

/// What replay does when a live request differs from the one recorded at the
/// same position.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Divergence {
    /// Fail the call, naming the fields that differ. The default wherever a
    /// divergence means a broken test.
    #[default]
    Strict,
    /// Report the difference and play the recorded response anyway.
    Warn,
}

fn default_true() -> bool {
    true
}

/// Model thinking/reasoning mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    Auto,
    Off,
    On,
    OnBudget(u32),
}

impl Settings {
    /// Load settings from user and local directories, with ENV override.
    /// Priority (low → high): `global_dir/settings.json` (shared by every
    /// scene) → `scene_dir/settings.json` → `local_dir/settings.json`
    /// (project). Each tier is immediately followed by its own gitignored
    /// `settings.local.json` overlay, which outranks the `settings.json`
    /// beside it. This is the **single canonical settings.json loader** —
    /// `daemon` and any other embedder should call this rather than parsing
    /// settings.json themselves. An embedder whose configuration does not
    /// live on this machine's disk calls [`load_from`](Self::load_from) with
    /// its own source and gets the same merge.
    ///
    /// Merging is a **generic recursive JSON merge** (later layer's object
    /// keys override/extend the earlier layer's, non-object values fully
    /// replace) applied directly to the JSON representation, not a
    /// hand-written field-by-field `Settings` merge — this is why adding a
    /// new field to `Settings` needs no changes here: it flows through
    /// automatically. `paths` is deliberately excluded from every layer
    /// before merging — a settings.json file cannot override where its own
    /// layers live, that's decided by the caller's `global_dir`/`scene_dir`/
    /// `local_dir`/`scope` arguments alone.
    ///
    /// **Never fails** — a layer that doesn't exist is skipped; a layer that
    /// fails to parse is skipped with a `tracing::warn!` (path + error)
    /// rather than aborting startup or silently losing the problem. If the
    /// fully-merged JSON somehow doesn't deserialize into `Settings` (e.g. a
    /// layer put a string where an object was expected), the merge is
    /// discarded with a warning and plain defaults are returned instead.
    ///
    /// `scope` identifies which product instance `scene_dir` belongs to (see
    /// `base::paths::ConfigPaths`) — no default at this layer; callers decide.
    ///
    /// `default_model` seeds `model.model_name` **before** any layer is
    /// merged in — same priority tier as `defaults_for`'s own hardcoded
    /// default, i.e. lower priority than every settings.json layer. This
    /// lets a caller's CLI `--model` flag act as a fallback (only takes
    /// effect when no settings.json layer specifies one) rather than a hard
    /// override.
    pub fn load(
        global_dir: PathBuf,
        scene_dir: PathBuf,
        local_dir: PathBuf,
        scope: &str,
        default_model: &str,
    ) -> Self {
        let source = crate::interface::config_source::FileTiers::new(
            global_dir.clone(),
            scene_dir.clone(),
            local_dir.clone(),
        );
        Self::load_from(
            &source,
            global_dir,
            scene_dir,
            local_dir,
            scope,
            default_model,
        )
    }

    /// [`load`](Self::load) with the layers coming from wherever `source` says
    /// instead of from the six files on disk.
    ///
    /// The merge is identical — that is the point of the split. `source`
    /// decides only which layers exist, in what order, and what JSON is in
    /// them; everything after that (stripping `paths`, holding an overlay's
    /// `permission_rules` apart, the recursive merge, the fall back to
    /// defaults if the result will not deserialize, validation) happens here
    /// and is the same for every source.
    ///
    /// The directory arguments stay because they are not where configuration
    /// is read from — they are where this scene's *data* lives, which
    /// `settings.paths` reports and which a settings file is never allowed to
    /// override.
    pub fn load_from(
        source: &dyn crate::interface::config_source::ConfigSource,
        global_dir: PathBuf,
        scene_dir: PathBuf,
        local_dir: PathBuf,
        scope: &str,
        default_model: &str,
    ) -> Self {
        let base = Self::defaults_for(default_model);
        let mut merged_json = serde_json::to_value(&base).unwrap_or_else(|_| serde_json::json!({}));

        let mut permission_rules: Vec<PermissionRule> = Vec::new();
        let mut local_permission_rules: Vec<PermissionRule> = Vec::new();

        for layer in source.layers() {
            let mut layer_json = layer.value;
            if let Some(obj) = layer_json.as_object_mut() {
                // `paths` is resolved from this function's own arguments,
                // never from settings.json content.
                obj.remove("paths");
                if let Some(rules) = obj.remove("permission_rules") {
                    match serde_json::from_value::<Vec<PermissionRule>>(rules) {
                        Ok(rules) => {
                            let tagged = rules.into_iter().map(|mut r| {
                                r.tier = Some(layer.tier);
                                r
                            });
                            if layer.machine_local {
                                local_permission_rules.extend(tagged);
                            } else {
                                permission_rules.extend(tagged);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(layer = layer.tier.name(), origin = %layer.origin, error = %e, "failed to parse permission_rules, ignoring them");
                        }
                    }
                }
            }
            merge_json_values(&mut merged_json, layer_json);
        }

        let mut settings: Settings = match serde_json::from_value(merged_json) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "merged settings.json failed to deserialize into Settings; falling back to defaults");
                base
            }
        };

        settings.permission_rules = permission_rules;
        settings.local_permission_rules = local_permission_rules;

        settings.paths = PathSettings {
            user_data_dir: scene_dir,
            global_data_dir: global_dir,
            local_data_dir: local_dir,
            scope: scope.to_string(),
        };

        // Validate: reject program-only permission modes from user config.
        if let Err(reason) = settings.validate() {
            tracing::warn!("Settings validation: {reason}");
            settings.permission_mode = PermissionMode::default();
        }

        settings
    }

    /// Validate settings consistency. Returns Err on invalid combinations.
    pub fn validate(&self) -> Result<(), String> {
        if !self.permission_mode.is_user_settable() {
            return Err(format!(
                "Permission mode '{:?}' is program-only and cannot be set in settings.json. \
                 Defaulting to Default.",
                self.permission_mode
            ));
        }
        Ok(())
    }

    /// Quick default for a given model name.
    pub fn defaults_for(model_name: &str) -> Self {
        Self {
            model: ModelSettings {
                api_type: ApiType::Anthropic,
                base_url: String::new(),
                auth_token: String::new(),
                model_name: model_name.to_string(),
                max_tokens: 2000,
                thinking_mode: ThinkingMode::Auto,
                fallback_model: None,
            },
            paths: PathSettings {
                user_data_dir: PathBuf::from("~/.atta/scenes/agent"),
                global_data_dir: PathBuf::from("~/.atta"),
                local_data_dir: PathBuf::from("."),
                scope: default_scope(),
            },
            execution: ExecutionSettings::default(),
            compaction: CompactionConfig::default(),
            sandbox: SandboxConfig::default(),
            plugins: PluginsConfig::default(),
            scripts: Vec::new(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            telemetry_enabled: true,
            allow_client_permission_override: false,
            session_dir: None,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            local_permission_rules: Vec::new(),
            hooks_config: None,
            mcp_servers: HashMap::new(),
            providers: HashMap::new(),
            default_provider: None,
            task_models: HashMap::new(),
            language: None,
            feature_flags: crate::features::FeatureFlags::default(),
        }
    }
}

/// JSON Schema for settings.json, generated from `Settings`'s own type
/// definitions (`#[derive(schemars::JsonSchema)]` on `Settings` and every
/// nested type) — not hand-maintained, so it can never drift from what
/// `Settings::load()` actually accepts. Published as `docs/schemas/settings.schema.json`
/// for editor autocomplete/validation (reference it from a settings.json file
/// via a `"$schema"` key). Regenerate with:
/// `cargo test -p base settings_schema_matches_committed_file -- --ignored`
/// after changing any `Settings`-reachable type (see that test for the exact
/// write-back invocation).
pub fn settings_json_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Settings)).unwrap_or(serde_json::Value::Null)
}

/// Recursively merge `over` into `base` in place: objects merge key-by-key
/// (recursing into shared keys), anything else (arrays, scalars, null) fully
/// replaces the corresponding `base` value. This is the same algorithm
/// `Settings::load()` uses to merge global/scene/project tiers, exposed here
/// so other crates (e.g. `daemon`'s `config.setProvider` RPC) can apply a
/// partial patch to a single settings.json tier with identical semantics.
pub fn merge_json_values(base: &mut serde_json::Value, over: serde_json::Value) {
    match (base, over) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(over_map)) => {
            for (k, v) in over_map {
                match base_map.get_mut(&k) {
                    Some(existing) => merge_json_values(existing, v),
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
        }
        (base_slot, over_val) => {
            *base_slot = over_val;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_execution_settings() {
        let s = ExecutionSettings::default();
        assert_eq!(s.max_parallelism, 10);
        assert_eq!(s.max_api_calls_per_turn, 25);
    }

    #[test]
    fn permission_mode_deserializes_bubble_and_dontask() {
        // dontAsk is user-settable; bubble and auto are program-only. Settings
        // must deserialize all but validation rejects non-user-settable modes.
        let b: PermissionMode = serde_json::from_str("\"bubble\"").unwrap();
        assert_eq!(b, PermissionMode::Bubble);
        assert!(!b.is_user_settable(), "Bubble is program-only");

        let a: PermissionMode = serde_json::from_str("\"auto\"").unwrap();
        assert_eq!(a, PermissionMode::Auto);
        assert!(!a.is_user_settable(), "Auto is program-only");

        let d: PermissionMode = serde_json::from_str("\"dontAsk\"").unwrap();
        assert_eq!(d, PermissionMode::DontAsk);
        assert!(d.is_user_settable(), "DontAsk is user-settable");

        // Validation: Bubble in settings is rejected.
        let s = Settings {
            permission_mode: PermissionMode::Bubble,
            ..Settings::defaults_for("test")
        };
        assert!(s.validate().is_err());

        // Validation: Auto in settings is rejected.
        let s = Settings {
            permission_mode: PermissionMode::Auto,
            ..Settings::defaults_for("test")
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn merge_json_values_recurses_into_nested_objects() {
        let mut base = serde_json::json!({
            "providers": {
                "deepseek": { "api_key": "old-key", "default_model": "deepseek-pro" }
            }
        });
        let over = serde_json::json!({
            "providers": {
                "deepseek": { "api_key": "new-key" }
            }
        });
        merge_json_values(&mut base, over);
        assert_eq!(base["providers"]["deepseek"]["api_key"], "new-key");
        // Untouched sibling field survives — this is the whole point of a
        // recursive merge over a whole-value replace.
        assert_eq!(
            base["providers"]["deepseek"]["default_model"],
            "deepseek-pro"
        );
    }

    #[test]
    fn merge_json_values_non_object_values_fully_replace() {
        let mut base = serde_json::json!({ "mcp_servers": {"a": 1} });
        let over = serde_json::json!({ "mcp_servers": {"b": 2} });
        merge_json_values(&mut base, over);
        // Both are objects, so this recurses (keeps "a", adds "b") — objects
        // always merge key-by-key, never fully replace each other.
        assert_eq!(base["mcp_servers"]["a"], 1);
        assert_eq!(base["mcp_servers"]["b"], 2);

        let mut base2 = serde_json::json!({ "language": "en" });
        let over2 = serde_json::json!({ "language": "zh-CN" });
        merge_json_values(&mut base2, over2);
        assert_eq!(base2["language"], "zh-CN");
    }

    fn write_settings(dir: &std::path::Path, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(SETTINGS_FILE), content).unwrap();
    }

    fn write_local_settings(dir: &std::path::Path, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(SETTINGS_LOCAL_FILE), content).unwrap();
    }

    #[test]
    fn local_overlay_outranks_the_settings_json_beside_it() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        write_settings(&project, r#"{"model": {"model_name": "committed"}}"#);
        write_local_settings(&project, r#"{"model": {"model_name": "machine-local"}}"#);

        let settings = Settings::load(
            root.path().join("global"),
            root.path().join("scene"),
            project,
            "code",
            "m",
        );
        assert_eq!(settings.model.model_name, "machine-local");
    }

    /// The local overlay's rules must *add to* the committed tier's, not
    /// replace them the way the generic array merge would — both sets have to
    /// reach the rule engine for `RuleSource::LocalSettings`'s higher priority
    /// to decide anything.
    #[test]
    fn local_overlay_permission_rules_add_to_rather_than_replace_the_committed_ones() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        write_settings(
            &project,
            r#"{"permission_rules": [{"tool": "Bash(rm:*)", "action": "deny"}]}"#,
        );
        write_local_settings(
            &project,
            r#"{"permission_rules": [{"tool": "Bash(ls)", "action": "allow"}]}"#,
        );

        let settings = Settings::load(
            root.path().join("global"),
            root.path().join("scene"),
            project,
            "code",
            "m",
        );
        assert_eq!(
            settings
                .permission_rules
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["Bash(rm:*)"],
            "the committed tier's rules must survive the overlay"
        );
        assert_eq!(
            settings
                .local_permission_rules
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["Bash(ls)"]
        );
    }

    #[test]
    fn local_overlay_is_read_from_every_tier() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let project = root.path().join("project");
        write_local_settings(
            &global,
            r#"{"permission_rules": [{"tool": "Bash(id)", "action": "allow"}]}"#,
        );
        write_local_settings(
            &project,
            r#"{"permission_rules": [{"tool": "Bash(ls)", "action": "allow"}]}"#,
        );

        let settings = Settings::load(global, root.path().join("scene"), project, "code", "m");
        assert_eq!(
            settings
                .local_permission_rules
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["Bash(id)", "Bash(ls)"]
        );
    }

    #[test]
    fn committed_tiers_contribute_rules_side_by_side_each_tagged_with_its_tier() {
        use crate::interface::config_source::Tier;
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let scene = root.path().join("scene");
        let project = root.path().join("project");
        write_settings(
            &global,
            r#"{"permission_rules": [{"tool": "Bash(rm:*)", "action": "deny"}]}"#,
        );
        write_settings(
            &scene,
            r#"{"permission_rules": [{"tool": "Read(~/.ssh/**)", "action": "deny"}]}"#,
        );
        write_settings(
            &project,
            r#"{"permission_rules": [{"tool": "Bash(ls)", "action": "allow"}]}"#,
        );

        let settings = Settings::load(global, scene, project, "code", "m");
        assert_eq!(
            settings
                .permission_rules
                .iter()
                .map(|r| (r.tool.as_str(), r.tier))
                .collect::<Vec<_>>(),
            vec![
                ("Bash(rm:*)", Some(Tier::Global)),
                ("Read(~/.ssh/**)", Some(Tier::Scene)),
                ("Bash(ls)", Some(Tier::Project)),
            ],
            "a project tier that writes any rule must not erase the user's"
        );
    }

    #[test]
    fn local_overlay_can_never_override_paths_either() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        write_local_settings(&global, r#"{"paths": {"scope": "hijacked"}}"#);

        let settings = Settings::load(
            global,
            root.path().join("scene"),
            root.path().join("project"),
            "real-scope",
            "m",
        );
        assert_eq!(settings.paths.scope, "real-scope");
    }

    #[test]
    fn load_merges_three_tiers_low_to_high() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let scene = root.path().join("scene");
        let project = root.path().join("project");
        write_settings(&global, r#"{"model": {"model_name": "global-model"}}"#);
        write_settings(&scene, r#"{"model": {"model_name": "scene-model"}}"#);
        write_settings(&project, r#"{}"#);

        let settings = Settings::load(global, scene, project, "code", "cli-default");
        assert_eq!(settings.model.model_name, "scene-model");
    }

    /// P3-14's acceptance: the same configuration, once from six files and
    /// once from a source that never touches a disk, has to produce the same
    /// `Settings` — including the two rules that are not part of the generic
    /// merge (an overlay's `permission_rules` held apart, `paths` ignored).
    #[test]
    fn an_in_memory_source_replaces_the_files_entirely() {
        use crate::interface::config_source::{InMemoryLayers, Tier};

        let global = r#"{"model": {"model_name": "global-model", "max_tokens": 111}}"#;
        let scene = r#"{"model": {"model_name": "scene-model"}}"#;
        let project = r#"{"paths": {"scope": "hijacked"},
                          "permission_rules": [{"tool": "Bash(rm:*)", "action": "deny"}]}"#;
        let project_local = r#"{"permission_rules": [{"tool": "Bash(ls)", "action": "allow"}]}"#;

        let root = tempfile::tempdir().unwrap();
        let global_dir = root.path().join("global");
        let scene_dir = root.path().join("scene");
        let project_dir = root.path().join("project");
        write_settings(&global_dir, global);
        write_settings(&scene_dir, scene);
        write_settings(&project_dir, project);
        write_local_settings(&project_dir, project_local);
        let from_files = Settings::load(global_dir, scene_dir, project_dir, "code", "cli-default");

        let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        let source = InMemoryLayers::new()
            .with(Tier::Global, false, "config-service:global", parse(global))
            .with(Tier::Scene, false, "config-service:scene", parse(scene))
            .with(
                Tier::Project,
                false,
                "config-service:project",
                parse(project),
            )
            .with(
                Tier::Project,
                true,
                "config-service:machine",
                parse(project_local),
            );
        // Directories that do not exist, so a fall-back to reading files
        // could not produce this result.
        let elsewhere = root.path().join("no-such-tree");
        let from_memory = Settings::load_from(
            &source,
            elsewhere.join("global"),
            elsewhere.join("scene"),
            elsewhere.join("project"),
            "code",
            "cli-default",
        );

        assert_eq!(from_memory.model.model_name, from_files.model.model_name);
        assert_eq!(from_memory.model.model_name, "scene-model");
        assert_eq!(from_memory.model.max_tokens, from_files.model.max_tokens);
        assert_eq!(
            from_memory
                .permission_rules
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["Bash(rm:*)"]
        );
        assert_eq!(
            from_memory
                .local_permission_rules
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["Bash(ls)"]
        );
        assert_eq!(from_memory.paths.scope, "code");
        assert_eq!(from_memory.paths.local_data_dir, elsewhere.join("project"));
    }

    #[test]
    fn load_falls_back_to_cli_default_model_when_no_layer_sets_it() {
        let root = tempfile::tempdir().unwrap();
        let settings = Settings::load(
            root.path().join("global"),
            root.path().join("scene"),
            root.path().join("project"),
            "code",
            "cli-default-model",
        );
        assert_eq!(settings.model.model_name, "cli-default-model");
    }

    #[test]
    fn load_lets_project_override_just_one_provider_field() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let project = root.path().join("project");
        write_settings(
            &global,
            r#"{"providers": {"deepseek": {"api_key": "global-key", "default_model": "deepseek-pro"}}}"#,
        );
        write_settings(
            &project,
            r#"{"providers": {"deepseek": {"api_key": "project-key"}}}"#,
        );

        let settings = Settings::load(global, root.path().join("scene"), project, "code", "m");
        let deepseek = &settings.providers["deepseek"];
        assert_eq!(deepseek.api_key.as_deref(), Some("project-key"));
        assert_eq!(deepseek.default_model.as_deref(), Some("deepseek-pro"));
    }

    #[test]
    fn load_never_lets_settings_json_override_paths() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        write_settings(&global, r#"{"paths": {"scope": "hijacked"}}"#);

        let settings = Settings::load(
            global,
            root.path().join("scene"),
            root.path().join("project"),
            "real-scope",
            "m",
        );
        assert_eq!(settings.paths.scope, "real-scope");
    }

    #[test]
    fn load_malformed_layer_warns_and_is_skipped_not_fatal() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        write_settings(&global, "not valid json {{{");
        // Must not panic — malformed layer is skipped, defaults are used.
        let settings = Settings::load(
            global,
            root.path().join("scene"),
            root.path().join("project"),
            "code",
            "fallback-model",
        );
        assert_eq!(settings.model.model_name, "fallback-model");
    }

    #[test]
    fn settings_json_schema_has_expected_top_level_properties() {
        let schema = settings_json_schema();
        let props = schema["properties"]
            .as_object()
            .expect("schema should have a top-level `properties` object");
        for key in [
            "model",
            "paths",
            "providers",
            "default_provider",
            "task_models",
            "hooks_config",
        ] {
            assert!(props.contains_key(key), "schema missing property `{key}`");
        }
    }

    /// Regenerates `docs/schemas/settings.schema.json` from the current
    /// `Settings` type definitions and asserts it matches what's committed —
    /// fails (rather than silently drifting) if a `Settings`-reachable type
    /// changed without regenerating the schema. `#[ignore]`d because it
    /// writes to the repo tree; not run by default `cargo test`.
    ///
    /// To regenerate after a real schema change: run this test once with
    /// `cargo test -p base settings_schema_matches_committed_file -- --ignored`
    /// (it writes the file even on "mismatch" before asserting), then commit
    /// the updated `docs/schemas/settings.schema.json`.
    #[test]
    #[ignore]
    fn settings_schema_matches_committed_file() {
        let schema = settings_json_schema();
        let pretty = serde_json::to_string_pretty(&schema).unwrap() + "\n";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/schemas/settings.schema.json");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing != pretty {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &pretty).unwrap();
        }
        assert_eq!(existing, pretty, "docs/schemas/settings.schema.json was out of date and has been regenerated — review the diff and commit it");
    }

    #[test]
    fn load_missing_directories_returns_plain_defaults() {
        let root = tempfile::tempdir().unwrap();
        let settings = Settings::load(
            root.path().join("nonexistent-global"),
            root.path().join("nonexistent-scene"),
            root.path().join("nonexistent-project"),
            "code",
            "fallback-model",
        );
        assert_eq!(settings.model.model_name, "fallback-model");
        assert!(settings.providers.is_empty());
    }
}

#[cfg(test)]
mod plugins_policy_tests {
    use super::PluginsConfig;

    /// A build with plugins compiled in serves them unless told otherwise —
    /// the compile-time feature is where "never" belongs.
    #[test]
    fn the_default_permits_everything() {
        let c = PluginsConfig::default();
        assert!(c.enabled);
        assert!(c.permits("anything"));
    }

    #[test]
    fn disabling_refuses_every_plugin() {
        let c = PluginsConfig {
            enabled: false,
            allow: vec!["github-tools".into()],
        };
        assert!(
            !c.permits("github-tools"),
            "an allow-list does not override being switched off"
        );
    }

    /// An allow-list is an allow-list: a name that is not on it is refused,
    /// not merely unmentioned.
    #[test]
    fn a_non_empty_allow_list_excludes_everything_else() {
        let c = PluginsConfig {
            enabled: true,
            allow: vec!["github-tools".into()],
        };
        assert!(c.permits("github-tools"));
        assert!(!c.permits("something-else"));
    }
}
