//! Agent types: the named sub-agent definitions a session can spawn.
//!
//! The data only. Built-in types, loading `agents/*.md`, merging tiers and
//! spawning live in the agents module; the type is here because plugin hosts
//! declare agent types too, and they sit below that module.

/// Where an [`AgentTypeDefinition`] came from, which decides whether its
/// `permission_mode` / `max_turns` are honored as written or clamped.
///
/// A definition can loosen the session it spawns into: `permission_mode`
/// overwrites the parent's mode, and `max_turns` overwrites the API-call cap.
/// That is the user's prerogative for a file they wrote themselves. It is not
/// something a package downloaded from a marketplace gets to do: the agents
/// module clamps such a definition to the parent session's mode and cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentTypeSource {
    /// Shipped with Agix.
    #[default]
    Builtin,
    /// Loaded from an `agents/` directory the user controls.
    LocalFile,
    /// Declared by an installed plugin.
    Plugin,
}

impl AgentTypeSource {
    /// May a definition from this source hand its sub-agent *more* than the
    /// session it was spawned from already had?
    pub fn may_loosen(self) -> bool {
        !matches!(self, Self::Plugin)
    }
}

/// A named agent type definition with associated system prompt and tool set.
#[derive(Debug, Clone, Default)]
pub struct AgentTypeDefinition {
    /// Unique name (e.g. "explore", "plan", "code-reviewer").
    pub name: String,
    /// Provenance, for the override clamp — see [`AgentTypeSource`].
    pub source: AgentTypeSource,
    /// Short description of the agent type's purpose.
    pub description: String,
    /// Tool names the agent type is allowed to use (empty = all tools).
    pub allowed_tools: Vec<String>,
    /// Tool names removed from the pool, applied before `allowed_tools`
    /// (deny first, then allow).
    pub disallowed_tools: Vec<String>,
    /// Optional model override (e.g. "claude-sonnet-4-20250514").
    pub model: Option<String>,
    /// Optional permission-mode override for spawned subagents of this type,
    /// independent of the parent session's mode.
    pub permission_mode: Option<crate::interface::settings::PermissionMode>,
    /// Optional effort/thinking-mode override. Maps onto
    /// `crate::interface::settings::ThinkingMode` — not a new concept,
    /// just this field's name.
    pub effort: Option<String>,
    /// Maximum API calls a spawned subagent of this type may make, applied
    /// as this subagent's own `execution.max_api_calls_per_turn` instead of
    /// inheriting the parent's cap wholesale. `None` = inherit parent's cap.
    pub max_turns: Option<u32>,
    /// Skill names to preload in full at subagent startup (not just their
    /// descriptions) — see `AgentTool`'s spawn path. A name that doesn't
    /// resolve, or resolves to a `disable_model_invocation: true` skill, is
    /// skipped with a `tracing::warn!` rather than failing the spawn.
    pub skills: Vec<String>,
    /// MCP server names to grant this subagent, referencing servers already
    /// connected in the parent session (`settings.mcp_servers` keys) — the
    /// parent's live connections are reused, not reconnected. A subagent
    /// gets **zero** MCP tools by default — MCP access is opt-in per
    /// subagent, not inherited; this is the opt-in list. Only name
    /// references to an already-configured server are supported, not inline
    /// server config.
    pub mcp_servers: Vec<String>,
    /// Scene id (`code`/`harnix`/`mini`) this agent type forces
    /// its sub-agents into, overriding the scene inherited from the parent.
    /// `None` (the default) inherits — see `Inner::scene_for_subagent`. An
    /// unknown id is ignored with a `tracing::warn!` and the inherited scene
    /// is kept, same non-fatal handling as an unrecognized `effort`.
    pub scene: Option<String>,
    /// System prompt injected into the sub-agent's context.
    pub system_prompt: String,
}

/// Parse the textual `permission_mode` an agent-type declaration carries —
/// agent `.md` frontmatter and plugin manifests both spell it as a string.
/// `auto`/`bubble` are program-only and deliberately unparseable here.
pub fn parse_permission_mode(value: &str) -> Option<crate::interface::settings::PermissionMode> {
    use crate::interface::settings::PermissionMode;
    match value {
        "default" => Some(PermissionMode::Default),
        "acceptEdits" | "accept_edits" | "accept-edits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" | "bypass_permissions" | "bypass-permissions" => {
            Some(PermissionMode::BypassPermissions)
        }
        "plan" => Some(PermissionMode::Plan),
        "dontAsk" | "dont_ask" | "dont-ask" => Some(PermissionMode::DontAsk),
        // "auto" is program-only (requires a transcript classifier) and not
        // settable from frontmatter; anything else is left unrecognized.
        _ => None,
    }
}
