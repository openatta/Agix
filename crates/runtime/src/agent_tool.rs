//! `AgentTool` — spawn sub-agents using the agent's own `Agent` engine.
//!
//! Implements `base::tool::Tool` for the `Agent` invocable by the model.
//! Also provides `resume_agent()` to continue a previous session's transcript.
//!
//! Uses `Builder::build()` + `Agent::run_turn()` instead of the legacy
//! `Engine::new()` path. The sub-agent inherits the parent's authenticated
//! Anthropic client but gets a restricted tool set.
//!
//! # Agent type registry
//!
//! The module defines built-in agent types (`builtin_agent_types()`) and can
//! load user-defined types from a directory (e.g. `~/.atta/scenes/<scope>/agents/*.md`
//! or `<project>/.atta/agents/*.md`) via `load_agent_types_from_dir()`. Each
//! type specifies a system prompt and an allowed tool set, which
//! `resolve_tools()` applies when spawning sub-agents.

use crate::agent::{Agent, Builder, EventReceiver, InputMessage, InputSender};
use anyhow::anyhow;
use async_trait::async_trait;
use base::context::EngineConfig;
use base::interface::event::AgentEvent;
use base::interface::model::{MessageRole, Model, ModelContentBlock, ModelMessage};
use base::interface::permission::Permission;
use base::interface::scene::AgentScene;
use base::interface::settings::{
    ExecutionSettings, ModelSettings, PathSettings, PermissionMode, SandboxConfig, Settings,
    ThinkingMode,
};
use base::tool::InMemoryToolRegistry;
use base::tool::ProgressSender;
use base::tool::ToolContext;
use base::tool::ToolResultContent;
use history::store::HistoryStore;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use telemetry::TelemetryHandle;
use tools::worktree::create_worktree;

// ═══════════════════════════════════════════════════════════
// Agent type registry
// ═══════════════════════════════════════════════════════════

/// Where an [`AgentTypeDefinition`] came from, which decides whether its
/// `permission_mode` / `max_turns` are honored as written or clamped.
///
/// A definition can loosen the session it spawns into: `permission_mode`
/// overwrites the parent's mode, and `max_turns` overwrites the API-call cap.
/// That is the user's prerogative for a file they wrote themselves. It is not
/// something a package downloaded from a marketplace gets to do — see
/// [`apply_agent_type_overrides`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentTypeSource {
    /// Shipped with AttaCore.
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
    fn may_loosen(self) -> bool {
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
    pub permission_mode: Option<base::interface::settings::PermissionMode>,
    /// Optional effort/thinking-mode override. Maps onto
    /// `base::interface::settings::ThinkingMode` — not a new concept,
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
    /// Scene id (`coding`/`chat`/`research`/`demo`) this agent type forces
    /// its sub-agents into, overriding the scene inherited from the parent.
    /// `None` (the default) inherits — see `Inner::scene_for_subagent`. An
    /// unknown id is ignored with a `tracing::warn!` and the inherited scene
    /// is kept, same non-fatal handling as an unrecognized `effort`.
    pub scene: Option<String>,
    /// System prompt injected into the sub-agent's context.
    pub system_prompt: String,
}

/// Return the five built-in agent types shipped with AttaCore.
///
/// Each type specifies its allowed tool set and system prompt. Custom types
/// can be loaded from disk via [`load_agent_types_from_dir`].
pub fn builtin_agent_types() -> Vec<AgentTypeDefinition> {
    vec![
        AgentTypeDefinition {
            name: "explore".into(),
            description: "Read-only file search and exploration specialist".into(),
            allowed_tools: vec![
                "Read".into(),
                "Grep".into(),
                "Glob".into(),
                "WebSearch".into(),
                "WebFetch".into(),
            ],
            model: None,
            system_prompt: EXPLORE_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "plan".into(),
            description: "Software architect and planning specialist".into(),
            allowed_tools: vec![
                "Read".into(),
                "Grep".into(),
                "Glob".into(),
                "WebSearch".into(),
                "WebFetch".into(),
                "Write".into(),
            ],
            model: None,
            system_prompt: PLAN_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "general-purpose".into(),
            description: "General-purpose AI coding agent with full tool access".into(),
            allowed_tools: vec![], // empty = all tools
            model: None,
            system_prompt: GENERAL_PURPOSE_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "claude".into(),
            description: "Claude AI assistant with full tool access".into(),
            allowed_tools: vec![], // empty = all tools
            model: None,
            system_prompt: CLAUDE_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "code-reviewer".into(),
            description: "Code review specialist using Read/Grep/Glob/LSP/Bash".into(),
            allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into(), "Bash".into()],
            model: None,
            system_prompt: CODE_REVIEWER_PROMPT.into(),
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "worker".into(),
            description: "Worker agent executing a precisely-scoped task within a team".into(),
            allowed_tools: vec![], // empty = all tools, matches pre-refactor behavior
            model: None,
            system_prompt: WORKER_PROMPT.into(),
            ..Default::default()
        },
    ]
}

/// Load agent type definitions from a directory of `*.md` files with YAML
/// frontmatter. The expected file format is:
///
/// ```markdown
/// ---
/// name: my-custom-agent
/// description: Specialized agent for custom task
/// allowed_tools: [Read, Grep, Glob, Write]
/// model: claude-sonnet-4-20250514
/// ---
/// System prompt body...
/// ```
///
/// * `name` — required (defaults to filename stem if omitted)
/// * `description` — required
/// * `allowed_tools` — optional comma/array list; empty = all tools
/// * `model` — optional model override
///
/// Returns all successfully parsed definitions. Malformed files are silently
/// skipped with a `tracing::warn!` message.
///
/// Sync (`std::fs`), not async — matches `skills::manager::SkillManager::load_dir`,
/// which is called from the same sync `Builder::build()` context this
/// function is meant to be called from (see `merge_agent_types`).
pub fn load_agent_types_from_dir(dir: &Path) -> Vec<AgentTypeDefinition> {
    let mut types = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return types, // directory doesn't exist yet
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "agent type: failed to read file");
                continue;
            }
        };
        match parse_agent_type_file(&content, &path) {
            Some(def) => types.push(def),
            None => {
                tracing::warn!(path = %path.display(), "agent type: failed to parse frontmatter");
            }
        }
    }
    types
}

/// Merge the built-in agent types with disk-loaded ones from up to three
/// tiers (pass fewer to skip a tier — e.g. tests often only want one dir).
///
/// Precedence low → high: built-in defaults < `plugin_types` (plugin-declared
/// `[[agents]]`) < `dirs[0]` (global) < `dirs[1]` (scene) < `dirs[2]`
/// (project) — same override order as skills (`crates/core/src/frozen/skill.rs`).
/// A definition with the same `name` as a lower-tier one replaces it
/// entirely (not field-merged — a type definition is one atomic unit,
/// same convention as `task_models` overrides in the multi-provider LLM
/// config). This means `.atta/agents/*.md` customization always wins over a
/// plugin's default, but a plugin's agent type still overrides the bare
/// built-in of the same name.
pub fn merge_agent_types(
    dirs: &[&Path],
    plugin_types: &[AgentTypeDefinition],
) -> std::collections::HashMap<String, AgentTypeDefinition> {
    let mut merged: std::collections::HashMap<String, AgentTypeDefinition> = builtin_agent_types()
        .into_iter()
        .map(|d| (d.name.clone(), d))
        .collect();
    for def in plugin_types {
        merged.insert(def.name.clone(), def.clone());
    }
    for dir in dirs {
        for def in load_agent_types_from_dir(dir) {
            merged.insert(def.name.clone(), def);
        }
    }
    merged
}

/// A live-reloaded agent-type catalog meant to be built **once** and shared
/// by every session a multi-session caller (a daemon's `SessionPool`) hands
/// out — see `AgentTool::with_shared_agent_types`'s doc comment for why:
/// `AgentTool::with_parent_tools` starts its own dedicated file-watcher
/// thread per call, which is N redundant threads (watching the exact same
/// directories) for N concurrent sessions in a daemon.
/// Shared, live-reloaded agent-type map: `subagent_type name -> AgentTypeDefinition`.
/// The inner `Arc<HashMap>` is swapped wholesale on reload (never mutated in
/// place), so a reader that clones it out under a brief read lock never sees
/// a half-updated map — see `SharedAgentTypeCatalog`/`Inner::agent_types`.
pub(crate) type SharedAgentTypeMap =
    Arc<std::sync::RwLock<Arc<std::collections::HashMap<String, AgentTypeDefinition>>>>;

pub struct SharedAgentTypeCatalog {
    agent_types: SharedAgentTypeMap,
    // Kept alive only for its background thread; see `Inner::_agent_type_watcher`.
    _watcher: Option<Arc<crate::agent_type_watcher::AgentTypeWatcher>>,
}

impl SharedAgentTypeCatalog {
    /// Merge `dirs`/`plugin_types` into the initial catalog and start
    /// watching `dirs` for `.md` changes. `notify` setup failing degrades to
    /// a build-time-only catalog (logged, non-fatal) — same handling as
    /// `AgentTool::with_parent_tools`.
    ///
    /// Caveat: the watcher's own file-triggered reloads always re-merge
    /// using the `plugin_types` snapshot given *here*, not whatever was last
    /// passed to `refresh()` — a plugin-driven update only takes effect
    /// immediately via `refresh()`; a `.md` file edit after that will
    /// re-apply this constructor's original `plugin_types` until the next
    /// explicit `refresh()`. Narrow edge case (plugin refresh followed by an
    /// unrelated file edit before the next plugin refresh); accepted rather
    /// than threading a second live-shared `plugin_types` reference through
    /// the watcher for it.
    pub fn build(dirs: &[&Path], plugin_types: &[AgentTypeDefinition]) -> Self {
        Self::build_with(dirs, plugin_types, true)
    }

    /// [`build`](Self::build) without the watcher — for a host that already
    /// watches these directories and calls [`refresh`](Self::refresh)
    /// itself. One `notify` stream per catalog is fine for one catalog; a
    /// host building one per project reaches the backend's per-process
    /// ceiling, and on macOS that is an abort rather than an error.
    pub fn unwatched(dirs: &[&Path], plugin_types: &[AgentTypeDefinition]) -> Self {
        Self::build_with(dirs, plugin_types, false)
    }

    fn build_with(dirs: &[&Path], plugin_types: &[AgentTypeDefinition], watch: bool) -> Self {
        let merged = merge_agent_types(dirs, plugin_types);
        let agent_types = Arc::new(std::sync::RwLock::new(Arc::new(merged)));
        if !watch {
            return Self {
                agent_types,
                _watcher: None,
            };
        }
        let owned_dirs: Vec<std::path::PathBuf> = dirs.iter().map(|p| p.to_path_buf()).collect();
        let _watcher = crate::agent_type_watcher::AgentTypeWatcher::watch(
            owned_dirs,
            plugin_types.to_vec(),
            agent_types.clone(),
        )
        .map(Arc::new)
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to enable shared agent-type file watching; live reload disabled for this pool");
        })
        .ok();
        Self {
            agent_types,
            _watcher,
        }
    }

    /// Re-merge the catalog in place — e.g. after a plugin install/enable/
    /// disable changes `plugin_types` and there's no filesystem event to
    /// trigger the watcher's own reload. See the caveat on `build()`.
    pub fn refresh(&self, dirs: &[&Path], plugin_types: &[AgentTypeDefinition]) {
        *self.agent_types.write().unwrap() = Arc::new(merge_agent_types(dirs, plugin_types));
    }

    /// Handle to pass to `AgentTool::with_shared_agent_types` for each session.
    pub fn handle(&self) -> SharedAgentTypeMap {
        self.agent_types.clone()
    }
}

/// Render the one-line-per-type catalog injected into `AgentTool::description()`
/// so the model can see what `subagent_type` values are actually available
/// (built-in + any custom types loaded from `.atta/agents/`) without a
/// separate discovery call. Sorted by name for stable output.
fn describe_agent_types(
    agent_types: &std::collections::HashMap<String, AgentTypeDefinition>,
) -> String {
    let mut names: Vec<&String> = agent_types.keys().collect();
    names.sort();
    let mut lines = vec![
        "Launch a sub-agent to handle complex, multi-step tasks independently.".to_string(),
        String::new(),
        "Available subagent_type values:".to_string(),
    ];
    for name in names {
        let d = &agent_types[name];
        lines.push(format!("- {name}: {}", d.description));
    }
    lines.join("\n")
}

/// Parse a single agent type definition from a markdown file with YAML
/// frontmatter. Returns `None` if the file lacks a valid `description`.
/// Parse the textual `permission_mode` an agent-type declaration carries —
/// agent `.md` frontmatter and plugin manifests both spell it as a string.
/// `auto`/`bubble` are program-only and deliberately unparseable here.
pub fn parse_permission_mode(value: &str) -> Option<base::interface::settings::PermissionMode> {
    use base::interface::settings::PermissionMode;
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

fn parse_agent_type_file(content: &str, path: &Path) -> Option<AgentTypeDefinition> {
    use base::frozen::frontmatter::{first_paragraph, parse_yaml_list, split_frontmatter};

    let (front, body) = split_frontmatter(content);
    let body = body.trim();

    let mut name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut description = String::new();
    let mut allowed_tools: Vec<String> = Vec::new();
    let mut disallowed_tools: Vec<String> = Vec::new();
    let mut model: Option<String> = None;
    let mut permission_mode: Option<base::interface::settings::PermissionMode> = None;
    let mut effort: Option<String> = None;
    let mut max_turns: Option<u32> = None;
    let mut skills: Vec<String> = Vec::new();
    let mut mcp_servers: Vec<String> = Vec::new();
    let mut scene: Option<String> = None;

    if let Some(yaml) = front {
        for line in yaml.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let key = k.trim();
            let raw = v.trim();
            if raw.is_empty() {
                continue;
            }
            let value = raw.trim_matches('"').trim_matches('\'');
            if value.is_empty() {
                continue;
            }
            match key {
                "name" => name = value.to_string(),
                "description" => description = value.to_string(),
                "allowed_tools" | "allowedTools" | "allowed-tools" => {
                    allowed_tools = parse_yaml_list(value);
                }
                "disallowed_tools" | "disallowedTools" | "disallowed-tools" => {
                    disallowed_tools = parse_yaml_list(value);
                }
                "model" => model = Some(value.to_string()),
                "permission_mode" | "permissionMode" => {
                    permission_mode = parse_permission_mode(value);
                }
                "effort" => effort = Some(value.to_string()),
                "max_turns" | "maxTurns" => {
                    max_turns = value.parse::<u32>().ok();
                }
                "skills" => {
                    skills = parse_yaml_list(value);
                }
                "mcp_servers" | "mcpServers" => {
                    mcp_servers = parse_yaml_list(value);
                }
                "scene" => scene = Some(value.to_string()),
                _ => {}
            }
        }
    }

    if description.is_empty() {
        description = first_paragraph(body);
    }

    if description.is_empty() {
        return None;
    }

    Some(AgentTypeDefinition {
        name,
        source: AgentTypeSource::LocalFile,
        description,
        allowed_tools,
        disallowed_tools,
        model,
        permission_mode,
        effort,
        max_turns,
        skills,
        mcp_servers,
        scene,
        system_prompt: body.to_string(),
    })
}

/// Resolve a scene id against the built-in set.
///
/// The fallback for callers that never wired a `SceneRegistry` — see
/// `Inner::scene_registry` and `Inner::resolve_scene_id`. This is also how a
/// sub-agent recovers its parent's scene when no explicit
/// `Arc<dyn AgentScene>` was wired in (the daemon sets `Settings.paths.scope`
/// to `scene.id()`, so the scope *is* the scene id there). Unknown ids return
/// `None` so the caller can fall back rather than guessing.
fn builtin_scene_by_id(id: &str) -> Option<Arc<dyn AgentScene>> {
    match id {
        "coding" => Some(Arc::new(scene::scene::coding::CodingScene)),
        "chat" => Some(Arc::new(scene::scene::chat::ChatScene)),
        "demo" => Some(Arc::new(scene::scene::demo::DemoScene)),
        "research" => Some(Arc::new(scene::scene::research::ResearchScene)),
        _ => None,
    }
}

// ═══════════════════════════════════════════════════════
// Input
// ═══════════════════════════════════════════════════════

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentInput {
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "subagentType")]
    pub subagent_type: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default, alias = "run_in_background", alias = "runInBackground")]
    pub background: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub auto_background_after_secs: Option<u64>,
    /// Persistent team membership (must be given together with `name`, or
    /// not at all — see `AgentTool::call`). Names a team this call joins:
    /// if `(team_name, name)` already names a member that's still alive,
    /// `prompt` is queued as its next message instead of spawning a new
    /// one. Requires `settings.execution.team_enabled` to be true for this
    /// session — `AgentTool::call` returns a validation error otherwise
    /// (the schema can't conditionally hide this field per-session, so the
    /// check happens at call time instead).
    #[serde(default)]
    pub team_name: Option<String>,
    /// This member's own name within `team_name` — see `team_name`'s doc
    /// comment. Reusing the same `(team_name, name)` pair across separate
    /// calls is *how* you keep messaging the same persistent member; it's
    /// not a collision to avoid.
    #[serde(default)]
    pub name: Option<String>,
    /// Only meaningful together with `team_name`+`name`. The permission
    /// grant this member runs under for its **entire** lifetime, chosen
    /// once at spawn time rather than negotiated per tool call ("one team,
    /// one authorization" — see `AgentTool::build_team_member_agent`'s doc
    /// comment for why interactive per-call negotiation doesn't work for a
    /// persistent member). Omit for the safe default (`Plan`: read-only
    /// tools only). Only consulted when spawning a **new** member — sending
    /// another message to an already-alive one ignores this (its grant was
    /// fixed when it was created); if a `team_name` doesn't have an
    /// established grant yet, the *first* call that spawns a member under
    /// it sets it for every member spawned under that name afterward,
    /// whether or not they repeat this field. `Auto`/`Bubble` aren't
    /// accepted here (rejected at call time) — `Auto` needs a transcript
    /// classifier this call site doesn't have, and `Bubble` needs a lead
    /// that's actually available to answer mid-call, which a persistent
    /// member's lead isn't (it only runs code during its own turns).
    #[serde(default)]
    pub permission_mode: Option<base::interface::settings::PermissionMode>,
}

// ═══════════════════════════════════════════════════════
// Type-specific prompts
// ═══════════════════════════════════════════════════════

const EXPLORE_PROMPT: &str = "\
You are a read-only file search specialist. Your job is to explore, find, and \
report — do NOT edit, write, or delete any files. Use Read/Glob/Grep/WebFetch/\
WebSearch/LSP tools to gather information. Return a concise structured summary \
with file paths and line references.";

const PLAN_PROMPT: &str = "\
You are a software architect and planning specialist. Your job is to design \
implementation plans — do NOT write or edit any code. Use FileRead/Glob/Grep \
to explore the codebase. Produce a concrete, step-by-step plan with specific \
file paths, crate names, and implementation approach.";

const GENERAL_PURPOSE_PROMPT: &str = "\nYou are a general-purpose AI coding agent. Execute the user's request thoroughly.\nUse tools to read, edit, and search code. Report findings clearly.\nFocus on correctness and completeness.";

const CLAUDE_PROMPT: &str = "\nYou are Claude, an AI assistant. Execute the user's request thoroughly.\nUse tools as needed. Report findings clearly and concisely.";

const CODE_REVIEWER_PROMPT: &str = "\
You are a code reviewer. Your job is to review code diffs for correctness, \
performance, and style issues. Use Read/Grep/Glob/LSP to examine the codebase \
and Bash for read-only inspection commands (e.g. git diff, cargo check, \
rustfmt --check). Report findings with specific file paths and line references. \
Do NOT make any edits.";

const WORKER_PROMPT: &str = "\nYou are a worker agent in a team. Execute the assigned task precisely.\nReport results concisely. Do not deviate from the assigned scope.";

fn bg_task_id() -> String {
    base::id::Id::new().to_string()
}

/// Write the first `Meta` line for a freshly spawned sidechain (sub-agent /
/// team member) session, linking it back to its parent via
/// `parent_session_id` and marking it `session_kind: Sidechain` — this is
/// what makes it discoverable via `HistoryStore::child_sessions` and hidden
/// from `session.list` by default. See docs/ARCHITECTURE.md §3.3.
///
/// `project_root` is left `None` here: this crate has no per-session project
/// identity yet (single-project daemon, P3's job), so there is nothing
/// correct to stamp — the design's §5.3 pinning rule only starts to matter
/// once one exists.
///
/// Best-effort: a failure here does not fail the spawn. The sub-agent still
/// runs and its `User`/`Assistant` entries still persist via
/// `SessionManager::persist` — it just isn't linkable back to its parent
/// (the same "no Meta at all" state every session was in before this).
async fn write_sidechain_meta(
    store: &Arc<dyn HistoryStore>,
    session_id: &str,
    parent_session_id: Option<String>,
    scene_id: &str,
    cwd: &Path,
    settings: &Settings,
) {
    let Ok(sid) = base::session::SessionId::parse(session_id) else {
        return;
    };
    let meta = history::entry::LogEntry::Meta {
        cwd: cwd.display().to_string(),
        started_at: store.environment().now(),
        model: settings.model.model_name.clone(),
        permission_mode: serde_json::to_value(settings.permission_mode)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "default".to_string()),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        attacode_version: env!("CARGO_PKG_VERSION").to_string(),
        parent_session_id,
        scene: Some(scene_id.to_string()),
        project_root: None,
        session_kind: history::entry::SessionKind::Sidechain,
        schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
    };
    if let Err(e) = store.append(sid, meta).await {
        tracing::warn!(session_id, error = %e, "failed to write sidechain Meta entry");
    }
}

/// Marks a one-shot sub-agent session as having run its task to conclusion
/// (`SIDECHAIN_TERMINAL` in `docs/daemon_rpc_protocol.md`). Called only
/// through `mark_sidechain_terminal`
/// (from `run_sub_inner`/`run_sub_tagged`/`resume_agent`) once their turn has
/// returned — a session with no such marker was either never a sidechain, is
/// still running, was cancelled mid-flight, or was cut off from outside
/// (process exit, a future `agent.stop`, parent crash), all of which must
/// stay resumable. Best-effort, same rationale as `write_sidechain_meta`.
async fn write_sidechain_terminal_marker(
    store: &Arc<dyn HistoryStore>,
    session_id: &str,
    state: history::entry::SessionEndState,
) {
    let Ok(sid) = base::session::SessionId::parse(session_id) else {
        return;
    };
    let entry = history::entry::LogEntry::SessionEnd { state };
    if let Err(e) = store.append(sid, entry).await {
        tracing::warn!(session_id, error = %e, "failed to write sidechain SessionEnd marker");
    }
}

/// Run one turn against a sub-agent and collect its response text — the
/// shared core of `run_sub`/`run_sub_inner`/the persistent team-member loop.
/// Takes `event_rx` **borrowed**, not owned, specifically so a persistent
/// team member can call this repeatedly across many turns against the same
/// `Agent`/channel pair instead of a fresh one being built per message.
///
/// `AgentEvent::TurnComplete` is **not** guaranteed to be emitted on every
/// return path of `run_user_turn` (`crate::turn`) — several early returns
/// there (a plain model error, an already-cancelled token, the max-turns
/// guard) skip it, only the fully-successful tail sends it. Draining until
/// `TurnComplete` unconditionally deadlocks forever on those paths, because
/// the event channel's sender lives inside `agent`, which nothing drops
/// until this function's caller does — and the caller can't do that until
/// this function returns. `agent.run_turn(..)` resolving is itself the
/// authoritative "the turn is over" signal (a plain future, not gated on
/// any event); this races the drain against that future and, once it
/// resolves, only waits a short bounded grace window for the drain to
/// catch up on anything still buffered — never indefinitely on
/// `TurnComplete` itself. Confirmed reproducible even against unmodified
/// `turn.rs` (a plain model error is enough to trigger it), not specific to
/// any one caller.
///
/// Returns the collected text alongside whether the turn ended because
/// `cancel` fired (`TurnOutcome::stop_reason == "cancelled"`) rather than
/// running to a real conclusion — callers that mark sidechain terminal state
/// need to tell the two apart (see `mark_sidechain_terminal`).
async fn run_one_turn(
    agent: &mut Agent,
    event_rx: &mut EventReceiver,
    input_tx: &InputSender,
    prompt: String,
    cancel: tokio_util::sync::CancellationToken,
    // When set, every event this sub-agent emits is also mirrored onto the
    // parent session's channel wrapped in `AgentEvent::SubagentProgress`, so
    // the host can render sub-agent activity live (S-1). Purely additive: the
    // text return value is identical either way.
    tag: Option<SubagentTag>,
) -> (Result<String, base::error::ToolError>, bool) {
    let turn_id = uuid::Uuid::new_v4().to_string();
    let _ = input_tx.send(InputMessage::User {
        content: prompt.clone(),
        attachments: vec![],
        turn_id: turn_id.clone(),
    });

    if let Some(tag) = &tag {
        tag.spawned();
    }

    let mut text = String::new();
    let run_turn_fut = agent.run_turn(prompt, turn_id, cancel);
    tokio::pin!(run_turn_fut);
    let outcome = loop {
        tokio::select! {
            biased;
            ev = event_rx.recv() => {
                // `ev` is `None` when the channel closed mid-turn (shouldn't
                // happen — `agent` outlives this loop — but don't spin on a
                // dead channel if it somehow does); nothing to do in that case.
                if let Some(ev) = ev {
                    if let AgentEvent::TextDelta { text: ref t, .. } = ev {
                        text.push_str(t);
                    }
                    if let Some(tag) = &tag {
                        tag.forward(ev);
                    }
                }
            }
            outcome = &mut run_turn_fut => break outcome,
        }
    };

    // Grace window: `run_turn` resolving doesn't guarantee every already-
    // queued event has been *received* by us yet (channel delivery isn't
    // instantaneous) — drain whatever shows up in a short bounded window,
    // then stop regardless of whether more might theoretically still come.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(ev)) => {
                if let AgentEvent::TextDelta { text: ref t, .. } = ev {
                    text.push_str(t);
                }
                if let Some(tag) = &tag {
                    tag.forward(ev);
                }
            }
            Ok(None) | Err(_) => break,
        }
    }

    let cancelled = matches!(&outcome, Ok(o) if o.stop_reason == "cancelled");
    let result = match outcome {
        Ok(_) | Err(crate::turn::TurnError::Shutdown) => Ok(text),
        Err(e) => Err(base::error::ToolError::Execution(anyhow!("sub: {e}"))),
    };
    if let Some(tag) = &tag {
        tag.completed(match (&result, cancelled) {
            (_, true) => "cancelled",
            (Ok(_), _) => "completed",
            (Err(_), _) => "failed",
        });
    }
    (result, cancelled)
}

