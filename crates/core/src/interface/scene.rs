//! `AgentScene` trait — defines agent behavior for a domain.
//!
//! Scenes are code-level (compile-time), bound at Engine creation, immutable.

use crate::prompt::PromptBlock;
use std::borrow::Cow;

/// Context passed to `AgentScene::build_system_prompt()`.
#[derive(Debug, Clone)]
pub struct ScenePromptContext<'a> {
    pub cwd: Cow<'a, str>,
    pub os: Cow<'a, str>,
    pub shell: Cow<'a, str>,
    pub home_dir: Cow<'a, str>,
    /// Current date string (e.g. "2026-06-10")
    pub date: Cow<'a, str>,
    /// Resolved model name (e.g. "claude-sonnet-4-6")
    pub model_name: Cow<'a, str>,
    pub skills_text: Option<Cow<'a, str>>,
    pub mcp_instructions: Option<Cow<'a, str>>,
    pub session_memory: Option<Cow<'a, str>>,
    /// Whether the working directory is a git repository.
    pub is_git: bool,
    /// Current git branch name (e.g. "main").
    pub git_branch: Option<Cow<'a, str>>,
    /// Whether the cwd is a git worktree.
    pub is_worktree: bool,
    /// Raw git status output (truncated).
    pub git_status: Option<Cow<'a, str>>,
    /// User's language preference (e.g. "zh-CN"). None = no preference.
    pub language: Option<Cow<'a, str>>,
    /// Scratchpad directory path for temporary files.
    pub scratchpad_dir: Option<Cow<'a, str>>,
    /// Output style content (loaded from config). None = default style.
    pub output_style_content: Option<Cow<'a, str>>,
    /// Comma-separated list of tool names available in this session.
    /// Used to conditionally include tool-specific guidance.
    pub available_tools: Option<Cow<'a, str>>,
    /// Whether any tool result has actually been cleared (time-based/cached
    /// micro-compact, tool-result budget enforcement, or a `MicroCompact`
    /// main-compaction pass) at any point in this session. Gates
    /// `CodingScene`'s `function_result_clearing` prompt section — without
    /// this it was injected unconditionally every turn even when nothing
    /// had ever been cleared.
    pub tool_results_ever_cleared: bool,
}

/// Context for building the `<system-reminder>` block.
#[derive(Debug, Clone)]
pub struct ReminderContext<'a> {
    pub cwd: Cow<'a, str>,
    pub git_status: Option<Cow<'a, str>>,
    pub memory_summary: Option<Cow<'a, str>>,
}

/// Token budget configuration for a scene.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// Trigger auto-compact when input exceeds this threshold (0 = disabled).
    pub compact_threshold: usize,
    /// Number of recent messages to keep during compaction.
    pub compact_keep_recent: usize,
}

/// Execution limits a scene imposes on its own turns.
///
/// This type used to carry `max_parallelism` and `max_agent_depth` as well.
/// Both were removed rather than left as decoration, because a limit that
/// constrains nothing is worse than an absent one — it reads like a guarantee:
///
/// - `max_parallelism`: tool-call fan-out is bounded by `EngineConfig`, which
///   is derived from `Settings`, not from the scene; there is no scene-level
///   consumer to route it to without inverting that ownership.
/// - `max_agent_depth`: delegation depth *is* enforced now, by
///   `AgentTool::Inner::spawn_guard` counting against
///   `EngineConfig::max_agent_depth` on every spawn path. It stays on
///   `EngineConfig` rather than moving here — the bound belongs to the
///   engine, and a scene that wants a different one changes that config.
///
/// What remains is honoured — see `runtime::turn`'s `max_calls`.
#[derive(Debug, Clone)]
pub struct ExecutionParams {
    /// Ceiling on LLM round trips within a single turn for this scene.
    ///
    /// Combined with `Settings.execution.max_api_calls_per_turn` by taking
    /// the **lower** of the two: a scene declaring a tight loop budget is
    /// making a statement about its own workload that a deployment-wide
    /// setting should not be able to silently widen, and a deployment that
    /// wants to be stricter than the scene still gets its way.
    ///
    /// Defaults to no ceiling, so a scene that says nothing lets the setting
    /// decide. It used to default to the same number the setting did, which
    /// made the `min` a no-op in one direction and a silent clamp in the
    /// other: an operator raising the setting for a long-running task got
    /// the scene's number back with nothing to indicate why.
    pub max_api_calls_per_turn: u32,
}

impl Default for ExecutionParams {
    fn default() -> Self {
        Self {
            max_api_calls_per_turn: u32::MAX,
        }
    }
}

