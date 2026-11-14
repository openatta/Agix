//! `SceneSpec` — what a scene is: its identity, tool and module surface,
//! budgets and per-turn reminders, as data, plus the one behavior a scene
//! supplies itself, its prompt ([`ScenePrompt`]). Bound at session creation
//! and immutable thereafter.

use crate::context::config::PathStyle;
use crate::interface::tool::{Tool, ToolOrigin};
use crate::interface::tool_selector::{ToolPattern, ToolSelector};
use crate::prompt::PromptBlock;
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::Arc;

/// Context passed to [`ScenePrompt`].
#[derive(Debug, Clone, Default)]
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
    /// the code scene's `function_result_clearing` prompt section — without
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

impl<'a> ReminderContext<'a> {
    pub fn new(cwd: impl Into<Cow<'a, str>>) -> Self {
        Self {
            cwd: cwd.into(),
            git_status: None,
            memory_summary: None,
        }
    }
    pub fn git_status(mut self, status: impl Into<Cow<'a, str>>) -> Self {
        self.git_status = Some(status.into());
        self
    }
    pub fn memory_summary(mut self, summary: impl Into<Cow<'a, str>>) -> Self {
        self.memory_summary = Some(summary.into());
        self
    }
}

/// Tools that only work when someone answers. A session whose scene is not
/// [`SceneSpec::interactive`] does not register them.
pub const NEEDS_A_USER: &[&str] = &["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"];

/// Token budget configuration for a scene.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// Trigger auto-compact when input exceeds this threshold (0 = disabled).
    pub compact_threshold: usize,
    /// Number of recent messages to keep during compaction.
    pub compact_keep_recent: usize,
}

impl TokenBudget {
    pub fn new(compact_threshold: usize, compact_keep_recent: usize) -> Self {
        Self {
            compact_threshold,
            compact_keep_recent,
        }
    }
}

/// Execution limits a scene imposes on its own turns.
///
/// Tool-call fan-out (`max_parallelism`) and delegation depth
/// (`max_agent_depth`) are deliberately not here: both are bounded by
/// `EngineConfig`, which is derived from `Settings`, not from the scene, and a
/// scene that wants a different bound changes that config.
///
/// What is here is honoured — see `runtime::turn`'s `max_calls`.
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
    /// decide. Defaulting to the setting's own number would turn the `min`
    /// into a silent clamp: an operator raising the setting for a
    /// long-running task would get the scene's number back.
    pub max_api_calls_per_turn: u32,
}

impl ExecutionParams {
    pub fn new(max_api_calls_per_turn: u32) -> Self {
        Self {
            max_api_calls_per_turn,
        }
    }
}

impl Default for ExecutionParams {
    fn default() -> Self {
        Self {
            max_api_calls_per_turn: u32::MAX,
        }
    }
}

/// The scene's system prompt and the facts it sends once in the opening
/// context message.
pub trait ScenePrompt: Send + Sync {
    fn system(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock>;

    /// Facts about this session — working directory, platform, scratchpad —
    /// sent once in the opening context message instead of the system prompt.
    ///
    /// Some endpoints (DeepSeek) cache the request prefix in the order system,
    /// tools, messages: anything per-session in the system prompt makes every
    /// new session re-read the whole tool table uncached.
    fn session_context(&self, _ctx: &ScenePromptContext) -> String {
        String::new()
    }
}

/// A prompt that is the same blocks every time and has no session context.
pub struct FixedPrompt(pub Vec<PromptBlock>);

impl ScenePrompt for FixedPrompt {
    fn system(&self, _ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        self.0.clone()
    }
}

/// What the per-turn `<system-reminder>` carries, in the order listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reminder {
    GitStatus,
    Memory,
}

/// Which tools a session in the scene may have, and which of them are
/// advertised by name only.
///
/// `allow` and `deny` are one [`ToolSelector`] layer: a deny always holds, and
/// `allow: None` allows every tool the deny list leaves. Both only narrow the
/// registry the host assembled; [`SceneSpec::extra_tools`] is how a scene adds
/// to it.
///
/// `deferred` tools stay allowed and callable, but are advertised to the model
/// by name and one-line description only, the schema fetched on demand via
/// `ToolSearch`. Names the session does not have are ignored. The list
/// becomes the session's `EngineConfig::deferred_tools`, and covers MCP tools
/// that arrive later in the session too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSurface {
    pub allow: Option<Vec<ToolPattern>>,
    pub deny: Vec<ToolPattern>,
    pub deferred: Vec<String>,
}