/// After a one-shot sidechain's turn has finished — successfully, with an
/// error, or because `cancel` fired — writes the matching `SessionEnd`
/// terminal marker. The single place `run_sub_tagged`, `run_sub_inner`, and
/// `resume_agent` all route through instead of each hand-rolling the same
/// Completed/Failed mapping (and each risking forgetting the call, as
/// `resume_agent` once did).
///
/// A cancelled turn is deliberately left unmarked: cancellation means the
/// work was interrupted, not that the conversation reached a real endpoint,
/// so the sidechain must stay resumable exactly like one that's still
/// mid-flight.
async fn mark_sidechain_terminal(
    store: Option<&Arc<dyn HistoryStore>>,
    session_id: &str,
    result: &Result<String, base::error::ToolError>,
    cancelled: bool,
) {
    let Some(store) = store else { return };
    if cancelled {
        return;
    }
    let state = if result.is_ok() {
        history::entry::SessionEndState::Completed
    } else {
        history::entry::SessionEndState::Failed
    };
    write_sidechain_terminal_marker(store, session_id, state).await;
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The persistent-member counterpart to `run_sub_inner` — instead of one
/// `run_one_turn` call and done, this loops for as long as messages keep
/// arriving on `message_rx`, reusing the same `Agent`/`event_rx`/`input_tx`
/// across every one of them (that reuse *is* the persistence — a fresh
/// `Agent` per message would have no memory of the previous ones). Exits
/// when `cancel` fires (`AgentTool::stop_team_member`/reclaim) or
/// `message_rx` closes (sender side — the map entry in
/// `Inner::team_members` — dropped, which shouldn't happen while this loop
/// is still registered there, but is handled instead of assumed).
///
/// Each message gets its own `RunningTask` (registered by the caller before
/// queuing it) — status/output are written there exactly like
/// `AgentTool::launch_bg`'s one-shot background tasks, so the model polls a
/// specific message's reply via the ordinary `TaskOutput`/`TaskStop`
/// mechanism, not a new one invented for this.
#[allow(clippy::too_many_arguments)]
async fn run_persistent_member_loop(
    mut agent: Agent,
    mut event_rx: EventReceiver,
    input_tx: InputSender,
    mut message_rx: tokio::sync::mpsc::UnboundedReceiver<PersistentMessage>,
    cancel: tokio_util::sync::CancellationToken,
    registry: Option<Arc<team::registry::TeamRegistry>>,
    team_name: String,
    member_name: String,
    idle_since: Arc<std::sync::atomic::AtomicI64>,
    idle_seq: Arc<std::sync::atomic::AtomicU64>,
    idle_seq_counter: Arc<std::sync::atomic::AtomicU64>,
    session: Arc<base::context::SessionState>,
) {
    loop {
        let msg = tokio::select! {
            _ = cancel.cancelled() => break,
            m = message_rx.recv() => m,
        };
        let Some(PersistentMessage { prompt, task }) = msg else {
            break;
        };

        idle_since.store(i64::MAX, std::sync::atomic::Ordering::SeqCst);
        idle_seq.store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
        if let Some(r) = &registry {
            r.update_member_lifecycle(
                &team_name,
                &member_name,
                team::coordinator::TeammateLifecycle::Active,
                None,
            );
        }

        // Persistent members have no one-shot sidechain terminal marker to
        // write (they outlive any single message), so the cancelled flag
        // `run_one_turn` reports isn't needed here — only the text result is.
        let (result, _cancelled) = run_one_turn(
            &mut agent,
            &mut event_rx,
            &input_tx,
            prompt,
            task.cancel.clone(),
            // No `SubagentTag`: this loop outlives any single parent turn (it
            // is a persistent team member, driven by `message_rx`, not by one
            // `Agent` tool call), so there is no parent turn to attribute its
            // events to. Persistent members surface through `TeamProgress`
            // and `TeamList` instead.
            None,
        )
        .await;

        match &result {
            Ok(text) => {
                task.output
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(text);
                *task.status.lock().unwrap_or_else(|e| e.into_inner()) =
                    base::context::RunningStatus::Completed;
            }
            Err(e) => {
                *task.status.lock().unwrap_or_else(|e| e.into_inner()) =
                    base::context::RunningStatus::Failed(e.to_string());
            }
        }
        session.persist_running_task(&task);
        session.remove_running_task_persistence(&task.task_id);

        let now = now_secs();
        idle_since.store(now, std::sync::atomic::Ordering::SeqCst);
        idle_seq.store(
            idle_seq_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            std::sync::atomic::Ordering::SeqCst,
        );
        if let Some(r) = &registry {
            r.update_member_lifecycle(
                &team_name,
                &member_name,
                team::coordinator::TeammateLifecycle::Idle,
                Some(now as u64),
            );
        }
    }
    if let Some(r) = &registry {
        r.update_member_lifecycle(
            &team_name,
            &member_name,
            team::coordinator::TeammateLifecycle::Shutdown,
            None,
        );
    }
}

/// Build a sub-agent's `Settings` by cloning the parent's real settings and
/// overriding only the model-selection fields — the sub-agent inherits the
/// parent's actual scope/paths (skills/hooks/agents/memory all resolve
/// correctly), auth, sandbox policy, etc.
/// Map a five-level `effort` (`low`/`medium`/`high`/`xhigh`/`max`) onto
/// AttaCore's coarser `ThinkingMode` (`Off`/`Auto`/`On`/`OnBudget`).
/// Lossy by construction — AttaCore has no separate effort axis, this is
/// the closest existing knob, not a new concept. Unrecognized values are
/// left as `None` (no override) rather than guessing.
fn effort_to_thinking_mode(effort: &str) -> Option<ThinkingMode> {
    match effort {
        "low" => Some(ThinkingMode::Off),
        "medium" => Some(ThinkingMode::Auto),
        "high" | "xhigh" | "max" => Some(ThinkingMode::On),
        _ => None,
    }
}

/// How much a permission mode restrains the agent. Higher restrains more.
///
/// There is no natural ordering on `PermissionMode` — the variants describe
/// different policies, not points on a scale — so this is a deliberate
/// ranking, used for exactly one question: is the mode an agent type asks for
/// at least as restrictive as the one it would otherwise inherit
/// ([`apply_agent_type_overrides`])? Ties are fine; only a strict decrease is
/// refused.
fn permission_restraint(mode: base::interface::settings::PermissionMode) -> u8 {
    use base::interface::settings::PermissionMode as M;
    match mode {
        M::Plan => 100,
        M::DontAsk => 90,
        M::Bubble => 60,
        M::Default => 50,
        M::Auto => 40,
        M::AcceptEdits => 30,
        M::Yolo => 20,
        M::BypassPermissions => 10,
    }
}

/// Apply an `AgentTypeDefinition`'s `permission_mode`/`effort`/`max_turns`
/// overrides to an already-built subagent `Settings`, in place. Called after
/// `sub_settings()`/the inline equivalent in `run_sub_inner` — kept as a
/// separate step (not folded into `settings_from_parent`) so it applies
/// identically regardless of which of the two settings-construction paths
/// built the base `Settings`.
///
/// Overrides from a plugin-declared type are **clamped**: the mode may only
/// hold or increase [`permission_restraint`], and the API-call cap may only
/// hold or decrease. An agent type is a plain data declaration, and once
/// plugins can ship one it arrives over the network — a package naming
/// `permission_mode = "bypassPermissions"` would otherwise switch the
/// permission gate off for everything its sub-agent does, and `max_turns =
/// 10000` would spend the user's tokens at a rate they never agreed to.
///
/// Types the user controls keep the unclamped behavior: overriding your own
/// session is the point of writing the file. `ExecutionParams`'s
/// `max_api_calls_per_turn` documents the same "one side must not silently
/// widen the other" rule for scenes; this brings agent types in line.
fn apply_agent_type_overrides(settings: &mut Settings, def: &AgentTypeDefinition) {
    if let Some(mode) = def.permission_mode {
        if def.source.may_loosen()
            || permission_restraint(mode) >= permission_restraint(settings.permission_mode)
        {
            settings.permission_mode = mode;
        } else {
            tracing::warn!(
                agent_type = %def.name,
                requested = ?mode,
                inherited = ?settings.permission_mode,
                "plugin-declared agent type asked for a looser permission mode; keeping the inherited one"
            );
        }
    }
    if let Some(effort) = &def.effort {
        if let Some(mode) = effort_to_thinking_mode(effort) {
            settings.model.thinking_mode = mode;
        } else {
            tracing::warn!(effort = %effort, agent_type = %def.name, "unrecognized effort value, ignoring");
        }
    }
    if let Some(max_turns) = def.max_turns {
        settings.execution.max_api_calls_per_turn = if def.source.may_loosen() {
            max_turns
        } else {
            max_turns.min(settings.execution.max_api_calls_per_turn)
        };
    }
}

fn settings_from_parent(
    parent: &Settings,
    model_name: String,
    max_tokens: u32,
    fallback_model: Option<String>,
    instruction_file: Option<&std::path::PathBuf>,
) -> Settings {
    let mut settings = parent.clone();
    settings.model.model_name = model_name;
    settings.model.max_tokens = max_tokens;
    settings.model.fallback_model = fallback_model;
    // S-2: the sub-agent must see the project's AGENTS.md / CLAUDE.md, or its
    // output style diverges from the main agent's for no reason the user can
    // see. Cloning the parent already carries `Settings.instruction_file`;
    // `instruction_file` here additionally covers the case where the parent
    // got its instruction file from `Builder::instruction_file(..)` rather
    // than from settings.json — a `Builder`-level value that is invisible in
    // `Settings` and would otherwise be silently dropped for sub-agents.
    if let Some(p) = instruction_file {
        settings.instruction_file = Some(p.clone());
    }
    settings
}

/// Fallback used when no parent `Settings` is available (`AgentTool`
/// constructed without `.with_settings(...)` — tests, or the generic
/// `AgentSpawner` bridge which doesn't carry one). Scope is a fixed
/// `"code"` stand-in since there's no real one to thread through.
///
/// The user-level roots hang off `local_data_dir`, which means a
/// parentless sub-agent keeps its state beside the project. They used to be
/// derived from `$HOME`: with no instance configuration in hand, the
/// fallback would pick the invoking user's home and write there — the one
/// directory it has no grounds to assume it owns. Project-local is the
/// honest answer to "nobody told me where this instance's state lives".
fn fallback_settings(
    model_name: String,
    max_tokens: u32,
    fallback_model: Option<String>,
    local_data_dir: std::path::PathBuf,
    instruction_file: Option<&std::path::PathBuf>,
) -> Settings {
    Settings {
        model: ModelSettings {
            api_type: base::provider::ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name,
            max_tokens,
            thinking_mode: ThinkingMode::Auto,
            fallback_model,
        },
        paths: PathSettings {
            user_data_dir: local_data_dir.join("scenes").join("code"),
            global_data_dir: local_data_dir.clone(),
            local_data_dir,
            scope: "code".to_string(),
        },
        execution: ExecutionSettings::default(),
        compaction: Default::default(),
        sandbox: SandboxConfig::default(),
        plugins: Default::default(),
        // S-2: no parent `Settings` to inherit from here, but the caller may
        // still know the instruction file (`AgentTool::set_instruction_file`).
        instruction_file: instruction_file.cloned(),
        prompt_append: None,
        prompt_override: None,
        recorder: None,
        telemetry_url: None,
        session_dir: None,
        memory_enabled: true,
        disable_skill_shell_execution: false,
        permission_mode: PermissionMode::default(),
        permission_rules: Vec::new(),
        local_permission_rules: Vec::new(),
        allow_client_permission_override: false,
        telemetry_enabled: true,
        hooks_config: None,
        mcp_servers: Default::default(),
        providers: Default::default(),
        default_provider: None,
        task_models: Default::default(),
        language: None,
        feature_flags: Default::default(),
        // A sub-agent inherits no script bindings: a script bound in the
        // parent's configuration applies to the parent's prompt, and quietly
        // running it against every spawned agent's prompt too would be a
        // scope nobody asked for.
        scripts: Vec::new(),
    }
}

// ═══════════════════════════════════════════════════════
// Inner state
// ═══════════════════════════════════════════════════════

/// Excluded from the tool set a sub-agent inherits, so a "full access" type
/// (general-purpose/claude/any custom type with an empty `allowed_tools`)
/// doesn't hand delegation straight back to the child.
///
/// This filter is a convenience, not the bound: what actually stops a
/// delegation chain is `Inner::depth` counted against
/// `EngineConfig::max_agent_depth`, enforced in `spawn_guard` on every spawn
/// path and mirrored by `Builder::build()` withholding this tool at the
/// limit. Filtering alone cannot bound anything — `build()` creates a fresh
/// per-session registry and registers `Agent` into it regardless of what
/// `resolve_tools` handed over, and `RuntimeAgentSpawner` (Skill
/// `context: fork`, team members) bypasses `resolve_tools` entirely by
/// passing `sub_tools()` through unfiltered.
const AGENT_TOOL_NAME: &str = "Agent";

/// A live persistent team member (`Agent` tool's `team_name`+`name` mode).
/// Its `Agent`/`event_rx`/`input_tx` live inside `run_persistent_member_loop`
/// (the spawned task `join` would represent, if kept — not kept here since
/// nothing currently awaits it; `cancel` is enough to stop it).
struct PersistentMember {
    /// Internal-only identifier, never surfaced to the model — routing is
    /// always by the `(team_name, member_name)` map key (reusing the same
    /// name is *how* you keep messaging the same member; that's the point
    /// of persistence). This exists purely for logs/observability so two
    /// members that happened to reuse a name at different times (one
    /// stopped, a new one spawned under the same key later) are
    /// distinguishable in a trace.
    id: String,
    message_tx: tokio::sync::mpsc::UnboundedSender<PersistentMessage>,
    cancel: tokio_util::sync::CancellationToken,
    /// Epoch seconds since this member last went idle (finished a turn with
    /// nothing queued after it), or `i64::MAX` while it's currently
    /// processing one — the sentinel keeps a mid-turn member permanently
    /// ineligible for the idle-timeout reclaim rule (b) without needing a
    /// separate `Active`/`Idle` flag alongside the timestamp. Wall-clock,
    /// so it's meaningful to compare against `team_member_idle_timeout_secs`
    /// and to show in `TeamList` — but *not* used to order which member is
    /// "oldest idle" for the total-count reclaim rule (a): see `idle_seq`.
    idle_since: Arc<std::sync::atomic::AtomicI64>,
    /// Monotonically increasing on every idle transition (`u64::MAX` while
    /// `Active`, same sentinel pattern as `idle_since`) — reclaim rule (a)
    /// sorts by *this*, not by `idle_since`. Two members can easily go idle
    /// within the same wall-clock second (`idle_since` only has 1-second
    /// resolution), which would make a plain timestamp sort non-
    /// deterministically tie-break between them; a strictly-increasing
    /// counter can't tie, so "oldest idle" always means the same thing
    /// regardless of how fast members actually run.
    idle_seq: Arc<std::sync::atomic::AtomicU64>,
}

struct PersistentMessage {
    prompt: String,
    task: Arc<base::context::RunningTask>,
}

#[derive(Clone)]
struct Inner {
    model: Arc<dyn Model>,
    config: Arc<EngineConfig>,
    fallback_tools: Arc<InMemoryToolRegistry>,
    parent_tools: Arc<InMemoryToolRegistry>,
    /// Delegation depth of the agent that *owns* this tool, not of the
    /// children it spawns — those get `depth + 1` (see `spawn_guard`).
    depth: u32,
    mailbox: Option<(std::sync::Arc<team::mailbox::MailboxStore>, String)>,
    /// Built-in types merged with any disk-loaded custom types (see
    /// `merge_agent_types`). Keyed by `subagent_type` name. `RwLock<Arc<..>>`
    /// (not a plain `Arc<..>`) so `agent_type_watcher::AgentTypeWatcher` can
    /// swap the whole map in place when a `.atta/agents/*.md` file changes —
    /// readers clone the inner `Arc` out under a brief read lock (see
    /// `Inner::agent_type_def`) rather than holding the guard, so a lookup
    /// never blocks a concurrent reload or vice versa.
    agent_types: SharedAgentTypeMap,
    /// Kept alive only so the watcher's background thread keeps running —
    /// dropped (and thus stopped) only once every clone of this `Inner` is
    /// gone. `None` when watching wasn't set up (no watched dirs existed, or
    /// `notify` setup failed) — degrades to the pre-watcher behavior of a
    /// build-time-only catalog, same as `SkillManager::enable_watching`'s
    /// failure mode.
    _agent_type_watcher: Option<Arc<crate::agent_type_watcher::AgentTypeWatcher>>,
    /// Precomputed from the catalog at construction time — see
    /// `describe_agent_types`. Deliberately **not** kept live-reloaded like
    /// `agent_types` itself: `Tool::description(&self) -> &str` returns a
    /// borrowed `&str`, which can't be produced from data read out from
    /// behind a lock inside the function body (nothing to borrow it from).
    /// Net effect: a subagent_type added on disk after this `AgentTool` was
    /// built becomes *usable* immediately (via `agent_type_def`), but won't
    /// appear in the model-facing catalog text (`description()`) until the
    /// next full rebuild. Functional reload without cosmetic reload — an
    /// accepted, documented scope boundary, not an oversight.
    description: Arc<str>,
    /// The parent `Agent`'s own settings, when known (set via
    /// `.with_settings(...)`) — `sub_settings()`/`run_sub_inner()` clone this
    /// (overriding only the model fields) instead of a hardcoded
    /// `scope: "code"` stand-in, so a sub-agent's skills/hooks/agents
    /// lookups resolve against the parent's *real* scope, not a guess.
    /// `None` when unset (e.g. in tests, or the generic `AgentSpawner`
    /// bridge) falls back to the previous hardcoded-`"code"` behavior.
    parent_settings: Option<Arc<Settings>>,
    /// Multi-provider per-task-type model routing — see
    /// `Builder::task_router` / `Inner::model_for_subagent`. `None` (the
    /// default) means every sub-agent spawn inherits `model` unchanged,
    /// exactly matching behavior before multi-provider routing existed.
    task_router: Option<Arc<base::provider::TaskRouter>>,
    /// The operator's scripts that follow a delegate into its own session —
    /// the rings around tool and model calls, never the prompt contributions.
    /// See `BoundScripts::for_delegate` for where that line is drawn and why.
    delegate_scripts: Option<Arc<base::interface::script_adapters::BoundScripts>>,
    /// The parent session's `SkillManager`, when known — used to resolve
    /// the `skills:` preload field on `AgentTypeDefinition`. Set via
    /// `AgentTool::set_skill_manager` *after* construction (interior
    /// mutability, `RwLock` not a plain field) because `Builder::build()`
    /// constructs `AgentTool` before it loads skills — see the call site in
    /// `agent.rs` for why reordering wasn't the simpler fix. `None` (tests,
    /// the generic `AgentSpawner` bridge) means `skills:` preload is
    /// silently skipped, matching how `parent_settings`/`task_router`
    /// absence degrades elsewhere in this struct.
    skill_manager: Arc<std::sync::RwLock<Option<Arc<skills::manager::SkillManager>>>>,
    /// A snapshot of the parent session's connected MCP tools, used to
    /// resolve `AgentTypeDefinition.mcp_servers` by reusing already-
    /// connected tool instances (not reconnecting). Same interior-
    /// mutability / set-after-construction pattern as `skill_manager` and
    /// for the same reason (`Builder::build()` connects MCP after
    /// `AgentTool` exists) — but a `Vec` snapshot rather than a live
    /// `Arc<McpManager>` reference: `McpManager` isn't `Clone` (it holds a
    /// `Vec<Box<dyn McpNotificationHandler>>`) and `refresh_tools()` needs
    /// `&mut self`, which would conflict with `AgentTool` also holding a
    /// shared reference to the same live instance. A subagent therefore
    /// sees the MCP tools connected *as of when this was set* — a later
    /// `config.reload` reconnect in the parent session doesn't retroactively
    /// update it, an accepted scope boundary (matches `description`'s own
    /// "not kept live" doc comment on this same struct).
    mcp_tool_adapters: Arc<std::sync::RwLock<Vec<Arc<dyn base::tool::Tool>>>>,
    /// The shared `TeamRegistry` `Builder::build()` also hands to
    /// `TeamCreate`/`TeamList`/`TeamDelete` — so persistent team members
    /// (spawned here, not through those tools) update the same state
    /// `TeamList` reads. Same interior-mutability / set-after-construction
    /// pattern as `skill_manager`/`mcp_tool_adapters`: `None` when team
    /// coordination isn't enabled for this session (`settings.execution.
    /// team_enabled == false`) or in tests that don't wire it — persistent
    /// team members just don't update `TeamList`'s view in that case (they
    /// still work; `TeamList` isn't reachable either when team_enabled is
    /// off, since it isn't registered).
    team_registry: Arc<std::sync::RwLock<Option<Arc<team::registry::TeamRegistry>>>>,
    /// Live persistent team members, keyed by `(team_name, member_name)`.
    /// See `AgentTool::spawn_or_message_team_member`'s doc comment for the
    /// full mechanism. Scoped to this `AgentTool` (i.e. this session) —
    /// members don't survive a daemon restart or migrate across sessions.
    team_members:
        Arc<std::sync::RwLock<std::collections::HashMap<(String, String), PersistentMember>>>,
    /// Source of `PersistentMember::idle_seq` values — one counter shared
    /// by every member in this session, so "oldest idle" has a single,
    /// unambiguous, collision-free ordering across all of them.
    idle_seq_counter: Arc<std::sync::atomic::AtomicU64>,
    /// Serializes `spawn_or_message_team_member`'s whole find-or-create
    /// decision. Without this, two concurrent calls for the same
    /// `(team_name, member_name)` key (the `Agent` tool is
    /// `is_concurrency_safe`, so this is reachable — e.g. the model issuing
    /// two parallel tool calls that both target the same not-yet-existing
    /// member) can both read "doesn't exist yet" before either has
    /// inserted, and both spawn — the second insert silently overwrites the
    /// first's map entry while its loop keeps running orphaned, unreachable
    /// and never reclaimed. One session-wide lock (not per-key) is enough:
    /// the section it guards is cheap (map lookup/insert, sync `Agent`
    /// construction, no real I/O), so serializing it isn't a meaningful
    /// bottleneck for something that isn't a hot path to begin with.
    team_spawn_lock: Arc<tokio::sync::Mutex<()>>,
    /// The **parent session's** event channel. Every event a sub-agent emits
    /// is re-emitted here wrapped in `AgentEvent::SubagentProgress`, so the
    /// host can render sub-agent activity live instead of seeing a long
    /// pause followed by a wall of text (S-1). `None` (tests, embedders that
    /// never call `set_event_sender`) degrades to the previous behavior:
    /// events are still collected internally into the returned text, they
    /// just aren't mirrored anywhere.
    ///
    /// Same interior-mutability, set-after-construction pattern as
    /// `skill_manager` — `Builder::build()` creates the `AgentTool` before it
    /// creates the session's event channel.
    event_tx: Arc<std::sync::RwLock<Option<crate::agent::EventSender>>>,
    /// The parent session's hook runner, so sub-agent spawns can fire
    /// `SubagentStart`/`SubagentStop`. Same set-after-construction pattern as
    /// `event_tx` — `Builder::build()` constructs the `AgentTool` before the
    /// `HookRunner` exists. `None` (tests, embedders that never wire it)
    /// fires nothing.
    hooks: Arc<std::sync::RwLock<Option<Arc<hooks::HookRunner>>>>,
    /// The parent session's history store, so a sub-agent's transcript is
    /// persisted and a failed sub-agent can be inspected afterwards (S-4).
    /// The sub-agent gets its own fresh session id, so it lands in its own
    /// JSONL file — the parent's transcript is never written to.
    history_store: Arc<std::sync::RwLock<Option<Arc<dyn HistoryStore>>>>,
    /// The **parent agent's** `Permission` impl. Sub-agent permission checks
    /// are routed here (S-3) instead of being hard-permitted, so a session
    /// that opts into a real permission handler can't be bypassed just by
    /// spawning a sub-agent. `None` keeps the historical `AlwaysPermit`
    /// behavior for `AgentTool`s built without wiring (tests).
    parent_permission: Arc<std::sync::RwLock<Option<Arc<dyn Permission>>>>,
    /// The **parent agent's** pending-permission registry, so a sub-agent's
    /// `Prompt` outcome can be turned into a real, answerable question
    /// instead of an automatic refusal (N-8).
    ///
    /// A sub-agent has no channel of its own to a human, so `ParentPermission`
    /// used to convert every `Prompt` into a `Deny`. Once the default mode
    /// became *ask*, that meant a sub-agent could not run a test suite, a
    /// build, or a commit — anything not covered by a rule was refused
    /// outright, which is most of what a sub-agent is spawned to do. The
    /// parent, though, does have a channel: registering the prompt here and
    /// emitting it on `event_tx` puts it in front of the same host that
    /// answers the parent's own prompts, via the same
    /// `session.respondToPrompt` RPC. `None` (unwired embedders) keeps the
    /// refuse-with-an-explanation behavior.
    parent_pending_permissions: Arc<std::sync::RwLock<Option<crate::agent::PendingPermissions>>>,
    /// The parent agent's scene. Sub-agents inherit it (S-5) rather than
    /// always being built as `CodingScene` — a Research-scene parent used to
    /// hand its sub-agents a programming-shop system prompt. `None` falls
    /// back to resolving `Settings.paths.scope` via `resolve_scene_id`, then to
    /// `CodingScene`.
    parent_scene: Arc<std::sync::RwLock<Option<Arc<dyn AgentScene>>>>,
    /// Every scene this process knows about, for resolving an agent type's
    /// `scene:` id. `None` falls back to [`builtin_scene_by_id`], which is
    /// what an embedder that never wires a registry (tests, library callers)
    /// gets — the built-in four, exactly as before. A host that registers
    /// scenes beyond those (the daemon, which also registers plugin scenes)
    /// wires its registry here so those ids resolve too.
    scene_registry: Arc<std::sync::RwLock<Option<Arc<scene::scene::SceneRegistry>>>>,
    /// The parent's resolved instruction file (AGENTS.md / CLAUDE.md), for
    /// the case where it came from `Builder::instruction_file(..)` and so
    /// isn't visible in `Settings` — see `settings_from_parent`.
    instruction_file: Arc<std::sync::RwLock<Option<std::path::PathBuf>>>,
    /// The parent `Agent`'s own session id, so a freshly spawned sub-agent's
    /// first `Meta` line can record `parent_session_id` and thereby become
    /// discoverable via `HistoryStore::child_sessions`. Same set-after-
    /// construction pattern as `event_tx`/`hooks`/`history_store` —
    /// `Builder::build()` creates the `AgentTool` before its own session id
    /// is finalized. `None` (tests, embedders that never call
    /// `set_parent_session_id`) means spawned sub-agents' `Meta` lines are
    /// written with `parent_session_id: None` — they still persist, they're
    /// just not linkable back to a parent.
    parent_session_id: Arc<std::sync::RwLock<Option<String>>>,
    /// The parent `Agent`'s own telemetry handle — sub-agents built through
    /// this `AgentTool` inherit it instead of each falling back to
    /// `Builder::build()`'s own noop default (a channel whose receiver is
    /// dropped immediately, silently discarding every event). Same set-
    /// after-construction pattern as the other parent-state fields on this
    /// struct. `None` (tests, embedders that never call
    /// `set_telemetry_handle`) means spawned sub-agents get `Builder`'s
    /// usual noop fallback — unchanged from before this field existed.
    telemetry_handle: Arc<std::sync::RwLock<Option<TelemetryHandle>>>,
}

impl Inner {
    /// Depth to build a child at, or the message to hand the model when the
    /// chain has run out of room.
    ///
    /// Every spawn path goes through here — the `Agent` tool, Skill
    /// `context: fork` and team members via `RuntimeAgentSpawner`, and
    /// background spawns — because that is the only place all of them share.
    /// Bounding them individually by tool-registry filtering does not work:
    /// `RuntimeAgentSpawner` hands `sub_tools()` over unfiltered, and
    /// `Builder::build()` registers `Agent` into the fresh per-session
    /// registry it creates for the child regardless.
    ///
    /// Refusing with a normal tool error rather than a panic or a silent
    /// no-op is the point: the model sees "you cannot delegate further" and
    /// can do the work itself, which is what a runaway retry loop needs in
    /// order to terminate instead of recursing until the process dies.
    fn spawn_guard(&self) -> Result<u32, String> {
        let max = self.config.max_agent_depth;
        if self.depth >= max {
            return Err(format!(
                "Delegation depth limit reached ({max}). This agent is already {} \
                 level(s) deep and cannot spawn another sub-agent. Complete the \
                 task directly instead of delegating it.",
                self.depth
            ));
        }
        Ok(self.depth + 1)
    }

    /// The model instance a sub-agent spawn should use. All three spawn
    /// points (`run_sub`, `run_sub_inner`, resume) are the same task type —
    /// `task_models` in settings.json is keyed by a small fixed taxonomy
    /// (`main`/`subagent`/`team`/`classifier`/`compact`/`web_fetch`), not
    /// by `subagent_type` (which agent *kind* — "general-purpose",
    /// "code-reviewer", etc. — a different axis entirely), so the lookup
    /// key here is always the literal `"subagent"` task type regardless of
    /// which `subagent_type` was requested.
    /// Everything a delegated session inherits from the one that spawned it.
    ///
    /// Written once because the list is not derivable from anything: each item
    /// is on it because somebody noticed it was missing, and the noticing
    /// happened once per item — a transcript that was not persisted, telemetry
    /// dropped on the floor, a policy that stopped at the first delegation. A
    /// fifth spawn site copying three of the four is the shape every one of
    /// those had.
    ///
    /// Deliberately not here: what a delegate decides for itself — its scene,
    /// its model, its tool set, its depth, its own settings. Those differ per
    /// spawn site, which is why they stay at the call sites.
    fn inherited_by_a_delegate(&self, mut builder: Builder) -> Builder {
        // A policy that stops at the first delegation is not a policy; the
        // prompt contributions stay behind. See `BoundScripts::for_delegate`.
        if let Some(scripts) = &self.delegate_scripts {
            builder = builder.bound_scripts(scripts.for_delegate());
        }
        // Its own session id keeps a delegate's transcript in a file of its
        // own rather than mixed into the parent's.
        if let Some(store) = self.history_store() {
            builder = builder.history_store(store);
        }
        // Without this, every event a delegate produces — tool timings,
        // permission decisions, cost — goes to `Builder::build()`'s noop.
        if let Some(handle) = self.telemetry_handle() {
            builder = builder.telemetry_handle(handle);
        }
        builder
    }

    fn model_for_subagent(&self) -> Arc<dyn Model> {
        match &self.task_router {
            Some(router) => router.model_for("subagent"),
            None => self.model.clone(),
        }
    }

    /// Look up one agent type by name, cloning it out from behind the
    /// `agent_types` lock. Always returns an owned value (never a borrow
    /// tied to the read guard) — required now that `agent_types` can be
    /// swapped in place by the watcher; a borrow into the old map would
    /// dangle the instant a reload swaps it out. The clone itself is cheap:
    /// a handful of strings and a short tool-name list, not a hot-path
    /// concern at sub-agent-spawn frequency.
    fn agent_type_def(&self, name: &str) -> Option<AgentTypeDefinition> {
        self.agent_types.read().unwrap().get(name).cloned()
    }

    fn skill_manager(&self) -> Option<Arc<skills::manager::SkillManager>> {
        self.skill_manager.read().unwrap().clone()
    }

    fn mcp_tool_adapters(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        self.mcp_tool_adapters.read().unwrap().clone()
    }

    fn event_tx(&self) -> Option<crate::agent::EventSender> {
        self.event_tx.read().unwrap().clone()
    }

    fn history_store(&self) -> Option<Arc<dyn HistoryStore>> {
        self.history_store.read().unwrap().clone()
    }

    fn instruction_file(&self) -> Option<std::path::PathBuf> {
        self.instruction_file.read().unwrap().clone()
    }

    fn parent_session_id(&self) -> Option<String> {
        self.parent_session_id.read().unwrap().clone()
    }

    fn telemetry_handle(&self) -> Option<TelemetryHandle> {
        self.telemetry_handle.read().unwrap().clone()
    }

    /// See `AgentTool::permission_handler` — same resolution, reachable from
    /// the static `run_sub_inner` path (background spawns), which used to
    /// hardcode `AlwaysPermit` and so silently escaped the parent's rules.
    fn permission_handler(&self) -> Arc<dyn Permission> {
        if let Some((ref mailbox, ref label)) = self.mailbox {
            // Team member: bubble the prompt up to the coordinator's mailbox.
            return Arc::new(team::coordinator::PermissionBridge::new(
                mailbox.clone(),
                label.clone(),
                "coordinator",
            ));
        }
        if let Some(parent) = self.parent_permission.read().unwrap().clone() {
            return Arc::new(ParentPermission {
                inner: parent,
                event_tx: self.event_tx(),
                pending: self.parent_pending_permissions.read().unwrap().clone(),
            });
        }
        Arc::new(AlwaysPermit)
    }

    /// The scene a sub-agent should run in.
    ///
    /// Precedence: the agent type's own `scene:` override > the parent's
    /// scene (wired via `AgentTool::set_scene`) > the scene named by
    /// `Settings.paths.scope` > `CodingScene`. The last two steps mean a
    /// daemon session inherits correctly even without explicit wiring: the
    /// daemon sets `scope` to `scene.id()`.
    /// Build a sub-agent's `Settings`, inheriting the parent's when known.
    /// Shared by the `&self` (`run_sub`, resume) and static (`run_sub_inner`,
    /// background) paths so both apply exactly the same inheritance.
    fn sub_settings(inner: &Inner, model_name: Option<&str>, cwd: std::path::PathBuf) -> Settings {
        let c = &inner.config;
        let model_name = model_name.unwrap_or(&c.model).to_string();
        let instruction_file = inner.instruction_file();
        match &inner.parent_settings {
            Some(parent) => settings_from_parent(
                parent,
                model_name,
                c.max_tokens,
                c.fallback_model.clone(),
                instruction_file.as_ref(),
            ),
            None => fallback_settings(
                model_name,
                c.max_tokens,
                c.fallback_model.clone(),
                cwd,
                instruction_file.as_ref(),
            ),
        }
    }

    fn scene_for_subagent(
        &self,
        def: Option<&AgentTypeDefinition>,
        settings: &Settings,
    ) -> Arc<dyn AgentScene> {
        let parent = self.parent_scene.read().unwrap().clone();
        if let Some(name) = def.and_then(|d| d.scene.as_deref()) {
            match self.resolve_scene_id(name) {
                Some(requested) => {
                    // An agent type may *narrow* the scene, never widen it
                    // (N-17).
                    //
                    // Agent types are loaded from `.atta/agents/*.md`, which
                    // is repository content — on a cloned repo it is
                    // attacker-supplied. A scene's `tools()` whitelist is the
                    // enforcement point for "this session may not run shell
                    // commands" (ChatScene and ResearchScene both rely on it,
                    // and say so in their doc comments), so letting a file in
                    // the repo pick the scene meant a Research session could
                    // spawn `Agent(subagent_type: "x")` where `x.md` said
                    // `scene: code` and get `Bash` back. The boundary the
                    // parent scene draws has to hold across a spawn.
                    //
                    // "Narrower" is judged by tool surface: the requested
                    // scene is accepted only when everything it exposes is
                    // also exposed by the parent.
                    match parent.as_ref() {
                        Some(p) if !scene_is_narrower_or_equal(requested.as_ref(), p.as_ref()) => {
                            tracing::warn!(
                                scene = %name,
                                parent_scene = %p.name(),
                                agent_type = %def.map(|d| d.name.as_str()).unwrap_or(""),
                                "agent type requested a scene that grants tools the parent scene \
                                 does not; ignoring it and inheriting the parent's scene"
                            );
                        }
                        _ => return requested,
                    }
                }
                None => {
                    tracing::warn!(
                        scene = %name,
                        agent_type = %def.map(|d| d.name.as_str()).unwrap_or(""),
                        "unrecognized scene in agent type definition, inheriting parent's scene instead"
                    );
                }
            }
        }
        if let Some(parent) = parent {
            return parent;
        }
        self.resolve_scene_id(&settings.paths.scope)
            .unwrap_or_else(|| Arc::new(scene::scene::coding::CodingScene))
    }

    /// A scene id resolved against the host's registry when one was wired,
    /// otherwise against the built-in set. Plugin scenes (`plugin:<name>`)
    /// only exist in the registry, so without this an agent type naming one
    /// would silently fall through to "unrecognized scene" and inherit the
    /// parent's instead.
    fn resolve_scene_id(&self, id: &str) -> Option<Arc<dyn AgentScene>> {
        if let Some(registry) = self.scene_registry.read().unwrap().as_ref() {
            return registry.resolve(id);
        }
        builtin_scene_by_id(id)
    }
}

/// Does `candidate` expose a subset of `parent`'s tools?
///
/// Used to decide whether an agent type's `scene:` override may be honored —
/// see `scene_for_subagent`. A scene's `tools()` is an allow-list, with the
/// empty list meaning "everything registered is allowed"; `disallowed_tools()`
/// subtracts from it.
///
/// So: a parent with an empty allow-list (CodingScene) permits any child. A
/// parent with a real allow-list permits only a child whose own allow-list is
/// non-empty and contained in it, minus anything the parent disallows.
fn scene_is_narrower_or_equal(candidate: &dyn AgentScene, parent: &dyn AgentScene) -> bool {
    let parent_allow = parent.tools();
    let parent_deny = parent.disallowed_tools();
    if parent_allow.is_empty() && parent_deny.is_empty() {
        return true;
    }
    let candidate_allow = candidate.tools();
    if candidate_allow.is_empty() {
        // "Everything registered" is never a subset of a real allow-list.
        return false;
    }
    candidate_allow
        .iter()
        .all(|t| !parent_deny.contains(t) && (parent_allow.is_empty() || parent_allow.contains(t)))
}

/// Everything needed to re-emit one sub-agent's events on the parent's
/// channel. Built once per spawn; cloned into the collector task.
#[derive(Clone)]
struct SubagentTag {
    parent_tx: crate::agent::EventSender,
    agent_label: String,
    agent_session_id: String,
    agent_type: Option<String>,
    parent_session_id: String,
    parent_turn: u32,
}

impl SubagentTag {
    /// Build a tag, or `None` when there's no parent channel to forward to
    /// (in which case the collector degrades to pure text accumulation).
    fn new(
        inner: &Inner,
        agent_session_id: &str,
        agent_type: Option<&str>,
        parent: Option<(&str, u32)>,
    ) -> Option<Self> {
        let parent_tx = inner.event_tx()?;
        let (parent_session_id, parent_turn) = parent.unwrap_or(("", 0));
        Some(Self {
            parent_tx,
            agent_label: subagent_label(agent_type, agent_session_id),
            agent_session_id: agent_session_id.to_string(),
            agent_type: agent_type.map(str::to_string),
            parent_session_id: parent_session_id.to_string(),
            parent_turn,
        })
    }

    fn forward(&self, event: AgentEvent) {
        let _ = self.parent_tx.send(AgentEvent::SubagentProgress {
            agent_label: self.agent_label.clone(),
            agent_session_id: self.agent_session_id.clone(),
            agent_type: self.agent_type.clone(),
            parent_session_id: self.parent_session_id.clone(),
            parent_turn: self.parent_turn,
            event: Box::new(event),
        });
    }

    /// Bracket the delegation with `AgentSpawned`/`AgentCompleted` on the
    /// parent's channel.
    ///
    /// Sent unwrapped, not through `forward` — these describe the *parent's*
    /// timeline ("a delegation started/ended here"), whereas
    /// `SubagentProgress` wraps events that happened inside the child. An
    /// embedder tracking sub-agent lifecycle wants the pair; it should not
    /// have to infer boundaries from the `agent_label` on progress events.
    ///
    /// `agent_id` is the same `agent_label` every `SubagentProgress` for
    /// this run carries, so the three correlate without extra bookkeeping.
    fn spawned(&self) {
        let _ = self.parent_tx.send(AgentEvent::AgentSpawned {
            agent_id: self.agent_label.clone(),
            parent_turn: self.parent_turn,
            turn_id: self.agent_session_id.clone(),
        });
    }

    fn completed(&self, outcome: &str) {
        let _ = self.parent_tx.send(AgentEvent::AgentCompleted {
            agent_id: self.agent_label.clone(),
            outcome: outcome.to_string(),
            turn_id: self.agent_session_id.clone(),
        });
    }
}

/// Stable, human-readable label for one sub-agent run — identical on every
/// event that run emits, so a host can fold them under a single node.
fn subagent_label(agent_type: Option<&str>, session_id: &str) -> String {
    let short: String = session_id.chars().filter(|c| *c != '-').take(8).collect();
    format!("{}#{short}", agent_type.unwrap_or("agent"))
}

/// Render the full content of every named skill for preloading into a
/// spawned subagent's initial context (`AgentTypeDefinition.skills`).
///
/// A name that doesn't resolve, or resolves to a `disable_model_invocation:
/// true` skill (preloading draws from the same invocable set the Skill tool
/// itself can call), is skipped with a `tracing::warn!` rather than failing
/// the whole spawn.
/// Returns an empty string when nothing resolved, so callers can
/// unconditionally prepend the result without an extra branch.
fn preload_skills_text(mgr: &skills::manager::SkillManager, skill_names: &[String]) -> String {
    let mut sections = Vec::new();
    for name in skill_names {
        let Some(info) = mgr.get(name) else {
            tracing::warn!(skill = %name, "agent skills: preload — skill not found, skipping");
            continue;
        };
        if info.disable_model_invocation {
            tracing::warn!(
                skill = %name,
                "agent skills: preload — skill has disable_model_invocation set, cannot be preloaded, skipping"
            );
            continue;
        }
        let Some(content) = mgr.get_skill_content(name) else {
            tracing::warn!(skill = %name, "agent skills: preload — skill has no readable content, skipping");
            continue;
        };
        sections.push(format!("## Preloaded skill: {name}\n\n{content}"));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!(
            "<system-reminder>\nThe following skills were preloaded for this task — apply them without needing to call the Skill tool.\n\n{}\n</system-reminder>\n\n",
            sections.join("\n\n---\n\n")
        )
    }
}

pub struct AgentTool {
    inner: Arc<Inner>,
}

impl AgentTool {
    /// Construct with only the built-in agent types (no `.atta/agents/`
    /// scan) — convenience for callers that don't need custom types (e.g.
    /// tests, or embedders that manage their own catalog).
    pub fn new(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        fallback_tools: Arc<InMemoryToolRegistry>,
    ) -> Self {
        Self::with_parent_tools(
            model,
            config,
            fallback_tools.clone(),
            fallback_tools,
            &[],
            &[],
        )
    }

    /// `agent_dirs` are scanned low → high priority (see `merge_agent_types`)
    /// for `.atta/agents/*.md` custom type definitions, on top of the 6
    /// built-in types and any `plugin_agent_types`. Pass `&[]` for either to
    /// skip that tier.
    pub fn with_parent_tools(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        fallback_tools: Arc<InMemoryToolRegistry>,
        agent_dirs: &[&Path],
        plugin_agent_types: &[AgentTypeDefinition],
    ) -> Self {
        let agent_types = merge_agent_types(agent_dirs, plugin_agent_types);
        let description: Arc<str> = describe_agent_types(&agent_types).into();
        let agent_types = Arc::new(std::sync::RwLock::new(Arc::new(agent_types)));

        // Live reload: watch the same directories for `.md` changes and keep
        // `agent_types` current without a session rebuild — see
        // `agent_type_watcher`'s module doc for why a full re-merge (not a
        // per-file patch) is the correct reload strategy here. `notify`
        // setup failing (permissions, inotify/kqueue limits) degrades to the
        // pre-watcher, build-time-only catalog rather than failing
        // construction — same non-fatal handling as
        // `SkillManager::enable_watching`.
        let owned_dirs: Vec<std::path::PathBuf> =
            agent_dirs.iter().map(|p| p.to_path_buf()).collect();
        let _agent_type_watcher = crate::agent_type_watcher::AgentTypeWatcher::watch(
            owned_dirs,
            plugin_agent_types.to_vec(),
            agent_types.clone(),
        )
        .map(Arc::new)
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to enable agent-type file watching; live reload disabled for this session");
        })
        .ok();

        Self {
            inner: Arc::new(Inner {
                model,
                config,
                fallback_tools,
                parent_tools,
                depth: 0,
                mailbox: None,
                agent_types,
                _agent_type_watcher,
                description,
                parent_settings: None,
                task_router: None,
                delegate_scripts: None,
                skill_manager: Arc::new(std::sync::RwLock::new(None)),
                mcp_tool_adapters: Arc::new(std::sync::RwLock::new(Vec::new())),
                team_registry: Arc::new(std::sync::RwLock::new(None)),
                team_members: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                idle_seq_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                team_spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
                event_tx: Arc::new(std::sync::RwLock::new(None)),
                hooks: Arc::new(std::sync::RwLock::new(None)),
                history_store: Arc::new(std::sync::RwLock::new(None)),
                parent_permission: Arc::new(std::sync::RwLock::new(None)),
                parent_pending_permissions: Arc::new(std::sync::RwLock::new(None)),
                parent_scene: Arc::new(std::sync::RwLock::new(None)),
                scene_registry: Arc::new(std::sync::RwLock::new(None)),
                instruction_file: Arc::new(std::sync::RwLock::new(None)),
                parent_session_id: Arc::new(std::sync::RwLock::new(None)),
                telemetry_handle: Arc::new(std::sync::RwLock::new(None)),
            }),
        }
    }

    /// Like [`with_parent_tools`](Self::with_parent_tools), but for a daemon
    /// session that shares one `SessionPool`-level agent-type catalog +
    /// watcher instead of building (and watching) its own.
    ///
    /// `with_parent_tools` spawns a dedicated `AgentTypeWatcher` — its own
    /// `notify` background thread — per call, i.e. per session. In a daemon
    /// with many concurrent sessions all watching the exact same three
    /// `agents/` directories, that's N redundant threads for one set of
    /// directories. Here, `shared_agent_types` is expected to already be
    /// live-reloaded by a watcher the caller owns (see
    /// `SessionPool::agent_types` / `SessionPool::_agent_type_watcher`) — no
    /// watcher is started by this constructor, and `Inner::_agent_type_watcher`
    /// is `None`. `description` is computed from a one-time read of
    /// `shared_agent_types`'s *current* contents at construction time — same
    /// documented "functional reload without cosmetic reload" boundary as
    /// `with_parent_tools` already has, just now shared across sessions
    /// rather than reset per session.
    pub fn with_shared_agent_types(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        fallback_tools: Arc<InMemoryToolRegistry>,
        shared_agent_types: SharedAgentTypeMap,
    ) -> Self {
        let description: Arc<str> = {
            let snapshot = shared_agent_types.read().unwrap();
            describe_agent_types(&snapshot).into()
        };
        Self {
            inner: Arc::new(Inner {
                model,
                config,
                fallback_tools,
                parent_tools,
                depth: 0,
                mailbox: None,
                agent_types: shared_agent_types,
                _agent_type_watcher: None,
                description,
                parent_settings: None,
                task_router: None,
                delegate_scripts: None,
                skill_manager: Arc::new(std::sync::RwLock::new(None)),
                mcp_tool_adapters: Arc::new(std::sync::RwLock::new(Vec::new())),
                team_registry: Arc::new(std::sync::RwLock::new(None)),
                team_members: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                idle_seq_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                team_spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
                event_tx: Arc::new(std::sync::RwLock::new(None)),
                hooks: Arc::new(std::sync::RwLock::new(None)),
                history_store: Arc::new(std::sync::RwLock::new(None)),
                parent_permission: Arc::new(std::sync::RwLock::new(None)),
                parent_pending_permissions: Arc::new(std::sync::RwLock::new(None)),
                parent_scene: Arc::new(std::sync::RwLock::new(None)),
                scene_registry: Arc::new(std::sync::RwLock::new(None)),
                instruction_file: Arc::new(std::sync::RwLock::new(None)),
                parent_session_id: Arc::new(std::sync::RwLock::new(None)),
                telemetry_handle: Arc::new(std::sync::RwLock::new(None)),
            }),
        }
    }

    /// Attach the parent `Agent`'s own settings so sub-agents inherit its
    /// real scope/paths instead of a hardcoded stand-in. See the
    /// `Inner::parent_settings` doc comment.
    pub fn with_settings(mut self, settings: Arc<Settings>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.parent_settings = Some(settings);
        self.inner = Arc::new(inner);
        self
    }

    /// Record how deep the owning agent already sits in the delegation
    /// chain, so `spawn_guard` can refuse to go past
    /// `EngineConfig::max_agent_depth`. Set by `Builder::build()` from
    /// `Builder::agent_depth`; `0` (a root session) otherwise.
    pub fn with_depth(mut self, depth: u32) -> Self {
        let mut inner = (*self.inner).clone();
        inner.depth = depth;
        self.inner = Arc::new(inner);
        self
    }

    /// The scripts every agent this tool spawns inherits.
    pub fn with_delegate_scripts(
        mut self,
        scripts: Arc<base::interface::script_adapters::BoundScripts>,
    ) -> Self {
        let mut inner = (*self.inner).clone();
        inner.delegate_scripts = Some(scripts);
        self.inner = Arc::new(inner);
        self
    }

    /// See `Inner::task_router` / `Inner::model_for_subagent`.
    pub fn with_task_router(mut self, router: Arc<base::provider::TaskRouter>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.task_router = Some(router);
        self.inner = Arc::new(inner);
        self
    }

    /// Attach the session's `SkillManager` so `AgentTypeDefinition.skills`
    /// (preload) can resolve. Callable through `&self` (interior mutability
    /// via `Inner::skill_manager`'s `RwLock`) rather than the `.with_X(self)
    /// -> Self` builder pattern the other two use, because `Builder::build()`
    /// only has a `SkillManager` to hand *after* it already needs the
    /// finished `Arc<AgentTool>` for hook construction — see the call site
    /// in `agent.rs`.
    pub fn set_skill_manager(&self, mgr: Arc<skills::manager::SkillManager>) {
        *self.inner.skill_manager.write().unwrap() = Some(mgr);
    }

    /// Attach a snapshot of the parent session's connected MCP tools — same
    /// interior-mutability, set-after-construction pattern as
    /// `set_skill_manager`. Resolves `AgentTypeDefinition.mcp_servers` when
    /// a subagent is spawned. Pass `mcp_manager.tool_adapters().to_vec()`;
    /// see `Inner::mcp_tool_adapters`'s doc comment for why this takes a
    /// snapshot `Vec` rather than the live `McpManager`.
    pub fn set_mcp_tool_adapters(&self, adapters: Vec<Arc<dyn base::tool::Tool>>) {
        *self.inner.mcp_tool_adapters.write().unwrap() = adapters;
    }

    /// Attach the same shared `TeamRegistry` `Builder::build()` hands to
    /// `TeamCreate`/`TeamList`/`TeamDelete`, so persistent team members
    /// spawned through this `AgentTool` (see `spawn_or_message_team_member`)
    /// update state those tools can see. Same interior-mutability,
    /// set-after-construction pattern as `set_skill_manager`/
    /// `set_mcp_tool_adapters` — only called when `settings.execution.
    /// team_enabled` is true (see the call site in `agent.rs`).
    pub fn set_team_registry(&self, registry: Arc<team::registry::TeamRegistry>) {
        *self.inner.team_registry.write().unwrap() = Some(registry);
    }

    /// Attach the parent session's event channel so sub-agent activity is
    /// mirrored onto it (S-1) — see `Inner::event_tx`. Set-after-construction
    /// (`&self`, not the `with_X(self) -> Self` builder form) because
    /// `Builder::build()` creates this `AgentTool` before it creates the
    /// session's event channel.
    pub fn set_event_sender(&self, tx: crate::agent::EventSender) {
        *self.inner.event_tx.write().unwrap() = Some(tx);
    }

    /// Attach the parent session's hook runner so sub-agent spawns fire
    /// `SubagentStart`/`SubagentStop` — see `Inner::hooks`.
    pub fn set_hooks(&self, runner: Arc<hooks::HookRunner>) {
        *self.inner.hooks.write().unwrap() = Some(runner);
    }

    /// Fire one sub-agent lifecycle hook. Notification only — the response is
    /// discarded, so a hook cannot veto a spawn or a completion. No-op when no
    /// runner was wired, or when nothing subscribes to the event.
    async fn fire_subagent_hook(
        &self,
        event: hooks::HookEvent,
        agent_session_id: &str,
        agent_type: &Option<String>,
        stop_reason: Option<&str>,
    ) {
        let Some(runner) = self.inner.hooks.read().unwrap().clone() else {
            return;
        };
        if !runner.has_hooks_for(event) {
            return;
        }
        let mut input = hooks::HookInput::lifecycle(
            format!("{event:?}"),
            agent_session_id.to_string(),
            String::new(),
            String::new(),
        )
        .with_agent_type(agent_type.clone());
        if let Some(r) = stop_reason {
            input = input.with_stop_reason(r);
        }
        let _ = runner.run(event, &input).await;
    }

    /// Attach the parent session's history store so sub-agent transcripts are
    /// persisted (S-4) — see `Inner::history_store`.
    pub fn set_history_store(&self, store: Arc<dyn HistoryStore>) {
        *self.inner.history_store.write().unwrap() = Some(store);
    }

    /// Record the parent `Agent`'s own session id, so sub-agents spawned
    /// from here can stamp `parent_session_id` on their own `Meta` line —
    /// see `Inner::parent_session_id`.
    pub fn set_parent_session_id(&self, id: String) {
        *self.inner.parent_session_id.write().unwrap() = Some(id);
    }

    /// Record the parent `Agent`'s own telemetry handle, so sub-agents
    /// spawned from here inherit it instead of getting `Builder::build()`'s
    /// noop default — see `Inner::telemetry_handle`.
    pub fn set_telemetry_handle(&self, handle: TelemetryHandle) {
        *self.inner.telemetry_handle.write().unwrap() = Some(handle);
    }

    /// Route sub-agent permission checks to the parent agent's `Permission`
    /// impl (S-3) — see `Inner::parent_permission`.
    pub fn set_parent_permission(&self, permission: Arc<dyn Permission>) {
        *self.inner.parent_permission.write().unwrap() = Some(permission);
    }

    /// Route sub-agent permission *prompts* to the parent session's host
    /// (N-8) — see `Inner::parent_pending_permissions`. Without this a
    /// sub-agent still inherits the parent's rules, but anything those rules
    /// leave undecided is refused rather than asked about.
    pub fn set_parent_pending_permissions(
        &self,
        pending: Arc<
            std::sync::Mutex<
                std::collections::HashMap<
                    String,
                    tokio::sync::oneshot::Sender<crate::agent::PermissionDecision>,
                >,
            >,
        >,
    ) {
        *self.inner.parent_pending_permissions.write().unwrap() = Some(pending);
    }

    /// Let sub-agents inherit the parent's scene (S-5) — see
    /// `Inner::parent_scene`.
    pub fn set_scene(&self, scene: Arc<dyn AgentScene>) {
        *self.inner.parent_scene.write().unwrap() = Some(scene);
    }

    /// Let an agent type's `scene:` id resolve against every scene the host
    /// registered, not just the built-in four — see `Inner::scene_registry`.
    pub fn set_scene_registry(&self, registry: Arc<scene::scene::SceneRegistry>) {
        *self.inner.scene_registry.write().unwrap() = Some(registry);
    }

    /// Let sub-agents inherit an instruction file that the parent got from
    /// `Builder::instruction_file(..)` rather than from settings.json (S-2)
    /// — see `settings_from_parent`.
    pub fn set_instruction_file(&self, path: Option<std::path::PathBuf>) {
        *self.inner.instruction_file.write().unwrap() = path;
    }

    /// Run this `AgentTool`'s sub-agents as team members whose permission
    /// prompts bubble up to the coordinator's mailbox
    /// (`team::coordinator::PermissionBridge`).
    ///
    /// Deliberately **not** wired at the composition root today: the bridge
    /// blocks the sub-agent until someone calls
    /// `PermissionBridge::receive_decision`, and the only thing that would
    /// (`team::polling::MailboxPoller`) has no production caller either — so
    /// enabling it would turn every non-safe worker tool call into a 120s
    /// stall followed by a timeout denial. Sub-agent permission checks route
    /// to the parent's real `Permission` impl instead (see
    /// `permission_handler`); this stays available for an embedder that runs
    /// its own poller.
    pub fn with_mailbox(
        mut self,
        store: std::sync::Arc<team::mailbox::MailboxStore>,
        label: impl Into<String>,
    ) -> Self {
        let mut inner = (*self.inner).clone();
        inner.mailbox = Some((store, label.into()));
        self.inner = Arc::new(inner);
        self
    }

    /// Returns the fallback tool registry for sub-agent creation.
    pub(crate) fn sub_tools(&self) -> Arc<InMemoryToolRegistry> {
        self.inner.fallback_tools.clone()
    }

    /// Returns a permission handler for sub-agent creation.
    pub(crate) fn sub_permission(&self) -> Arc<dyn Permission> {
        self.permission_handler()
    }

    /// System prompt injected into the sub-agent's context for the given
    /// `subagent_type`, looked up from the merged built-in + disk catalog.
    fn type_prompt(&self, t: Option<&str>) -> Option<String> {
        t.and_then(|name| self.inner.agent_type_def(name))
            .map(|d| d.system_prompt)
    }

    fn build_prompt(&self, input: &AgentInput) -> String {
        if let Some(p) = self.type_prompt(input.subagent_type.as_deref()) {
            format!("{p}\n\nTask: {}", input.prompt)
        } else {
            input.prompt.clone()
        }
    }

    /// Does `tool_name` match an `allowed_tools`/`disallowed_tools` entry?
    /// Exact match, or an MCP server-level pattern: `mcp__<server>` or
    /// `mcp__<server>__*` matches every tool from that server (AttaCore's
    /// MCP tools are already named `mcp__<server>__<tool>`, so this is a
    /// plain prefix check, not a general glob).
    fn tool_name_matches(tool_name: &str, pattern: &str) -> bool {
        if tool_name == pattern {
            return true;
        }
        if let Some(server) = pattern
            .strip_prefix("mcp__")
            .map(|s| s.trim_end_matches("__*"))
        {
            return tool_name
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split("__").next())
                == Some(server);
        }
        false
    }

    /// Resolve the tool set for a given subagent type.
    ///
    /// Returns a filtered [`InMemoryToolRegistry`] containing only the
    /// built-in tools the named agent type is allowed to use, plus (if
    /// `AgentTypeDefinition.mcp_servers` names any) the matching MCP tools
    /// from the parent's already-connected `McpManager`. Unknown types, and
    /// types whose `allowed_tools` is empty (the "full access" convention —
    /// see `general-purpose`/`claude`), fall back to the full built-in tool
    /// set **minus "Agent" itself** — see `AGENT_TOOL_NAME` doc comment for
    /// why. MCP is never part of that "full access" default: a subagent
    /// gets zero MCP tools unless `mcp_servers` explicitly grants them,
    /// regardless of how the built-in `allowed_tools` resolves — MCP access
    /// is opt-in per subagent, not inherited.
    fn resolve_tools(&self, subagent_type: Option<&str>) -> Arc<InMemoryToolRegistry> {
        let def = subagent_type.and_then(|name| self.inner.agent_type_def(name));
        let disallowed: Vec<String> = def
            .as_ref()
            .map(|d| d.disallowed_tools.clone())
            .unwrap_or_default();
        let allowed_names: Option<Vec<String>> = def
            .as_ref()
            .map(|d| d.allowed_tools.clone())
            .filter(|tools| !tools.is_empty());

        let registry = InMemoryToolRegistry::new();
        match &allowed_names {
            None => {
                // Full access minus `disallowed_tools` — every tool except
                // "Agent" (no recursive spawning) and anything explicitly denied.
                for tool in self
                    .inner
                    .parent_tools
                    .all()
                    .iter()
                    .chain(self.inner.fallback_tools.all().iter())
                {
                    if tool.name() != AGENT_TOOL_NAME
                        && !disallowed
                            .iter()
                            .any(|n| Self::tool_name_matches(tool.name(), n))
                    {
                        registry.register(tool.clone());
                    }
                }
            }
            Some(allowed) => {
                // Collect from both parent and fallback to cover all available tools.
                // `disallowed_tools` is applied first, then `allowed_tools` is
                // resolved against what's left; a tool listed in both ends up
                // removed either way.
                for tool in self
                    .inner
                    .parent_tools
                    .all()
                    .iter()
                    .chain(self.inner.fallback_tools.all().iter())
                {
                    if tool.name() != AGENT_TOOL_NAME
                        && !disallowed
                            .iter()
                            .any(|n| Self::tool_name_matches(tool.name(), n))
                        && allowed
                            .iter()
                            .any(|n| Self::tool_name_matches(tool.name(), n))
                    {
                        registry.register(tool.clone());
                    }
                }
            }
        }

        // MCP: additive, opt-in-only grant from `mcp_servers`, reusing the
        // parent's live connections rather than reconnecting. Applies on
        // top of either branch above — a restricted `allowed_tools` list
        // that doesn't mention any `mcp__...` name still lets a granted MCP
        // server's tools through, since `mcp_servers` is a separate axis
        // from the built-in tool allow/deny lists.
        if let Some(d) = &def {
            if !d.mcp_servers.is_empty() {
                let adapters = self.inner.mcp_tool_adapters();
                if adapters.is_empty() {
                    tracing::warn!(
                        agent_type = %d.name,
                        "agent mcp_servers: set but no MCP tools are attached to this AgentTool — no MCP tools granted"
                    );
                }
                for tool in &adapters {
                    if d.mcp_servers.iter().any(|server| {
                        Self::tool_name_matches(tool.name(), &format!("mcp__{server}"))
                    }) {
                        registry.register(tool.clone());
                    }
                }
            }
        }

        Arc::new(registry)
    }

    fn sub_settings(&self, model_name: Option<&str>, cwd: std::path::PathBuf) -> Settings {
        Inner::sub_settings(&self.inner, model_name, cwd)
    }

    /// Create a permission handler appropriate for this sub-agent's context.
    ///
    /// Resolution order:
    /// 1. mailbox configured (running as a team member) → [`PermissionBridge`],
    ///    which forwards the prompt to the coordinator and waits for a decision;
    /// 2. the parent agent's own `Permission` impl, when wired via
    ///    [`AgentTool::set_parent_permission`] — so a session using
    ///    `RuleSetPermission` (or any other real handler) can't have its rules
    ///    bypassed by spawning a sub-agent (S-3);
    /// 3. [`AlwaysPermit`], the historical behavior, for `AgentTool`s built
    ///    without any wiring (tests, embedders).
    pub(crate) fn permission_handler(&self) -> Arc<dyn Permission> {
        self.inner.permission_handler()
    }

    /// Core: run sub-agent and collect text output.
    ///
    /// `subagent_type` — when it names a type in the merged catalog (built-in
    /// or `.atta/agents/*.md`) that declares a `model` override, that model
    /// is used instead of the parent's. `None` (or an unknown/type-less
    /// call, e.g. from the generic `AgentSpawner` bridge used by team/skill-fork)
    /// keeps the parent's model, matching prior behavior.
    pub(crate) async fn run_sub(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
    ) -> Result<String, base::error::ToolError> {
        self.run_sub_tagged(prompt, tools, cwd, cancel, perm, subagent_type, None)
            .await
    }

    /// `run_sub` plus the parent's `(session_id, turn_no)` for event
    /// attribution. Split out rather than folded into `run_sub`'s signature
    /// so the cross-crate `AgentSpawner` bridge (which has no `ToolContext`,
    /// and lives behind a trait in `base`) keeps working unchanged — it just
    /// forwards events with an empty parent id, which still carries the
    /// stable per-run `agent_label`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_sub_tagged(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
        parent: Option<(&str, u32)>,
    ) -> Result<String, base::error::ToolError> {
        let child_depth = self
            .inner
            .spawn_guard()
            .map_err(base::error::ToolError::Denied)?;
        let cwd_for_meta = cwd.clone();
        let def = subagent_type.and_then(|t| self.inner.agent_type_def(t));
        let model_override = def.as_ref().and_then(|d| d.model.clone());
        let mut sub_settings = self.sub_settings(model_override.as_deref(), cwd);
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings, d);
        }
        // S-5: inherit the parent's scene instead of always CodingScene, with
        // the agent type's own `scene:` winning over the inherited one.
        let scene = self.inner.scene_for_subagent(def.as_ref(), &sub_settings);
        let scene_id = scene.id().to_string();
        let settings = Arc::new(sub_settings);

        let prompt = match (&def, self.inner.skill_manager()) {
            (Some(d), Some(mgr)) if !d.skills.is_empty() => {
                format!("{}{prompt}", preload_skills_text(&mgr, &d.skills))
            }
            _ => prompt,
        };

        let sid = base::id::Id::new().to_string();
        let tag = SubagentTag::new(&self.inner, &sid, subagent_type, parent);
        let sid_for_hook = sid.clone();
        let sid_for_meta = sid.clone();
        // Resolved once and shared by the sub-agent's own lineage and its `Meta`
        // line — two answers to "who spawned this" that must not be able to
        // disagree.
        let parent_session_id = parent
            .map(|(sid, _)| sid.to_string())
            .or_else(|| self.inner.parent_session_id());
        let mut builder = Builder::new()
            .session_id(sid)
            .lineage(parent_session_id.clone(), subagent_type)
            .scene(scene)
            .model(self.inner.model_for_subagent())
            .tools(tools)
            .settings(settings.clone())
            .agent_depth(child_depth)
            .permission(perm);
        // S-4: persist the sub-agent's transcript. Its own fresh session id
        // keeps it in a separate JSONL file from the parent's.
        let history_store = self.inner.history_store();
        builder = self.inner.inherited_by_a_delegate(builder);
        let (mut agent, mut event_rx, input_tx) = builder
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;
        if let Some(store) = &history_store {
            write_sidechain_meta(
                store,
                &sid_for_meta,
                parent_session_id,
                &scene_id,
                &cwd_for_meta,
                &settings,
            )
            .await;
        }

        // SubagentStart / SubagentStop bracket the sub-agent's whole run.
        // `Stop` fires on both success and failure — a hook that cleans up
        // after a sub-agent must not be skipped just because it errored — with
        // the outcome carried in `stop_reason`.
        let sub_type = subagent_type.map(|s| s.to_string());
        self.fire_subagent_hook(
            hooks::HookEvent::SubagentStart,
            &sid_for_hook,
            &sub_type,
            None,
        )
        .await;
        let (result, cancelled) =
            run_one_turn(&mut agent, &mut event_rx, &input_tx, prompt, cancel, tag).await;
        drop(input_tx);
        self.fire_subagent_hook(
            hooks::HookEvent::SubagentStop,
            &sid_for_hook,
            &sub_type,
            Some(if result.is_ok() { "end_turn" } else { "error" }),
        )
        .await;
        // §5.6: the one-shot task has now run to conclusion either way —
        // mark it terminal so a stale `session.resume` against it is
        // rejected instead of silently continuing a "finished" sub-agent.
        mark_sidechain_terminal(history_store.as_ref(), &sid_for_hook, &result, cancelled).await;
        result
    }

    /// Background counterpart to `run_sub`, for `AgentSpawner::spawn_agent_background`
    /// (the `context: fork` + `background: true` skill path) — same shape as
    /// `launch_bg` (task registration + `tokio::spawn(run_sub_inner(..))` +
    /// status/output bookkeeping on completion), minus the `worktree`
    /// handling `launch_bg` does from `AgentInput.worktree`: skill fork
    /// doesn't have an equivalent frontmatter field, so there's nothing to
    /// branch on here. Returns the task id immediately; the caller (the
    /// `Skill` tool) hands that id back to the model to poll via
    /// `TaskOutput`/`TaskStop` — the same mechanism `Agent`'s own
    /// `background: true` argument already uses.
    pub(crate) async fn spawn_background(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
        session: Arc<base::context::SessionState>,
    ) -> String {
        let tid = bg_task_id();
        let task = session.register_running_task(tid.clone());
        let subagent_type = subagent_type.map(str::to_string);
        let inner = self.inner.clone();
        let tc = task.clone();
        let tid_c = tid.clone();
        let session_c = session.clone();

        tokio::spawn(async move {
            let r =
                Self::run_sub_inner(&inner, prompt, tools, cwd, cancel, subagent_type.as_deref())
                    .await;
            let mut s = tc.status.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*s, base::context::RunningStatus::Running) {
                *s = match &r {
                    Ok(_) => base::context::RunningStatus::Completed,
                    Err(e) => base::context::RunningStatus::Failed(e.to_string()),
                };
            }
            drop(s);
            if let Ok(ref text) = r {
                tc.output
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(text);
            }
            session_c.persist_running_task(&tc);
            session_c.remove_running_task_persistence(&tid_c);
        });
        tid
    }

    async fn launch_bg(
        &self,
        input: &AgentInput,
        ctx: &ToolContext,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let tid = bg_task_id();
        let task = ctx.session.register_running_task(tid.clone());
        let mut worktree_handle = match &input.worktree {
            Some(s) => match create_worktree(&ctx.exec, &ctx.session.cwd, s).await {
                Ok(h) => Some(h),
                Err(e) => {
                    *task.status.lock().unwrap_or_else(|e| e.into_inner()) =
                        base::context::RunningStatus::Failed(format!("worktree: {e}"));
                    return Ok(bg_result(&tid, "worktree failed"));
                }
            },
            None => None,
        };
        let cwd = worktree_handle
            .as_ref()
            .map(|h| h.path().to_path_buf())
            .unwrap_or_else(|| ctx.session.cwd.clone());
        let tools = self.resolve_tools(input.subagent_type.as_deref());
        let prompt = self.build_prompt(input);
        let subagent_type = input.subagent_type.clone();
        let inner = self.inner.clone();
        let tc = task.clone();
        let tid_c = tid.clone();
        let outer_cancel = ctx.cancel.child_token();
        let session = ctx.session.clone();
        let _events_tx = ctx.events_tx.clone();

        tokio::spawn(async move {
            let r = Self::run_sub_inner(
                &inner,
                prompt,
                tools,
                cwd,
                outer_cancel,
                subagent_type.as_deref(),
            )
            .await;
            if let Some(h) = worktree_handle.as_mut() {
                h.cleanup().await;
            }
            let mut s = tc.status.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*s, base::context::RunningStatus::Running) {
                *s = match &r {
                    Ok(_) => base::context::RunningStatus::Completed,
                    Err(e) => base::context::RunningStatus::Failed(e.to_string()),
                };
            }
            if let Ok(ref text) = r {
                tc.output
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(text);
            }
            session.persist_running_task(&tc);
            session.remove_running_task_persistence(&tid_c);
        });
        Ok(bg_result(&tid, "spawned"))
    }

    /// Build the `Agent`/channels for a team member, without running any
    /// turn yet — the persistent-member counterpart to `run_sub_inner`'s
    /// preamble, split out because a persistent member's caller needs to
    /// keep the built `Agent` alive across many turns rather than run one
    /// and discard it. Deliberately skips the `skills:` preload text
    /// injection `run_sub_inner` does for one-shot sub-agents — that only
    /// makes sense prepended to a *first* message, and persistent members
    /// don't have a clean single "first message" hook here (the very first
    /// message goes through the same code path as every later one). The
    /// `agent_type`'s `model`/`permission_mode`/`effort`/`max_turns`
    /// overrides still apply.
    /// `permission_mode` is the already-*resolved* grant this member runs
    /// under for its entire lifetime (resolution — explicit override vs.
    /// the team's established grant vs. the safe default — happens in
    /// `spawn_or_message_team_member`, once, not here) — built into a real
    /// `permissions::RuleSetPermission`, not the `AlwaysPermit`/
    /// `PermissionBridge` `self.permission_handler()` would otherwise
    /// return (`PermissionBridge` specifically doesn't work for a
    /// persistent member: it forwards to the lead via mailbox and blocks up
    /// to 120s for a reply, but the lead only runs code during its own
    /// `run_turn()` calls — it has no code path listening for that message
    /// while idle between user turns, so every non-whitelisted tool call
    /// would just time out and get denied).
    async fn build_team_member_agent(
        &self,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        subagent_type: Option<&str>,
        permission_mode: base::interface::settings::PermissionMode,
    ) -> Result<(Agent, EventReceiver, InputSender), base::error::ToolError> {
        let child_depth = self
            .inner
            .spawn_guard()
            .map_err(base::error::ToolError::Denied)?;
        let cwd_for_meta = cwd.clone();
        let def = subagent_type.and_then(|t| self.inner.agent_type_def(t));
        let model_override = def.as_ref().and_then(|d| d.model.clone());
        let mut sub_settings = self.sub_settings(model_override.as_deref(), cwd);
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings, d);
        }
        // S-5: inherit the parent's scene, same as `run_sub_tagged`/`run_sub_inner`.
        let scene = self.inner.scene_for_subagent(def.as_ref(), &sub_settings);
        let scene_id = scene.id().to_string();
        let settings = Arc::new(sub_settings);
        let perm: Arc<dyn Permission> =
            Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
                Arc::new(permissions::gate::PermissionGate::new(
                    permissions::ruleset::RuleSet::new(vec![]),
                )),
                tools.clone(),
                permission_mode.into(),
            ));
        let sid = base::id::Id::new().to_string();
        let sid_for_meta = sid.clone();
        let mut builder = Builder::new()
            .session_id(sid)
            .scene(scene)
            .model(self.inner.model_for_subagent())
            .tools(tools)
            .settings(settings.clone())
            .agent_depth(child_depth)
            .permission(perm);
        // The operator's policy scripts follow the delegate — see
        // `BoundScripts::for_delegate`. Without this, spawning is a way
        // around every rule they wrote, chosen by the model.
        let history_store = self.inner.history_store();
        builder = self.inner.inherited_by_a_delegate(builder);
        let built = builder
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;
        if let Some(store) = history_store {
            write_sidechain_meta(
                &store,
                &sid_for_meta,
                self.inner.parent_session_id(),
                &scene_id,
                &cwd_for_meta,
                &settings,
            )
            .await;
        }
        Ok(built)
    }

    /// Persistent-member counterpart to `run_sub`/`launch_bg`: routes
    /// `prompt` to an existing, still-alive `(team_name, member_name)`
    /// member if one exists, or builds a fresh one under that key. Either
    /// way returns a task id — the reply to *this* message is retrieved via
    /// `TaskOutput`/`TaskStop` the same way any other background task's is,
    /// because unlike `run_sub`'s one-shot callers, the member itself
    /// outlives this one message and can't just hand its text straight back
    /// from this call.
    /// `permission_mode`: `Some` sets (or, for the team's first member,
    /// establishes) this team's permission grant — see
    /// `AgentInput::permission_mode`'s doc comment for "one team, one
    /// authorization". Only consulted when actually spawning a *new*
    /// member; a message to an already-alive member reuses whatever grant
    /// it was originally built with, it can't be changed mid-lifetime.
    ///
    /// Holds `Inner::team_spawn_lock` for its entire body — see that
    /// field's doc comment for why the find-or-create decision needs to be
    /// atomic across concurrent calls.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn_or_message_team_member(
        &self,
        team_name: String,
        member_name: String,
        prompt: String,
        subagent_type: Option<String>,
        cwd: std::path::PathBuf,
        session: Arc<base::context::SessionState>,
        permission_mode: Option<base::interface::settings::PermissionMode>,
    ) -> Result<String, base::error::ToolError> {
        let _spawn_guard = self.inner.team_spawn_lock.lock().await;
        let key = (team_name.clone(), member_name.clone());

        // Never evict the exact member this call is about to look for —
        // otherwise a message to an existing, currently-idle member could
        // have that member reclaimed a moment before the lookup below finds
        // it, silently spawning a fresh replacement (losing its
        // conversation) instead of reaching the one the caller meant.
        self.reclaim_idle_team_members(Some(&key)).await;

        let tid = bg_task_id();
        let task = session.register_running_task(tid.clone());

        let existing_tx = self
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|m| m.message_tx.clone());

        if let Some(tx) = existing_tx {
            if tx
                .send(PersistentMessage {
                    prompt: prompt.clone(),
                    task: task.clone(),
                })
                .is_ok()
            {
                return Ok(tid);
            }
            // The loop already exited (crashed, or stopped concurrently by
            // TeamDelete/reclaim right after the read above) — its map
            // entry is stale. Drop it and fall through to spawn fresh under
            // the same key, rather than surfacing a confusing send failure.
            self.inner
                .team_members
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
        }

        // Resolution order: explicit override on this call > this team's
        // already-established grant (set by whichever earlier call spawned
        // its first member) > the safe zero-config default. Persisted back
        // via `set_team_permission_mode_if_absent` so later spawns under
        // the same `team_name` (even without repeating `permission_mode`)
        // inherit whatever was decided here — "one team, one authorization".
        let registry = self
            .inner
            .team_registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let resolved_permission_mode = permission_mode
            .or_else(|| {
                registry
                    .as_ref()
                    .and_then(|r| r.team_permission_mode(&team_name))
            })
            .unwrap_or(base::interface::settings::PermissionMode::Plan);
        if let Some(r) = &registry {
            r.set_team_permission_mode_if_absent(&team_name, resolved_permission_mode);
        }

        let tools = self.resolve_tools(subagent_type.as_deref());
        let (agent, event_rx, input_tx) = self
            .build_team_member_agent(
                tools,
                cwd,
                subagent_type.as_deref(),
                resolved_permission_mode,
            )
            .await?;

        let member_id = format!("member-{}", bg_task_id());
        tracing::debug!(
            team = %team_name,
            member = %member_name,
            id = %member_id,
            "spawning new persistent team member"
        );
        let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel::<PersistentMessage>();
        let member_cancel = tokio_util::sync::CancellationToken::new();
        let idle_since = Arc::new(std::sync::atomic::AtomicI64::new(i64::MAX));
        let idle_seq = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));

        self.inner
            .team_members
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key,
                PersistentMember {
                    id: member_id,
                    message_tx: message_tx.clone(),
                    cancel: member_cancel.clone(),
                    idle_since: idle_since.clone(),
                    idle_seq: idle_seq.clone(),
                },
            );

        let _ = message_tx.send(PersistentMessage { prompt, task });

        tokio::spawn(run_persistent_member_loop(
            agent,
            event_rx,
            input_tx,
            message_rx,
            member_cancel,
            registry,
            team_name,
            member_name,
            idle_since,
            idle_seq,
            self.inner.idle_seq_counter.clone(),
            session,
        ));

        Ok(tid)
    }

    /// Stop a persistent team member and drop its handle — cancels its loop
    /// (interrupting it whether idle or mid-turn) and removes it from
    /// `team_members` so a later `spawn_or_message_team_member` call under
    /// the same key builds a fresh one instead of trying to reach the
    /// stopped one. No-op if the key isn't present.
    pub(crate) async fn stop_team_member(&self, team_name: &str, member_name: &str) {
        let key = (team_name.to_string(), member_name.to_string());
        let member = self
            .inner
            .team_members
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        if let Some(m) = member {
            tracing::debug!(
                team = %team_name,
                member = %member_name,
                id = %m.id,
                "stopping persistent team member"
            );
            m.cancel.cancel();
        }
    }

    /// Reclaim policy: (a) any single member idle longer than
    /// `team_member_idle_timeout_secs`, and (b) total pool size over
    /// `team_max_persistent_members` (oldest-idle evicted first, just
    /// enough to get back under the cap) — either condition alone triggers
    /// a sweep; they're independent, not combined. Never touches a
    /// currently-`Active` member (its `idle_since` sentinel makes it
    /// ineligible for either rule).
    ///
    /// Called opportunistically before spawning a new member, not on a
    /// separate ticking timer — a session that never spawns another member
    /// after the pool fills up won't self-trim until it does. An accepted
    /// simplification.
    ///
    /// `protect`, when given, excludes that exact `(team_name, member_name)`
    /// key from eviction candidates entirely — used by
    /// `spawn_or_message_team_member` so a sweep run just before it looks up
    /// an existing member can never evict the very member the caller is
    /// about to message (it still counts toward the total for rule (a),
    /// it's just never itself a victim).
    async fn reclaim_idle_team_members(&self, protect: Option<&(String, String)>) {
        let max_total = self.inner.config.team_max_persistent_members.max(1);
        let idle_timeout = self.inner.config.team_member_idle_timeout_secs;
        let now = now_secs();

        let victims: Vec<(String, String)> = {
            let members = self
                .inner
                .team_members
                .read()
                .unwrap_or_else(|e| e.into_inner());
            // `since` (wall clock, 1s resolution) drives the timeout rule
            // (b); `seq` (a strictly-increasing counter, never ties) drives
            // the "oldest idle first" ordering for the total-count rule
            // (a) — see `PersistentMember::idle_seq`'s doc comment for why
            // the timestamp alone isn't reliable enough to sort by.
            let mut idle_entries: Vec<((String, String), i64, u64)> = members
                .iter()
                .filter(|(k, _)| protect != Some(*k))
                .filter_map(|(k, m)| {
                    let since = m.idle_since.load(std::sync::atomic::Ordering::SeqCst);
                    let seq = m.idle_seq.load(std::sync::atomic::Ordering::SeqCst);
                    (since != i64::MAX).then_some((k.clone(), since, seq))
                })
                .collect();
            idle_entries.sort_by_key(|(_, _, seq)| *seq);

            let mut victims: Vec<(String, String)> = idle_entries
                .iter()
                .filter(|(_, since, _)| now.saturating_sub(*since) as u64 >= idle_timeout)
                .map(|(k, _, _)| k.clone())
                .collect();

            let mut remaining = members.len();
            for (k, _, _) in &idle_entries {
                if remaining <= max_total {
                    break;
                }
                remaining -= 1;
                if !victims.contains(k) {
                    victims.push(k.clone());
                }
            }
            victims
        };

        for (team_name, member_name) in victims {
            self.stop_team_member(&team_name, &member_name).await;
        }
    }

    /// Static helper for background execution. `subagent_type` — see
    /// `run_sub`'s doc comment; resolved the same way against `inner.agent_types`.
    async fn run_sub_inner(
        inner: &Inner,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
    ) -> Result<String, base::error::ToolError> {
        let child_depth = inner
            .spawn_guard()
            .map_err(base::error::ToolError::Denied)?;
        // S-3: same resolution as the foreground path — a background spawn
        // used to hardcode `AlwaysPermit`, escaping the parent's rules.
        let perm: Arc<dyn Permission> = inner.permission_handler();
        let cwd_for_meta = cwd.clone();
        let def = subagent_type.and_then(|t| inner.agent_type_def(t));
        let model_name = def.as_ref().and_then(|d| d.model.clone());
        let mut sub_settings = Inner::sub_settings(inner, model_name.as_deref(), cwd);
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings, d);
        }
        let scene = inner.scene_for_subagent(def.as_ref(), &sub_settings);
        let scene_id = scene.id().to_string();
        let settings = Arc::new(sub_settings);

        let prompt = match (&def, inner.skill_manager()) {
            (Some(d), Some(mgr)) if !d.skills.is_empty() => {
                format!("{}{prompt}", preload_skills_text(&mgr, &d.skills))
            }
            _ => prompt,
        };

        let sid = base::id::Id::new().to_string();
        let sid_for_meta = sid.clone();
        let tag = SubagentTag::new(inner, &sid, subagent_type, None);
        let mut builder = Builder::new()
            .session_id(sid)
            .scene(scene)
            .model(inner.model_for_subagent())
            .tools(tools)
            .settings(settings.clone())
            .agent_depth(child_depth)
            .permission(perm);
        let history_store = inner.history_store();
        builder = inner.inherited_by_a_delegate(builder);
        let (mut agent, mut event_rx, input_tx) = builder
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;
        if let Some(store) = &history_store {
            write_sidechain_meta(
                store,
                &sid_for_meta,
                inner.parent_session_id(),
                &scene_id,
                &cwd_for_meta,
                &settings,
            )
            .await;
        }

        let (result, cancelled) =
            run_one_turn(&mut agent, &mut event_rx, &input_tx, prompt, cancel, tag).await;
        drop(input_tx);
        // §5.6: see the matching comment in `run_sub_tagged`.
        mark_sidechain_terminal(history_store.as_ref(), &sid_for_meta, &result, cancelled).await;
        result
    }

    // ── Feature #27: Resume agent — continue a previous session ──

    /// Resume a sub-agent from a previous session's transcript.
    ///
    /// Loads the transcript entries from `history_store`, projects them into
    /// model messages, creates a new agent pre-populated with those messages,
    /// and runs the given `prompt` as the resumed task.
    ///
    /// Emits a `resume_action` telemetry event via structured tracing.
    pub async fn resume_agent(
        &self,
        session_id: &str,
        history_store: Arc<dyn HistoryStore>,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, base::error::ToolError> {
        let child_depth = self
            .inner
            .spawn_guard()
            .map_err(base::error::ToolError::Denied)?;
        let start = std::time::Instant::now();

        // 1. Load transcript from history store
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|e| base::error::ToolError::Execution(anyhow!("invalid session id: {e}")))?;
        let entries = history_store
            .load(sid)
            .await
            .map_err(|e| base::error::ToolError::Execution(anyhow!("load transcript: {e}")))?;
        if entries.is_empty() {
            return Err(base::error::ToolError::Execution(anyhow!(
                "no entries found for session {session_id}"
            )));
        }
        let projection = history_store.projection();
        let projected = history::transcript::project_messages_with(&entries, projection);
        let report = history::transcript::resume_projection_report_with(&entries, projection);

        // 2. Convert projected history messages to ModelMessages
        let mut model_messages: Vec<ModelMessage> = Vec::with_capacity(projected.len());
        for msg in &projected {
            match msg {
                base::message::Message::User { content } => {
                    model_messages.push(ModelMessage {
                        role: MessageRole::User,
                        content: convert_content_blocks(content),
                    });
                }
                base::message::Message::Assistant { content, .. } => {
                    model_messages.push(ModelMessage {
                        role: MessageRole::Assistant,
                        content: convert_content_blocks(content),
                    });
                }
                base::message::Message::System { .. } => {
                    // UI-only notifications, skip for API
                }
            }
        }

        // 3. Inject resume context as a system-reminder user message
        let resume_context = format!(
            "<system-reminder>\n\
             This is a **resumed** session (previous session: `{session_id}`).\n\
             The following transcript has been loaded into context. \
             Continue from where you left off.\n\n\
             New task: {prompt}\n\
             </system-reminder>"
        );
        model_messages.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: resume_context,
            }],
        });

        // 4. Build and run sub-agent with pre-loaded messages
        let cwd_for_meta = cwd.clone();
        let sub_settings = self.sub_settings(None, cwd);
        let scene = self.inner.scene_for_subagent(None, &sub_settings);
        let scene_id = scene.id().to_string();
        let settings = Arc::new(sub_settings);
        let perm: Arc<dyn Permission> = self.permission_handler();

        // The resumed-from session's own recorded parent (if any) — this
        // new session continues the same lineage under a fresh id, so it
        // should still be findable via `HistoryStore::child_sessions` off
        // the *original* parent, not orphaned.
        let original_parent = entries.iter().find_map(|env| match &env.entry {
            history::entry::LogEntry::Meta {
                parent_session_id, ..
            } => parent_session_id.clone(),
            _ => None,
        });

        let new_sid = base::id::Id::new().to_string();
        let new_sid_for_meta = new_sid.clone();
        let tag = SubagentTag::new(&self.inner, &new_sid, None, None);
        let mut builder = Builder::new()
            .session_id(new_sid.clone())
            .scene(scene)
            .model(self.inner.model_for_subagent())
            .tools(tools)
            .settings(settings.clone())
            .agent_depth(child_depth)
            .permission(perm)
            .history_store(history_store.clone());
        // The operator's policy scripts follow the delegate — see
        // `BoundScripts::for_delegate`. Without this, spawning is a way
        // around every rule they wrote, chosen by the model.
        builder = self.inner.inherited_by_a_delegate(builder);
        let (mut agent, mut event_rx, input_tx) = builder
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;
        write_sidechain_meta(
            &history_store,
            &new_sid_for_meta,
            original_parent.or_else(|| self.inner.parent_session_id()),
            &scene_id,
            &cwd_for_meta,
            &settings,
        )
        .await;

        // Pre-load historical messages into the new agent's session
        agent.session.messages = model_messages;
        agent.session.turn_count = projected.len() as u32;

        // 5. Run the agent. Routed through `run_one_turn` — same as
        // `run_sub_tagged`/`run_sub_inner` — rather than the previous
        // spawn-a-collector-and-await-it-unconditionally approach: that
        // approach waited on `TurnComplete`, which a cancelled turn (see
        // `run_one_turn`'s doc comment) never emits, so a cancelled resume
        // hung forever instead of returning.
        let (result, cancelled) =
            run_one_turn(&mut agent, &mut event_rx, &input_tx, prompt, cancel, tag).await;
        drop(input_tx);

        // 6. Emit resume telemetry via structured tracing
        let latency_ms = start.elapsed().as_millis() as u64;
        let warning_str: Option<String> = report.warning.map(|w| format!("{:?}", w));
        tracing::info!(
            target: "telemetry",
            event_type = "resume_action",
            session_id = %session_id,
            new_session_id = %new_sid,
            source = "jsonl",
            entry_count = report.entry_count,
            projected_message_count = report.projected_message_count,
            compact_boundary_count = report.compact_boundary_count,
            sidechain_entry_count = report.sidechain_entry_count,
            warning = %warning_str.unwrap_or_default(),
            latency_ms,
            "resume agent completed"
        );

        // §5.6: a resumed sidechain is a one-shot task exactly like
        // `run_sub_tagged`/`run_sub_inner`'s — mark it terminal so a stale
        // resume against *this* new session is rejected in turn, unless it
        // never ran to conclusion (see `mark_sidechain_terminal`).
        mark_sidechain_terminal(Some(&history_store), &new_sid_for_meta, &result, cancelled).await;
        result
    }
}

