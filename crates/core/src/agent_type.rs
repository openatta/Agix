//! Agent types: the named sub-agent definitions a session can spawn.
//!
//! The data only. Built-in types, loading `agents/*.md`, merging tiers and
//! spawning live in the agents module; the type is here because plugin hosts
//! declare agent types too, and they sit below that module.

/// Where an [`AgentTypeDefinition`] came from, which decides whether it may
/// hand its sub-agent more than the parent session had.
///
/// `permission_mode` and `max_turns` may loosen what the parent had only for
/// a definition the user wrote or accepted: one of their own, or one in a
/// project they trust. A plugin's, or an untrusted project's, is held to the
/// parent's — and an untrusted project's does not replace a built-in, plugin
/// or user type of the same name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentTypeSource {
    /// Shipped with Agix.
    #[default]
    Builtin,
    /// The global or scene `agents/` directory.
    User,
    /// The project's `.agix/agents/`, and whether the project is trusted.
    Project { trusted: bool },
    /// Declared by an installed plugin.
    Plugin,
}

impl AgentTypeSource {
    /// May a definition from this source hand its sub-agent *more* than the
    /// session it was spawned from already had?
    pub fn may_loosen(self) -> bool {
        match self {
            Self::Builtin | Self::User => true,
            Self::Project { trusted } => trusted,
            Self::Plugin => false,
        }
    }

    /// Does a definition from this source replace one of the same name from
    /// the built-ins, a plugin or another tier?
    pub fn may_shadow(self) -> bool {
        matches!(self, Self::User | Self::Project { trusted: true })
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
    /// Optional permission mode for spawned subagents of this type. Whether
    /// it may be looser than the parent session's depends on `source`.
    pub permission_mode: Option<crate::interface::settings::PermissionMode>,
    /// Optional `ModelSettings::effort` override for spawned subagents, by
    /// name (`low` … `max`).
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
/// `bubble` is program-only and deliberately unparseable here.
pub fn parse_permission_mode(value: &str) -> Option<crate::interface::settings::PermissionMode> {
    use crate::interface::settings::PermissionMode;
    match value {
        "default" => Some(PermissionMode::Default),
        "acceptEdits" | "accept_edits" | "accept-edits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" | "bypass_permissions" | "bypass-permissions" | "auto" | "yolo" => {
            Some(PermissionMode::BypassPermissions)
        }
        "plan" => Some(PermissionMode::Plan),
        "dontAsk" | "dont_ask" | "dont-ask" => Some(PermissionMode::DontAsk),
        _ => None,
    }
}