impl ToolSurface {
    /// Lists as configured, an empty `allow` allowing every tool.
    pub fn from_lists(allow: &[impl AsRef<str>], deny: &[impl AsRef<str>]) -> Self {
        Self {
            allow: (!allow.is_empty()).then(|| {
                allow
                    .iter()
                    .map(|p| ToolPattern::parse(p.as_ref()))
                    .collect()
            }),
            deny: deny
                .iter()
                .map(|p| ToolPattern::parse(p.as_ref()))
                .collect(),
            deferred: Vec::new(),
        }
    }

    pub fn allowing(allow: &[impl AsRef<str>]) -> Self {
        Self::from_lists(allow, &[] as &[&str])
    }

    pub fn denying(deny: &[impl AsRef<str>]) -> Self {
        Self::from_lists(&[] as &[&str], deny)
    }

    pub fn deferring(mut self, deferred: &[impl AsRef<str>]) -> Self {
        self.deferred = deferred.iter().map(|n| n.as_ref().to_string()).collect();
        self
    }

    pub fn selector(&self) -> ToolSelector {
        if self.allow.is_none() && self.deny.is_empty() {
            return ToolSelector::all();
        }
        ToolSelector::layer(self.allow.clone(), self.deny.clone())
    }

    pub fn permits(&self, name: &str, origin: &ToolOrigin) -> bool {
        self.selector().admits(name, origin)
    }
}

/// Which modules (`agents`, `mcp`, `skills`, ...) a session in the scene
/// runs. `allow: None` runs every module the host assembled; naming one the
/// host did not assemble adds nothing. The delegates a session spawns inherit
/// the narrowed list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleSurface {
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
}

impl ModuleSurface {
    pub fn new(allow: Option<Vec<String>>, deny: Vec<String>) -> Self {
        Self { allow, deny }
    }

    pub fn permits(&self, id: &str) -> bool {
        self.allow
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|m| m == id))
            && !self.deny.iter().any(|m| m == id)
    }
}

/// A scene: everything but its prompt is data, so a host that builds on a
/// built-in scene changes the fields it cares about and keeps the rest,
/// instead of wrapping a trait object and forwarding every method.
#[derive(Clone)]
#[non_exhaustive]
pub struct SceneSpec {
    pub id: String,
    pub name: String,
    pub description: String,
    pub prompt: Arc<dyn ScenePrompt>,
    pub tools: ToolSurface,
    /// Tools this scene registers into the session's own registry, before
    /// `tools.deferred` is applied.
    ///
    /// A name already registered is **rejected**, not substituted:
    /// `Builder::build()` fails rather than let a scene shadow `Bash` or
    /// `Edit`. They are built with no session context, so they suit
    /// self-contained tools.
    pub extra_tools: Vec<Arc<dyn Tool>>,
    pub modules: ModuleSurface,
    /// Whether a session must be bound to a project root. The daemon refuses
    /// a session with no `project_root` for a scene that says yes.
    pub requires_project: bool,
    /// `false` means the Team tools are never registered for the session.
    pub supports_team: bool,
    /// Whether someone is there to answer a question, a plan to approve, a
    /// permission prompt. When nobody is, a call the permission policy would
    /// ask about is refused instead, the tools in [`NEEDS_A_USER`] are left
    /// out, and the prompt must not count on a reply.
    pub interactive: bool,
    /// How the file tools word paths when `Settings::path_style` is not set.
    pub path_style: PathStyle,
    pub token_budget: TokenBudget,
    pub execution: ExecutionParams,
    /// The built-in skills a session lists, by name. A built-in is also left
    /// out when a tool it needs is not permitted here.
    pub bundled_skills: Vec<String>,
    pub reminders: Vec<Reminder>,
    /// When set, the session is named after its first turn by a model call
    /// with this prompt, `{first_message}` replaced by the user's message.
    pub session_naming: Option<String>,
    /// Replaces the intro sentence of post-turn durable-memory extraction,
    /// whose default is code-flavored. The JSON output instructions that
    /// follow it are fixed.
    pub memory_extraction_intro: Option<String>,
}