/// Routes a sub-agent's permission checks to the parent session's own
/// `Permission` impl (S-3), with one adaptation: a `Prompt` outcome is
/// turned into a `Deny`.
///
/// A sub-agent has no channel to ask a human anything — `AgentEvent::
/// PermissionPrompt` from a sub-agent goes out on the *sub-agent's* event
/// channel, and the answer would have to come back as an
/// `InputMessage::PermissionResponse` on the sub-agent's input channel,
/// which no host has a handle on. Passing `Prompt` through unchanged would
/// therefore stall the sub-agent until its cancellation token fired. Denying
/// with an actionable reason keeps the parent's rules authoritative (the
/// point of this wrapper) while failing fast and legibly: the model sees the
/// denial as a tool error and can report it, and the user can pre-approve
/// the rule at the parent session level and retry.
/// A sub-agent's `Permission`: the parent's rules, plus the parent's channel
/// to a human.
///
/// Delegating the *decision* to the parent (S-3) is what stops a session from
/// having its rules bypassed by spawning a sub-agent. But an `Ask` verdict
/// then has nowhere to go — a sub-agent has no host attached to it. This used
/// to be resolved by refusing: every `Prompt` became a `Deny` with an
/// explanation. Under the old `bypassPermissions` default that was nearly
/// invisible; once the default became *ask*, it meant a sub-agent could not
/// run a build, a test suite, or a commit, because none of those match a rule
/// by default. `Agent`/`Team` were the features it broke, and running work in
/// parallel is most of what they exist for.
///
/// The parent does have a host. So forward the question to it: register the
/// prompt in the parent's own pending-permission registry and emit it on the
/// parent's event channel, which is the same pair `turn.rs` uses for the
/// parent's own prompts — so `session.respondToPrompt` answers a sub-agent's
/// question with no new RPC and no new plumbing. The parent's input
/// demultiplexer intercepts `PermissionResponse` ahead of turn dispatch, so
/// this resolves even though the parent turn is blocked inside the `Agent`
/// tool call that spawned us.
///
/// With no channel wired (`event_tx`/`pending` are `None` — tests, embedders
/// that never call the setters), it falls back to the previous
/// refuse-with-an-explanation behavior rather than blocking forever.
struct ParentPermission {
    inner: Arc<dyn Permission>,
    event_tx: Option<crate::agent::EventSender>,
    pending: Option<crate::agent::PendingPermissions>,
}