/// The central trait that defines a scene (domain) for the AGENT.
///
/// Implementations are code-level (Rust source files), registered at compile time,
/// bound to an Engine instance at creation, and immutable thereafter.
pub trait AgentScene: Send + Sync + 'static {
    /// Unique scene identifier (e.g. "coding", "demo").
    fn id(&self) -> &str;

    /// Human-readable name.
    fn name(&self) -> &str;

    /// Short description.
    fn description(&self) -> &str;

    /// Build the system prompt skeleton as protocol-agnostic blocks.
    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock>;

    /// Tool whitelist for this scene (empty = all registered tools).
    fn tools(&self) -> Vec<String>;

    /// Whether a session in this scene must be bound to a project root.
    ///
    /// A host reads this to decide whether creating a session needs a
    /// "choose a project" step; the daemon enforces it, refusing a session
    /// with no `project_root` for a scene that requires one. Scenes whose
    /// work *is* a project (writing code, researching a codebase) say yes;
    /// a chat has nothing to bind to.
    fn requires_project(&self) -> bool {
        false
    }

    /// Whether this scene takes part in multi-agent teams.
    ///
    /// `false` means the Team tools are never registered for the session at
    /// all — the model does not see them and cannot be refused at call time,
    /// which is a different thing from being allowed to try and failing.
    fn supports_team(&self) -> bool {
        false
    }

    /// Tools this scene contributes that no other scene has.
    ///
    /// The three-way split between this, `tools()` and `deferred_tools()` is
    /// laid out in docs/ARCHITECTURE.md §3.5.
    ///
    /// [`tools`](Self::tools) can only ever *narrow* — it is a whitelist
    /// intersected with whatever registry the host assembled, so a scene had
    /// no way to offer a capability the host did not already know about. This
    /// is the other direction: the returned tools are registered into the
    /// session's own registry, so a scene owns its tool surface in both
    /// directions.
    ///
    /// Registered before [`deferred_tools`](Self::deferred_tools) is applied,
    /// so a scene can defer its own contributions like any other tool.
    ///
    /// A name already present in the registry is **rejected**, not
    /// substituted — silently shadowing `Bash` or `Edit` from a scene
    /// definition would be indistinguishable from the real thing at the call
    /// site. `Builder::build()` fails with `EngineError::Internal`. Returning
    /// tools that need session state (permissions, MCP clients, the spawner)
    /// is out of scope here: this runs with no session context, so it suits
    /// self-contained tools, the same constraint
    /// `tools::register_builtin_tools` works under.
    fn extra_tools(&self) -> Vec<std::sync::Arc<dyn crate::tool::Tool>> {
        vec![]
    }

    /// Token budget configuration.
    fn token_budget(&self) -> TokenBudget;

    // ── Agix-specific extensions (default noop) ──

    /// Build the `<system-reminder>` content injected before each turn.
    fn build_system_reminder(&self, _ctx: &ReminderContext) -> String {
        String::new()
    }

    /// Tools explicitly disallowed in this scene.
    fn disallowed_tools(&self) -> Vec<String> {
        vec![]
    }

    /// Whether a session in this scene may have `name` at all: listed in a
    /// non-empty [`tools`](Self::tools) whitelist (an entry ending in `*`,
    /// such as `mcp__*`, covers every name with that prefix), or, with an
    /// empty whitelist, not in [`disallowed_tools`](Self::disallowed_tools).
    ///
    /// The session's tool registry is filtered by this, so advertising,
    /// `ToolSearch` and dispatch all see the same set.
    fn permits_tool(&self, name: &str) -> bool {
        let entry_covers = |entry: &String| match entry.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => entry == name,
        };
        let allowed = self.tools();
        if allowed.is_empty() {
            !self.disallowed_tools().iter().any(entry_covers)
        } else {
            allowed.iter().any(entry_covers)
        }
    }

    /// Tools this scene wants *deferred*: still allowed and still callable,
    /// but advertised to the model by name + one-line description only, with
    /// the full JSON schema fetched on demand via `ToolSearch`.
    ///
    /// Every entry in `tools()` normally ships its complete input schema in
    /// the request's `tools` array on every single API call. For a tool the
    /// scene expects to be used rarely, that is a fixed per-call token cost
    /// for a schema the model usually ignores. Listing it here trades one
    /// extra round trip (`ToolSearch{query: "select:<name>"}`) when the model
    /// does want it, for not paying the schema the rest of the time.
    ///
    /// Names not present in `tools()`/the registry are ignored. Enforcement
    /// lives in `runtime::agent::Builder::build()`, which wraps the named
    /// tools in `tools::deferred::DeferredTool` — see that module for what the
    /// wrapper does and does not change.
    ///
    /// Default: empty, i.e. every tool keeps its full schema.
    fn deferred_tools(&self) -> Vec<String> {
        vec![]
    }

    /// Execution limits this scene imposes on its own turns.
    ///
    /// Consumed by `runtime::turn::Agent::run_user_turn`, which resolves the
    /// turn's API-call ceiling as `min(settings, scene)`. Before that it was
    /// called from nowhere in production: `grep` found only this definition
    /// and the scene impls, so every value a scene declared here constrained
    /// nothing at all. See [`ExecutionParams`] for what was dropped and why.
    fn execution_params(&self) -> ExecutionParams {
        ExecutionParams::default()
    }

    /// 是否在首轮完成后自动生成 session 名称（通过额外的 LLM 调用）。
    /// 默认 false —— CODING 等场景不需要。
    fn auto_name_session(&self) -> bool {
        false
    }

    /// 生成 session 名称的 prompt（仅当 auto_name_session() = true 时调用）。
    /// 参数 `first_message` 是用户的首条消息内容。
    fn session_name_prompt(&self, _first_message: &str) -> Option<String> {
        None
    }

    /// Override the intro sentence used by post-turn durable-memory
    /// extraction (see `runtime::turn::extract_memories_after_turn`). The
    /// built-in default is coding-flavored ("not derivable from the current
    /// codebase or git history") — scenes without a codebase concept (chat,
    /// research) can return `Some(..)` here to describe what counts as a
    /// durable memory in their own domain. `None` (default) keeps the
    /// built-in wording. Only the intro is replaceable; the JSON output
    /// schema instructions that follow it are fixed, since the extraction
    /// call site parses that exact shape.
    fn memory_extraction_prompt(&self) -> Option<String> {
        None
    }
}