impl SceneSpec {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        prompt: Arc<dyn ScenePrompt>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: String::new(),
            prompt,
            tools: ToolSurface::default(),
            extra_tools: Vec::new(),
            modules: ModuleSurface::default(),
            requires_project: false,
            supports_team: false,
            interactive: false,
            path_style: PathStyle::Absolute,
            token_budget: TokenBudget {
                compact_threshold: 150_000,
                compact_keep_recent: 20,
            },
            execution: ExecutionParams::default(),
            bundled_skills: vec!["skillify".into()],
            reminders: Vec::new(),
            session_naming: None,
            memory_extraction_intro: None,
        }
    }

    pub fn system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        self.prompt.system(ctx)
    }

    pub fn session_context(&self, ctx: &ScenePromptContext) -> String {
        self.prompt.session_context(ctx)
    }

    pub fn permits_tool(&self, name: &str, origin: &ToolOrigin) -> bool {
        self.tools.permits(name, origin)
    }

    /// The `<system-reminder>` blocks injected before each turn, one per
    /// entry in `reminders` that has something to say.
    pub fn reminder(&self, ctx: &ReminderContext) -> String {
        let mut out = String::new();
        for reminder in &self.reminders {
            let body = match reminder {
                Reminder::GitStatus => ctx.git_status.as_deref(),
                Reminder::Memory => ctx.memory_summary.as_deref(),
            };
            if let Some(body) = body {
                out.push_str(&format!("\n<system-reminder>\n{body}\n</system-reminder>"));
            }
        }
        out
    }

    pub fn session_name_prompt(&self, first_message: &str) -> Option<String> {
        self.session_naming
            .as_ref()
            .map(|template| template.replace("{first_message}", first_message))
    }
}

impl std::fmt::Debug for SceneSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneSpec")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("tools", &self.tools)
            .field("modules", &self.modules)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SceneSpec {
        SceneSpec::new("t", "T", Arc::new(FixedPrompt(vec![])))
    }

    fn reminder_ctx<'a>(git: Option<&'a str>, memory: Option<&'a str>) -> ReminderContext<'a> {
        ReminderContext {
            cwd: Cow::Borrowed("/w"),
            git_status: git.map(Cow::Borrowed),
            memory_summary: memory.map(Cow::Borrowed),
        }
    }

    #[test]
    fn a_deny_holds_under_an_allow_list() {
        let mut s = spec();
        s.tools = ToolSurface::from_lists(&["mcp__*", "Read"], &["mcp__shell__*"]);
        assert!(s.permits_tool("Read", &ToolOrigin::Builtin));
        assert!(s.permits_tool("mcp__github__diff", &ToolOrigin::mcp("github")));
        assert!(!s.permits_tool("mcp__shell__run", &ToolOrigin::mcp("shell")));
        assert!(!s.permits_tool("Bash", &ToolOrigin::Builtin));
    }

    #[test]
    fn defaults_admit_every_tool_and_module() {
        let s = spec();
        assert_eq!(s.tools.selector(), ToolSelector::all());
        assert!(s.permits_tool("Bash", &ToolOrigin::Builtin));
        assert!(s.modules.permits("agents"));
        assert_eq!(s.bundled_skills, ["skillify"]);
        assert_eq!(s.reminder(&reminder_ctx(Some("M a.rs"), Some("mem"))), "");
    }

    #[test]
    fn modules_are_admitted_and_not_disabled() {
        let modules = ModuleSurface {
            allow: Some(vec!["skills".into(), "mcp".into()]),
            deny: vec!["mcp".into()],
        };
        assert!(modules.permits("skills"));
        assert!(!modules.permits("mcp"));
        assert!(!modules.permits("team"));
    }

    #[test]
    fn reminders_render_in_the_order_listed_and_skip_what_is_absent() {
        let mut s = spec();
        s.reminders = vec![Reminder::Memory, Reminder::GitStatus];
        assert_eq!(
            s.reminder(&reminder_ctx(Some("M a.rs"), Some("mem"))),
            "\n<system-reminder>\nmem\n</system-reminder>\n<system-reminder>\nM a.rs\n</system-reminder>"
        );
        assert_eq!(
            s.reminder(&reminder_ctx(Some("M a.rs"), None)),
            "\n<system-reminder>\nM a.rs\n</system-reminder>"
        );
    }

    #[test]
    fn the_naming_template_takes_the_first_message() {
        let mut s = spec();
        assert_eq!(s.session_name_prompt("hi"), None);
        s.session_naming = Some("title: {first_message}".into());
        assert_eq!(s.session_name_prompt("hi").as_deref(), Some("title: hi"));
    }
}