#[async_trait]
impl Permission for ParentPermission {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        use base::interface::permission::PermissionOutcome;
        let outcome = self
            .inner
            .check(tool_name, tool_input, cwd, session_id)
            .await;
        let PermissionOutcome::Prompt {
            prompt_id,
            message,
            paths,
        } = outcome
        else {
            return outcome;
        };

        let (Some(event_tx), Some(pending)) = (self.event_tx.as_ref(), self.pending.as_ref())
        else {
            return PermissionOutcome::Deny {
                reason: format!(
                    "`{tool_name}` needs interactive approval, and this sub-agent has no channel \
                     to ask on. Approve it in the parent session (or run this step there) and \
                     retry. Original prompt: {message}"
                ),
            };
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(prompt_id.clone(), tx);
        // Prefixed so the host can tell a sub-agent's question apart from the
        // parent's own — the prompt itself is otherwise identical, which is
        // deliberate: it is answered through the identical path.
        if event_tx
            .send(base::interface::event::AgentEvent::PermissionPrompt {
                prompt_id: prompt_id.clone(),
                tool_name: tool_name.to_string(),
                message: format!("[sub-agent {session_id}] {message}"),
                paths,
                turn_id: String::new(),
                tool_use_id: None,
            })
            .is_err()
        {
            // Parent channel gone (session tearing down): fail closed.
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&prompt_id);
            return PermissionOutcome::Deny {
                reason: format!("`{tool_name}`: the parent session closed before it could answer"),
            };
        }

        match rx.await {
            Ok(crate::agent::PermissionDecision::Permit) => PermissionOutcome::Permit,
            Ok(crate::agent::PermissionDecision::PermitAlways { .. }) => {
                // Persist on the *parent's* handler, which is where the rule
                // engine actually lives — `self.inner` is the parent's
                // `Permission`, so a sub-agent's "always" answer applies to
                // the rest of the session exactly like the parent's would.
                self.inner.add_persistent_allow(tool_name, None);
                PermissionOutcome::Permit
            }
            Ok(crate::agent::PermissionDecision::Deny { reason }) => {
                PermissionOutcome::Deny { reason }
            }
            // Sender dropped without an answer — fail closed, same as the
            // parent's own path does.
            Err(_) => PermissionOutcome::Deny {
                reason: format!("`{tool_name}`: permission request closed without an answer"),
            },
        }
    }
}

struct AlwaysPermit;
#[async_trait]
impl Permission for AlwaysPermit {
    async fn check(
        &self,
        _tn: &str,
        _i: &Value,
        _c: &std::path::Path,
        _s: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

/// Convert from history/content-block format into the model-runtime format.
/// Skips image, thinking, and redacted-thinking blocks (not supported by model API).
fn convert_content_blocks(blocks: &[base::message::ContentBlock]) -> Vec<ModelContentBlock> {
    blocks
        .iter()
        .filter_map(|block| match block {
            base::message::ContentBlock::Text { text, .. } => {
                Some(ModelContentBlock::Text { text: text.clone() })
            }
            base::message::ContentBlock::ToolUse { id, name, input } => {
                Some(ModelContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                })
            }
            base::message::ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let content_str = match content {
                    base::message::ToolResultContent::Text(s) => s.clone(),
                    base::message::ToolResultContent::Blocks(blocks) => {
                        serde_json::to_string(blocks).unwrap_or_default()
                    }
                };
                Some(ModelContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content_str,
                    is_error: Some(*is_error),
                })
            }
            // Skip Image, Thinking, RedactedThinking — not supported by model API
            _ => None,
        })
        .collect()
}

fn bg_result(task_id: &str, status: &str) -> base::tool::ToolResult {
    base::tool::ToolResult {
        content: ToolResultContent::Text(format!(
            "background task spawned (task_id: {task_id}, status: {status})"
        )),
        is_error: false,
        structured_content: None,
        mcp_meta: None,
        new_messages: None,
    }
}

// ═══════════════════════════════════════════════════════
// Core Tool impl
// ═══════════════════════════════════════════════════════

#[async_trait]
impl base::tool::Tool for AgentTool {
    fn name(&self) -> &str {
        AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        &self.inner.description
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AgentInput)).unwrap_or(Value::Null)
    }
    async fn prompt(&self, _: &base::tool::PromptContext) -> String {
        include_str!("agent_tool.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let inp: AgentInput = serde_json::from_value(input)
            .map_err(|e| base::error::ToolError::Validation(format!("{e}")))?;

        // Persistent team member — must give both team_name and name, or
        // neither; one without the other is almost certainly a mistake
        // (e.g. the model meant to join a team but dropped a field), not a
        // meaningful partial state to silently fall through on. Gated on
        // `team_registry` being set (`set_team_registry`, only called when
        // `settings.execution.team_enabled` is true — see `agent.rs`):
        // `AgentInput`'s schema is generated once, unconditionally, so the
        // model can technically *set* `team_name`/`name` even when team
        // coordination is off for this session; without this check it would
        // silently spawn a persistent member nobody gated in via settings,
        // exactly the exposure `team_enabled` exists to prevent for
        // `TeamCreate`/`TeamList`/`TeamDelete` (see `ExecutionSettings::
        // team_enabled`'s doc comment) — this closes the same gap for the
        // `Agent` tool's own team-joining fields.
        match (&inp.team_name, &inp.name) {
            (Some(team_name), Some(name)) => {
                if self
                    .inner
                    .team_registry
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
                {
                    return Err(base::error::ToolError::Validation(
                        "team_name/name require team coordination to be enabled for this \
                         session (settings.execution.team_enabled)"
                            .into(),
                    ));
                }
                if matches!(
                    inp.permission_mode,
                    Some(base::interface::settings::PermissionMode::Auto)
                        | Some(base::interface::settings::PermissionMode::Bubble)
                ) {
                    return Err(base::error::ToolError::Validation(
                        "permission_mode: \"auto\" and \"bubble\" are not valid here — \"auto\" \
                         needs a transcript classifier this call site doesn't have, and \
                         \"bubble\" needs an available lead, which a persistent member's lead \
                         (only runs code during its own turns) doesn't reliably have either"
                            .into(),
                    ));
                }
                let tid = self
                    .spawn_or_message_team_member(
                        team_name.clone(),
                        name.clone(),
                        inp.prompt.clone(),
                        inp.subagent_type.clone(),
                        ctx.cwd.clone(),
                        ctx.session.clone(),
                        inp.permission_mode,
                    )
                    .await?;
                return Ok(bg_result(&tid, "queued for team member"));
            }
            (None, None) => {}
            _ => {
                return Err(base::error::ToolError::Validation(
                    "team_name and name must both be given together, or neither".into(),
                ));
            }
        }

        // Background
        if inp.background {
            return self.launch_bg(&inp, &ctx).await;
        }

        // Sync
        let mut worktree_handle = match &inp.worktree {
            Some(s) => match create_worktree(&ctx.exec, &ctx.session.cwd, s).await {
                Ok(h) => Some(h),
                Err(e) => return Err(base::error::ToolError::Execution(anyhow!("worktree: {e}"))),
            },
            None => None,
        };
        let cwd = worktree_handle
            .as_ref()
            .map(|h| h.path().to_path_buf())
            .unwrap_or_else(|| ctx.session.cwd.clone());
        let tools = self.resolve_tools(inp.subagent_type.as_deref());
        let prompt = self.build_prompt(&inp);
        let perm = self.permission_handler();

        // Bound rather than matched inline so the worktree is cleaned up
        // before the result is turned into a ToolResult — the sub-agent may
        // have failed, and the temporary worktree must go either way.
        let result = self
            .run_sub_tagged(
                prompt,
                tools,
                cwd,
                ctx.cancel.child_token(),
                perm,
                inp.subagent_type.as_deref(),
                Some((ctx.session_id.as_str(), ctx.turn_no)),
            )
            .await;

        if let Some(h) = worktree_handle.as_mut() {
            h.cleanup().await;
        }

        match result {
            Ok(text) => Ok(base::tool::ToolResult {
                content: ToolResultContent::Text(text),
                is_error: false,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            }),
            Err(e) => Ok(base::tool::ToolResult {
                content: ToolResultContent::Text(format!("sub-agent error: {e}")),
                is_error: true,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            }),
        }
    }
}

// Only base::tool::Tool impl — legacy bridge removed.

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use std::io::Write;

    /// A directory of agent-type `.md` files that outlives the assertions
    /// reading them.
    ///
    /// An `AgentTool` built over a directory keeps a `notify` watcher on it,
    /// and a `.md` file disappearing re-runs the merge with that agent type
    /// gone. Deleting the directory partway through a test therefore races the
    /// watcher against itself: the unlink of the file is dispatched, removing
    /// the directory tears the watch down, and whichever lands first decides
    /// whether the catalog under assertion still holds anything. A `TempDir`
    /// is cleaned up when the test ends, once nothing is going to read it
    /// again.
    fn agent_type_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir for agent-type files")
    }

    fn write_agent_md(dir: &Path, filename: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let mut f = std::fs::File::create(dir.join(filename)).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn merge_agent_types_with_no_dirs_returns_only_builtins() {
        let merged = merge_agent_types(&[], &[]);
        assert_eq!(merged.len(), builtin_agent_types().len());
        assert!(merged.contains_key("explore"));
        assert!(merged.contains_key("code-reviewer"));
        assert!(merged.contains_key("worker"));
    }

    #[test]
    fn load_agent_types_from_dir_parses_frontmatter() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "security-auditor.md",
            "---\nname: security-auditor\ndescription: Finds security issues\nallowed_tools: [Read, Grep, Glob]\nmodel: claude-opus-4-8\n---\nYou are a security auditor.",
        );
        let types = load_agent_types_from_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(types.len(), 1);
        let t = &types[0];
        assert_eq!(t.name, "security-auditor");
        assert_eq!(t.description, "Finds security issues");
        assert_eq!(t.allowed_tools, vec!["Read", "Grep", "Glob"]);
        assert_eq!(t.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(t.system_prompt, "You are a security auditor.");
    }

    #[test]
    fn load_agent_types_from_dir_parses_new_fields() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-newfields-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "db-reader.md",
            "---\nname: db-reader\ndescription: Read-only DB queries\ndisallowed_tools: AskUserQuestion\npermission_mode: plan\neffort: high\nmax_turns: 10\nskills: api-conventions, error-handling\nmcp_servers: github, sentry\n---\nBody.",
        );
        let types = load_agent_types_from_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(types.len(), 1);
        let t = &types[0];
        assert_eq!(t.disallowed_tools, vec!["AskUserQuestion"]);
        assert_eq!(
            t.permission_mode,
            Some(base::interface::settings::PermissionMode::Plan)
        );
        assert_eq!(t.effort.as_deref(), Some("high"));
        assert_eq!(t.max_turns, Some(10));
        assert_eq!(t.skills, vec!["api-conventions", "error-handling"]);
        assert_eq!(t.mcp_servers, vec!["github", "sentry"]);
    }

    #[test]
    fn load_agent_types_from_dir_falls_back_to_first_paragraph_description() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-para-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "helper.md",
            "---\nname: helper\n---\nThis agent helps\nwith multi-line stuff.\n\nMore body.",
        );
        let types = load_agent_types_from_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(types.len(), 1);
        assert_eq!(
            types[0].description,
            "This agent helps with multi-line stuff."
        );
    }

    #[test]
    fn preload_skills_text_includes_full_body_and_skips_missing_and_disabled() {
        let dir = std::env::temp_dir().join(format!(
            "atta-skills-preload-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("api-conventions")).unwrap();
        std::fs::write(
            dir.join("api-conventions").join("SKILL.md"),
            "---\ndescription: API conventions\n---\nUse RESTful naming.",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("manual-only")).unwrap();
        std::fs::write(
            dir.join("manual-only").join("SKILL.md"),
            "---\ndescription: Manual only\ndisable_model_invocation: true\n---\nSecret steps.",
        )
        .unwrap();

        let mgr = skills::manager::SkillManager::new();
        mgr.load_dir_subdirs(&dir, skills::manager::SkillSource::Project)
            .unwrap();

        // `get_skill_content` reads a disk skill's body lazily at lookup
        // time (not cached at load time) — the fixture directory must still
        // exist when `preload_skills_text` runs, so cleanup happens after.
        let text = preload_skills_text(
            &mgr,
            &[
                "api-conventions".to_string(),
                "manual-only".to_string(),
                "does-not-exist".to_string(),
            ],
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(text.contains("api-conventions"));
        assert!(text.contains("Use RESTful naming."));
        // disable_model_invocation and unresolved names are skipped, not
        // included and not fatal to the rest of the preload.
        assert!(!text.contains("Secret steps."));
    }

    #[test]
    fn preload_skills_text_is_empty_when_nothing_resolves() {
        let mgr = skills::manager::SkillManager::new();
        let text = preload_skills_text(&mgr, &["nope".to_string()]);
        assert_eq!(text, "");
    }

    #[test]
    fn set_skill_manager_makes_it_available_via_inner() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("test-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[]);
        assert!(agent_tool.inner.skill_manager().is_none());
        agent_tool.set_skill_manager(Arc::new(skills::manager::SkillManager::new()));
        assert!(agent_tool.inner.skill_manager().is_some());
    }

    #[test]
    fn tool_name_matches_exact_and_mcp_server_pattern() {
        assert!(AgentTool::tool_name_matches("Read", "Read"));
        assert!(!AgentTool::tool_name_matches("Read", "Write"));
        assert!(AgentTool::tool_name_matches(
            "mcp__github__list_prs",
            "mcp__github"
        ));
        assert!(AgentTool::tool_name_matches(
            "mcp__github__list_prs",
            "mcp__github__*"
        ));
        assert!(!AgentTool::tool_name_matches(
            "mcp__gitlab__list_prs",
            "mcp__github"
        ));
    }

    #[test]
    fn merge_agent_types_disk_definition_overrides_builtin_by_name() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "explore.md",
            "---\ndescription: Custom explore override\n---\nCustom body.",
        );
        let merged = merge_agent_types(&[&dir], &[]);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(merged["explore"].description, "Custom explore override");
        assert_eq!(merged["explore"].system_prompt, "Custom body.");
        // Untouched built-ins survive.
        assert!(merged.contains_key("code-reviewer"));
    }

    #[test]
    fn merge_agent_types_precedence_is_low_to_high_across_dirs() {
        let base = std::env::temp_dir().join(format!(
            "atta-agent-types-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let global_dir = base.join("global");
        let scene_dir = base.join("scene");
        let project_dir = base.join("project");
        write_agent_md(
            &global_dir,
            "reviewer.md",
            "---\ndescription: from global\n---\nbody",
        );
        write_agent_md(
            &scene_dir,
            "reviewer.md",
            "---\ndescription: from scene\n---\nbody",
        );
        write_agent_md(
            &project_dir,
            "reviewer.md",
            "---\ndescription: from project\n---\nbody",
        );

        let merged = merge_agent_types(&[&global_dir, &scene_dir, &project_dir], &[]);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(merged["reviewer"].description, "from project");
    }

    #[test]
    fn merge_agent_types_plugin_definition_overrides_builtin_by_name() {
        let plugin_types = vec![AgentTypeDefinition {
            name: "explore".into(),
            description: "Plugin-provided explore override".into(),
            allowed_tools: vec!["Read".into()],
            model: None,
            system_prompt: "Plugin explore body.".into(),
            ..Default::default()
        }];
        let merged = merge_agent_types(&[], &plugin_types);
        assert_eq!(
            merged["explore"].description,
            "Plugin-provided explore override"
        );
        assert_eq!(merged["explore"].system_prompt, "Plugin explore body.");
        // Untouched built-ins survive.
        assert!(merged.contains_key("code-reviewer"));
    }

    #[test]
    fn merge_agent_types_dirs_override_plugin_definition_of_same_name() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-types-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "explore.md",
            "---\ndescription: from disk dir\n---\nDisk body.",
        );
        let plugin_types = vec![AgentTypeDefinition {
            name: "explore".into(),
            description: "from plugin".into(),
            allowed_tools: vec![],
            model: None,
            system_prompt: "Plugin body.".into(),
            ..Default::default()
        }];
        // Confirm priority order: built-in < plugin < dirs.
        let merged_plugin_only = merge_agent_types(&[], &plugin_types);
        assert_eq!(merged_plugin_only["explore"].description, "from plugin");

        let merged = merge_agent_types(&[&dir], &plugin_types);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(merged["explore"].description, "from disk dir");
        assert_eq!(merged["explore"].system_prompt, "Disk body.");
    }

    #[test]
    fn describe_agent_types_lists_every_type_sorted_by_name() {
        let merged = merge_agent_types(&[], &[]);
        let text = describe_agent_types(&merged);
        assert!(text.contains("- code-reviewer: "));
        assert!(text.contains("- explore: "));
        // Sorted: "claude" sorts before "code-reviewer" before "explore".
        let claude_pos = text.find("- claude:").unwrap();
        let explore_pos = text.find("- explore:").unwrap();
        assert!(claude_pos < explore_pos);
    }

    struct DummyModel;
    #[async_trait]
    impl Model for DummyModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            // Most tests using `DummyModel` only exercise settings/catalog
            // derivation and never actually call `.stream()`. The
            // background-spawn test below is the exception — it needs
            // `run_sub_inner`'s `agent.run_turn(...)` to return promptly
            // (through the real error path, not a hang) so it can observe
            // the task transition to `Failed` without a full mock model.
            Err(base::interface::model::ModelError::Internal(
                "DummyModel does not implement streaming".into(),
            ))
        }
    }

    struct NamedTool(&'static str);
    #[async_trait]
    impl base::tool::Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
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
            _input: Value,
            _ctx: ToolContext,
            _progress: ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult {
                content: ToolResultContent::Text("ok".into()),
                is_error: false,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            })
        }
    }

    fn test_agent_tool() -> AgentTool {
        let tools = InMemoryToolRegistry::new();
        tools.register(Arc::new(NamedTool("Read")));
        tools.register(Arc::new(NamedTool("Bash")));
        let tools = Arc::new(tools);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("test-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[]);
        // Register self into the same registry it holds, matching how
        // `Builder::build()` wires it — needed so `resolve_tools()`'s
        // recursion guard has something to actually filter out.
        agent_tool
    }

    // ---- model_for_subagent / multi-provider routing ----

    #[test]
    fn model_for_subagent_uses_parent_model_when_no_router_configured() {
        let agent_tool = test_agent_tool();
        let picked = agent_tool.inner.model_for_subagent();
        assert!(Arc::ptr_eq(&picked, &agent_tool.inner.model));
    }

    #[test]
    fn model_for_subagent_routes_through_task_router_when_configured() {
        let routed_model: Arc<dyn Model> = Arc::new(DummyModel);
        let mut providers = std::collections::HashMap::new();
        providers.insert("other".to_string(), routed_model.clone());
        let mut resolved = std::collections::HashMap::new();
        resolved.insert(
            "subagent".to_string(),
            base::provider::ResolvedModel {
                provider_id: "other".into(),
                model: "x".into(),
            },
        );
        let default_model: Arc<dyn Model> = Arc::new(DummyModel);
        let router = Arc::new(base::provider::TaskRouter::new(
            providers,
            resolved,
            default_model,
        ));

        let agent_tool = test_agent_tool().with_task_router(router);
        let picked = agent_tool.inner.model_for_subagent();
        assert!(Arc::ptr_eq(&picked, &routed_model));
        assert!(!Arc::ptr_eq(&picked, &agent_tool.inner.model));
    }

    #[test]
    fn model_for_subagent_falls_back_to_default_when_no_subagent_override() {
        // No "subagent" entry in `resolved` — router's own `default`
        // (which may differ from `inner.model`) should win, proving the
        // router is actually consulted rather than short-circuited.
        let router_default: Arc<dyn Model> = Arc::new(DummyModel);
        let router = Arc::new(base::provider::TaskRouter::new(
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            router_default.clone(),
        ));

        let agent_tool = test_agent_tool().with_task_router(router);
        let picked = agent_tool.inner.model_for_subagent();
        assert!(Arc::ptr_eq(&picked, &router_default));
        assert!(!Arc::ptr_eq(&picked, &agent_tool.inner.model));
    }

    #[test]
    fn resolve_tools_full_access_excludes_agent_itself() {
        let agent_tool = test_agent_tool();
        // Register "Agent" into the same registry the tool was built with,
        // simulating `Builder::build()`'s self-registration.
        let self_arc = Arc::new(AgentTool::with_parent_tools(
            agent_tool.inner.model.clone(),
            agent_tool.inner.config.clone(),
            agent_tool.inner.parent_tools.clone(),
            agent_tool.inner.fallback_tools.clone(),
            &[],
            &[],
        ));
        agent_tool.inner.parent_tools.register(self_arc);

        // "general-purpose" is a full-access type (empty allowed_tools).
        let resolved = agent_tool.resolve_tools(Some("general-purpose"));
        let names: Vec<String> = resolved
            .all()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "Read"));
        assert!(names.iter().any(|n| n == "Bash"));
        assert!(
            !names.iter().any(|n| n == AGENT_TOOL_NAME),
            "sub-agent must not be able to spawn further sub-agents"
        );
    }

    #[test]
    fn resolve_tools_restricted_type_filters_to_allowed_list() {
        let agent_tool = test_agent_tool();
        let resolved = agent_tool.resolve_tools(Some("explore"));
        let names: Vec<String> = resolved
            .all()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "Read")); // explore allows Read
        assert!(!names.iter().any(|n| n == "Bash")); // explore doesn't allow Bash
    }

    #[test]
    fn resolve_tools_mcp_servers_grants_matching_server_tools_only() {
        let tmp = agent_type_dir();
        let dir = tmp.path();
        write_agent_md(
            dir,
            "github-agent.md",
            "---\ndescription: Uses only the github MCP server\nmcp_servers: github\n---\nbody",
        );
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[dir], &[]);

        // Two servers' worth of MCP tools attached to the parent — only
        // "github"'s should make it into the resolved registry.
        let adapters: Vec<Arc<dyn base::tool::Tool>> = vec![
            Arc::new(NamedTool("mcp__github__list_prs")),
            Arc::new(NamedTool("mcp__gitlab__list_mrs")),
        ];
        agent_tool.set_mcp_tool_adapters(adapters);

        let resolved = agent_tool.resolve_tools(Some("github-agent"));
        let names: Vec<String> = resolved
            .all()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "mcp__github__list_prs"));
        assert!(!names.iter().any(|n| n == "mcp__gitlab__list_mrs"));
    }

    #[test]
    fn resolve_tools_without_mcp_servers_field_grants_no_mcp_tools() {
        // Confirms MCP access is opt-in, not inherited by default — even
        // the "full access" (empty allowed_tools) convention doesn't pull
        // in MCP tools unless mcp_servers explicitly names a server.
        let agent_tool = test_agent_tool();
        agent_tool.set_mcp_tool_adapters(vec![Arc::new(NamedTool("mcp__github__list_prs"))]);

        let resolved = agent_tool.resolve_tools(Some("general-purpose"));
        let names: Vec<String> = resolved
            .all()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(!names.iter().any(|n| n.starts_with("mcp__")));
    }

    #[test]
    fn resolve_tools_disallowed_tools_removes_from_full_access_pool() {
        let tmp = agent_type_dir();
        let dir = tmp.path();
        write_agent_md(
            dir,
            "no-writes.md",
            "---\ndescription: Inherits every tool except file writes\ndisallowed_tools: Write, Edit\n---\nbody",
        );
        let tools = InMemoryToolRegistry::new();
        tools.register(Arc::new(NamedTool("Read")));
        tools.register(Arc::new(NamedTool("Write")));
        tools.register(Arc::new(NamedTool("Edit")));
        let tools = Arc::new(tools);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[dir], &[]);

        let resolved = agent_tool.resolve_tools(Some("no-writes"));
        let names: Vec<String> = resolved
            .all()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(!names.iter().any(|n| n == "Write"));
        assert!(!names.iter().any(|n| n == "Edit"));
        // Full-access convention (empty allowed_tools) still applies to
        // everything else — disallowed_tools only removes, doesn't restrict
        // to an allowlist.
        assert!(names.iter().any(|n| n == "Read"));
    }

    #[test]
    fn resolve_tools_disallowed_tools_applied_before_allowed_tools() {
        let tmp = agent_type_dir();
        let dir = tmp.path();
        // A tool listed in both allowed_tools and disallowed_tools must end
        // up removed either way.
        write_agent_md(
            dir,
            "conflicting.md",
            "---\ndescription: Lists Bash in both allow and deny\nallowed_tools: Read, Bash\ndisallowed_tools: Bash\n---\nbody",
        );
        let tools = InMemoryToolRegistry::new();
        tools.register(Arc::new(NamedTool("Read")));
        tools.register(Arc::new(NamedTool("Bash")));
        let tools = Arc::new(tools);
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[dir], &[]);

        let resolved = agent_tool.resolve_tools(Some("conflicting"));
        let names: Vec<String> = resolved
            .all()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "Read"));
        assert!(!names.iter().any(|n| n == "Bash"));
    }

    #[test]
    fn custom_agent_type_model_override_is_applied_to_sub_settings() {
        let tmp = agent_type_dir();
        let dir = tmp.path();
        write_agent_md(
            dir,
            "opus-reviewer.md",
            "---\ndescription: Reviewer pinned to a specific model\nmodel: claude-opus-4-8\n---\nbody",
        );
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[dir], &[]);

        // Same lookup `run_sub`/`run_sub_inner` perform: resolve the type's
        // `model` override, then thread it into `sub_settings()`.
        let model_override = agent_tool
            .inner
            .agent_type_def("opus-reviewer")
            .and_then(|d| d.model);
        assert_eq!(model_override.as_deref(), Some("claude-opus-4-8"));

        let settings = agent_tool.sub_settings(
            model_override.as_deref(),
            std::path::PathBuf::from("/tmp/cwd"),
        );
        assert_eq!(settings.model.model_name, "claude-opus-4-8");

        // A call with no override (or an unknown type) still falls back to
        // the parent's configured model — this must not regress.
        let settings_default = agent_tool.sub_settings(None, std::path::PathBuf::from("/tmp/cwd"));
        assert_eq!(settings_default.model.model_name, "parent-default-model");
    }

    #[test]
    fn effort_to_thinking_mode_maps_five_levels_onto_three() {
        assert_eq!(effort_to_thinking_mode("low"), Some(ThinkingMode::Off));
        assert_eq!(effort_to_thinking_mode("medium"), Some(ThinkingMode::Auto));
        assert_eq!(effort_to_thinking_mode("high"), Some(ThinkingMode::On));
        assert_eq!(effort_to_thinking_mode("xhigh"), Some(ThinkingMode::On));
        assert_eq!(effort_to_thinking_mode("max"), Some(ThinkingMode::On));
        assert_eq!(effort_to_thinking_mode("not-a-level"), None);
    }

    #[test]
    fn apply_agent_type_overrides_sets_permission_mode_effort_and_max_turns() {
        let def = AgentTypeDefinition {
            name: "db-reader".into(),
            description: "d".into(),
            permission_mode: Some(base::interface::settings::PermissionMode::Plan),
            effort: Some("high".into()),
            max_turns: Some(5),
            ..Default::default()
        };
        let mut settings = Settings::defaults_for("test-model");
        let original_max_calls = settings.execution.max_api_calls_per_turn;
        apply_agent_type_overrides(&mut settings, &def);
        assert_eq!(
            settings.permission_mode,
            base::interface::settings::PermissionMode::Plan
        );
        assert_eq!(settings.model.thinking_mode, ThinkingMode::On);
        assert_eq!(settings.execution.max_api_calls_per_turn, 5);
        assert_ne!(
            settings.execution.max_api_calls_per_turn,
            original_max_calls
        );
    }

    /// A plugin ships over the network. Letting its agent type name
    /// `bypassPermissions` would switch the permission gate off for
    /// everything that sub-agent does, and a huge `max_turns` would spend the
    /// user's tokens at a rate they never agreed to.
    #[test]
    fn plugin_agent_type_cannot_loosen_permissions_or_raise_the_call_cap() {
        use base::interface::settings::PermissionMode as M;
        let def = AgentTypeDefinition {
            name: "greedy".into(),
            source: AgentTypeSource::Plugin,
            description: "d".into(),
            permission_mode: Some(M::BypassPermissions),
            max_turns: Some(10_000),
            ..Default::default()
        };
        let mut settings = Settings::defaults_for("test-model");
        settings.permission_mode = M::Default;
        settings.execution.max_api_calls_per_turn = 25;

        apply_agent_type_overrides(&mut settings, &def);

        assert_eq!(
            settings.permission_mode,
            M::Default,
            "a plugin must not be able to switch the permission gate off"
        );
        assert_eq!(
            settings.execution.max_api_calls_per_turn, 25,
            "a plugin must not be able to raise the API-call cap"
        );
    }

    /// The clamp is one-directional: a plugin restraining its own sub-agent
    /// more than the session does is exactly what a well-behaved plugin
    /// should be able to declare.
    #[test]
    fn plugin_agent_type_may_still_tighten() {
        use base::interface::settings::PermissionMode as M;
        let def = AgentTypeDefinition {
            name: "careful".into(),
            source: AgentTypeSource::Plugin,
            description: "d".into(),
            permission_mode: Some(M::Plan),
            max_turns: Some(5),
            ..Default::default()
        };
        let mut settings = Settings::defaults_for("test-model");
        settings.permission_mode = M::AcceptEdits;
        settings.execution.max_api_calls_per_turn = 25;

        apply_agent_type_overrides(&mut settings, &def);

        assert_eq!(settings.permission_mode, M::Plan);
        assert_eq!(settings.execution.max_api_calls_per_turn, 5);
    }

    /// Overriding your own session is the point of writing the file, so a
    /// type loaded from a directory the user controls keeps the unclamped
    /// behavior.
    #[test]
    fn local_agent_type_keeps_unclamped_override_behavior() {
        use base::interface::settings::PermissionMode as M;
        let def = AgentTypeDefinition {
            name: "mine".into(),
            source: AgentTypeSource::LocalFile,
            description: "d".into(),
            permission_mode: Some(M::BypassPermissions),
            max_turns: Some(200),
            ..Default::default()
        };
        let mut settings = Settings::defaults_for("test-model");
        settings.permission_mode = M::Default;
        settings.execution.max_api_calls_per_turn = 25;

        apply_agent_type_overrides(&mut settings, &def);

        assert_eq!(settings.permission_mode, M::BypassPermissions);
        assert_eq!(settings.execution.max_api_calls_per_turn, 200);
    }

    #[test]
    fn permission_restraint_orders_plan_above_default_above_bypass() {
        use base::interface::settings::PermissionMode as M;
        assert!(permission_restraint(M::Plan) > permission_restraint(M::Default));
        assert!(permission_restraint(M::Default) > permission_restraint(M::AcceptEdits));
        assert!(permission_restraint(M::AcceptEdits) > permission_restraint(M::BypassPermissions));
        assert!(permission_restraint(M::DontAsk) > permission_restraint(M::Default));
    }

    #[test]
    fn apply_agent_type_overrides_leaves_settings_untouched_when_fields_absent() {
        let def = AgentTypeDefinition {
            name: "plain".into(),
            description: "d".into(),
            ..Default::default()
        };
        let mut settings = Settings::defaults_for("test-model");
        let before = (
            settings.permission_mode,
            settings.model.thinking_mode.clone(),
            settings.execution.max_api_calls_per_turn,
        );
        apply_agent_type_overrides(&mut settings, &def);
        assert_eq!(
            (
                settings.permission_mode,
                settings.model.thinking_mode,
                settings.execution.max_api_calls_per_turn
            ),
            before
        );
    }

    #[test]
    fn run_sub_applies_permission_mode_and_max_turns_override_end_to_end() {
        let tmp = agent_type_dir();
        let dir = tmp.path();
        write_agent_md(
            dir,
            "cautious.md",
            "---\ndescription: A cautious read-only researcher\npermission_mode: plan\nmax_turns: 3\n---\nbody",
        );
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[dir], &[]);

        let def = agent_tool.inner.agent_type_def("cautious").unwrap();
        let mut settings = agent_tool.sub_settings(None, std::path::PathBuf::from("/tmp/cwd"));
        apply_agent_type_overrides(&mut settings, &def);
        assert_eq!(
            settings.permission_mode,
            base::interface::settings::PermissionMode::Plan
        );
        assert_eq!(settings.execution.max_api_calls_per_turn, 3);
    }

    /// `AgentTool::spawn_background` (the `context: fork` + `background:
    /// true` skill path, via `RuntimeAgentSpawner::spawn_agent_background`)
    /// must return a task id immediately — without waiting for the spawned
    /// sub-agent turn to finish — and that id must already be registered on
    /// the *caller's* session (the real `session.register_running_task` /
    /// `find_running_task` machinery `TaskOutput`/`TaskStop` also use), in
    /// `Running` status. Driving a full turn to actual completion needs a
    /// working mock `Model` (`DummyModel` only covers settings-derivation
    /// tests elsewhere in this file) — out of scope here: this test is
    /// about the background-spawn plumbing (does it return without
    /// blocking, does it register under the id it hands back), not turn
    /// execution.
    #[tokio::test]
    async fn spawn_background_returns_immediately_with_a_registered_task() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools.clone(), &[], &[]);
        use base::interface::agent_spawner::AgentSpawner as _;
        let spawner = crate::agent_spawner_impl::RuntimeAgentSpawner::new(Arc::new(agent_tool));
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));

        let start = std::time::Instant::now();
        let task_id = spawner
            .spawn_agent_background(
                "do the thing".into(),
                vec![],
                std::path::PathBuf::from("/tmp"),
                tokio_util::sync::CancellationToken::new(),
                None,
                session.clone(),
            )
            .await
            .expect("spawn_agent_background itself should not error — it only registers the task");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "spawn_agent_background must return immediately, not block on the sub-agent turn"
        );

        let task = session
            .find_running_task(&task_id)
            .expect("spawn_background must register the task under the id it returns");
        assert!(matches!(
            *task.status.lock().unwrap_or_else(|e| e.into_inner()),
            base::context::RunningStatus::Running
        ));
    }

    /// Regression: `agent_types` used to be a build-time-only snapshot — a
    /// `.atta/agents/*.md` file edited after `with_parent_tools()` had no
    /// effect on a running `AgentTool` at all, not even a partial one; the
    /// only way to pick it up was rebuilding the whole `Agent`. This
    /// exercises the real `notify` watcher end to end (not just the merge
    /// logic in isolation), the same way
    /// `skills::manager::editing_a_watched_skill_file_is_picked_up_without_a_manual_reload`
    /// does for Skills.
    #[test]
    fn editing_an_agent_type_file_is_picked_up_without_rebuilding_agent_tool() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-watch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "watched-reviewer.md",
            "---\ndescription: original\nmodel: claude-sonnet-4-6\n---\noriginal body",
        );

        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[&dir], &[]);

        assert_eq!(
            agent_tool
                .inner
                .agent_type_def("watched-reviewer")
                .unwrap()
                .model
                .as_deref(),
            Some("claude-sonnet-4-6"),
            "sanity check on the initial build-time load"
        );

        // Edit the file after the `AgentTool` (and its watcher) already exist
        // — this is the part that used to have zero effect.
        write_agent_md(
            &dir,
            "watched-reviewer.md",
            "---\ndescription: updated\nmodel: claude-opus-4-8\n---\nupdated body",
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut picked_up = false;
        while std::time::Instant::now() < deadline {
            if agent_tool
                .inner
                .agent_type_def("watched-reviewer")
                .and_then(|d| d.model)
                .as_deref()
                == Some("claude-opus-4-8")
            {
                picked_up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            picked_up,
            "watcher never reported the agent-type file change within 5s"
        );
    }

    #[test]
    fn sub_settings_without_parent_settings_uses_the_passed_cwd() {
        // `test_agent_tool()` never calls `.with_settings(...)`, so this
        // exercises the `fallback_settings()` branch — `local_data_dir` must
        // reflect the `cwd` argument, not a hardcoded `"."`.
        let agent_tool = test_agent_tool();
        let cwd = std::path::PathBuf::from("/tmp/some-real-project");
        let settings = agent_tool.sub_settings(None, cwd.clone());
        assert_eq!(settings.paths.local_data_dir, cwd);
        // And the user-level roots stay inside it: with no parent settings
        // there is no instance root to inherit, and the invoking user's home
        // is not a substitute for one.
        assert!(settings.paths.global_data_dir.starts_with(&cwd));
        assert!(settings.paths.user_data_dir.starts_with(&cwd));
    }

    // NOTE: a true end-to-end regression test for the worktree-cleanup fix
    // (create a real git worktree via `AgentTool::call`, let the sub-agent
    // turn run, assert the directory is gone) was attempted here and
    // removed. It reliably hung: `run_sub`/`run_sub_inner` both spawn a
    // `t_handle` task that loops on `event_rx.recv()` until it sees
    // `AgentEvent::TurnComplete`, then `.await` that handle before
    // returning — but `TurnComplete` is only sent from the fully-successful
    // tail of `run_user_turn` (turn.rs); every early return (a plain model
    // error, `cancel.is_cancelled()`, the max-turns guard, etc.) skips it.
    // Against `DummyModel` (which always errors) or a pre-cancelled token,
    // `run_turn` returns via one of those early paths, `t_handle` never
    // sees a `TurnComplete` and never sees its sender drop (the `Agent`
    // holding it is kept alive by the very call that's waiting on
    // `t_handle`), and the wait never resolves — a real deadlock, not an
    // artifact of the worktree fix. It reproduces on unmodified `turn.rs`
    // too (confirmed by reverting this session's other uncommitted edits
    // there and rerunning), so it predates this change and is out of scope
    // for it; flagged separately rather than fixed here.
    //
    // The fix above was instead verified by: (1) reading both call sites —
    // the handle is now held in a `let mut` bound outside the spawned
    // task/match and `.cleanup().await` is called on it after `run_sub`/
    // `run_sub_inner` resolves, on both the success and error arms; (2) the
    // existing suite in this module (32 tests, including
    // `spawn_background_returns_immediately_with_a_registered_task`) still
    // passes unchanged; (3) `tools::worktree`'s own tests independently
    // cover that `create_worktree`/`WorktreeHandle::cleanup` themselves
    // work correctly — what was missing was purely that the caller here
    // never invoked `cleanup()` at all.
    //
    // UPDATE: the deadlock described above is now fixed (`run_one_turn`'s
    // doc comment) — `run_sub`/`run_sub_inner` no longer wait unconditionally
    // on `AgentEvent::TurnComplete`. The regression test right below proves
    // that fix directly; a real end-to-end worktree-cleanup test through
    // `AgentTool::call` would now be possible where it wasn't before, but
    // hasn't been added (this module's existing coverage — `resolve_tools`,
    // `sub_settings`, agent-type overrides — already exercises `run_sub`'s
    // surrounding plumbing without needing a real turn to complete; adding
    // one is a reasonable follow-up, not done here to keep this change
    // scoped to the deadlock fix itself).

    /// Regression: `run_sub` used to hang **indefinitely** (not just
    /// slowly) whenever the sub-agent's model call errored — `DummyModel`
    /// always errors, so this is the simplest reproduction. Wrapped in a
    /// generous but bounded timeout so a reintroduced regression fails
    /// this test in 10s instead of hanging the whole test binary the way
    /// the original bug did (confirmed by hand during debugging: the old
    /// code ran for 13+ minutes before being killed).
    #[tokio::test]
    async fn run_sub_returns_promptly_on_a_plain_model_error_instead_of_hanging() {
        let agent_tool = test_agent_tool();
        let tools = Arc::new(InMemoryToolRegistry::new());
        let perm: Arc<dyn Permission> = Arc::new(AlwaysPermit);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            agent_tool.run_sub(
                "do the thing".into(),
                tools,
                std::path::PathBuf::from("/tmp"),
                tokio_util::sync::CancellationToken::new(),
                perm,
                None,
            ),
        )
        .await
        .expect(
            "run_sub must return within 10s on a plain model error, not hang \
             indefinitely waiting for a TurnComplete event that never comes",
        );
        assert!(
            result.is_err(),
            "DummyModel always errors on .stream(); run_sub should surface that as Err"
        );
    }

    // ── Persistent team members (phase 3) ──

    fn team_key(team: &str, member: &str) -> (String, String) {
        (team.to_string(), member.to_string())
    }

    fn team_enabled_agent_tool() -> AgentTool {
        let agent_tool = test_agent_tool();
        agent_tool.set_team_registry(Arc::new(team::registry::TeamRegistry::new()));
        agent_tool
    }

    /// P5: `build_team_member_agent` used to hardcode `CodingScene`
    /// unconditionally, handing a Research- or Chat-scene team a
    /// programming-shop member regardless of what spawned it. Called
    /// directly (not through `spawn_or_message_team_member`, which only
    /// returns a task id) so the built `Agent`'s own scene is inspectable.
    #[tokio::test]
    async fn team_member_inherits_the_parents_scene_instead_of_always_coding() {
        let agent_tool = team_enabled_agent_tool();
        agent_tool.set_scene(Arc::new(scene::scene::research::ResearchScene));

        let (agent, _event_rx, _input_tx) = agent_tool
            .build_team_member_agent(
                Arc::new(InMemoryToolRegistry::new()),
                std::path::PathBuf::from("/tmp"),
                None,
                base::interface::settings::PermissionMode::Plan,
            )
            .await
            .expect("build should succeed");
        assert_eq!(agent.scene.id(), "research");
    }

    fn agent_tool_with_config(config: EngineConfig) -> AgentTool {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let agent_tool =
            AgentTool::with_parent_tools(model, Arc::new(config), tools.clone(), tools, &[], &[]);
        agent_tool.set_team_registry(Arc::new(team::registry::TeamRegistry::new()));
        agent_tool
    }

    /// Poll until the given key's member has gone idle (finished its most
    /// recent turn) — bounded so a regression here fails fast instead of
    /// hanging the test binary.
    async fn wait_until_idle(agent_tool: &AgentTool, key: &(String, String)) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let idle = {
                let members = agent_tool
                    .inner
                    .team_members
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                members
                    .get(key)
                    .map(|m| m.idle_since.load(std::sync::atomic::Ordering::SeqCst))
            };
            match idle {
                Some(v) if v != i64::MAX => return,
                None => panic!("member {key:?} disappeared from the pool while waiting"),
                _ => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "member {key:?} never went idle within 10s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn call_rejects_team_name_and_name_when_team_coordination_is_not_enabled() {
        let agent_tool = test_agent_tool(); // no set_team_registry — team coordination "off"
        let ctx = ToolContext::for_test(std::path::PathBuf::from("/tmp"));
        let input = serde_json::json!({ "prompt": "hi", "team_name": "t", "name": "worker" });
        let result =
            base::tool::Tool::call(&agent_tool, input, ctx, ProgressSender::noop("t")).await;
        assert!(
            result.is_err(),
            "team_name/name must be rejected when team coordination isn't enabled \
             for this session, not silently spawn a persistent member anyway"
        );
    }

    #[tokio::test]
    async fn call_rejects_team_name_given_without_name() {
        let agent_tool = team_enabled_agent_tool();
        let ctx = ToolContext::for_test(std::path::PathBuf::from("/tmp"));
        let input = serde_json::json!({ "prompt": "hi", "team_name": "t" });
        let result =
            base::tool::Tool::call(&agent_tool, input, ctx, ProgressSender::noop("t")).await;
        assert!(
            result.is_err(),
            "team_name without name must be rejected, not silently treated as a normal call"
        );
    }

    #[tokio::test]
    async fn spawn_or_message_team_member_reuses_the_same_member_across_calls() {
        let agent_tool = team_enabled_agent_tool();
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));
        let key = team_key("t1", "worker");

        let tid1 = agent_tool
            .spawn_or_message_team_member(
                "t1".into(),
                "worker".into(),
                "first message".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .expect("first spawn should succeed");
        wait_until_idle(&agent_tool, &key).await;
        let id1 = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .unwrap()
            .id
            .clone();
        assert_eq!(
            agent_tool
                .inner
                .team_members
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1
        );

        let tid2 = agent_tool
            .spawn_or_message_team_member(
                "t1".into(),
                "worker".into(),
                "second message".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .expect("second call to the same key should succeed");
        assert_ne!(tid1, tid2, "each message gets its own task id");
        wait_until_idle(&agent_tool, &key).await;

        let members = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(members.len(), 1, "still exactly one member under this key");
        assert_eq!(
            members.get(&key).unwrap().id,
            id1,
            "second call must reuse the same member instance, not spawn a new one"
        );
    }

    #[tokio::test]
    async fn stop_team_member_makes_the_next_call_spawn_a_fresh_one() {
        let agent_tool = team_enabled_agent_tool();
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));
        let key = team_key("t2", "worker");

        agent_tool
            .spawn_or_message_team_member(
                "t2".into(),
                "worker".into(),
                "msg1".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &key).await;
        let id1 = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .unwrap()
            .id
            .clone();

        agent_tool.stop_team_member("t2", "worker").await;
        assert!(agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .is_none());

        agent_tool
            .spawn_or_message_team_member(
                "t2".into(),
                "worker".into(),
                "msg2".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &key).await;
        let id2 = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .unwrap()
            .id
            .clone();
        assert_ne!(
            id1, id2,
            "after stop_team_member, the same key must spawn a genuinely new member"
        );
    }

    /// Reclaim rule (a): total pool over the cap evicts oldest-idle first,
    /// just enough to get back under it — not everything.
    #[tokio::test]
    async fn reclaim_evicts_oldest_idle_members_once_total_exceeds_the_cap() {
        let mut config = EngineConfig::defaults_for("test");
        config.team_max_persistent_members = 2;
        config.team_member_idle_timeout_secs = 1_000_000; // effectively disabled here
        let agent_tool = agent_tool_with_config(config);
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));

        for name in ["a", "b", "c", "d"] {
            agent_tool
                .spawn_or_message_team_member(
                    "team".into(),
                    name.into(),
                    "hi".into(),
                    None,
                    std::path::PathBuf::from("/tmp"),
                    session.clone(),
                    None,
                )
                .await
                .unwrap();
            wait_until_idle(&agent_tool, &team_key("team", name)).await;
        }

        let members = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            members.len(),
            3,
            "cap is 2 but a newly-spawned member always lands before the next sweep, \
             so 3 remain (2 kept + 1 just added): {:?}",
            members.keys().collect::<Vec<_>>()
        );
        assert!(
            !members.contains_key(&team_key("team", "a")),
            "oldest-idle member 'a' should have been reclaimed"
        );
        for name in ["b", "c", "d"] {
            assert!(
                members.contains_key(&team_key("team", name)),
                "{name} should remain"
            );
        }
    }

    /// Reclaim rule (b): idle-timeout is independent of the total-count
    /// rule — a single member past the timeout gets reclaimed even with a
    /// pool far under the cap.
    #[tokio::test]
    async fn reclaim_evicts_a_member_idle_past_the_timeout_regardless_of_total_count() {
        let mut config = EngineConfig::defaults_for("test");
        config.team_max_persistent_members = 100; // effectively disabled here
        config.team_member_idle_timeout_secs = 0; // any idle time at all reclaims
        let agent_tool = agent_tool_with_config(config);
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));

        agent_tool
            .spawn_or_message_team_member(
                "team".into(),
                "a".into(),
                "hi".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &team_key("team", "a")).await;

        // Spawning a second member triggers a reclaim sweep at its start.
        agent_tool
            .spawn_or_message_team_member(
                "team".into(),
                "b".into(),
                "hi".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();

        let members = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner());
        assert!(
            !members.contains_key(&team_key("team", "a")),
            "member idle past the timeout must be reclaimed regardless of total count"
        );
        assert!(members.contains_key(&team_key("team", "b")));
    }

    /// Regression test for a fix where a reclaim sweep run just before
    /// looking up an existing member could evict that exact member (if it
    /// happened to be the oldest-idle one and the pool was already over the
    /// cap), silently spawning a fresh replacement instead of reaching the
    /// one the caller meant to message. `reclaim_idle_team_members` now
    /// takes a `protect` key that's excluded from eviction candidates.
    #[tokio::test]
    async fn spawn_or_message_team_member_never_reclaims_the_member_it_is_about_to_message() {
        let mut config = EngineConfig::defaults_for("test");
        config.team_max_persistent_members = 1;
        config.team_member_idle_timeout_secs = 1_000_000; // only the count rule fires here
        let agent_tool = agent_tool_with_config(config);
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));
        let key_a = team_key("team", "a");
        let key_b = team_key("team", "b");

        // Spawns "a" (pool at cap, no eviction yet), then "b" (the sweep
        // before it runs while the pool is still at cap, so nothing is
        // evicted and the pool ends up one over the cap) — mirroring
        // `reclaim_evicts_oldest_idle_members_once_total_exceeds_the_cap`'s
        // documented "newly-spawned member always lands before the next
        // sweep" behavior.
        agent_tool
            .spawn_or_message_team_member(
                "team".into(),
                "a".into(),
                "hi".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &key_a).await;

        agent_tool
            .spawn_or_message_team_member(
                "team".into(),
                "b".into(),
                "hi".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &key_b).await;

        let id_a_before = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key_a)
            .expect("a should still be present before the next sweep")
            .id
            .clone();

        // Pool is now 2-over-a-cap-of-1, with "a" the oldest-idle (and thus
        // the default eviction candidate). Messaging "a" again must reach
        // the same member, not have it swept out from under this call.
        agent_tool
            .spawn_or_message_team_member(
                "team".into(),
                "a".into(),
                "second message".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .expect("messaging the existing, about-to-be-protected member should succeed");
        wait_until_idle(&agent_tool, &key_a).await;

        let members = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            members.get(&key_a).map(|m| m.id.clone()),
            Some(id_a_before),
            "the call's own target member must survive its own reclaim sweep"
        );
        assert!(
            !members.contains_key(&key_b),
            "the reclaim sweep still had to evict someone to get back under the cap — \
             it should have picked 'b' since 'a' was protected"
        );
    }

    /// Regression test for a TOCTOU race where two concurrent calls for the
    /// same never-before-seen `(team_name, member_name)` key could both
    /// observe "doesn't exist yet" and both spawn a member, with the second
    /// insert silently overwriting the first's map entry while its loop
    /// kept running orphaned. `team_spawn_lock` now serializes the whole
    /// find-or-create decision.
    #[tokio::test]
    async fn concurrent_first_calls_for_a_new_key_spawn_exactly_one_member() {
        let agent_tool = Arc::new(team_enabled_agent_tool());
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));
        let key = team_key("t4", "worker");

        let (r1, r2) = tokio::join!(
            agent_tool.spawn_or_message_team_member(
                "t4".into(),
                "worker".into(),
                "msg-a".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            ),
            agent_tool.spawn_or_message_team_member(
                "t4".into(),
                "worker".into(),
                "msg-b".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            ),
        );
        let tid1 = r1.expect("first concurrent call should succeed");
        let tid2 = r2.expect("second concurrent call should succeed");
        assert_ne!(
            tid1, tid2,
            "each call gets its own task id even when they race"
        );

        wait_until_idle(&agent_tool, &key).await;
        let members = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            members.len(),
            1,
            "two concurrent first-calls for the same key must not double-spawn: {:?}",
            members.keys().collect::<Vec<_>>()
        );
    }

    /// End-to-end across the `runtime`/`team` boundary: `TeamDelete`'s
    /// `cleanup_team` (via `AgentSpawner::stop_team_member`) must actually
    /// stop a still-alive persistent member, not just remove its registry
    /// bookkeeping — this was the whole point of giving `TeamDeleteTool` a
    /// spawner reference in `agent.rs`.
    #[tokio::test]
    async fn team_delete_cleanup_stops_a_persistent_member() {
        let agent_tool = Arc::new(team_enabled_agent_tool());
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));
        let key = team_key("t3", "worker");

        agent_tool
            .spawn_or_message_team_member(
                "t3".into(),
                "worker".into(),
                "hi".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &key).await;
        assert!(agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&key));

        let registry = agent_tool
            .inner
            .team_registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("team_enabled_agent_tool sets a registry");
        let spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> = Arc::new(
            crate::agent_spawner_impl::RuntimeAgentSpawner::new(agent_tool.clone()),
        );
        let coordinator = team::coordinator::DefaultCoordinator::with_agent_spawner(spawner)
            .with_registry(registry);

        use team::coordinator::Coordinator as _;
        coordinator.cleanup_team("t3").await.expect(
            "cleanup_team should find 't3' — the loop registered it via update_member_lifecycle",
        );

        assert!(
            agent_tool
                .inner
                .team_members
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .is_none(),
            "TeamDelete's cleanup must actually stop the persistent member, \
             not just clean up the registry entry"
        );
    }

    // ── Persistent member permission grants ──

    #[tokio::test]
    async fn call_rejects_auto_permission_mode_for_persistent_members() {
        let agent_tool = team_enabled_agent_tool();
        let ctx = ToolContext::for_test(std::path::PathBuf::from("/tmp"));
        let input = serde_json::json!({
            "prompt": "hi", "team_name": "t", "name": "worker", "permission_mode": "auto",
        });
        let result =
            base::tool::Tool::call(&agent_tool, input, ctx, ProgressSender::noop("t")).await;
        assert!(
            result.is_err(),
            "\"auto\" needs a transcript classifier this call site doesn't have"
        );
    }

    #[tokio::test]
    async fn call_rejects_bubble_permission_mode_for_persistent_members() {
        let agent_tool = team_enabled_agent_tool();
        let ctx = ToolContext::for_test(std::path::PathBuf::from("/tmp"));
        let input = serde_json::json!({
            "prompt": "hi", "team_name": "t", "name": "worker", "permission_mode": "bubble",
        });
        let result =
            base::tool::Tool::call(&agent_tool, input, ctx, ProgressSender::noop("t")).await;
        assert!(
            result.is_err(),
            "\"bubble\" needs an available lead, which a persistent member's lead isn't"
        );
    }

    /// "One team, one authorization": the *first* call that spawns a member
    /// under a team name establishes its permission grant; a later call
    /// under the same name that doesn't repeat `permission_mode` must
    /// inherit it, not silently fall back to the zero-config default.
    #[tokio::test]
    async fn spawn_or_message_team_member_establishes_team_permission_mode_for_later_spawns() {
        let agent_tool = team_enabled_agent_tool();
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));

        agent_tool
            .spawn_or_message_team_member(
                "t4".into(),
                "a".into(),
                "hi".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                Some(base::interface::settings::PermissionMode::BypassPermissions),
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &team_key("t4", "a")).await;

        // Second member, same team, no explicit override this time.
        agent_tool
            .spawn_or_message_team_member(
                "t4".into(),
                "b".into(),
                "hi".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &team_key("t4", "b")).await;

        let registry = agent_tool
            .inner
            .team_registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap();
        assert_eq!(
            registry.team_permission_mode("t4"),
            Some(base::interface::settings::PermissionMode::BypassPermissions),
            "the team's grant, established by the first member, must not have been \
             overwritten by the second spawn's implicit default"
        );
    }

    /// Zero-config spawn (no team-wide grant established yet, no explicit
    /// override) must resolve to the safe default (`Plan`), not something
    /// unrestricted — this is what a caller gets if they don't think about
    /// permissions at all.
    #[tokio::test]
    async fn spawn_or_message_team_member_defaults_to_plan_when_nothing_specified() {
        let agent_tool = team_enabled_agent_tool();
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));

        agent_tool
            .spawn_or_message_team_member(
                "t5".into(),
                "a".into(),
                "hi".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &team_key("t5", "a")).await;

        let registry = agent_tool
            .inner
            .team_registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap();
        assert_eq!(
            registry.team_permission_mode("t5"),
            Some(base::interface::settings::PermissionMode::Plan),
            "an unconfigured spawn must resolve to (and record) the safe Plan default"
        );
    }

    /// A non-read-only tool that behaves like most real production tools do
    /// for their *general/uncertain* case (`Bash`'s own `check_permissions`
    /// returns `Allow` only for known-safe read-only commands, `Ask`
    /// otherwise) — unlike `NamedTool` above, which unconditionally reports
    /// `is_read_only() == true` and never overrides `check_permissions()`
    /// (so it always short-circuits to `Allow`, regardless of mode — not
    /// representative of a real mutating tool for this test's purpose).
    struct AskingWriteTool;
    #[async_trait]
    impl base::tool::Tool for AskingWriteTool {
        fn name(&self) -> &str {
            "Write"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        fn is_read_only(&self, _: &Value) -> bool {
            false
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            false
        }
        async fn check_permissions(
            &self,
            _: &Value,
            _: &ToolContext,
        ) -> base::tool::PermissionDecision {
            base::tool::PermissionDecision::ask("write?")
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: ToolContext,
            _progress: ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult::text("ok"))
        }
    }

    /// Empirical check (not just reading the gate's source): does the
    /// `RuleSetPermission` built for team members under the zero-config
    /// `Plan` default actually deny `SendMessage`, the way `PermissionGate`'s
    /// bare mode-dispatch (`Plan if !is_read_only => Deny`) would suggest in
    /// isolation? `SendMessageTool::check_permissions()` returns `Allow`
    /// unconditionally, and `PermissionGate::check()`'s step 2 consults the
    /// tool's own `check_permissions()` *before* falling through to mode
    /// dispatch — so the real answer depends on call-path order, not just
    /// the mode-dispatch snippet alone.
    ///
    /// Confirmed here directly, in code — this test caught a real mistake
    /// during review: an earlier pass over this file flagged "`Plan`
    /// defaulting the team's permission grant silently breaks `SendMessage`"
    /// as an Important bug purely by reading the mode-dispatch branch in
    /// isolation, without tracing that `check_permissions()` runs first and
    /// short-circuits for `SendMessage`/`ReadMail`/`ListPeers` specifically
    /// (they all return `Allow` unconditionally from their own
    /// `check_permissions()`, exempting them from mode-based restriction by
    /// design — the same pattern the now-dead `PermissionBridge`'s
    /// `safe_tools` whitelist used to hand-implement for a different,
    /// unreachable code path). Writing this test to confirm it *before*
    /// "fixing" it caught the mistake instead of shipping an unnecessary
    /// change.
    #[tokio::test]
    async fn plan_mode_permits_send_message_despite_not_being_read_only() {
        let mailbox = std::sync::Arc::new(team::mailbox::MailboxStore::new(vec![
            "sender".into(),
            "peer".into(),
        ]));
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(team::mailbox::SendMessageTool::new(
            mailbox.clone(),
            "sender",
        )));
        tools.register(Arc::new(AskingWriteTool));

        let perm = permissions::rule_set_permission::RuleSetPermission::new(
            Arc::new(permissions::gate::PermissionGate::new(
                permissions::ruleset::RuleSet::new(vec![]),
            )),
            tools,
            base::interface::settings::PermissionMode::Plan.into(),
        );

        let send_outcome = perm
            .check(
                "SendMessage",
                &serde_json::json!({"peer": "x", "message": "y"}),
                std::path::Path::new("/tmp"),
                "test-session",
            )
            .await;
        assert!(
            matches!(
                send_outcome,
                base::interface::permission::PermissionOutcome::Permit
            ),
            "SendMessage must be permitted under Plan mode (via its own \
             check_permissions(), not the read-only mode dispatch) — got {send_outcome:?}"
        );

        // Sanity check the other half of the claim: Plan mode still denies
        // an ordinary non-read-only tool that doesn't self-authorize the
        // way the mailbox tools do — proving Plan's restriction is real for
        // everything except the deliberately-exempted mailbox tools, not
        // that this test's setup accidentally permits everything.
        let write_outcome = perm
            .check(
                "Write",
                &serde_json::json!({}),
                std::path::Path::new("/tmp"),
                "test-session",
            )
            .await;
        assert!(
            matches!(write_outcome, base::interface::permission::PermissionOutcome::Deny { .. }),
            "an ordinary non-read-only tool must still be denied under Plan mode — got {write_outcome:?}"
        );
    }

    // ═══════════════════════════════════════════════════════
    // S-1 / S-2 / S-3 / S-5 — sub-agent visibility + inheritance
    // ═══════════════════════════════════════════════════════

    /// Parent `Settings` for a sub-agent spawn, pointed at a throwaway temp
    /// directory so `Builder::build()`'s skill/agent/plugin scans have
    /// nothing real to find.
    fn parent_settings_for_test(scope: &str) -> (Settings, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "atta-subagent-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Settings::defaults_for("test-model");
        s.paths = PathSettings {
            user_data_dir: dir.clone(),
            global_data_dir: dir.clone(),
            local_data_dir: dir.clone(),
            scope: scope.to_string(),
        };
        // Keep the turn from making an extra post-turn extraction call.
        s.memory_enabled = false;
        (s, dir)
    }

    /// One tool-use round then a text answer — the minimum needed to observe
    /// a sub-agent's tool call and its final text.
    struct ProbeThenTextModel {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Model for ProbeThenTextModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            use base::interface::model::ModelEvent;
            let call_no = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events: Vec<Result<ModelEvent, base::interface::model::ModelError>> =
                if call_no == 0 {
                    vec![
                        Ok(ModelEvent::ToolUse {
                            id: "toolu_probe".into(),
                            name: "Probe".into(),
                            input: serde_json::json!({}),
                        }),
                        Ok(ModelEvent::EndTurn {
                            stop_reason: "tool_use".into(),
                            usage: Default::default(),
                        }),
                    ]
                } else {
                    vec![
                        Ok(ModelEvent::TextDelta {
                            text: "sub-agent answer".into(),
                        }),
                        Ok(ModelEvent::EndTurn {
                            stop_reason: "end_turn".into(),
                            usage: Default::default(),
                        }),
                    ]
                };
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// Records every execution so a test can prove a denial actually stopped
    /// the tool from running, not merely logged something.
    struct CountingProbeTool {
        runs: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl base::tool::Tool for CountingProbeTool {
        fn name(&self) -> &str {
            "Probe"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: ToolContext,
            _progress: ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(base::tool::ToolResult {
                content: ToolResultContent::Text("probed".into()),
                is_error: false,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            })
        }
    }

    /// A parent `Permission` impl that records what it was asked about and
    /// answers with a fixed outcome.
    struct RecordingPermission {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
        deny: bool,
    }

    #[async_trait]
    impl Permission for RecordingPermission {
        async fn check(
            &self,
            tool_name: &str,
            _input: &Value,
            _cwd: &Path,
            _session_id: &str,
        ) -> base::interface::permission::PermissionOutcome {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(tool_name.to_string());
            if self.deny {
                base::interface::permission::PermissionOutcome::Deny {
                    reason: "parent said no".into(),
                }
            } else {
                base::interface::permission::PermissionOutcome::Permit
            }
        }
    }

    /// Build an `AgentTool` wired the way `Builder::build()` wires it, with a
    /// scripted model and a `Probe` tool the sub-agent can call.
    fn wired_agent_tool(
        scope: &str,
    ) -> (
        AgentTool,
        Arc<InMemoryToolRegistry>,
        Arc<std::sync::atomic::AtomicUsize>,
        std::path::PathBuf,
    ) {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = InMemoryToolRegistry::new();
        tools.register(Arc::new(CountingProbeTool { runs: runs.clone() }));
        let tools = Arc::new(tools);
        let model: Arc<dyn Model> = Arc::new(ProbeThenTextModel {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let config = Arc::new(EngineConfig::defaults_for("test-model"));
        let (settings, dir) = parent_settings_for_test(scope);
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools.clone(), &[], &[])
                .with_settings(Arc::new(settings));
        (agent_tool, tools, runs, dir)
    }

    /// P1: a sub-agent spawned with a history store and a parent session id
    /// must persist its own transcript as a sidechain linked back to the
    /// parent — this is what makes `HistoryStore::child_sessions` (and thus
    /// `session.list {parent_session_id}`) find it.
    #[tokio::test]
    async fn subagent_transcript_is_written_as_a_linked_sidechain() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("coding");
        let history_dir = std::env::temp_dir().join(format!(
            "atta-subagent-history-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store: Arc<dyn HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &dir,
                history::path::HistoryRoots::under(&history_dir),
            )
            .await
            .unwrap(),
        );
        agent_tool.set_history_store(store.clone());
        agent_tool.set_parent_session_id("parent-session-1".to_string());

        let _text = agent_tool
            .run_sub_tagged(
                "go".into(),
                tools,
                dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                Some("explore"),
                Some(("parent-session-1", 1)),
            )
            .await
            .expect("sub-agent turn should complete");

        let children = store.child_sessions("parent-session-1").await.unwrap();
        assert_eq!(
            children.len(),
            1,
            "exactly one sidechain must be linked to the parent"
        );
        let entries = store.load(children[0]).await.unwrap();
        match &entries[0].entry {
            history::entry::LogEntry::Meta {
                parent_session_id,
                session_kind,
                scene,
                ..
            } => {
                assert_eq!(parent_session_id.as_deref(), Some("parent-session-1"));
                assert_eq!(*session_kind, history::entry::SessionKind::Sidechain);
                assert_eq!(scene.as_deref(), Some("coding"));
            }
            other => panic!("expected LogEntry::Meta as the first entry, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&history_dir);
    }

    /// A cancelled sub-agent turn (`cancel` already fired) must *not* get a
    /// terminal marker — cancellation means the work was interrupted, not
    /// that the conversation reached a real endpoint, so a stale
    /// `session.resume` against it must still be allowed to continue rather
    /// than being rejected as `SIDECHAIN_TERMINAL`.
    #[tokio::test]
    async fn cancelled_subagent_run_writes_no_terminal_marker() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("coding");
        let history_dir = std::env::temp_dir().join(format!(
            "atta-subagent-cancelled-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store: Arc<dyn HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &dir,
                history::path::HistoryRoots::under(&history_dir),
            )
            .await
            .unwrap(),
        );
        agent_tool.set_history_store(store.clone());
        agent_tool.set_parent_session_id("parent-session-1".to_string());

        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let _text = agent_tool
            .run_sub_tagged(
                "go".into(),
                tools,
                dir.clone(),
                cancel,
                Arc::new(AlwaysPermit),
                Some("explore"),
                Some(("parent-session-1", 1)),
            )
            .await
            .expect("a cancelled turn still resolves Ok — cancellation isn't an error");

        let children = store.child_sessions("parent-session-1").await.unwrap();
        let entries = store.load(children[0]).await.unwrap();
        assert!(
            !entries
                .iter()
                .any(|env| matches!(env.entry, history::entry::LogEntry::SessionEnd { .. })),
            "a cancelled sub-agent must not be marked terminal, entries: {entries:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&history_dir);
    }

    /// P5 §5.6: a one-shot sub-agent's transcript must end with a
    /// `SessionEnd` marker once its single turn concludes — that marker is
    /// what lets `session.resume` reject a stale resume against a
    /// "finished" sidechain with `SIDECHAIN_TERMINAL` instead of silently
    /// reattaching to it.
    #[tokio::test]
    async fn subagent_run_writes_a_terminal_marker_when_its_task_concludes() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("coding");
        let history_dir = std::env::temp_dir().join(format!(
            "atta-subagent-terminal-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store: Arc<dyn HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &dir,
                history::path::HistoryRoots::under(&history_dir),
            )
            .await
            .unwrap(),
        );
        agent_tool.set_history_store(store.clone());
        agent_tool.set_parent_session_id("parent-session-1".to_string());

        let _text = agent_tool
            .run_sub_tagged(
                "go".into(),
                tools,
                dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                Some("explore"),
                Some(("parent-session-1", 1)),
            )
            .await
            .expect("sub-agent turn should complete");

        let children = store.child_sessions("parent-session-1").await.unwrap();
        let entries = store.load(children[0]).await.unwrap();
        match &entries
            .last()
            .expect("transcript should not be empty")
            .entry
        {
            history::entry::LogEntry::SessionEnd { state } => {
                assert_eq!(*state, history::entry::SessionEndState::Completed);
            }
            other => panic!("expected LogEntry::SessionEnd as the last entry, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&history_dir);
    }

    /// P5 §5.5: a sub-agent must inherit the parent's telemetry handle
    /// instead of `Builder::build()`'s noop default (a channel whose
    /// receiver is dropped immediately) — otherwise every event the
    /// sub-agent produces (tool timings, permission decisions, cost) is
    /// silently discarded, which is indistinguishable from "nothing was
    /// ever recorded" from the outside.
    #[tokio::test]
    async fn subagent_telemetry_events_reach_the_parents_sink() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("coding");
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        agent_tool.set_telemetry_handle(telemetry::TelemetryHandle::new(tx));

        let _text = agent_tool
            .run_sub_tagged(
                "go".into(),
                tools,
                dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                Some("explore"),
                Some(("parent-session-1", 1)),
            )
            .await
            .expect("sub-agent turn should complete");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            rx.try_recv().is_ok(),
            "the sub-agent's turn must have emitted at least one telemetry event onto the \
             parent's sink instead of the noop default"
        );
    }

    /// S-1: a sub-agent's events must reach the *parent's* channel, tagged
    /// with a stable label, and the text return value the model sees must be
    /// unchanged by that forwarding.
    #[tokio::test]
    async fn subagent_events_reach_the_parent_channel_tagged_without_changing_the_text_result() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("coding");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent_tool.set_event_sender(crate::event_bus::EventBus::new(tx));

        let text = agent_tool
            .run_sub_tagged(
                "go".into(),
                tools,
                dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                Some("explore"),
                Some(("parent-session", 7)),
            )
            .await
            .expect("sub-agent turn should complete");
        let _ = std::fs::remove_dir_all(&dir);

        // Forwarding is additive: the value handed back to the model must be
        // byte-identical to what the un-forwarded path produces.
        let (plain_tool, plain_tools, _r, plain_dir) = wired_agent_tool("coding");
        let baseline = plain_tool
            .run_sub_tagged(
                "go".into(),
                plain_tools,
                plain_dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                Some("explore"),
                None,
            )
            .await
            .expect("baseline sub-agent turn should complete");
        let _ = std::fs::remove_dir_all(&plain_dir);
        assert_eq!(text, baseline);
        assert!(text.ends_with("sub-agent answer"), "got {text:?}");

        let mut labels = std::collections::HashSet::new();
        let mut kinds = Vec::new();
        let mut saw_probe_tool_use = false;
        let mut lifecycle = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            // The run is bracketed by `AgentSpawned`/`AgentCompleted` on the
            // parent channel; those describe the parent's timeline, so they
            // arrive unwrapped rather than inside `SubagentProgress`.
            // Asserted below, after the progress events.
            let AgentEvent::SubagentProgress {
                agent_label,
                agent_type,
                parent_session_id,
                parent_turn,
                event,
                ..
            } = ev
            else {
                match ev {
                    AgentEvent::AgentSpawned { agent_id, .. } => {
                        lifecycle.push(("spawned", agent_id))
                    }
                    AgentEvent::AgentCompleted {
                        agent_id, outcome, ..
                    } => {
                        assert_eq!(outcome, "completed");
                        lifecycle.push(("completed", agent_id));
                    }
                    other => panic!("unexpected event on the parent channel: {other:?}"),
                }
                continue;
            };
            assert_eq!(agent_type.as_deref(), Some("explore"));
            assert_eq!(parent_session_id, "parent-session");
            assert_eq!(parent_turn, 7);
            labels.insert(agent_label);
            if let AgentEvent::ToolUse { ref name, .. } = *event {
                saw_probe_tool_use = name == "Probe";
            }
            kinds.push(match *event {
                AgentEvent::TextDelta { .. } => "text",
                AgentEvent::ToolUse { .. } => "tool_use",
                AgentEvent::TurnComplete { .. } => "turn_complete",
                _ => "other",
            });
        }

        assert!(
            !kinds.is_empty(),
            "the host must see sub-agent activity, not silence"
        );
        assert_eq!(
            labels.len(),
            1,
            "every event of one sub-agent run must carry the same label, got {labels:?}"
        );
        assert!(
            labels.iter().next().unwrap().starts_with("explore#"),
            "label should name the agent type, got {labels:?}"
        );
        assert!(
            saw_probe_tool_use,
            "the sub-agent's tool call must be visible to the host, saw {kinds:?}"
        );
        assert!(
            kinds.contains(&"turn_complete"),
            "the host needs a terminal marker for the sub-agent node, saw {kinds:?}"
        );

        // `AgentEvent::AgentSpawned`/`AgentCompleted` were declared but never
        // emitted anywhere, so an embedder wiring itself to them got silence
        // and had to reconstruct the boundaries from `agent_label` instead.
        let label = labels.iter().next().unwrap().clone();
        assert_eq!(
            lifecycle,
            vec![("spawned", label.clone()), ("completed", label),],
            "the delegation must be bracketed by AgentSpawned/AgentCompleted \
             carrying the same label its progress events use"
        );
    }

    /// S-3: a sub-agent's permission checks must reach the parent's
    /// `Permission` impl, and a `Deny` there must actually stop the tool.
    #[tokio::test]
    async fn subagent_permission_check_routes_to_parent_and_deny_blocks_the_tool() {
        let (agent_tool, tools, runs, dir) = wired_agent_tool("coding");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        agent_tool.set_parent_permission(Arc::new(RecordingPermission {
            seen: seen.clone(),
            deny: true,
        }));

        // This is the handler the `Agent` tool itself uses for a spawn.
        let perm = agent_tool.sub_permission();
        let _ = agent_tool
            .run_sub_tagged(
                "go".into(),
                tools,
                dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                perm,
                None,
                None,
            )
            .await;
        let _ = std::fs::remove_dir_all(&dir);

        let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            seen.iter().any(|t| t == "Probe"),
            "the sub-agent's tool call must be checked against the parent's Permission impl, saw {seen:?}"
        );
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a Deny from the parent must actually prevent the sub-agent's tool from running"
        );
    }

    /// An `Ask` from the parent must not stall the sub-agent forever — a
    /// sub-agent has no channel a human could answer on.
    #[tokio::test]
    async fn parent_prompt_outcome_becomes_a_legible_denial_inside_a_subagent() {
        struct AlwaysPrompts;
        #[async_trait]
        impl Permission for AlwaysPrompts {
            async fn check(
                &self,
                _t: &str,
                _i: &Value,
                _c: &Path,
                _s: &str,
            ) -> base::interface::permission::PermissionOutcome {
                base::interface::permission::PermissionOutcome::Prompt {
                    prompt_id: "p1".into(),
                    message: "run rm -rf?".into(),
                    paths: vec![],
                }
            }
        }

        let agent_tool = test_agent_tool();
        agent_tool.set_parent_permission(Arc::new(AlwaysPrompts));
        let outcome = agent_tool
            .sub_permission()
            .check("Bash", &serde_json::json!({}), Path::new("/tmp"), "s")
            .await;
        match outcome {
            base::interface::permission::PermissionOutcome::Deny { reason } => {
                assert!(reason.contains("parent session"), "got {reason}");
                assert!(reason.contains("run rm -rf?"), "got {reason}");
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    /// A `Permit` from the parent still permits — the wrapper only adapts
    /// the `Prompt` case.
    #[tokio::test]
    async fn parent_permit_passes_through_unchanged() {
        let agent_tool = test_agent_tool();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        agent_tool.set_parent_permission(Arc::new(RecordingPermission {
            seen: seen.clone(),
            deny: false,
        }));
        let outcome = agent_tool
            .sub_permission()
            .check("Bash", &serde_json::json!({}), Path::new("/tmp"), "s")
            .await;
        assert!(matches!(
            outcome,
            base::interface::permission::PermissionOutcome::Permit
        ));
        assert_eq!(seen.lock().unwrap().as_slice(), ["Bash"]);
    }

    /// Without wiring, behavior is unchanged: allow-all, as before.
    #[test]
    fn permission_handler_falls_back_to_always_permit_when_no_parent_is_wired() {
        let agent_tool = test_agent_tool();
        let perm = agent_tool.sub_permission();
        let outcome = futures::executor::block_on(perm.check(
            "Bash",
            &serde_json::json!({}),
            Path::new("/tmp"),
            "s",
        ));
        assert!(matches!(
            outcome,
            base::interface::permission::PermissionOutcome::Permit
        ));
    }

    /// S-5: the sub-agent inherits the parent's scene rather than always
    /// being handed `CodingScene`, and an agent type's own `scene:` wins.
    #[test]
    fn subagent_scene_inherits_parent_and_agent_type_override_wins() {
        let agent_tool = test_agent_tool();
        let research: Arc<dyn AgentScene> = Arc::new(scene::scene::research::ResearchScene);
        agent_tool.set_scene(research);
        let settings = Settings::defaults_for("test-model");

        assert_eq!(
            agent_tool.inner.scene_for_subagent(None, &settings).id(),
            "research",
            "a Research-scene parent must not hand its sub-agent a coding system prompt"
        );

        // N-17: an agent type may *narrow* the scene, never widen it.
        // `.atta/agents/*.md` is repository content; letting it name
        // `scene: coding` (whose empty allow-list means "every registered
        // tool", `Bash` included) would let a file in the repo hand a
        // Research session — which forbids `Bash` by design — a shell.
        let widening = AgentTypeDefinition {
            name: "coder".into(),
            description: "d".into(),
            scene: Some("coding".into()),
            ..Default::default()
        };
        assert_eq!(
            agent_tool
                .inner
                .scene_for_subagent(Some(&widening), &settings)
                .id(),
            "research",
            "an agent type must not be able to widen the parent scene's tool surface"
        );

        // Narrowing is fine: ChatScene's allow-list is a strict subset of
        // Research's (no Write, no TeamCreate/TeamDelete, no Agent).
        let narrowing = AgentTypeDefinition {
            name: "chatter".into(),
            description: "d".into(),
            scene: Some("chat".into()),
            ..Default::default()
        };
        assert_eq!(
            agent_tool
                .inner
                .scene_for_subagent(Some(&narrowing), &settings)
                .id(),
            "chat",
            "an agent type narrowing the parent scene must still be honored"
        );

        let bogus = AgentTypeDefinition {
            name: "bogus".into(),
            description: "d".into(),
            scene: Some("nonexistent".into()),
            ..Default::default()
        };
        assert_eq!(
            agent_tool
                .inner
                .scene_for_subagent(Some(&bogus), &settings)
                .id(),
            "research",
            "an unknown scene id is ignored, not fatal — inherited scene is kept"
        );
    }

    /// A scene id that exists only in the host's registry — which is what
    /// every `plugin:<name>` scene is — must resolve. Before the registry was
    /// wired in, `scene_for_subagent` matched against a hardcoded list of the
    /// four built-ins, so such an id hit the "unrecognized scene" branch and
    /// the sub-agent silently inherited its parent's scene instead.
    #[test]
    fn agent_type_can_name_a_scene_that_only_the_registry_knows() {
        let agent_tool = test_agent_tool();
        let settings = Settings::defaults_for("test-model");
        let named = AgentTypeDefinition {
            name: "specialist".into(),
            description: "d".into(),
            scene: Some("plugin:demo".into()),
            ..Default::default()
        };

        // Parent is Chat, so the requested scene must be no wider than Chat's
        // tool surface for N-17 to admit it. Reusing ChatScene under a
        // plugin-style id keeps this test about *resolution*, not about the
        // narrowing rule the test above already covers.
        let chat: Arc<dyn AgentScene> = Arc::new(scene::scene::chat::ChatScene);
        agent_tool.set_scene(chat);

        assert_eq!(
            agent_tool
                .inner
                .scene_for_subagent(Some(&named), &settings)
                .id(),
            "chat",
            "without a registry, a non-builtin id is unresolvable and the parent scene is kept"
        );

        let mut registry = scene::scene::SceneRegistry::new();
        registry.register_builtin();
        registry.register(Arc::new(AliasScene {
            id: "plugin:demo",
            inner: Arc::new(scene::scene::chat::ChatScene),
        }));
        agent_tool.set_scene_registry(Arc::new(registry));

        assert_eq!(
            agent_tool
                .inner
                .scene_for_subagent(Some(&named), &settings)
                .id(),
            "plugin:demo",
            "a registry-only scene id must resolve once the registry is wired in"
        );
    }

    /// A scene that borrows another's behavior under a different id — stands
    /// in for a plugin scene without depending on the plugin crate.
    struct AliasScene {
        id: &'static str,
        inner: Arc<dyn AgentScene>,
    }

    impl AgentScene for AliasScene {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
        fn description(&self) -> &str {
            "alias"
        }
        fn build_system_prompt(
            &self,
            ctx: &base::interface::scene::ScenePromptContext,
        ) -> Vec<base::interface::prompt::PromptBlock> {
            self.inner.build_system_prompt(ctx)
        }
        fn tools(&self) -> Vec<String> {
            self.inner.tools()
        }
        fn token_budget(&self) -> base::interface::scene::TokenBudget {
            self.inner.token_budget()
        }
        fn disallowed_tools(&self) -> Vec<String> {
            self.inner.disallowed_tools()
        }
    }

    /// With no explicit scene wired in, `Settings.paths.scope` (which the
    /// daemon sets to `scene.id()`) is the fallback, and an unknown scope
    /// still degrades to the historical `CodingScene`.
    #[test]
    fn subagent_scene_falls_back_to_settings_scope_then_coding() {
        let agent_tool = test_agent_tool();
        let mut settings = Settings::defaults_for("test-model");

        settings.paths.scope = "chat".into();
        assert_eq!(
            agent_tool.inner.scene_for_subagent(None, &settings).id(),
            "chat"
        );

        settings.paths.scope = "code".into(); // the historical stand-in value
        assert_eq!(
            agent_tool.inner.scene_for_subagent(None, &settings).id(),
            "coding"
        );
    }

    /// S-2: a sub-agent must be able to see AGENTS.md / CLAUDE.md, both when
    /// the parent got it from settings.json and when it came from
    /// `Builder::instruction_file(..)` (invisible in `Settings`).
    #[test]
    fn subagent_inherits_the_parent_instruction_file() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("test-model"));

        let mut parent = Settings::defaults_for("test-model");
        parent.instruction_file = Some(std::path::PathBuf::from("/proj/AGENTS.md"));
        let agent_tool = AgentTool::with_parent_tools(
            model.clone(),
            config.clone(),
            tools.clone(),
            tools.clone(),
            &[],
            &[],
        )
        .with_settings(Arc::new(parent));
        assert_eq!(
            agent_tool
                .sub_settings(None, std::path::PathBuf::from("/tmp"))
                .instruction_file,
            Some(std::path::PathBuf::from("/proj/AGENTS.md")),
            "sub-agents must inherit the project's instruction file from settings"
        );

        // Builder-level instruction file: not present in `Settings` at all.
        let bare = AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[]);
        assert_eq!(
            bare.sub_settings(None, std::path::PathBuf::from("/tmp"))
                .instruction_file,
            None
        );
        bare.set_instruction_file(Some(std::path::PathBuf::from("/proj/CLAUDE.md")));
        assert_eq!(
            bare.sub_settings(None, std::path::PathBuf::from("/tmp"))
                .instruction_file,
            Some(std::path::PathBuf::from("/proj/CLAUDE.md"))
        );
    }

    #[test]
    fn agent_type_frontmatter_parses_scene_override() {
        let dir = std::env::temp_dir().join(format!(
            "atta-agent-scene-front-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "digger.md",
            "---\nname: digger\ndescription: Source digger\nscene: research\n---\nBody.",
        );
        let types = load_agent_types_from_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(types[0].scene.as_deref(), Some("research"));
    }

    /// The daemon relays the forwarded event by serializing it — a nested
    /// `Box<AgentEvent>` inside an internally-tagged enum must round-trip.
    #[test]
    fn subagent_progress_event_round_trips_through_json() {
        let ev = AgentEvent::SubagentProgress {
            agent_label: "explore#abc12345".into(),
            agent_session_id: "sub-1".into(),
            agent_type: Some("explore".into()),
            parent_session_id: "parent-1".into(),
            parent_turn: 3,
            event: Box::new(AgentEvent::ToolUse {
                id: "toolu_1".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "/a"}),
                turn_id: "t".into(),
            }),
        };
        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["kind"], "subagent_progress");
        assert_eq!(json["event"]["kind"], "tool_use");
        assert_eq!(json["event"]["name"], "Read");
        let back: AgentEvent = serde_json::from_value(json).expect("deserialize");
        match back {
            AgentEvent::SubagentProgress {
                agent_label, event, ..
            } => {
                assert_eq!(agent_label, "explore#abc12345");
                assert!(matches!(*event, AgentEvent::ToolUse { .. }));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn subagent_label_is_stable_and_names_the_agent_type() {
        let sid = "3f2a1b7c-dead-beef-0000-111122223333";
        assert_eq!(subagent_label(Some("explore"), sid), "explore#3f2a1b7c");
        assert_eq!(subagent_label(None, sid), "agent#3f2a1b7c");
        assert_eq!(
            subagent_label(Some("explore"), sid),
            subagent_label(Some("explore"), sid),
            "the label must not change between events of the same run"
        );
    }

    /// `resume_agent` builds and runs a fresh sidechain session exactly like
    /// `run_sub_tagged`/`run_sub_inner` — it must write that new session's
    /// terminal marker too once its turn concludes, or a stale
    /// `session.resume` against *it* would silently reattach instead of
    /// being rejected. Regression test for the bug where `resume_agent`
    /// wrote `write_sidechain_meta` but never called
    /// `write_sidechain_terminal_marker`.
    #[tokio::test]
    async fn resume_agent_writes_a_terminal_marker_when_its_task_concludes() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("coding");
        let history_dir = std::env::temp_dir().join(format!(
            "atta-resume-terminal-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store: Arc<dyn HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &dir,
                history::path::HistoryRoots::under(&history_dir),
            )
            .await
            .unwrap(),
        );
        agent_tool.set_history_store(store.clone());
        agent_tool.set_parent_session_id("parent-session-1".to_string());

        // Seed a source session to resume from.
        let _text = agent_tool
            .run_sub_tagged(
                "go".into(),
                tools.clone(),
                dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                Some("explore"),
                Some(("parent-session-1", 1)),
            )
            .await
            .expect("sub-agent turn should complete");
        let source_sid = store
            .child_sessions("parent-session-1")
            .await
            .unwrap()
            .remove(0);

        let _text = agent_tool
            .resume_agent(
                &source_sid.to_string(),
                store.clone(),
                "continue".into(),
                tools,
                dir.clone(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("resumed turn should complete");

        // The resumed run is a fresh sidechain of its own, also linked to
        // the parent — it's the *other* child besides `source_sid`.
        let children = store.child_sessions("parent-session-1").await.unwrap();
        let resumed_sid = children
            .into_iter()
            .find(|sid| *sid != source_sid)
            .expect("resume_agent must have created a second, distinct sidechain session");
        let entries = store.load(resumed_sid).await.unwrap();
        match &entries
            .last()
            .expect("transcript should not be empty")
            .entry
        {
            history::entry::LogEntry::SessionEnd { state } => {
                assert_eq!(*state, history::entry::SessionEndState::Completed);
            }
            other => panic!("expected LogEntry::SessionEnd as the last entry, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&history_dir);
    }

    /// Mirrors `cancelled_subagent_run_writes_no_terminal_marker` for the
    /// `resume_agent` path: a resumed session cancelled mid-turn must stay
    /// resumable, not get nailed down as `Completed`.
    #[tokio::test]
    async fn cancelled_resume_agent_writes_no_terminal_marker() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("coding");
        let history_dir = std::env::temp_dir().join(format!(
            "atta-resume-cancelled-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store: Arc<dyn HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &dir,
                history::path::HistoryRoots::under(&history_dir),
            )
            .await
            .unwrap(),
        );
        agent_tool.set_history_store(store.clone());
        agent_tool.set_parent_session_id("parent-session-1".to_string());

        let _text = agent_tool
            .run_sub_tagged(
                "go".into(),
                tools.clone(),
                dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                Some("explore"),
                Some(("parent-session-1", 1)),
            )
            .await
            .expect("sub-agent turn should complete");
        let source_sid = store
            .child_sessions("parent-session-1")
            .await
            .unwrap()
            .remove(0);

        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let _text = agent_tool
            .resume_agent(
                &source_sid.to_string(),
                store.clone(),
                "continue".into(),
                tools,
                dir.clone(),
                cancel,
            )
            .await
            .expect("a cancelled resume still resolves Ok — cancellation isn't an error");

        let children = store.child_sessions("parent-session-1").await.unwrap();
        let resumed_sid = children
            .into_iter()
            .find(|sid| *sid != source_sid)
            .expect("resume_agent must have created a second, distinct sidechain session");
        let entries = store.load(resumed_sid).await.unwrap();
        assert!(
            !entries
                .iter()
                .any(|env| matches!(env.entry, history::entry::LogEntry::SessionEnd { .. })),
            "a cancelled resume must not be marked terminal, entries: {entries:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&history_dir);
    }
}
