//! `AgentTool` — spawn sub-agents using the agent's own `Agent` engine.
//!
//! Implements `base::tool::Tool` for the `Agent` invocable by the model.
//! Each sub-agent is a session of its own built with `Builder::build()`; it
//! inherits the parent's model client but gets a restricted tool set.
//!
//! # Agent type registry
//!
//! The module defines built-in agent types (`builtin_agent_types()`) and can
//! load user-defined types from a directory (e.g. `~/.agix/scenes/<scope>/agents/*.md`
//! or `<project>/.agix/agents/*.md`) via `load_agent_types_from_dir()`. Each
//! type specifies a system prompt and an allowed tool set, which
//! `resolve_tools()` applies when spawning sub-agents.

use anyhow::anyhow;
use async_trait::async_trait;
use base::context::EngineConfig;
use base::interface::event::AgentEvent;
use base::interface::model::Model;
use base::interface::permission::Permission;
use base::interface::scene::Scene;
use base::interface::settings::{
    Effort, ExecutionSettings, ModelSettings, PathSettings, PermissionMode, SandboxConfig,
    Settings, ThinkingMode,
};
use base::tool::InMemoryToolRegistry;
use base::tool::ProgressSender;
use base::tool::ToolContext;
use base::tool::ToolResultContent;
use history::store::HistoryStore;
use runtime::agent::{Agent, Builder, EventReceiver, InputMessage, InputSender};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use telemetry::TelemetryHandle;
use tools::worktree::create_worktree;

pub use base::agent_type::{parse_permission_mode, AgentTypeDefinition, AgentTypeSource};

/// Return the five built-in agent types shipped with Agix.
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
/// config). This means `.agix/agents/*.md` customization always wins over a
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
    _watcher: Option<Arc<crate::type_watcher::AgentTypeWatcher>>,
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
        let _watcher = crate::type_watcher::AgentTypeWatcher::watch(
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
        *self
            .agent_types
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Arc::new(merge_agent_types(dirs, plugin_types));
    }

    /// Handle to pass to `AgentTool::with_shared_agent_types` for each session.
    pub fn handle(&self) -> SharedAgentTypeMap {
        self.agent_types.clone()
    }
}

/// Render the one-line-per-type catalog injected into `AgentTool::description()`
/// so the model can see what `subagent_type` values are actually available
/// (built-in + any custom types loaded from `.agix/agents/`) without a
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
/// `Arc<dyn Scene>` was wired in (the daemon sets `Settings.paths.scope`
/// to `scene.id()`, so the scope *is* the scene id there). Unknown ids return
/// `None` so the caller can fall back rather than guessing.
fn builtin_scene_by_id(id: &str) -> Option<Arc<dyn Scene>> {
    match id {
        "code" => Some(Arc::new(scene::code::CodeScene::default())),
        "harnix" => Some(Arc::new(scene::harnix::HarnixScene)),
        "mini" => Some(Arc::new(scene::mini::MiniScene)),
        _ => None,
    }
}

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
    /// A JSON Schema the sub-agent's final answer must satisfy; the answer is
    /// then also returned as structured content.
    #[serde(default)]
    pub output_schema: Option<Value>,
    /// A team to join, together with `name`. If that member is alive, `prompt`
    /// is queued as its next message instead of spawning a new one. Needs
    /// team mode enabled for the session.
    // The schema cannot hide this per session, so `AgentTool::call` rejects it
    // when team mode is off.
    #[serde(default)]
    pub team_name: Option<String>,
    /// This member's name within `team_name`. Reuse the same pair to keep
    /// messaging the same member.
    #[serde(default)]
    pub name: Option<String>,
    /// With `team_name` + `name`: the permission mode a new member runs under
    /// for its whole lifetime (default `plan`, read-only). The first member
    /// spawned under a team fixes it for the team; messaging a live member
    /// ignores it. `bubble` is rejected.
    // Fixed at spawn because a persistent member's lead is not around to
    // answer per-call prompts (see `AgentTool::build_team_member_agent`), and
    // `bubble` needs that lead.
    #[serde(default)]
    pub permission_mode: Option<base::interface::settings::PermissionMode>,
}

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

/// How a task that ran a sub-agent ended.
fn finished(result: &Result<String, base::error::ToolError>) -> base::context::RunningStatus {
    match result {
        Ok(_) => base::context::RunningStatus::Completed,
        Err(e) => base::context::RunningStatus::Failed(e.to_string()),
    }
}

/// A sub-agent's [`base::context::TaskInbox`]: messages join its running turn
/// at the next step boundary, as a user's steering does.
#[derive(Debug)]
struct SteerInbox(runtime::steering::SteerHandle);

impl base::context::TaskInbox for SteerInbox {
    fn deliver(&self, message: String) -> Result<(), String> {
        match self.0.offer(base::id::Id::new().to_string(), message) {
            runtime::steering::SteerAck::Accepted => Ok(()),
            runtime::steering::SteerAck::NotRunning => {
                Err("the sub-agent is not in a turn that can take messages".into())
            }
        }
    }
}

fn open_inbox(task: Option<&base::context::RunningTask>, agent: &Agent) {
    if let Some(task) = task {
        let _ = task.inbox.set(Arc::new(SteerInbox(agent.steer_handle())));
    }
}

const NOTIFIED_RESULT_BYTES: usize = 4 * 1024;

/// What a caller fixes about one sub-agent run besides its prompt and tools.
#[derive(Default)]
pub(crate) struct SubRun {
    /// The task the run's cost and structured output are reported on.
    task: Option<Arc<base::context::RunningTask>>,
    /// The sidechain's id, when the caller has already handed it out.
    session_id: Option<String>,
    /// Over the agent type's model.
    model: Option<String>,
    allowed_tools: Vec<String>,
    disallowed_tools: Vec<String>,
    deny_rules: Vec<base::permission::PermissionRule>,
    output_schema: Option<Value>,
    /// A mode the caller asked for (a team's), held to the parent's.
    permission_mode: Option<base::interface::settings::PermissionMode>,
    /// A `TeamCreate` member, whose spawn and completion the team
    /// coordinator records with its stage.
    team_member: bool,
    /// The team the run is a member of.
    seat: Option<base::interface::agent_spawner::TeamSeat>,
}

impl SubRun {
    fn for_task(task: &Arc<base::context::RunningTask>, session_id: &str) -> Self {
        Self {
            task: Some(task.clone()),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        }
    }

    fn permission(&self, base: Arc<dyn Permission>) -> Arc<dyn Permission> {
        if self.deny_rules.is_empty() {
            return base;
        }
        Arc::new(DenyRulesFirst {
            rules: permissions::rule_set_permission::RuleSetPermission::new(
                Arc::new(permissions::gate::PermissionGate::deny_rules_only(
                    permissions::ruleset::RuleSet::new(self.deny_rules.clone()),
                )),
                Arc::new(InMemoryToolRegistry::new()),
                base::permission::PermissionMode::Default,
            ),
            inner: base,
        })
    }
}

/// A permission with a host's `deny` rules in front: what they refuse is
/// refused, anything else is decided as it would have been.
///
/// The rules sit in a deny-only gate, so its only verdicts of its own are
/// the denials; its mode never decides anything here.
struct DenyRulesFirst {
    rules: permissions::rule_set_permission::RuleSetPermission,
    inner: Arc<dyn Permission>,
}

#[async_trait]
impl Permission for DenyRulesFirst {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        use base::interface::permission::PermissionOutcome;
        match self
            .rules
            .check(tool_name, tool_input, cwd, session_id)
            .await
        {
            denied @ PermissionOutcome::Deny { .. } => denied,
            _ => {
                self.inner
                    .check(tool_name, tool_input, cwd, session_id)
                    .await
            }
        }
    }
    async fn check_for_delegate(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
        delegate: &Arc<base::context::SessionState>,
    ) -> base::interface::permission::PermissionOutcome {
        use base::interface::permission::PermissionOutcome;
        match self
            .rules
            .check(tool_name, tool_input, cwd, session_id)
            .await
        {
            denied @ PermissionOutcome::Deny { .. } => denied,
            _ => {
                self.inner
                    .check_for_delegate(tool_name, tool_input, cwd, session_id, delegate)
                    .await
            }
        }
    }
    fn bind_tool_registry(&self, tools: Arc<dyn base::tool::ToolRegistry>) {
        self.rules.bind_tool_registry(tools.clone());
        self.inner.bind_tool_registry(tools);
    }
    fn bind_session_state(&self, session: Arc<base::context::SessionState>) {
        self.inner.bind_session_state(session);
    }
    fn add_temporary_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.inner.add_temporary_allow(tool_name, rule_content);
    }
    fn clear_temporary_allows(&self) {
        self.inner.clear_temporary_allows();
    }
    fn add_persistent_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.inner.add_persistent_allow(tool_name, rule_content);
    }
}

/// What one of a sub-agent's events tells its task: a model call's cost, the
/// structured output it handed in.
fn note_on_task(task: Option<&base::context::RunningTask>, event: &AgentEvent) {
    let Some(task) = task else { return };
    match event {
        AgentEvent::StepEnd { usage, .. } => task.add_usage(*usage),
        AgentEvent::TurnComplete {
            structured_output: Some(output),
            ..
        } => {
            let _ = task.structured_output.set(output.clone());
        }
        _ => {}
    }
}

/// Record how a background sub-agent's task ended and tell the agent that
/// started it. A task stopped on purpose ends without a notice: whoever
/// stopped it already knows.
fn conclude_background_task(
    session: &base::context::SessionState,
    task: &base::context::RunningTask,
    result: &Result<String, base::error::ToolError>,
    notify_parent: bool,
) {
    if let Ok(text) = result {
        task.append_output(text);
    }
    // Posted before the task ends, so whoever wakes on its end and reads the
    // result (TaskWait, TaskOutput) finds the notice there to withdraw.
    if notify_parent && !task.has_ended() {
        session.post_notice(Some(task.task_id.clone()), task_notification(task, result));
    }
    session.finish_task(&task.task_id, finished(result));
}

fn task_notification(
    task: &base::context::RunningTask,
    result: &Result<String, base::error::ToolError>,
) -> String {
    let status = if result.is_ok() { "done" } else { "failed" };
    let mut notice = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<status>{status}</status>\n",
        task.task_id
    );
    let detail = task
        .detail
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(d) = detail["description"].as_str() {
        notice.push_str(&format!("<description>{d}</description>\n"));
    }
    if let Err(e) = result {
        notice.push_str(&format!("<error>{e}</error>\n"));
    }
    let tail = task.output_tail(NOTIFIED_RESULT_BYTES);
    if !tail.text.is_empty() {
        let more = if tail.from_byte > 0 {
            " (the start is cut; TaskOutput has what is kept)"
        } else {
            ""
        };
        notice.push_str(&format!("<result{more}>\n{}\n</result>\n", tail.text));
    }
    notice.push_str("</task-notification>");
    notice
}

/// Write the first `Meta` line for a freshly spawned sidechain (sub-agent /
/// team member) session, linking it back to its parent via
/// `parent_session_id` and marking it `session_kind: Sidechain` — this is
/// what makes it discoverable via `HistoryStore::child_sessions` and hidden
/// from `session.list` by default. See docs/ARCHITECTURE.md §3.3.
///
/// `project_root` is left `None` here: this crate has no per-session project
/// identity, so there is nothing correct to stamp.
///
/// Best-effort: a failure here does not fail the spawn. The sub-agent still
/// runs and its `User`/`Assistant` entries still persist via
/// `SessionManager::persist` — it just isn't linkable back to its parent.
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
        agix_version: env!("CARGO_PKG_VERSION").to_string(),
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
/// (from `run_sub_inner`/`run_sub_tagged`) once their turn has
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
    // the host can render sub-agent activity live. Purely additive: the
    // text return value is identical either way.
    tag: Option<SubagentTag>,
    task: Option<&base::context::RunningTask>,
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
                    note_on_task(task, &ev);
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
                note_on_task(task, &ev);
                if let Some(tag) = &tag {
                    tag.forward(ev);
                }
            }
            Ok(None) | Err(_) => break,
        }
    }

    let cancelled = matches!(&outcome, Ok(o) if o.stop_reason == "cancelled");
    let result = match outcome {
        Ok(o) => Ok(with_stop_note(text, &o.stop_reason)),
        Err(runtime::turn::TurnError::Shutdown) => Ok(text),
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

/// A sub-agent's answer, with a line saying so when its turn was cut off by
/// a limit rather than finished — without it the parent reads a partial or
/// empty answer as the whole one.
fn with_stop_note(text: String, stop_reason: &str) -> String {
    let note = match stop_reason {
        "max_turns" | "host_ceiling" => {
            "The sub-agent reached its limit of model calls (max_turns) before it finished"
        }
        "max_structured_output_retries" => {
            "The sub-agent stopped after too many attempts at its structured output"
        }
        _ => return text,
    };
    if text.trim().is_empty() {
        format!("[{note}; it gave no answer.]")
    } else {
        format!("{text}\n\n[{note}; the answer above may be incomplete.]")
    }
}

/// [`run_one_turn`] on a task of its own, so a foreground sub-agent is
/// scheduled like any other agent instead of being polled from inside the
/// parent's turn. The parent still waits for it; if the parent's tool call is
/// dropped instead, the guard aborts the sub-agent rather than leaving it
/// running unobserved.
#[allow(clippy::too_many_arguments)]
async fn run_one_turn_detached(
    session_id: &str,
    mut agent: Agent,
    mut event_rx: EventReceiver,
    input_tx: InputSender,
    prompt: String,
    cancel: tokio_util::sync::CancellationToken,
    tag: Option<SubagentTag>,
    task: Option<Arc<base::context::RunningTask>>,
) -> (Result<String, base::error::ToolError>, bool) {
    use tracing::Instrument;
    let span = tracing::info_span!("subagent", session = %session_id);
    let mut task = AbortOnDrop(tokio::spawn(
        async move {
            let outcome = run_one_turn(
                &mut agent,
                &mut event_rx,
                &input_tx,
                prompt,
                cancel,
                tag,
                task.as_deref(),
            )
            .await;
            drop(input_tx);
            outcome
        }
        .instrument(span),
    ));
    match (&mut task.0).await {
        Ok(outcome) => outcome,
        Err(e) if e.is_panic() => (
            Err(base::error::ToolError::Execution(anyhow!(
                "sub-agent panicked: {}",
                panic_message(e.into_panic())
            ))),
            false,
        ),
        Err(_) => (
            Err(base::error::ToolError::Execution(anyhow!(
                "sub-agent task was aborted"
            ))),
            true,
        ),
    }
}

/// A background sub-agent's run, ending in the error its panic left rather
/// than unwinding past the bookkeeping that ends its task.
async fn without_panics(
    run: impl std::future::Future<Output = Result<String, base::error::ToolError>> + Send + 'static,
) -> Result<String, base::error::ToolError> {
    match tokio::spawn(run).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => Err(base::error::ToolError::Execution(anyhow!(
            "sub-agent panicked: {}",
            panic_message(e.into_panic())
        ))),
        Err(_) => Err(base::error::ToolError::Execution(anyhow!(
            "sub-agent task was aborted"
        ))),
    }
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(non-string panic payload)".into())
}

/// After a one-shot sidechain's turn has finished — successfully, with an
/// error, or because `cancel` fired — writes the matching `SessionEnd`
/// terminal marker. The single place `run_sub_tagged` and `run_sub_inner`
/// route through instead of each hand-rolling the same Completed/Failed
/// mapping.
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
    stopped: Option<(std::sync::Weak<hooks::HookRunner>, SubagentPayload)>,
    spawned: Option<SpawnTelemetry>,
) {
    let mut last_failed = false;
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
        open_inbox(Some(&task), &agent);
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
            Some(task.as_ref()),
        )
        .await;

        last_failed = result.is_err();
        conclude_background_task(&session, &task, &result, true);

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
    if let Some(spawned) = spawned {
        spawned.finish(last_failed);
    }
    if let Some((runner, payload)) = stopped {
        if let Some(runner) = runner.upgrade() {
            payload
                .stopped("shutdown")
                .fire(&runner, hooks::HookEvent::SubagentStop)
                .await;
        }
    }
}

fn parse_effort(effort: &str) -> Option<Effort> {
    serde_json::from_value(serde_json::Value::String(effort.to_string())).ok()
}

/// Whether an agent type's `allowed_tools` / `disallowed_tools` let a tool
/// through. No type, or a type with neither list, admits everything.
fn type_admits_tool(def: Option<&AgentTypeDefinition>, name: &str) -> bool {
    def.map_or(true, |def| {
        lists_admit_tool(&def.allowed_tools, &def.disallowed_tools, name)
    })
}

fn lists_admit_tool(allowed: &[String], disallowed: &[String], name: &str) -> bool {
    let listed = |names: &[String]| names.iter().any(|n| AgentTool::tool_name_matches(name, n));
    !listed(disallowed) && (allowed.is_empty() || listed(allowed))
}

fn contributed_tool_filter(
    def: Option<&AgentTypeDefinition>,
    run: &SubRun,
) -> runtime::agent::ToolFilter {
    let def = def.cloned();
    let (allowed, disallowed) = (run.allowed_tools.clone(), run.disallowed_tools.clone());
    Arc::new(move |name| {
        type_admits_tool(def.as_ref(), name) && lists_admit_tool(&allowed, &disallowed, name)
    })
}

/// Apply an `AgentTypeDefinition`'s `permission_mode`/`effort`/`max_turns`
/// overrides to an already-built subagent `Settings`, in place — the same
/// step whichever path built the base `Settings`.
///
/// A type's mode replaces the parent's, wider or narrower, as
/// `PermissionMode::for_delegate` allows. A plugin's types arrive over the
/// network and can only tighten; the API-call cap is clamped the same way for
/// them.
fn apply_agent_type_overrides(settings: &mut Settings, def: &AgentTypeDefinition) {
    if let Some(mode) = def.permission_mode {
        let applied = if def.source.may_loosen() {
            settings.permission_mode.for_delegate(mode)
        } else {
            mode.min_permissive(settings.permission_mode)
        };
        if applied != mode {
            tracing::warn!(
                agent_type = %def.name,
                requested = ?mode,
                inherited = ?settings.permission_mode,
                applied = ?applied,
                "agent type's permission mode held by its parent's"
            );
        }
        settings.permission_mode = applied;
    }
    if let Some(effort) = &def.effort {
        if let Some(level) = parse_effort(effort) {
            settings.model.effort = Some(level);
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

/// Build a sub-agent's `Settings` by cloning the parent's real settings and
/// overriding only the model-selection fields — the sub-agent inherits the
/// parent's actual scope/paths (skills/hooks/agents/memory all resolve
/// correctly), auth, sandbox policy, etc.
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
    // Cloning the parent already carries `Settings.instruction_file`;
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
/// The user-level roots hang off `local_data_dir`, not `$HOME`: with no
/// instance configuration in hand, the invoking user's home is the one
/// directory this has no grounds to assume it owns.
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
            effort: None,
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
        instruction_file: instruction_file.cloned(),
        prompt_append: None,
        prompt_override: None,
        recorder: None,
        telemetry_url: None,
        session_dir: None,
        memory_enabled: true,
        disable_skill_shell_execution: false,
        path_style: Default::default(),
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
        // A sub-agent inherits no script bindings: a script bound in the
        // parent's configuration applies to the parent's prompt, and quietly
        // running it against every spawned agent's prompt too would be a
        // scope nobody asked for.
        scripts: Vec::new(),
    }
}

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

pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 6;

/// The cap on sub-agents running at once in one session tree.
#[derive(Clone, Debug)]
pub struct SubagentSlots {
    slots: Arc<tokio::sync::Semaphore>,
    cap: usize,
}

impl SubagentSlots {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(cap)),
            cap,
        }
    }

    pub fn running(&self) -> usize {
        self.cap - self.slots.available_permits()
    }
}

/// Leave to start one sub-agent: the depth it runs at and, when it counts
/// against the session's cap, the slot it holds until it ends.
struct Admission {
    depth: u32,
    _slot: Option<tokio::sync::OwnedSemaphorePermit>,
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
    /// Sub-agents running anywhere under the root session; shared by every
    /// delegate's `AgentTool` (see `AgentsModule::for_delegates`).
    subagent_slots: SubagentSlots,
    /// Built-in types merged with any disk-loaded custom types (see
    /// `merge_agent_types`). Keyed by `subagent_type` name. `RwLock<Arc<..>>`
    /// (not a plain `Arc<..>`) so `agent_type_watcher::AgentTypeWatcher` can
    /// swap the whole map in place when a `.agix/agents/*.md` file changes —
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
    _agent_type_watcher: Option<Arc<crate::type_watcher::AgentTypeWatcher>>,
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
    /// The plugins every delegate is built with, as its parent was — the
    /// QuickJS layer among them, which decides itself what follows a delegate.
    delegate_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    /// The tools those plugins registered in this session. A delegate is not
    /// handed these copies: its own plugins register its own.
    plugin_tools: Arc<std::collections::HashSet<String>>,
    /// Agent types this session's plugins declare, over the shared catalog:
    /// above built-in and package types, below the ones on disk.
    session_agent_types: Arc<std::collections::HashMap<String, AgentTypeDefinition>>,
    process_groups: Option<Arc<base::interface::exec::local::GroupLedger>>,
    /// What every delegate is assembled with: the parent session's modules.
    modules: Vec<Arc<dyn runtime::module::Module>>,
    /// The parent session's `SkillManager`, when known — used to resolve
    /// the `skills:` preload field on `AgentTypeDefinition`. Set via
    /// `AgentTool::set_skill_catalog` *after* construction (interior
    /// mutability, `RwLock` not a plain field) because the catalog is set
    /// later in the same assembly (`AgentsModule::on_session` in
    /// `agents/src/lib.rs`). `None` (tests,
    /// the generic `AgentSpawner` bridge) means `skills:` preload is
    /// silently skipped, matching how `parent_settings`/`task_router`
    /// absence degrades elsewhere in this struct.
    skill_manager:
        Arc<std::sync::RwLock<Option<Arc<dyn base::interface::skill_catalog::SkillCatalog>>>>,
    /// A snapshot of the parent session's connected MCP tools, used to
    /// resolve `AgentTypeDefinition.mcp_servers` by reusing already-
    /// connected tool instances (not reconnecting). Same interior-
    /// mutability / set-after-construction pattern as `skill_manager` and
    /// for the same reason (`Builder::build()` connects MCP after
    /// `AgentTool` exists) — but a `Vec` snapshot rather than a live
    /// `Arc<McpManager>` reference: `McpManager` isn't `Clone` and `refresh_tools()` needs
    /// `&mut self`, which would conflict with `AgentTool` also holding a
    /// shared reference to the same live instance. A subagent therefore
    /// sees the MCP tools connected *as of when this was set* — a later
    /// `config.reload` reconnect in the parent session doesn't retroactively
    /// update it, an accepted scope boundary (matches `description`'s own
    /// "not kept live" doc comment on this same struct).
    mcp_tool_adapters: Arc<std::sync::RwLock<Vec<Arc<dyn base::tool::Tool>>>>,
    /// The shared `TeamRegistry` `AgentsModule::on_session` also hands to
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
    /// pause followed by a wall of text. `None` (tests, embedders that
    /// never call `set_event_sender`): events are still collected internally
    /// into the returned text, they just aren't mirrored anywhere.
    ///
    /// Same interior-mutability, set-after-construction pattern as
    /// `skill_manager` — `Builder::build()` creates the `AgentTool` before it
    /// creates the session's event channel.
    event_tx: Arc<std::sync::RwLock<Option<runtime::agent::EventSender>>>,
    /// The parent session's hook runner, so sub-agent spawns can fire
    /// `SubagentStart`/`SubagentStop`. Same set-after-construction pattern as
    /// `event_tx` — `Builder::build()` constructs the `AgentTool` before the
    /// `HookRunner` exists. `None` (tests, embedders that never wire it)
    /// fires nothing. Weak: the runner's `"agent"` hooks spawn through this
    /// tool, and a strong edge back would keep both alive past the session.
    hooks: Arc<std::sync::RwLock<Option<std::sync::Weak<hooks::HookRunner>>>>,
    /// Cancelled when the parent session ends; every persistent team
    /// member's token is a child of it.
    session_ended: Arc<std::sync::RwLock<tokio_util::sync::CancellationToken>>,
    /// The parent session's history store, so a sub-agent's transcript is
    /// persisted and a failed sub-agent can be inspected afterwards.
    /// The sub-agent gets its own fresh session id, so it lands in its own
    /// JSONL file — the parent's transcript is never written to.
    history_store: Arc<std::sync::RwLock<Option<Arc<dyn HistoryStore>>>>,
    /// The **parent agent's** `Permission` impl. Sub-agent permission checks
    /// are routed here instead of being hard-permitted, so a session
    /// that opts into a real permission handler can't be bypassed just by
    /// spawning a sub-agent. `None` means `AlwaysPermit`, for `AgentTool`s
    /// built without wiring (tests).
    parent_permission: Arc<std::sync::RwLock<Option<Arc<dyn Permission>>>>,
    /// The **parent agent's** pending-permission registry, so a sub-agent's
    /// `Prompt` outcome can be turned into a real, answerable question
    /// instead of an automatic refusal.
    ///
    /// A sub-agent has no channel of its own to a human; the parent does.
    /// Registering the prompt here and emitting it on `event_tx` puts it in
    /// front of the same host that answers the parent's own prompts, via the
    /// same `session.respondToPrompt` RPC. `None` (unwired embedders) refuses
    /// with an explanation.
    parent_pending_permissions: Arc<std::sync::RwLock<Option<runtime::agent::PendingPermissions>>>,
    /// The parent agent's scene. Sub-agents inherit it rather than
    /// always being built as `CodeScene`. `None` falls
    /// back to resolving `Settings.paths.scope` via `resolve_scene_id`, then to
    /// `CodeScene`.
    parent_scene: Arc<std::sync::RwLock<Option<Arc<dyn Scene>>>>,
    /// Every scene this process knows about, for resolving an agent type's
    /// `scene:` id. `None` falls back to [`builtin_scene_by_id`], which is
    /// what an embedder that never wires a registry (tests, library callers)
    /// gets — the built-in four. A host that registers
    /// scenes beyond those (the daemon, which also registers plugin scenes)
    /// wires its registry here so those ids resolve too.
    scene_registry: Arc<std::sync::RwLock<Option<Arc<scene::SceneRegistry>>>>,
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
    /// usual noop fallback.
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
    /// Refuses rather than waits when the session is at its cap: an agent
    /// holding a slot while it waits for one for its own child could
    /// deadlock a tree whose every slot is held that way.
    fn admit(&self) -> Result<Admission, String> {
        let depth = self.spawn_guard()?;
        let slot = self
            .subagent_slots
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                format!(
                    "{} sub-agents are already running in this session, the most it allows \
                     (execution.max_concurrent_subagents). Wait for one to finish (TaskWait), \
                     or do this part yourself.",
                    self.subagent_slots.cap
                )
            })?;
        Ok(Admission {
            depth,
            _slot: Some(slot),
        })
    }

    /// [`admit`](Self::admit) for a program's fan-out: at the cap it waits
    /// for a slot instead of refusing. Only a root session runs programs and
    /// a root session holds no slot, so what it waits on is never itself.
    async fn admit_waiting(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Admission, String> {
        let depth = self.spawn_guard()?;
        let slot = tokio::select! {
            slot = self.subagent_slots.slots.clone().acquire_owned() => {
                slot.map_err(|_| "the session's sub-agent slots are closed".to_string())?
            }
            _ = cancel.cancelled() => {
                return Err("stopped while waiting for a sub-agent slot".into());
            }
        };
        Ok(Admission {
            depth,
            _slot: Some(slot),
        })
    }

    /// TeamCreate members are bounded by `team_stage_concurrency` instead.
    fn admit_team_member(&self) -> Result<Admission, String> {
        Ok(Admission {
            depth: self.spawn_guard()?,
            _slot: None,
        })
    }

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
        // A policy that stops at the first delegation is not a policy.
        for plugin in &self.delegate_plugins {
            builder = builder.plugin(plugin.clone());
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
        if let Some(ledger) = &self.process_groups {
            builder = builder.process_groups(ledger.clone());
        }
        // One runner for the whole tree: `once` holds across it, and the
        // root's file watcher is the only one.
        if let Some(runner) = self.hooks() {
            builder = builder.hooks(runner);
        }
        builder.modules(self.modules.iter().cloned())
    }

    fn hooks(&self) -> Option<Arc<hooks::HookRunner>> {
        self.hooks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }

    fn session_ended(&self) -> tokio_util::sync::CancellationToken {
        self.session_ended
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn fire_subagent_hook(&self, event: hooks::HookEvent, sub: &SubagentPayload) {
        if let Some(runner) = self.hooks() {
            sub.fire(&runner, event).await;
        }
    }

    /// The model instance a sub-agent spawn should use. Every spawn point is
    /// the same task type — `task_models` in settings.json is keyed by a
    /// small fixed taxonomy (`main`/`subagent`/`compact`/`memory`/
    /// `web_search`), not by `subagent_type` (which agent *kind* — "general-purpose",
    /// "code-reviewer", etc. — a different axis entirely), so the lookup
    /// key here is always the literal `"subagent"` task type regardless of
    /// which `subagent_type` was requested.
    fn model_for_subagent(&self) -> Arc<dyn Model> {
        match &self.task_router {
            Some(router) => router.model_for("subagent"),
            None => self.model.clone(),
        }
    }

    /// Look up one agent type by name, cloning it out from behind the
    /// `agent_types` lock. Always returns an owned value (never a borrow
    /// tied to the read guard) — `agent_types` can be swapped in place by
    /// the watcher, and a borrow into the replaced map would
    /// dangle the instant a reload swaps it out. The clone itself is cheap:
    /// a handful of strings and a short tool-name list, not a hot-path
    /// concern at sub-agent-spawn frequency.
    fn agent_type_def(&self, name: &str) -> Option<AgentTypeDefinition> {
        let shared = self
            .agent_types
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned();
        match (shared, self.session_agent_types.get(name)) {
            (Some(on_disk), _) if on_disk.source == AgentTypeSource::LocalFile => Some(on_disk),
            (_, Some(declared)) => Some(declared.clone()),
            (shared, None) => shared,
        }
    }

    fn skill_manager(&self) -> Option<Arc<dyn base::interface::skill_catalog::SkillCatalog>> {
        self.skill_manager
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn mcp_tool_adapters(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        self.mcp_tool_adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn event_tx(&self) -> Option<runtime::agent::EventSender> {
        self.event_tx
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn history_store(&self) -> Option<Arc<dyn HistoryStore>> {
        self.history_store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn instruction_file(&self) -> Option<std::path::PathBuf> {
        self.instruction_file
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn parent_session_id(&self) -> Option<String> {
        self.parent_session_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn telemetry_handle(&self) -> Option<TelemetryHandle> {
        self.telemetry_handle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// See `AgentTool::permission_handler` — same resolution, reachable from
    /// the static `run_sub_inner` path (background spawns).
    fn permission_handler(&self) -> Arc<dyn Permission> {
        self.permission_handler_for(None)
    }

    /// `permission_handler` for a sub-agent running as `task`, which shows as
    /// waiting while a question of the sub-agent's waits on the host.
    fn permission_handler_for(
        &self,
        task: Option<Arc<base::context::RunningTask>>,
    ) -> Arc<dyn Permission> {
        self.permission_handler_asking(task, true)
    }

    /// `permission_handler_for`, where `can_ask: false` refuses what the
    /// parent's rules leave to a question instead of taking it to the host.
    fn permission_handler_asking(
        &self,
        task: Option<Arc<base::context::RunningTask>>,
        can_ask: bool,
    ) -> Arc<dyn Permission> {
        if let Some(parent) = self
            .parent_permission
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Arc::new(ParentPermission {
                inner: if can_ask {
                    parent
                } else {
                    Arc::new(NoPrompt(parent))
                },
                event_tx: self.event_tx(),
                pending: self
                    .parent_pending_permissions
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                timeout_secs: self.config.permission_prompt_timeout_secs,
                own_tools: Default::default(),
                task,
                session: Default::default(),
            });
        }
        Arc::new(AlwaysPermit)
    }

    /// Build a sub-agent's `Settings`, inheriting the parent's when known.
    /// Shared by the `&self` (`run_sub`) and static (`run_sub_inner`,
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

    /// The parent's tools a delegate starts from: all of them but the ones
    /// the parent's scene contributed. Those belong to the scene, and the
    /// delegate's own scene contributes its tools again when it is built —
    /// inheriting them as well would register each twice.
    fn inherited_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        let parent_scene = self
            .parent_scene
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let scene_tools: std::collections::HashSet<String> = parent_scene
            .map(|scene| {
                scene
                    .extra_tools()
                    .iter()
                    .map(|t| t.name().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let mut out: Vec<Arc<dyn base::tool::Tool>> = Vec::new();
        for tool in self
            .parent_tools
            .all()
            .into_iter()
            .chain(self.fallback_tools.all())
        {
            if !scene_tools.contains(tool.name())
                && !self.plugin_tools.contains(tool.name())
                && !out.iter().any(|t| t.name() == tool.name())
            {
                out.push(tool);
            }
        }
        out
    }

    /// The scene a sub-agent should run in.
    ///
    /// Precedence: the agent type's own `scene:` override > the parent's
    /// scene (wired via `AgentTool::set_scene`) > the scene named by
    /// `Settings.paths.scope` > `CodeScene`. The last two steps mean a
    /// daemon session inherits correctly even without explicit wiring: the
    /// daemon sets `scope` to `scene.id()`.
    fn scene_for_subagent(
        &self,
        def: Option<&AgentTypeDefinition>,
        settings: &Settings,
    ) -> Arc<dyn Scene> {
        let parent = self
            .parent_scene
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(name) = def.and_then(|d| d.scene.as_deref()) {
            match self.resolve_scene_id(name) {
                Some(requested) => {
                    // An agent type may *narrow* the scene, never widen it.
                    //
                    // Agent types are loaded from `.agix/agents/*.md`, which
                    // is repository content — on a cloned repo it is
                    // attacker-supplied. A scene's `tools()` whitelist is the
                    // enforcement point for "this session may not run shell
                    // commands" (the `harnix` and `mini` scenes rely on it), so
                    // letting a file in the repo pick the scene meant a narrow
                    // session could
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
            .unwrap_or_else(|| Arc::new(scene::code::CodeScene::default()))
    }

    /// A scene id resolved against the host's registry when one was wired,
    /// otherwise against the built-in set. Plugin scenes (`plugin:<name>`)
    /// only exist in the registry, so without this an agent type naming one
    /// would silently fall through to "unrecognized scene" and inherit the
    /// parent's instead.
    fn resolve_scene_id(&self, id: &str) -> Option<Arc<dyn Scene>> {
        if let Some(registry) = self
            .scene_registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
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
/// So: a parent with an empty allow-list (CodeScene) permits any child. A
/// parent with a real allow-list permits only a child whose own allow-list is
/// non-empty and contained in it, minus anything the parent disallows.
fn scene_is_narrower_or_equal(candidate: &dyn Scene, parent: &dyn Scene) -> bool {
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

/// The `agent_spawned` / `agent_completed` pair for one sub-agent run, in
/// the telemetry of the session that spawned it. The sub-agent's own events
/// go to the same handle under its own session id, which is `agent_id`.
struct SpawnTelemetry {
    handle: TelemetryHandle,
    parent_session_id: String,
    parent_turn: u32,
    agent_id: String,
    role: String,
    started: std::time::Instant,
}

impl SpawnTelemetry {
    fn start(
        inner: &Inner,
        parent: Option<(&str, u32, &str)>,
        agent_session_id: &str,
        agent_type: Option<&str>,
        model: &str,
        background: bool,
    ) -> Option<Self> {
        let handle = inner.telemetry_handle()?;
        let (parent_session_id, parent_turn) = match parent {
            Some((sid, turn, _)) => (sid.to_string(), turn),
            None => (inner.parent_session_id().unwrap_or_default(), 0),
        };
        let role = agent_type.unwrap_or("default").to_string();
        let _ = handle.record(telemetry::TelemetryEvent::agent_spawned(
            &parent_session_id,
            parent_turn,
            None,
            telemetry::AgentSpawnedPayload {
                agent_id: agent_session_id.to_string(),
                role: role.clone(),
                model: (!model.is_empty()).then(|| model.to_string()),
                stage_name: None,
                background,
            },
        ));
        Some(Self {
            handle,
            parent_session_id,
            parent_turn,
            agent_id: agent_session_id.to_string(),
            role,
            started: std::time::Instant::now(),
        })
    }

    fn finish(self, had_error: bool) {
        let _ = self
            .handle
            .record(telemetry::TelemetryEvent::agent_completed(
                &self.parent_session_id,
                self.parent_turn,
                None,
                telemetry::AgentCompletedPayload {
                    agent_id: self.agent_id,
                    role: self.role,
                    had_error,
                    duration_ms: self.started.elapsed().as_millis() as u64,
                },
            ));
    }
}

/// What a sub-agent's `SubagentStart` / `SubagentStop` payload says about it.
#[derive(Clone)]
struct SubagentPayload {
    session_id: String,
    cwd: std::path::PathBuf,
    permission_mode: base::interface::settings::PermissionMode,
    agent_type: Option<String>,
    stop_reason: Option<String>,
}

impl SubagentPayload {
    fn of(
        session_id: &str,
        cwd: &std::path::Path,
        settings: &Settings,
        agent_type: Option<&str>,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            cwd: cwd.to_path_buf(),
            permission_mode: settings.permission_mode,
            agent_type: agent_type.map(str::to_string),
            stop_reason: None,
        }
    }

    fn stopped(mut self, reason: &str) -> Self {
        self.stop_reason = Some(reason.to_string());
        self
    }

    /// Fire one sub-agent lifecycle hook about this sub-agent. Notification
    /// only — the response is discarded, so a hook cannot veto a spawn or a
    /// completion.
    async fn fire(&self, runner: &hooks::HookRunner, event: hooks::HookEvent) {
        if !runner.has_hooks_for(event) {
            return;
        }
        let mut input = hooks::HookInput::lifecycle(
            format!("{event:?}"),
            self.session_id.clone(),
            self.cwd.display().to_string(),
            serde_json::to_value(self.permission_mode)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "default".into()),
        )
        .with_agent_type(self.agent_type.clone());
        if let (hooks::HookEvent::SubagentStop, Some(reason)) = (event, &self.stop_reason) {
            input = input.with_stop_reason(reason.clone());
        }
        let _ = runner.run(event, &input).await;
    }
}

/// How a one-shot sub-agent's run ended, as `SubagentStop` reports it.
fn stop_reason_of(
    result: &Result<String, base::error::ToolError>,
    cancelled: bool,
) -> &'static str {
    match (result, cancelled) {
        (_, true) => "cancelled",
        (Ok(_), _) => "end_turn",
        (Err(_), _) => "error",
    }
}

/// Everything needed to re-emit one sub-agent's events on the parent's
/// channel. Built once per spawn; cloned into the collector task.
#[derive(Clone)]
struct SubagentTag {
    parent_tx: runtime::agent::EventSender,
    agent_label: String,
    agent_session_id: String,
    agent_type: Option<String>,
    parent_session_id: String,
    parent_turn: u32,
    parent_tool_use_id: Option<String>,
    task_id: Option<String>,
}

impl SubagentTag {
    /// Build a tag, or `None` when there's no parent channel to forward to
    /// (in which case the collector degrades to pure text accumulation).
    fn new(
        inner: &Inner,
        agent_session_id: &str,
        agent_type: Option<&str>,
        parent: Option<(&str, u32, &str)>,
    ) -> Option<Self> {
        let parent_tx = inner.event_tx()?;
        let (parent_session_id, parent_turn, parent_tool_use_id) = parent.unwrap_or(("", 0, ""));
        Some(Self {
            parent_tx,
            agent_label: subagent_label(agent_type, agent_session_id),
            agent_session_id: agent_session_id.to_string(),
            agent_type: agent_type.map(str::to_string),
            parent_session_id: parent_session_id.to_string(),
            parent_turn,
            parent_tool_use_id: (!parent_tool_use_id.is_empty())
                .then(|| parent_tool_use_id.to_string()),
            task_id: None,
        })
    }

    fn for_task(mut self, task: Option<&Arc<base::context::RunningTask>>) -> Self {
        self.task_id = task.map(|t| t.task_id.clone());
        self
    }

    fn forward(&self, event: AgentEvent) {
        let _ = self.parent_tx.send(AgentEvent::SubagentProgress {
            agent_label: self.agent_label.clone(),
            agent_session_id: self.agent_session_id.clone(),
            agent_type: self.agent_type.clone(),
            parent_session_id: self.parent_session_id.clone(),
            parent_turn: self.parent_turn,
            parent_tool_use_id: self.parent_tool_use_id.clone(),
            task_id: self.task_id.clone(),
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
fn preload_skills_text(
    mgr: &dyn base::interface::skill_catalog::SkillCatalog,
    skill_names: &[String],
) -> String {
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
        let Some(content) = mgr.body(name) else {
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
    /// Construct with only the built-in agent types (no `.agix/agents/`
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
    /// for `.agix/agents/*.md` custom type definitions, on top of the 6
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
        let _agent_type_watcher = crate::type_watcher::AgentTypeWatcher::watch(
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
                subagent_slots: SubagentSlots::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS),
                agent_types,
                _agent_type_watcher,
                description,
                parent_settings: None,
                task_router: None,
                delegate_plugins: Vec::new(),
                plugin_tools: Default::default(),
                session_agent_types: Default::default(),
                process_groups: None,
                modules: Vec::new(),
                skill_manager: Arc::new(std::sync::RwLock::new(None)),
                mcp_tool_adapters: Arc::new(std::sync::RwLock::new(Vec::new())),
                team_registry: Arc::new(std::sync::RwLock::new(None)),
                team_members: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                idle_seq_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                team_spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
                event_tx: Arc::new(std::sync::RwLock::new(None)),
                hooks: Arc::new(std::sync::RwLock::new(None)),
                session_ended: Default::default(),
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
    /// `with_parent_tools` has, but shared across sessions rather than reset
    /// per session.
    pub fn with_shared_agent_types(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        fallback_tools: Arc<InMemoryToolRegistry>,
        shared_agent_types: SharedAgentTypeMap,
    ) -> Self {
        let description: Arc<str> = {
            let snapshot = shared_agent_types
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            describe_agent_types(&snapshot).into()
        };
        Self {
            inner: Arc::new(Inner {
                model,
                config,
                fallback_tools,
                parent_tools,
                depth: 0,
                subagent_slots: SubagentSlots::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS),
                agent_types: shared_agent_types,
                _agent_type_watcher: None,
                description,
                parent_settings: None,
                task_router: None,
                delegate_plugins: Vec::new(),
                plugin_tools: Default::default(),
                session_agent_types: Default::default(),
                process_groups: None,
                modules: Vec::new(),
                skill_manager: Arc::new(std::sync::RwLock::new(None)),
                mcp_tool_adapters: Arc::new(std::sync::RwLock::new(Vec::new())),
                team_registry: Arc::new(std::sync::RwLock::new(None)),
                team_members: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                idle_seq_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                team_spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
                event_tx: Arc::new(std::sync::RwLock::new(None)),
                hooks: Arc::new(std::sync::RwLock::new(None)),
                session_ended: Default::default(),
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
    pub fn with_modules(mut self, modules: Vec<Arc<dyn runtime::module::Module>>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.modules = modules;
        self.inner = Arc::new(inner);
        self
    }

    pub fn with_subagent_slots(mut self, slots: SubagentSlots) -> Self {
        let mut inner = (*self.inner).clone();
        inner.subagent_slots = slots;
        self.inner = Arc::new(inner);
        self
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        let mut inner = (*self.inner).clone();
        inner.depth = depth;
        self.inner = Arc::new(inner);
        self
    }

    /// The plugins every agent this tool spawns is built with, and the tools
    /// they registered in this session.
    pub fn with_delegate_plugins(
        mut self,
        plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
        their_tools: &[String],
    ) -> Self {
        let mut inner = (*self.inner).clone();
        inner.delegate_plugins = plugins;
        inner.plugin_tools = Arc::new(their_tools.iter().cloned().collect());
        self.inner = Arc::new(inner);
        self
    }

    /// Agent types this session's plugins declare, on top of the catalog this
    /// tool was built with. Only the types loaded from disk outrank them.
    pub fn with_session_agent_types(mut self, types: &[AgentTypeDefinition]) -> Self {
        if types.is_empty() {
            return self;
        }
        let mut inner = (*self.inner).clone();
        inner.session_agent_types =
            Arc::new(types.iter().map(|d| (d.name.clone(), d.clone())).collect());
        let mut visible = inner
            .agent_types
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .clone();
        for (name, definition) in inner.session_agent_types.iter() {
            if visible
                .get(name)
                .map_or(true, |d| d.source != AgentTypeSource::LocalFile)
            {
                visible.insert(name.clone(), definition.clone());
            }
        }
        inner.description = describe_agent_types(&visible).into();
        self.inner = Arc::new(inner);
        self
    }

    pub fn with_process_groups(
        mut self,
        ledger: Arc<base::interface::exec::local::GroupLedger>,
    ) -> Self {
        let mut inner = (*self.inner).clone();
        inner.process_groups = Some(ledger);
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

    /// Attach the session's skill catalog so `AgentTypeDefinition.skills`
    /// (preload) can resolve.
    pub fn set_skill_catalog(&self, mgr: Arc<dyn base::interface::skill_catalog::SkillCatalog>) {
        *self
            .inner
            .skill_manager
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(mgr);
    }

    /// Attach a snapshot of the parent session's connected MCP tools — same
    /// interior-mutability, set-after-construction pattern as
    /// `set_skill_catalog`. Resolves `AgentTypeDefinition.mcp_servers` when
    /// a subagent is spawned. Pass `mcp_manager.tool_adapters().to_vec()`;
    /// see `Inner::mcp_tool_adapters`'s doc comment for why this takes a
    /// snapshot `Vec` rather than the live `McpManager`.
    pub fn set_mcp_tool_adapters(&self, adapters: Vec<Arc<dyn base::tool::Tool>>) {
        *self
            .inner
            .mcp_tool_adapters
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = adapters;
    }

    /// Attach the same shared `TeamRegistry` `AgentsModule::on_session` hands
    /// to `TeamCreate`/`TeamList`/`TeamDelete`, so persistent team members
    /// spawned through this `AgentTool` (see `spawn_or_message_team_member`)
    /// update state those tools can see. Same interior-mutability,
    /// set-after-construction pattern as `set_skill_catalog`/
    /// `set_mcp_tool_adapters` — only called when `settings.execution.
    /// team_enabled` is true and the scene supports teams (see
    /// `AgentsModule::on_session` in `agents/src/lib.rs`).
    pub fn set_team_registry(&self, registry: Arc<team::registry::TeamRegistry>) {
        *self
            .inner
            .team_registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(registry);
    }

    /// Attach the parent session's event channel so sub-agent activity is
    /// mirrored onto it — see `Inner::event_tx`. Set-after-construction
    /// (`&self`, not the `with_X(self) -> Self` builder form) because
    /// `Builder::build()` creates this `AgentTool` before it creates the
    /// session's event channel.
    pub fn set_event_sender(&self, tx: runtime::agent::EventSender) {
        *self
            .inner
            .event_tx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    }

    /// Attach the parent session's hook runner: every delegate runs its hooks
    /// on it, and sub-agent spawns fire `SubagentStart`/`SubagentStop` there
    /// — see `Inner::hooks`.
    pub fn set_hooks(&self, runner: Arc<hooks::HookRunner>) {
        *self
            .inner
            .hooks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(&runner));
    }

    /// The token the parent session cancels when it ends: persistent team
    /// members stop on it, mid-turn or idle.
    pub fn set_session_ended(&self, ended: tokio_util::sync::CancellationToken) {
        *self
            .inner
            .session_ended
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ended;
    }

    /// Attach the parent session's history store so sub-agent transcripts are
    /// persisted — see `Inner::history_store`.
    pub fn set_history_store(&self, store: Arc<dyn HistoryStore>) {
        *self
            .inner
            .history_store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(store);
    }

    /// Record the parent `Agent`'s own session id, so sub-agents spawned
    /// from here can stamp `parent_session_id` on their own `Meta` line —
    /// see `Inner::parent_session_id`.
    pub fn set_parent_session_id(&self, id: String) {
        *self
            .inner
            .parent_session_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id);
    }

    /// Record the parent `Agent`'s own telemetry handle, so sub-agents
    /// spawned from here inherit it instead of getting `Builder::build()`'s
    /// noop default — see `Inner::telemetry_handle`.
    pub fn set_telemetry_handle(&self, handle: TelemetryHandle) {
        *self
            .inner
            .telemetry_handle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    /// Route sub-agent permission checks to the parent agent's `Permission`
    /// impl — see `Inner::parent_permission`.
    pub fn set_parent_permission(&self, permission: Arc<dyn Permission>) {
        *self
            .inner
            .parent_permission
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(permission);
    }

    /// Route sub-agent permission *prompts* to the parent session's host
    /// — see `Inner::parent_pending_permissions`. Without this a
    /// sub-agent still inherits the parent's rules, but anything those rules
    /// leave undecided is refused rather than asked about.
    pub fn set_parent_pending_permissions(
        &self,
        pending: Arc<
            std::sync::Mutex<
                std::collections::HashMap<
                    String,
                    tokio::sync::oneshot::Sender<runtime::agent::PermissionDecision>,
                >,
            >,
        >,
    ) {
        *self
            .inner
            .parent_pending_permissions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pending);
    }

    /// Let sub-agents inherit the parent's scene — see
    /// `Inner::parent_scene`.
    pub fn set_scene(&self, scene: Arc<dyn Scene>) {
        *self
            .inner
            .parent_scene
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(scene);
    }

    /// Let an agent type's `scene:` id resolve against every scene the host
    /// registered, not just the built-in four — see `Inner::scene_registry`.
    pub fn set_scene_registry(&self, registry: Arc<scene::SceneRegistry>) {
        *self
            .inner
            .scene_registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(registry);
    }

    /// Let sub-agents inherit an instruction file that the parent got from
    /// `Builder::instruction_file(..)` rather than from settings.json
    /// — see `settings_from_parent`.
    pub fn set_instruction_file(&self, path: Option<std::path::PathBuf>) {
        *self
            .inner
            .instruction_file
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
    }

    /// Returns the fallback tool registry for sub-agent creation.
    /// The parent's tools a delegate inherits, narrowed to `allowed`; empty
    /// allows them all.
    pub(crate) fn sub_tools(&self, allowed: &[String]) -> Arc<InMemoryToolRegistry> {
        let registry = InMemoryToolRegistry::new();
        for tool in self.inner.inherited_tools() {
            if lists_admit_tool(allowed, &[], tool.name()) {
                registry.register(tool);
            }
        }
        Arc::new(registry)
    }

    /// Returns a permission handler for sub-agent creation.
    pub(crate) fn sub_permission(&self) -> Arc<dyn Permission> {
        self.permission_handler()
    }

    /// A team member's permission: the parent's, judged in the member's own
    /// mode, as any sub-agent's is — the parent's deny rules and its host's
    /// answers reach the member. With no parent wired (an `AgentTool` built
    /// on its own) there are no rules to inherit, and the member is judged by
    /// its mode alone.
    pub(crate) fn team_member_permission(
        &self,
        tools: &Arc<InMemoryToolRegistry>,
        mode: base::interface::settings::PermissionMode,
    ) -> Arc<dyn Permission> {
        let wired = self
            .inner
            .parent_permission
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        if wired {
            return self.permission_handler();
        }
        Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
            Arc::new(permissions::gate::PermissionGate::empty()),
            tools.clone(),
            mode.into(),
        ))
    }

    /// System prompt injected into the sub-agent's context for the given
    /// `subagent_type`, looked up from the merged built-in + disk catalog.
    fn type_prompt(&self, t: Option<&str>) -> Option<String> {
        t.and_then(|name| self.inner.agent_type_def(name))
            .map(|d| d.system_prompt)
    }

    fn build_prompt(&self, subagent_type: Option<&str>, prompt: &str) -> String {
        if let Some(p) = self.type_prompt(subagent_type) {
            format!("{p}\n\nTask: {prompt}")
        } else {
            prompt.to_string()
        }
    }

    /// Does `tool_name` match an `allowed_tools`/`disallowed_tools` entry?
    /// Exact match, or an MCP server-level pattern: `mcp__<server>` or
    /// `mcp__<server>__*` matches every tool from that server (Agix's
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
        // `disallowed_tools` and `allowed_tools` together; a tool listed in
        // both ends up removed either way.
        let registry = InMemoryToolRegistry::new();
        for tool in self.inner.inherited_tools() {
            if tool.name() != AGENT_TOOL_NAME && type_admits_tool(def.as_ref(), tool.name()) {
                registry.register(tool);
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
    /// 1. the parent agent's own `Permission` impl, when wired via
    ///    [`AgentTool::set_parent_permission`] — so a session using
    ///    `RuleSetPermission` (or any other real handler) can't have its rules
    ///    bypassed by spawning a sub-agent;
    /// 2. [`AlwaysPermit`], for `AgentTool`s built
    ///    without any wiring (tests, embedders).
    pub(crate) fn permission_handler(&self) -> Arc<dyn Permission> {
        self.inner.permission_handler()
    }

    /// Core: run sub-agent and collect text output.
    ///
    /// `subagent_type` — when it names a type in the merged catalog (built-in
    /// or `.agix/agents/*.md`) that declares a `model` override, that model
    /// is used instead of the parent's. `None` (or an unknown/type-less
    /// call, e.g. from the generic `AgentSpawner` bridge used by team/skill-fork)
    /// keeps the parent's model.
    pub(crate) async fn run_sub(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
    ) -> Result<String, base::error::ToolError> {
        self.run_sub_tagged(
            prompt,
            tools,
            cwd,
            cancel,
            perm,
            subagent_type,
            None,
            SubRun::default(),
        )
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
        parent: Option<(&str, u32, &str)>,
        run: SubRun,
    ) -> Result<String, base::error::ToolError> {
        let admission = self.inner.admit().map_err(base::error::ToolError::Denied)?;
        self.run_sub_admitted(
            admission,
            prompt,
            tools,
            cwd,
            cancel,
            perm,
            subagent_type,
            parent,
            run,
        )
        .await
    }

    /// `run_sub` without an agent type, on `model`.
    pub(crate) async fn run_sub_on_model(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        model: String,
    ) -> Result<String, base::error::ToolError> {
        self.run_sub_tagged(
            prompt,
            tools,
            cwd,
            cancel,
            perm,
            None,
            None,
            SubRun {
                model: Some(model),
                ..SubRun::default()
            },
        )
        .await
    }

    /// `run_sub` for a `TeamCreate` member, in the team's `permission_mode`
    /// held to the parent's.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_team_member(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
        permission_mode: Option<base::interface::settings::PermissionMode>,
        seat: Option<base::interface::agent_spawner::TeamSeat>,
    ) -> Result<String, base::error::ToolError> {
        let admission = self
            .inner
            .admit_team_member()
            .map_err(base::error::ToolError::Denied)?;
        self.run_sub_admitted(
            admission,
            prompt,
            tools,
            cwd,
            cancel,
            perm,
            subagent_type,
            None,
            SubRun {
                permission_mode,
                team_member: true,
                seat,
                ..SubRun::default()
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_sub_admitted(
        &self,
        admission: Admission,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
        parent: Option<(&str, u32, &str)>,
        run: SubRun,
    ) -> Result<String, base::error::ToolError> {
        let Admission {
            depth: child_depth,
            _slot,
        } = admission;
        let cwd_for_meta = cwd.clone();
        let def = subagent_type.and_then(|t| self.inner.agent_type_def(t));
        let model_override = run
            .model
            .clone()
            .or_else(|| def.as_ref().and_then(|d| d.model.clone()));
        let mut sub_settings = self.sub_settings(model_override.as_deref(), cwd);
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings, d);
        }
        if let Some(mode) = run.permission_mode {
            sub_settings.permission_mode = mode.min_permissive(sub_settings.permission_mode);
        }
        let scene = self.inner.scene_for_subagent(def.as_ref(), &sub_settings);
        let scene_id = scene.id().to_string();
        let settings = Arc::new(sub_settings);

        let prompt = match (&def, self.inner.skill_manager()) {
            (Some(d), Some(mgr)) if !d.skills.is_empty() => {
                format!("{}{prompt}", preload_skills_text(mgr.as_ref(), &d.skills))
            }
            _ => prompt,
        };

        let sid = run
            .session_id
            .clone()
            .unwrap_or_else(|| base::id::Id::new().to_string());
        let tag = SubagentTag::new(&self.inner, &sid, subagent_type, parent)
            .map(|tag| tag.for_task(run.task.as_ref()));
        let sid_for_hook = sid.clone();
        let sid_for_meta = sid.clone();
        // Resolved once and shared by the sub-agent's own lineage and its `Meta`
        // line — two answers to "who spawned this" that must not be able to
        // disagree.
        let parent_session_id = parent
            .map(|(sid, _, _)| sid.to_string())
            .or_else(|| self.inner.parent_session_id());
        let mut builder = Builder::new()
            .session_id(sid)
            .lineage(parent_session_id.clone(), subagent_type)
            .scene(scene)
            .contributed_tool_filter(contributed_tool_filter(def.as_ref(), &run))
            .model(self.inner.model_for_subagent())
            .tools(tools)
            .settings(settings.clone())
            .agent_depth(child_depth)
            .permission(run.permission(perm));
        if let Some(schema) = &run.output_schema {
            builder = builder.output_schema(schema.clone());
        }
        if let Some(seat) = run.seat.clone() {
            builder = builder.team_seat(seat);
        }
        let history_store = self.inner.history_store();
        builder = self.inner.inherited_by_a_delegate(builder);
        let (agent, event_rx, input_tx) = builder
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
        let payload = SubagentPayload::of(&sid_for_hook, &cwd_for_meta, &settings, subagent_type);
        self.inner
            .fire_subagent_hook(hooks::HookEvent::SubagentStart, &payload)
            .await;
        let spawned = (!run.team_member).then(|| {
            SpawnTelemetry::start(
                &self.inner,
                parent,
                &sid_for_hook,
                subagent_type,
                &settings.model.model_name,
                false,
            )
        });
        let (result, cancelled) = run_one_turn_detached(
            &sid_for_hook,
            agent,
            event_rx,
            input_tx,
            prompt,
            cancel,
            tag,
            run.task.clone(),
        )
        .await;
        if let Some(spawned) = spawned.flatten() {
            spawned.finish(result.is_err());
        }
        self.inner
            .fire_subagent_hook(
                hooks::HookEvent::SubagentStop,
                &payload.stopped(stop_reason_of(&result, cancelled)),
            )
            .await;
        // Marked terminal on failure too, so a stale `session.resume` against
        // it is rejected instead of silently continuing a "finished" sub-agent.
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
    ) -> Result<String, base::error::ToolError> {
        let admission = self.inner.admit().map_err(base::error::ToolError::Denied)?;
        let tid = bg_task_id();
        let sid = base::id::Id::new().to_string();
        let task = session.register_task(
            tid.clone(),
            base::context::TaskType::Agent,
            cancel.clone(),
            serde_json::json!({
                "agent_type": subagent_type,
                "background": true,
                "agent_session_id": sid,
            }),
        );
        let subagent_type = subagent_type.map(str::to_string);
        let inner = self.inner.clone();
        let run = SubRun::for_task(&task, &sid);
        let session_c = session.clone();

        tokio::spawn(async move {
            let r = without_panics(async move {
                Self::run_sub_inner(
                    &inner,
                    prompt,
                    tools,
                    cwd,
                    cancel,
                    subagent_type.as_deref(),
                    run,
                    admission,
                )
                .await
            })
            .await;
            conclude_background_task(&session_c, &task, &r, true);
        });
        Ok(tid)
    }

    async fn launch_bg(
        &self,
        input: &AgentInput,
        ctx: &ToolContext,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let admission = self.inner.admit().map_err(base::error::ToolError::Denied)?;
        let tid = bg_task_id();
        let sid = base::id::Id::new().to_string();
        let outer_cancel = ctx.cancel.child_token();
        let task = ctx.session.register_task(
            tid.clone(),
            base::context::TaskType::Agent,
            outer_cancel.clone(),
            serde_json::json!({
                "agent_type": input.subagent_type,
                "description": input.description,
                "background": true,
                "tool_use_id": ctx.tool_use_id,
                "agent_session_id": sid,
            }),
        );
        let mut worktree_handle = match &input.worktree {
            Some(s) => match create_worktree(
                &ctx.exec,
                &tools::bash::engine_git(ctx),
                &ctx.session.cwd,
                s,
            )
            .await
            {
                Ok(h) => Some(h),
                Err(e) => {
                    ctx.session.finish_task(
                        &tid,
                        base::context::RunningStatus::Failed(format!("worktree: {e}")),
                    );
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
        let prompt = self.build_prompt(input.subagent_type.as_deref(), &input.prompt);
        let subagent_type = input.subagent_type.clone();
        let inner = self.inner.clone();
        let run = SubRun {
            model: input.model.clone(),
            output_schema: input.output_schema.clone(),
            ..SubRun::for_task(&task, &sid)
        };
        let session = ctx.session.clone();

        tokio::spawn(async move {
            let r = without_panics(async move {
                Self::run_sub_inner(
                    &inner,
                    prompt,
                    tools,
                    cwd,
                    outer_cancel,
                    subagent_type.as_deref(),
                    run,
                    admission,
                )
                .await
            })
            .await;
            if let Some(h) = worktree_handle.as_mut() {
                h.cleanup().await;
            }
            conclude_background_task(&session, &task, &r, true);
        });
        Ok(bg_result(&tid, "spawned"))
    }

    /// A sub-agent a host asked for (`session.startAgent`): in the
    /// background, as a task of `session`, under the same limits as the
    /// model's own `Agent` calls and never with more than they would get.
    /// It stops when `session_cancel` does.
    pub(crate) fn start_for_host(
        &self,
        request: base::interface::agent_spawner::AgentRequest,
        session: Arc<base::context::SessionState>,
        session_cancel: tokio_util::sync::CancellationToken,
    ) -> Result<
        base::interface::agent_spawner::StartedAgent,
        base::interface::agent_spawner::StartAgentError,
    > {
        use base::interface::agent_spawner::{StartAgentError, StartedAgent};
        let admission = self.inner.admit().map_err(StartAgentError::Limit)?;
        let tid = bg_task_id();
        let sid = base::id::Id::new().to_string();
        let cancel = session_cancel.child_token();
        let task = session.register_task(
            tid.clone(),
            base::context::TaskType::Agent,
            cancel.clone(),
            serde_json::json!({
                "agent_type": request.subagent_type,
                "description": request.description,
                "background": true,
                "agent_session_id": sid,
                "tag": request.tag,
            }),
        );
        let tools = InMemoryToolRegistry::new();
        for tool in self.resolve_tools(request.subagent_type.as_deref()).all() {
            if lists_admit_tool(
                &request.allowed_tools,
                &request.disallowed_tools,
                tool.name(),
            ) {
                tools.register(tool);
            }
        }
        let prompt = self.build_prompt(request.subagent_type.as_deref(), &request.prompt);
        let run = SubRun {
            model: request.model,
            allowed_tools: request.allowed_tools,
            disallowed_tools: request.disallowed_tools,
            deny_rules: request
                .permission_rules
                .into_iter()
                .filter(|r| r.behavior == base::permission::RuleBehavior::Deny)
                .collect(),
            output_schema: request.output_schema,
            ..SubRun::for_task(&task, &sid)
        };
        let inner = self.inner.clone();
        let subagent_type = request.subagent_type;
        let notify_parent = request.notify_parent;
        let cwd = session.cwd.clone();
        tokio::spawn(async move {
            let r = without_panics(async move {
                Self::run_sub_inner(
                    &inner,
                    prompt,
                    Arc::new(tools),
                    cwd,
                    cancel,
                    subagent_type.as_deref(),
                    run,
                    admission,
                )
                .await
            })
            .await;
            conclude_background_task(&session, &task, &r, notify_parent);
        });
        Ok(StartedAgent {
            task_id: tid,
            agent_session_id: sid,
        })
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
    /// `spawn_or_message_team_member`, once, not here), held to the parent's
    /// mode; see [`Self::team_member_permission`] for its rules.
    async fn build_team_member_agent(
        &self,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        subagent_type: Option<&str>,
        permission_mode: base::interface::settings::PermissionMode,
        seat: Option<base::interface::agent_spawner::TeamSeat>,
    ) -> Result<(Agent, EventReceiver, InputSender, SubagentPayload), base::error::ToolError> {
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
        sub_settings.permission_mode = permission_mode.min_permissive(sub_settings.permission_mode);
        let scene = self.inner.scene_for_subagent(def.as_ref(), &sub_settings);
        let scene_id = scene.id().to_string();
        let perm = self.team_member_permission(&tools, sub_settings.permission_mode);
        let settings = Arc::new(sub_settings);
        let sid = base::id::Id::new().to_string();
        let sid_for_meta = sid.clone();
        let mut builder = Builder::new()
            .session_id(sid)
            .scene(scene)
            .contributed_tool_filter(contributed_tool_filter(def.as_ref(), &SubRun::default()))
            .model(self.inner.model_for_subagent())
            .tools(tools)
            .settings(settings.clone())
            .agent_depth(child_depth)
            .permission(perm);
        if let Some(seat) = seat {
            builder = builder.team_seat(seat);
        }
        // The session's plugins follow the delegate, the operator's policy
        // scripts among them. Without this, spawning is a way around every
        // rule they wrote, chosen by the model.
        let history_store = self.inner.history_store();
        builder = self.inner.inherited_by_a_delegate(builder);
        let (agent, event_rx, input_tx) = builder
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
        let payload = SubagentPayload::of(&sid_for_meta, &cwd_for_meta, &settings, subagent_type);
        Ok((agent, event_rx, input_tx, payload))
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
    /// it was built with, it can't be changed mid-lifetime.
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
        let register = |member_cancel: &tokio_util::sync::CancellationToken| {
            session.register_task(
                tid.clone(),
                base::context::TaskType::Teammate,
                member_cancel.child_token(),
                serde_json::json!({"team": team_name, "member": member_name}),
            )
        };

        let existing = self
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|m| (m.message_tx.clone(), m.cancel.clone()));
        let existing_entry_is_stale = existing.is_some();

        if let Some((tx, member_cancel)) = existing.filter(|(tx, _)| !tx.is_closed()) {
            let task = register(&member_cancel);
            if tx
                .send(PersistentMessage {
                    prompt: prompt.clone(),
                    task,
                })
                .is_ok()
            {
                return Ok(tid);
            }
        }
        if existing_entry_is_stale {
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
        let max_total = self.inner.config.team_max_persistent_members.max(1);
        let alive = self
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        if alive >= max_total {
            return Err(base::error::ToolError::Denied(format!(
                "team member limit reached: {alive} persistent members are busy, the most \
                 this session keeps (execution.team_max_persistent_members). Message one of \
                 them, wait for one to go idle, or stop one with TaskStop or TeamDelete."
            )));
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
        let (agent, event_rx, input_tx, payload) = self
            .build_team_member_agent(
                tools,
                cwd,
                subagent_type.as_deref(),
                resolved_permission_mode,
                Some(base::interface::agent_spawner::TeamSeat {
                    team: team_name.clone(),
                    member: member_name.clone(),
                }),
            )
            .await?;
        self.inner
            .fire_subagent_hook(hooks::HookEvent::SubagentStart, &payload)
            .await;
        let spawned = SpawnTelemetry::start(
            &self.inner,
            None,
            &payload.session_id,
            subagent_type.as_deref(),
            &agent.settings().model.model_name,
            true,
        );

        let member_id = format!("member-{}", bg_task_id());
        tracing::debug!(
            team = %team_name,
            member = %member_name,
            id = %member_id,
            "spawning new persistent team member"
        );
        let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel::<PersistentMessage>();
        let member_cancel = self.inner.session_ended().child_token();
        let task = register(&member_cancel);
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
            self.inner
                .hooks()
                .map(|runner| (Arc::downgrade(&runner), payload)),
            spawned,
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
    /// `team_max_persistent_members`, counting the member `protect` is about
    /// to add when it is not there yet (oldest-idle evicted first, just
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

            // A member about to be added needs a place of its own.
            let joining = protect.is_some_and(|k| !members.contains_key(k));
            let keep = if joining { max_total - 1 } else { max_total };
            let mut remaining = members.len();
            for (k, _, _) in &idle_entries {
                if remaining <= keep {
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
    #[allow(clippy::too_many_arguments)]
    async fn run_sub_inner(
        inner: &Inner,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
        run: SubRun,
        admission: Admission,
    ) -> Result<String, base::error::ToolError> {
        let Admission {
            depth: child_depth,
            _slot,
        } = admission;
        let perm: Arc<dyn Permission> =
            run.permission(inner.permission_handler_for(run.task.clone()));
        let cwd_for_meta = cwd.clone();
        let def = subagent_type.and_then(|t| inner.agent_type_def(t));
        let model_name = run
            .model
            .clone()
            .or_else(|| def.as_ref().and_then(|d| d.model.clone()));
        let mut sub_settings = Inner::sub_settings(inner, model_name.as_deref(), cwd);
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings, d);
        }
        let scene = inner.scene_for_subagent(def.as_ref(), &sub_settings);
        let scene_id = scene.id().to_string();
        let settings = Arc::new(sub_settings);

        let prompt = match (&def, inner.skill_manager()) {
            (Some(d), Some(mgr)) if !d.skills.is_empty() => {
                format!("{}{prompt}", preload_skills_text(mgr.as_ref(), &d.skills))
            }
            _ => prompt,
        };

        let sid = run
            .session_id
            .clone()
            .unwrap_or_else(|| base::id::Id::new().to_string());
        let sid_for_meta = sid.clone();
        let tag = SubagentTag::new(inner, &sid, subagent_type, None)
            .map(|tag| tag.for_task(run.task.as_ref()));
        let mut builder = Builder::new()
            .session_id(sid)
            .scene(scene)
            .contributed_tool_filter(contributed_tool_filter(def.as_ref(), &run))
            .model(inner.model_for_subagent())
            .tools(tools)
            .settings(settings.clone())
            .agent_depth(child_depth)
            .permission(perm);
        if let Some(schema) = &run.output_schema {
            builder = builder.output_schema(schema.clone());
        }
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

        let payload = SubagentPayload::of(&sid_for_meta, &cwd_for_meta, &settings, subagent_type);
        inner
            .fire_subagent_hook(hooks::HookEvent::SubagentStart, &payload)
            .await;
        open_inbox(run.task.as_deref(), &agent);
        let spawned = SpawnTelemetry::start(
            inner,
            None,
            &sid_for_meta,
            subagent_type,
            &settings.model.model_name,
            true,
        );
        let (result, cancelled) = run_one_turn(
            &mut agent,
            &mut event_rx,
            &input_tx,
            prompt,
            cancel,
            tag,
            run.task.as_deref(),
        )
        .await;
        if let Some(spawned) = spawned {
            spawned.finish(result.is_err());
        }
        drop(input_tx);
        inner
            .fire_subagent_hook(
                hooks::HookEvent::SubagentStop,
                &payload.stopped(stop_reason_of(&result, cancelled)),
            )
            .await;
        mark_sidechain_terminal(history_store.as_ref(), &sid_for_meta, &result, cancelled).await;
        result
    }
}

/// A sub-agent's `Permission`: the parent's rules, plus the parent's channel
/// to a human.
///
/// Delegating the *decision* to the parent is what stops a session from
/// having its rules bypassed by spawning a sub-agent. But an `Ask` verdict
/// then has nowhere to go — a sub-agent has no host attached to it, and a
/// prompt on the sub-agent's own channel would stall until cancellation.
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
/// that never call the setters), it refuses with an explanation rather than
/// blocking forever.
struct ParentPermission {
    inner: Arc<dyn Permission>,
    event_tx: Option<runtime::agent::EventSender>,
    pending: Option<runtime::agent::PendingPermissions>,
    /// How long a question waits for its answer; `0` waits indefinitely.
    timeout_secs: u64,
    /// The sub-agent's own tools. `inner` looks a tool up in the parent's,
    /// where a tool the sub-agent holds its own instance of — a
    /// `StructuredOutput` bound to its output schema — is a different one.
    own_tools: std::sync::RwLock<Option<Arc<dyn base::tool::ToolRegistry>>>,
    task: Option<Arc<base::context::RunningTask>>,
    /// The sub-agent's own session. When nobody attends it, a question is
    /// handed back for the turn to refuse, not sent up to wait for an answer.
    session: std::sync::RwLock<Option<Arc<base::context::SessionState>>>,
}

/// The parent's permission as a sub-agent a program started sees it: nobody
/// watches a program run, so a call that would need the user's approval is
/// refused rather than asked about.
struct NoPrompt(Arc<dyn Permission>);

fn refuse_a_question(
    tool_name: &str,
    outcome: base::interface::permission::PermissionOutcome,
) -> base::interface::permission::PermissionOutcome {
    use base::interface::permission::PermissionOutcome;
    match outcome {
        PermissionOutcome::Prompt { message, .. } => PermissionOutcome::Deny {
            reason: format!(
                "`{tool_name}` needs the user's approval, which a sub-agent started from a \
                 program cannot ask for. Leave this step out of the program and run it \
                 directly. Approval asked: {message}"
            ),
        },
        other => other,
    }
}

#[async_trait]
impl Permission for NoPrompt {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        refuse_a_question(
            tool_name,
            self.0.check(tool_name, tool_input, cwd, session_id).await,
        )
    }
    async fn check_for_delegate(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
        delegate: &Arc<base::context::SessionState>,
    ) -> base::interface::permission::PermissionOutcome {
        refuse_a_question(
            tool_name,
            self.0
                .check_for_delegate(tool_name, tool_input, cwd, session_id, delegate)
                .await,
        )
    }
    fn bind_tool_registry(&self, tools: Arc<dyn base::tool::ToolRegistry>) {
        self.0.bind_tool_registry(tools);
    }
    fn bind_session_state(&self, session: Arc<base::context::SessionState>) {
        self.0.bind_session_state(session);
    }
    fn add_temporary_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.0.add_temporary_allow(tool_name, rule_content);
    }
    fn clear_temporary_allows(&self) {
        self.0.clear_temporary_allows();
    }
    fn add_persistent_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.0.add_persistent_allow(tool_name, rule_content);
    }
}

/// Holds a task as waiting on one question until dropped — answered, timed
/// out, or the sub-agent stopped while it waited.
struct Waiting<'a> {
    task: Option<&'a base::context::RunningTask>,
    prompt_id: &'a str,
}

impl<'a> Waiting<'a> {
    fn on(task: Option<&'a base::context::RunningTask>, prompt_id: &'a str) -> Self {
        if let Some(task) = task {
            task.begin_waiting(prompt_id);
        }
        Self { task, prompt_id }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(task) = self.task {
            task.end_waiting(self.prompt_id);
        }
    }
}

/// What `mode` refuses on its own, for a parent handler that can only judge
/// in its own session's mode (the default `check_for_delegate`). A rule-set
/// parent has already refused all of this, and more precisely.
fn held_to_mode(
    outcome: base::interface::permission::PermissionOutcome,
    mode: base::permission::PermissionMode,
    tool: Option<&dyn base::tool::Tool>,
    input: &Value,
) -> base::interface::permission::PermissionOutcome {
    use base::interface::permission::PermissionOutcome;
    use base::permission::PermissionMode;
    match (mode, outcome) {
        (_, denied @ PermissionOutcome::Deny { .. }) => denied,
        (PermissionMode::Plan, _)
            if tool.is_some_and(|t| {
                !t.is_read_only(input) && !t.self_allow_overrides_mode() && !t.leaves_plan_mode()
            }) =>
        {
            PermissionOutcome::Deny {
                reason: "plan mode forbids non-readonly tools".into(),
            }
        }
        (PermissionMode::DontAsk, PermissionOutcome::Prompt { message, .. }) => {
            PermissionOutcome::Deny {
                reason: format!("dontAsk mode: {message}"),
            }
        }
        (_, outcome) => outcome,
    }
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
        self.decide(tool_name, tool_input, cwd, session_id, None)
            .await
    }

    async fn check_for_delegate(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
        delegate: &Arc<base::context::SessionState>,
    ) -> base::interface::permission::PermissionOutcome {
        self.decide(tool_name, tool_input, cwd, session_id, Some(delegate))
            .await
    }

    fn bind_tool_registry(&self, tools: Arc<dyn base::tool::ToolRegistry>) {
        *self
            .own_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tools);
    }

    fn bind_session_state(&self, session: Arc<base::context::SessionState>) {
        *self
            .session
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session);
    }
}

impl ParentPermission {
    /// The parent's verdict on a call of the sub-agent's — or, as `delegate`,
    /// of the sub-agent's own delegate — judged against the caller's session
    /// (its mode, what it has read), then asked of the host when it is a
    /// question.
    async fn decide(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
        delegate: Option<&Arc<base::context::SessionState>>,
    ) -> base::interface::permission::PermissionOutcome {
        use base::interface::permission::PermissionOutcome;
        let own = self
            .own_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|tools| tools.get(tool_name));
        let session = self
            .session
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let judged = delegate.cloned().or_else(|| session.clone());
        if let (Some(tool), None) = (&own, delegate) {
            let mut ctx = ToolContext::from_engine_ctx(
                cwd.to_path_buf(),
                tokio_util::sync::CancellationToken::new(),
            );
            if let Some(judged) = &judged {
                ctx.session = judged.clone();
            }
            if let base::tool::ValidationResult::Err(reason, _) =
                tool.validate_input(tool_input, &ctx).await
            {
                return PermissionOutcome::Deny { reason };
            }
        }
        let outcome = match &judged {
            Some(judged) => {
                let mode = session.as_ref().map_or(judged.permission_mode(), |own| {
                    own.permission_mode().for_delegate(judged.permission_mode())
                });
                held_to_mode(
                    self.inner
                        .check_for_delegate(tool_name, tool_input, cwd, session_id, judged)
                        .await,
                    mode,
                    own.as_deref(),
                    tool_input,
                )
            }
            None => {
                self.inner
                    .check(tool_name, tool_input, cwd, session_id)
                    .await
            }
        };
        if session.as_ref().is_some_and(|s| s.unattended) {
            return outcome;
        }
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
        let _waiting = Waiting::on(self.task.as_deref(), &prompt_id);
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
                task_id: self.task.as_ref().map(|task| task.task_id.clone()),
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

        let answer = match self.timeout_secs {
            0 => rx.await,
            secs => match tokio::time::timeout(std::time::Duration::from_secs(secs), rx).await {
                Ok(answer) => answer,
                Err(_) => {
                    pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&prompt_id);
                    return PermissionOutcome::Deny {
                        reason: format!(
                            "`{tool_name}`: no answer to the permission prompt within {secs}s"
                        ),
                    };
                }
            },
        };
        match answer {
            Ok(runtime::agent::PermissionDecision::Permit) => PermissionOutcome::Permit,
            Ok(runtime::agent::PermissionDecision::PermitAlways { .. }) => {
                // Persist on the *parent's* handler, which is where the rule
                // engine actually lives, for what was asked — this command,
                // this path — as the parent's own answer is. A tool not in
                // the sub-agent's registry has no content to scope it by, and
                // is permitted this once.
                if let Some(tool) = &own {
                    self.inner.add_persistent_allow(
                        tool_name,
                        tool.permission_match_content(tool_input).as_deref(),
                    );
                }
                PermissionOutcome::Permit
            }
            Ok(runtime::agent::PermissionDecision::Deny { reason }) => {
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

#[async_trait]
impl base::tool::Tool for AgentTool {
    fn callable_from_code(&self) -> bool {
        true
    }
    fn code_result(&self) -> Option<&'static str> {
        Some("the sub-agent's final text; an object when output_schema is given")
    }
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
        if ctx.from_code {
            if let Some(field) = [
                (inp.background, "background"),
                (
                    inp.team_name.is_some() || inp.name.is_some(),
                    "team_name/name",
                ),
                (inp.worktree.is_some(), "worktree"),
            ]
            .into_iter()
            .find_map(|(given, field)| given.then_some(field))
            {
                return Err(base::error::ToolError::Validation(format!(
                    "`{field}` is not available from a program: agent() runs a sub-agent and \
                     waits for it. Start that sub-agent with a direct Agent call instead."
                )));
            }
        }

        // Persistent team member — must give both team_name and name, or
        // neither; one without the other is almost certainly a mistake
        // (e.g. the model meant to join a team but dropped a field), not a
        // meaningful partial state to silently fall through on. Gated on
        // `team_registry` being set (`set_team_registry`, only called when
        // `settings.execution.team_enabled` is true — see
        // `AgentsModule::on_session` in `agents/src/lib.rs`):
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
                    Some(base::interface::settings::PermissionMode::Bubble)
                ) {
                    return Err(base::error::ToolError::Validation(
                        "permission_mode: \"bubble\" is not valid here — it needs an available \
                         lead, which a persistent member's lead (only runs code during its own \
                         turns) doesn't reliably have"
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
            Some(s) => match create_worktree(
                &ctx.exec,
                &tools::bash::engine_git(&ctx),
                &ctx.session.cwd,
                s,
            )
            .await
            {
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
        let prompt = self.build_prompt(inp.subagent_type.as_deref(), &inp.prompt);
        let sid = base::id::Id::new().to_string();

        // Bound rather than matched inline so the worktree is cleaned up
        // before the result is turned into a ToolResult — the sub-agent may
        // have failed, and the temporary worktree must go either way.
        let run_cancel = ctx.cancel.child_token();
        let task_scope = base::context::TaskScope::new(
            ctx.session.clone(),
            ctx.tool_use_id.clone(),
            base::context::TaskType::Agent,
            run_cancel.clone(),
            serde_json::json!({
                "agent_type": inp.subagent_type,
                "description": inp.description,
                "background": false,
                "tool_use_id": ctx.tool_use_id,
                "agent_session_id": sid,
            }),
        );
        let perm = self
            .inner
            .permission_handler_asking(Some(task_scope.task().clone()), !ctx.from_code);
        let admission = if ctx.from_code {
            self.inner.admit_waiting(&run_cancel).await
        } else {
            self.inner.admit()
        };
        let result = match admission {
            Ok(admission) => {
                self.run_sub_admitted(
                    admission,
                    prompt,
                    tools,
                    cwd,
                    run_cancel,
                    perm,
                    inp.subagent_type.as_deref(),
                    Some((
                        ctx.session_id.as_str(),
                        ctx.turn_no,
                        ctx.tool_use_id.as_str(),
                    )),
                    SubRun {
                        model: inp.model.clone(),
                        output_schema: inp.output_schema.clone(),
                        ..SubRun::for_task(task_scope.task(), &sid)
                    },
                )
                .await
            }
            Err(refused) => Err(base::error::ToolError::Denied(refused)),
        };

        if let Some(h) = worktree_handle.as_mut() {
            h.cleanup().await;
        }
        if let Ok(text) = &result {
            task_scope.task().append_output(text);
        }
        let structured = task_scope.task().structured_output.get().cloned();

        match result {
            Ok(text) => Ok(base::tool::ToolResult {
                content: ToolResultContent::Text(text),
                is_error: false,
                structured_content: structured,
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

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use base::interface::model::{MessageRole, ModelMessage};
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
            "agix-agent-types-test-{}-{}",
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
            "agix-agent-types-test-newfields-{}-{}",
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
            "agix-agent-types-test-para-{}-{}",
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
            "agix-skills-preload-test-{}-{}",
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
    fn set_skill_catalog_makes_it_available_via_inner() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let model: Arc<dyn Model> = Arc::new(DummyModel);
        let config = Arc::new(EngineConfig::defaults_for("test-model"));
        let agent_tool =
            AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[]);
        assert!(agent_tool.inner.skill_manager().is_none());
        agent_tool.set_skill_catalog(Arc::new(skills::manager::SkillManager::new()));
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
            "agix-agent-types-test-{}-{}",
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
            "agix-agent-types-test-{}-{}",
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
            "agix-agent-types-test-{}-{}",
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

    /// `AgentTool` implements `Tool::prompt()` with the whole of
    /// `agent_tool.prompt.md`. The real tool is classified as documented (so
    /// `build_tool_defs` points at it) and what `ToolSearch` would hand back
    /// is the actual file, not a paraphrase of its description.
    #[tokio::test]
    async fn the_real_agent_tools_guide_is_reachable() {
        use base::tool::Tool;
        let agent_tool = AgentTool::new(
            Arc::new(DummyModel),
            Arc::new(base::context::EngineConfig::defaults_for("test")),
            Arc::new(InMemoryToolRegistry::new()),
        );
        let guide = agent_tool
            .detailed_prompt(&base::tool::PromptContext::default())
            .await
            .expect("AgentTool ships agent_tool.prompt.md and must count as documented");
        assert_eq!(guide, include_str!("agent_tool.prompt.md"));
        assert_ne!(
            guide.trim(),
            agent_tool.description().trim(),
            "the guide must be distinguishable from the short description — \
             that difference is exactly what the default derivation keys on"
        );
    }

    #[test]
    fn the_guide_says_what_the_tool_does() {
        let guide = include_str!("agent_tool.prompt.md");
        assert!(
            guide.contains("you receive a `<task-notification>`"),
            "a finished background agent notifies its parent"
        );
        for stale in [
            "no automatic push",
            "no push notification",
            "FleetView",
            "code-reviewer:",
            "\"bypassPermissions\"` to let it",
        ] {
            assert!(!guide.contains(stale), "{stale}");
        }
        assert!(guide.contains("listed in this tool's description"));
        assert!(guide.contains("max_turns"));

        let schema = serde_json::to_value(schemars::schema_for!(AgentInput)).unwrap();
        assert!(
            schema["properties"]
                .get("auto_background_after_secs")
                .is_none(),
            "{schema}"
        );
    }

    #[test]
    fn a_sub_agent_cut_off_by_a_limit_says_so() {
        assert_eq!(with_stop_note("done".into(), "end_turn"), "done");
        assert!(with_stop_note(String::new(), "max_turns").contains("gave no answer"));
        let partial = with_stop_note("half".into(), "max_turns");
        assert!(partial.starts_with("half") && partial.contains("may be incomplete"));
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

    #[test]
    fn a_session_plugins_agent_types_sit_between_the_shared_catalog_and_the_disk() {
        let def = |name: &str, source: AgentTypeSource, prompt: &str| AgentTypeDefinition {
            name: name.into(),
            source,
            system_prompt: prompt.into(),
            ..Default::default()
        };
        let shared: std::collections::HashMap<String, AgentTypeDefinition> = [
            def("builtin", AgentTypeSource::Builtin, "shared"),
            def("on-disk", AgentTypeSource::LocalFile, "disk"),
        ]
        .into_iter()
        .map(|d| (d.name.clone(), d))
        .collect();
        let tools = Arc::new(InMemoryToolRegistry::new());
        let tool = AgentTool::with_shared_agent_types(
            Arc::new(DummyModel),
            Arc::new(EngineConfig::defaults_for("test-model")),
            tools.clone(),
            tools,
            Arc::new(std::sync::RwLock::new(Arc::new(shared))),
        )
        .with_session_agent_types(&[
            def("builtin", AgentTypeSource::Plugin, "plugin"),
            def("on-disk", AgentTypeSource::Plugin, "plugin"),
            def("new", AgentTypeSource::Plugin, "plugin"),
        ]);

        let prompt = |name: &str| tool.inner.agent_type_def(name).map(|d| d.system_prompt);
        assert_eq!(prompt("builtin").as_deref(), Some("plugin"));
        assert_eq!(prompt("on-disk").as_deref(), Some("disk"));
        assert_eq!(prompt("new").as_deref(), Some("plugin"));
        assert!(base::tool::Tool::description(&tool).contains("new"));
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
    fn effort_names_parse_to_their_levels() {
        assert_eq!(parse_effort("low"), Some(Effort::Low));
        assert_eq!(parse_effort("medium"), Some(Effort::Medium));
        assert_eq!(parse_effort("high"), Some(Effort::High));
        assert_eq!(parse_effort("xhigh"), Some(Effort::Xhigh));
        assert_eq!(parse_effort("max"), Some(Effort::Max));
        assert_eq!(parse_effort("not-a-level"), None);
    }

    #[test]
    fn apply_agent_type_overrides_sets_permission_mode_effort_and_max_turns() {
        let def = AgentTypeDefinition {
            name: "db-reader".into(),
            description: "d".into(),
            permission_mode: Some(base::interface::settings::PermissionMode::Plan),
            effort: Some("max".into()),
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
        assert_eq!(settings.model.effort, Some(Effort::Max));
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

    /// A type in a directory the user controls may raise its own API-call
    /// cap, but not loosen the permission mode: a project's `agents/` is
    /// whatever the repository says.
    #[test]
    fn local_agent_type_may_raise_its_cap_and_loosen_its_mode() {
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
    fn a_planning_parent_holds_a_type_to_plan_and_a_bypassing_one_takes_it_along() {
        use base::interface::settings::PermissionMode as M;
        let typed = |mode| AgentTypeDefinition {
            name: "mine".into(),
            source: AgentTypeSource::LocalFile,
            description: "d".into(),
            permission_mode: Some(mode),
            ..Default::default()
        };
        for (parent, asked, applied) in [
            (M::Plan, M::AcceptEdits, M::Plan),
            (M::Plan, M::DontAsk, M::DontAsk),
            (M::BypassPermissions, M::Plan, M::BypassPermissions),
            (M::AcceptEdits, M::Default, M::Default),
        ] {
            let mut settings = Settings::defaults_for("test-model");
            settings.permission_mode = parent;
            apply_agent_type_overrides(&mut settings, &typed(asked));
            assert_eq!(settings.permission_mode, applied, "{parent:?} + {asked:?}");
        }
        let mut settings = Settings::defaults_for("test-model");
        settings.permission_mode = M::Default;
        let plugin = AgentTypeDefinition {
            source: AgentTypeSource::Plugin,
            ..typed(M::BypassPermissions)
        };
        apply_agent_type_overrides(&mut settings, &plugin);
        assert_eq!(settings.permission_mode, M::Default);
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
            settings.model.effort,
            settings.execution.max_api_calls_per_turn,
        );
        apply_agent_type_overrides(&mut settings, &def);
        assert_eq!(
            (
                settings.permission_mode,
                settings.model.thinking_mode,
                settings.model.effort,
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
    /// the *caller's* session (the real `session.register_task` /
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
        let spawner = crate::spawner::RuntimeAgentSpawner::new(Arc::new(agent_tool));
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

    /// A `.agix/agents/*.md` file edited after `with_parent_tools()` takes
    /// effect on the running `AgentTool` without rebuilding the `Agent`. This
    /// exercises the real `notify` watcher end to end (not just the merge
    /// logic in isolation), the same way
    /// `skills::manager::editing_a_watched_skill_file_is_picked_up_without_a_manual_reload`
    /// does for Skills.
    #[test]
    fn editing_an_agent_type_file_is_picked_up_without_rebuilding_agent_tool() {
        let dir = std::env::temp_dir().join(format!(
            "agix-agent-watch-test-{}-{}",
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

    /// `run_sub` must return when the sub-agent's model call errors —
    /// `DummyModel` always errors. Wrapped in a bounded timeout so a hang
    /// fails this test in 10s instead of hanging the whole test binary.
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

    fn agent_tool_with_model(model: Arc<dyn Model>) -> AgentTool {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let config = Arc::new(EngineConfig::defaults_for("test-model"));
        AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[])
    }

    async fn run_sub_with(agent_tool: &AgentTool) -> Result<String, base::error::ToolError> {
        agent_tool
            .run_sub(
                "do the thing".into(),
                Arc::new(InMemoryToolRegistry::new()),
                std::path::PathBuf::from("/tmp"),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                None,
            )
            .await
    }

    struct PanickingModel;
    #[async_trait]
    impl Model for PanickingModel {
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
            panic!("model exploded");
        }
    }

    #[tokio::test]
    async fn a_panicking_sub_agent_becomes_an_error_for_its_parent() {
        let agent_tool = agent_tool_with_model(Arc::new(PanickingModel));
        let err = run_sub_with(&agent_tool).await.unwrap_err();
        assert!(err.to_string().contains("model exploded"), "{err}");
    }

    struct SetOnDrop(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct HangingModel {
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait]
    impl Model for HangingModel {
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
            let _guard = SetOnDrop(self.dropped.clone());
            self.started.notify_one();
            std::future::pending().await
        }
    }

    /// Never answers, and gives up only when its call is cancelled.
    struct AnswersOnlyCancellation {
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait]
    impl Model for AnswersOnlyCancellation {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let _guard = SetOnDrop(self.dropped.clone());
            self.started.notify_one();
            cancel.cancelled().await;
            Err(base::interface::model::ModelError::Cancelled)
        }
    }

    fn hanging_team_tool(
        cap: usize,
    ) -> (
        AgentTool,
        Arc<tokio::sync::Notify>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tools = Arc::new(InMemoryToolRegistry::new());
        let mut config = EngineConfig::defaults_for("test-model");
        config.team_max_persistent_members = cap;
        let agent_tool = AgentTool::with_parent_tools(
            Arc::new(AnswersOnlyCancellation {
                started: started.clone(),
                dropped: dropped.clone(),
            }),
            Arc::new(config),
            tools.clone(),
            tools,
            &[],
            &[],
        );
        agent_tool.set_team_registry(Arc::new(team::registry::TeamRegistry::new()));
        (agent_tool, started, dropped)
    }

    async fn message_member(
        agent_tool: &AgentTool,
        session: &Arc<base::context::SessionState>,
        name: &str,
    ) -> Result<String, base::error::ToolError> {
        agent_tool
            .spawn_or_message_team_member(
                "team".into(),
                name.into(),
                "work".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_member_past_the_cap_is_refused_while_every_member_is_busy() {
        let (agent_tool, started, _) = hanging_team_tool(1);
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));
        message_member(&agent_tool, &session, "a").await.unwrap();
        started.notified().await;

        let refused = message_member(&agent_tool, &session, "b")
            .await
            .expect_err("the only place is taken by a busy member");
        assert!(
            refused.to_string().contains("team member limit reached"),
            "{refused}"
        );
        assert_eq!(
            agent_tool
                .inner
                .team_members
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1
        );
        message_member(&agent_tool, &session, "a")
            .await
            .expect("a member already there can still be messaged");
    }

    #[tokio::test]
    async fn a_persistent_member_is_in_its_sessions_telemetry_from_start_to_stop() {
        let agent_tool = team_enabled_agent_tool();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        agent_tool.set_telemetry_handle(TelemetryHandle::new(tx));
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));
        message_member(&agent_tool, &session, "a").await.unwrap();
        wait_until_idle(&agent_tool, &team_key("team", "a")).await;
        agent_tool.stop_team_member("team", "a").await;

        let mut kinds = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !kinds.contains(&"agent_completed".to_string()) {
            let event = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("agent_completed within 5s")
                .expect("the handle is still open");
            kinds.push(event.kind().to_string());
        }
        let spawned = kinds.iter().position(|k| k == "agent_spawned");
        assert!(spawned.is_some_and(|i| i < kinds.len() - 1), "{kinds:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_end_of_the_parent_session_stops_a_member_mid_turn() {
        let (agent_tool, started, dropped) = hanging_team_tool(4);
        let ended = tokio_util::sync::CancellationToken::new();
        agent_tool.set_session_ended(ended.clone());
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));
        message_member(&agent_tool, &session, "a").await.unwrap();
        started.notified().await;

        ended.cancel();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the member kept its turn running after the session ended"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_parent_call_aborts_the_foreground_sub_agent() {
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let agent_tool = agent_tool_with_model(Arc::new(HangingModel {
            started: started.clone(),
            dropped: dropped.clone(),
        }));
        tokio::select! {
            _ = run_sub_with(&agent_tool) => panic!("the model never answers"),
            _ = started.notified() => {}
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the sub-agent kept running after its parent dropped the call"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Blocks its thread for the whole call, and records how many calls were
    /// in flight at once.
    #[derive(Default)]
    struct BlockingModel {
        in_flight: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl Model for BlockingModel {
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
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while self.peak.load(SeqCst) < 2 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            self.in_flight.fetch_sub(1, SeqCst);
            Err(base::interface::model::ModelError::Internal("done".into()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn foreground_sub_agents_of_one_turn_run_on_separate_threads() {
        let model = Arc::new(BlockingModel::default());
        let agent_tool = agent_tool_with_model(model.clone());
        let _ = futures::join!(run_sub_with(&agent_tool), run_sub_with(&agent_tool));
        assert_eq!(
            model.peak.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "two sub-agents whose model calls block their thread were polled one after the other"
        );
    }

    /// First call: waits for `go`, then answers. Later calls: record the last
    /// user message they were shown, then answer.
    struct SteerableModel {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        started: Arc<tokio::sync::Notify>,
        go: Arc<tokio::sync::Notify>,
        seen_later: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl Model for SteerableModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            use base::interface::model::ModelEvent;
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                self.started.notify_one();
                self.go.notified().await;
            } else {
                let last_user = messages
                    .iter()
                    .rev()
                    .find(|m| matches!(m.role, MessageRole::User))
                    .map(|m| format!("{:?}", m.content))
                    .unwrap_or_default();
                self.seen_later.lock().unwrap().push(last_user);
            }
            Ok(Box::new(futures::stream::iter(vec![
                Ok(ModelEvent::TextDelta {
                    text: "report".into(),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ])))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_background_sub_agent_reads_its_messages_and_reports_back_when_done() {
        let started = Arc::new(tokio::sync::Notify::new());
        let go = Arc::new(tokio::sync::Notify::new());
        let seen_later = Arc::new(std::sync::Mutex::new(Vec::new()));
        let agent_tool = agent_tool_with_model(Arc::new(SteerableModel {
            calls: Default::default(),
            started: started.clone(),
            go: go.clone(),
            seen_later: seen_later.clone(),
        }));
        let session = Arc::new(base::context::SessionState::new(std::env::temp_dir()));
        let tid = agent_tool
            .spawn_background(
                "look into it".into(),
                Arc::new(InMemoryToolRegistry::new()),
                std::env::temp_dir(),
                tokio_util::sync::CancellationToken::new(),
                None,
                session.clone(),
            )
            .await
            .unwrap();
        started.notified().await;
        tools::task_message::deliver(&session, &tid, "also check the lexer".into())
            .expect("a running sub-agent takes messages");
        go.notify_one();

        let task = session.find_running_task(&tid).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
            .await
            .expect("the sub-agent finishes");
        assert!(
            seen_later.lock().unwrap()[0].contains("also check the lexer"),
            "the message joins the sub-agent's turn: {:?}",
            seen_later.lock().unwrap()
        );
        let notices = session.take_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("<task-notification>"));
        assert!(notices[0].contains(&tid));
        assert!(notices[0].contains("<status>done</status>"));
        assert!(notices[0].contains("report"));
    }

    /// A scene that brings a tool of its own, the way an embedder's does: a
    /// fresh instance on every call.
    struct SceneWithProbe;
    impl base::interface::scene::Scene for SceneWithProbe {
        fn id(&self) -> &str {
            "with-probe"
        }
        fn name(&self) -> &str {
            "With probe"
        }
        fn description(&self) -> &str {
            "scene contributing scene_probe"
        }
        fn build_system_prompt(
            &self,
            _: &base::interface::scene::ScenePromptContext,
        ) -> Vec<base::interface::prompt::PromptBlock> {
            vec![]
        }
        fn tools(&self) -> Vec<String> {
            vec![]
        }
        fn token_budget(&self) -> base::interface::scene::TokenBudget {
            base::interface::scene::TokenBudget {
                compact_threshold: 150_000,
                compact_keep_recent: 20,
            }
        }
        fn extra_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
            vec![Arc::new(NamedTool("scene_probe"))]
        }
    }

    /// Answers at once, noting the tools each request offered.
    struct ToolCensusModel {
        offered: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    }
    #[async_trait]
    impl Model for ToolCensusModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            use base::interface::model::ModelEvent;
            self.offered
                .lock()
                .unwrap()
                .push(tools.into_iter().map(|t| t.name).collect());
            Ok(Box::new(futures::stream::iter(vec![
                Ok(ModelEvent::TextDelta {
                    text: "done".into(),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ])))
        }
    }

    /// Answers at once, noting the model each request asked for.
    struct ModelNameModel {
        asked_for: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl Model for ModelNameModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<ModelMessage>,
            params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            use base::interface::model::ModelEvent;
            self.asked_for.lock().unwrap().push(params.model);
            Ok(Box::new(futures::stream::iter(vec![
                Ok(ModelEvent::TextDelta {
                    text: "{\"ok\":true}".into(),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ])))
        }
    }

    #[tokio::test]
    async fn a_spawn_on_a_model_asks_for_that_model() {
        use base::interface::agent_spawner::AgentSpawner;
        let asked_for = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools = Arc::new(InMemoryToolRegistry::new());
        let agent_tool = AgentTool::with_parent_tools(
            Arc::new(ModelNameModel {
                asked_for: asked_for.clone(),
            }),
            Arc::new(EngineConfig::defaults_for("test-model")),
            tools.clone(),
            tools,
            &[],
            &[],
        );
        let spawner = crate::spawner::RuntimeAgentSpawner::new(Arc::new(agent_tool));

        let answer = spawner
            .spawn_agent_on_model(
                "review".into(),
                std::path::PathBuf::from("/tmp"),
                tokio_util::sync::CancellationToken::new(),
                "claude-haiku-4-5".into(),
            )
            .await
            .unwrap();
        assert_eq!(answer, "{\"ok\":true}");
        let asked_for = asked_for.lock().unwrap();
        assert!(!asked_for.is_empty());
        assert!(
            asked_for.iter().all(|m| m == "claude-haiku-4-5"),
            "{asked_for:?}"
        );
    }

    /// The `AgentTool` of a session on `SceneWithProbe`: its registry already
    /// holds the scene's tool, as `Builder::build()` leaves it.
    fn agent_tool_on_a_scene_with_a_tool(
        types: &[AgentTypeDefinition],
    ) -> (AgentTool, Arc<std::sync::Mutex<Vec<Vec<String>>>>) {
        let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(NamedTool("Read")));
        tools.register(Arc::new(NamedTool("scene_probe")));
        let agent_tool = AgentTool::with_parent_tools(
            Arc::new(ToolCensusModel {
                offered: offered.clone(),
            }),
            Arc::new(EngineConfig::defaults_for("test-model")),
            tools.clone(),
            tools,
            &[],
            types,
        );
        agent_tool.set_scene(Arc::new(SceneWithProbe));
        (agent_tool, offered)
    }

    fn offered_once(offered: &[Vec<String>], name: &str) -> bool {
        offered
            .first()
            .is_some_and(|tools| tools.iter().filter(|t| *t == name).count() == 1)
    }

    #[tokio::test]
    async fn a_foreground_sub_agent_on_its_parents_scene_gets_the_scenes_tools_once() {
        let (agent_tool, offered) = agent_tool_on_a_scene_with_a_tool(&[]);
        let result = base::tool::Tool::call(
            &agent_tool,
            serde_json::json!({ "prompt": "look", "description": "look" }),
            ToolContext::for_test(std::env::temp_dir()),
            ProgressSender::noop("t"),
        )
        .await
        .expect("the sub-agent is built and runs");
        assert!(!result.is_error, "{:?}", result.content);
        let offered = offered.lock().unwrap();
        assert!(offered_once(&offered, "scene_probe"), "{offered:?}");
        assert!(offered_once(&offered, "Read"), "{offered:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_background_sub_agent_and_a_spawned_one_get_the_scenes_tools_once() {
        let (agent_tool, offered) = agent_tool_on_a_scene_with_a_tool(&[]);
        let ctx = ToolContext::for_test(std::env::temp_dir());
        let session = ctx.session.clone();
        base::tool::Tool::call(
            &agent_tool,
            serde_json::json!({ "prompt": "look", "description": "look", "background": true }),
            ctx,
            ProgressSender::noop("t"),
        )
        .await
        .expect("the background sub-agent is started");
        let task = session.task_infos().pop().expect("a registered task");
        let task = session
            .find_running_task(task["task_id"].as_str().unwrap())
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
            .await
            .expect("the sub-agent finishes");
        assert!(
            offered_once(&offered.lock().unwrap(), "scene_probe"),
            "{:?}",
            offered.lock().unwrap()
        );

        offered.lock().unwrap().clear();
        use base::interface::agent_spawner::AgentSpawner as _;
        crate::spawner::RuntimeAgentSpawner::new(Arc::new(agent_tool))
            .spawn_agent(
                "look".into(),
                vec![],
                std::env::temp_dir(),
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
            .expect("a spawner sub-agent is built and runs");
        assert!(
            offered_once(&offered.lock().unwrap(), "scene_probe"),
            "{:?}",
            offered.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn an_agent_types_tool_lists_narrow_the_scenes_tools_too() {
        let types = [
            AgentTypeDefinition {
                name: "reader".into(),
                description: "reads".into(),
                allowed_tools: vec!["Read".into()],
                ..Default::default()
            },
            AgentTypeDefinition {
                name: "no-probe".into(),
                description: "no probe".into(),
                disallowed_tools: vec!["scene_probe".into()],
                ..Default::default()
            },
        ];
        for agent_type in ["reader", "no-probe"] {
            let (agent_tool, offered) = agent_tool_on_a_scene_with_a_tool(&types);
            base::tool::Tool::call(
                &agent_tool,
                serde_json::json!({
                    "prompt": "look", "description": "look", "subagent_type": agent_type
                }),
                ToolContext::for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("the sub-agent is built and runs");
            let offered = offered.lock().unwrap();
            assert!(
                offered[0].contains(&"Read".to_string())
                    && !offered[0].contains(&"scene_probe".to_string()),
                "{agent_type}: {offered:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_session_cap_refuses_one_sub_agent_too_many_but_not_a_team_member() {
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let slots = SubagentSlots::new(1);
        let agent_tool = agent_tool_with_model(Arc::new(HangingModel {
            started: started.clone(),
            dropped: dropped.clone(),
        }))
        .with_subagent_slots(slots.clone());

        let mut first = Box::pin(run_sub_with(&agent_tool));
        tokio::select! {
            _ = &mut first => panic!("the model never answers"),
            _ = started.notified() => {}
        }
        assert_eq!(slots.running(), 1);

        let refused = run_sub_with(&agent_tool).await.unwrap_err();
        assert!(
            refused.to_string().contains("max_concurrent_subagents"),
            "{refused}"
        );

        let member = agent_tool.run_team_member(
            "member task".into(),
            Arc::new(InMemoryToolRegistry::new()),
            std::path::PathBuf::from("/tmp"),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(AlwaysPermit),
            None,
            None,
            None,
        );
        tokio::select! {
            _ = member => panic!("the model never answers"),
            _ = started.notified() => {}
        }
        assert_eq!(slots.running(), 1, "a team member holds no slot");

        drop(first);
        assert_eq!(slots.running(), 0, "an ended sub-agent gives its slot back");
    }

    fn team_key(team: &str, member: &str) -> (String, String) {
        (team.to_string(), member.to_string())
    }

    fn team_enabled_agent_tool() -> AgentTool {
        let agent_tool = test_agent_tool();
        agent_tool.set_team_registry(Arc::new(team::registry::TeamRegistry::new()));
        agent_tool
    }

    /// Called directly (not through `spawn_or_message_team_member`, which only
    /// returns a task id) so the built `Agent`'s own scene is inspectable.
    #[tokio::test]
    async fn team_member_inherits_the_parents_scene_instead_of_always_coding() {
        let agent_tool = team_enabled_agent_tool();
        agent_tool.set_scene(Arc::new(scene::harnix::HarnixScene));

        let (agent, _event_rx, _input_tx, _) = agent_tool
            .build_team_member_agent(
                Arc::new(InMemoryToolRegistry::new()),
                std::path::PathBuf::from("/tmp"),
                None,
                base::interface::settings::PermissionMode::Plan,
                None,
            )
            .await
            .expect("build should succeed");
        assert_eq!(agent.scene().id(), "harnix");
    }

    /// A delegate runs its hooks on its parent's runner rather than one it
    /// builds from the same settings: `once` holds across the tree, and the
    /// parent's file watcher is the only one.
    #[tokio::test]
    async fn a_delegate_runs_its_hooks_on_the_parents_runner() {
        let agent_tool = team_enabled_agent_tool();
        let runner = Arc::new(hooks::HookRunner::empty());
        agent_tool.set_hooks(runner.clone());

        let (agent, _event_rx, _input_tx, _) = agent_tool
            .build_team_member_agent(
                Arc::new(InMemoryToolRegistry::new()),
                std::path::PathBuf::from("/tmp"),
                None,
                base::interface::settings::PermissionMode::Plan,
                None,
            )
            .await
            .expect("build should succeed");
        assert!(std::ptr::eq(agent.hooks(), Arc::as_ptr(&runner)));
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

    /// Reclaim rule (a): a pool at the cap makes room for a newcomer by
    /// evicting the oldest-idle member — just one, not everything.
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
            2,
            "the pool stays at its cap: {:?}",
            members.keys().collect::<Vec<_>>()
        );
        for name in ["a", "b"] {
            assert!(
                !members.contains_key(&team_key("team", name)),
                "oldest-idle member '{name}' should have been reclaimed"
            );
        }
        for name in ["c", "d"] {
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

    /// The reclaim sweep run just before looking up an existing member must
    /// not evict that exact member — here one idle past the timeout, which
    /// any other member would be reclaimed for — and spawn a fresh
    /// replacement instead of reaching the one the caller meant to message.
    #[tokio::test]
    async fn spawn_or_message_team_member_never_reclaims_the_member_it_is_about_to_message() {
        let mut config = EngineConfig::defaults_for("test");
        config.team_member_idle_timeout_secs = 0;
        let agent_tool = agent_tool_with_config(config);
        let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
            "/tmp",
        )));
        let key_a = team_key("team", "a");

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
        let id_a_before = agent_tool
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key_a)
            .expect("a is in the pool")
            .id
            .clone();

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
    }

    /// Two concurrent calls for the same never-before-seen
    /// `(team_name, member_name)` key must not both observe "doesn't exist
    /// yet" and both spawn a member, orphaning the first.
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
    /// bookkeeping — what `TeamDeleteTool` holds a spawner for.
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
        let spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner> =
            Arc::new(crate::spawner::RuntimeAgentSpawner::new(agent_tool.clone()));
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

    #[tokio::test]
    async fn auto_for_a_persistent_member_is_bypass() {
        let agent_tool = team_enabled_agent_tool();
        let ctx = ToolContext::for_test(std::path::PathBuf::from("/tmp"));
        let input = serde_json::json!({
            "prompt": "hi", "team_name": "t-auto", "name": "worker", "permission_mode": "auto",
        });
        base::tool::Tool::call(&agent_tool, input, ctx, ProgressSender::noop("t"))
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &team_key("t-auto", "worker")).await;

        let registry = agent_tool
            .inner
            .team_registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap();
        assert_eq!(
            registry.team_permission_mode("t-auto"),
            Some(base::interface::settings::PermissionMode::BypassPermissions)
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
    /// the mode-dispatch snippet alone. `SendMessage`/`ReadMail`/`ListPeers`
    /// are exempt from mode-based restriction by design.
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
                &serde_json::json!({"peer": "peer", "message": "y"}),
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

    /// Parent `Settings` for a sub-agent spawn, pointed at a throwaway temp
    /// directory so `Builder::build()`'s skill/agent/plugin scans have
    /// nothing real to find.
    fn parent_settings_for_test(scope: &str) -> (Settings, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "agix-subagent-test-{}-{}",
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

    /// A sub-agent spawned with a history store and a parent session id
    /// must persist its own transcript as a sidechain linked back to the
    /// parent — this is what makes `HistoryStore::child_sessions` (and thus
    /// `session.list {parent_session_id}`) find it.
    #[tokio::test]
    async fn subagent_transcript_is_written_as_a_linked_sidechain() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
        let history_dir = std::env::temp_dir().join(format!(
            "agix-subagent-history-test-{}-{}",
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
                Some(("parent-session-1", 1, "")),
                SubRun::default(),
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
                assert_eq!(scene.as_deref(), Some("code"));
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
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
        let history_dir = std::env::temp_dir().join(format!(
            "agix-subagent-cancelled-test-{}-{}",
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
                Some(("parent-session-1", 1, "")),
                SubRun::default(),
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

    /// A one-shot sub-agent's transcript must end with a
    /// `SessionEnd` marker once its single turn concludes — that marker is
    /// what lets `session.resume` reject a stale resume against a
    /// "finished" sidechain with `SIDECHAIN_TERMINAL` instead of silently
    /// reattaching to it.
    #[tokio::test]
    async fn subagent_run_writes_a_terminal_marker_when_its_task_concludes() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
        let history_dir = std::env::temp_dir().join(format!(
            "agix-subagent-terminal-test-{}-{}",
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
                Some(("parent-session-1", 1, "")),
                SubRun::default(),
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

    /// A sub-agent must inherit the parent's telemetry handle
    /// instead of `Builder::build()`'s noop default (a channel whose
    /// receiver is dropped immediately) — otherwise every event the
    /// sub-agent produces (tool timings, permission decisions, cost) is
    /// silently discarded, which is indistinguishable from "nothing was
    /// ever recorded" from the outside.
    #[tokio::test]
    async fn subagent_telemetry_events_reach_the_parents_sink() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
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
                Some(("parent-session-1", 1, "")),
                SubRun::default(),
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

    /// A sub-agent's events must reach the *parent's* channel, tagged
    /// with a stable label, and the text return value the model sees must be
    /// unchanged by that forwarding.
    #[tokio::test]
    async fn subagent_events_reach_the_parent_channel_tagged_without_changing_the_text_result() {
        let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent_tool.set_event_sender(runtime::event_bus::EventBus::new(tx));

        let text = agent_tool
            .run_sub_tagged(
                "go".into(),
                tools,
                dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                Some("explore"),
                Some(("parent-session", 7, "toolu_parent")),
                SubRun::default(),
            )
            .await
            .expect("sub-agent turn should complete");
        let _ = std::fs::remove_dir_all(&dir);

        // Forwarding is additive: the value handed back to the model must be
        // byte-identical to what the un-forwarded path produces.
        let (plain_tool, plain_tools, _r, plain_dir) = wired_agent_tool("code");
        let baseline = plain_tool
            .run_sub_tagged(
                "go".into(),
                plain_tools,
                plain_dir.clone(),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(AlwaysPermit),
                Some("explore"),
                None,
                SubRun::default(),
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
                parent_tool_use_id,
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
            assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_parent"));
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

    /// A sub-agent's permission checks must reach the parent's
    /// `Permission` impl, and a `Deny` there must actually stop the tool.
    #[tokio::test]
    async fn subagent_permission_check_routes_to_parent_and_deny_blocks_the_tool() {
        let (agent_tool, tools, runs, dir) = wired_agent_tool("code");
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
                SubRun::default(),
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

    /// The parent's rules decide a sub-agent's call in the sub-agent's own
    /// mode when that is stricter, and in the parent's when it is not.
    #[tokio::test]
    async fn a_sub_agents_call_is_judged_in_its_own_mode_unless_the_parent_plans() {
        use base::interface::permission::PermissionOutcome;
        use base::permission::PermissionMode;
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("notes.txt"), "x").unwrap();
        let registry = Arc::new(InMemoryToolRegistry::new());
        tools::register_builtin_tools(&registry);
        let parent = Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
            Arc::new(permissions::gate::PermissionGate::empty()),
            registry.clone(),
            PermissionMode::AcceptEdits,
        ));
        let parent_session = Arc::new(
            base::context::SessionState::new(project.path().to_path_buf())
                .with_permission_mode(PermissionMode::AcceptEdits),
        );
        parent.bind_session_state(parent_session.clone());
        let agent_tool = test_agent_tool();
        agent_tool.set_parent_permission(parent);
        let sub = agent_tool.sub_permission();
        sub.bind_tool_registry(registry);
        let sub_session = Arc::new(base::context::SessionState::new(
            project.path().to_path_buf(),
        ));
        sub.bind_session_state(sub_session.clone());
        let check = |tool: &'static str, input: Value| {
            let sub = sub.clone();
            let cwd = project.path().to_path_buf();
            async move { sub.check(tool, &input, &cwd, "sub").await }
        };
        let write = serde_json::json!({"file_path": project.path().join("a.txt"), "content": "x"});
        let read_outside = serde_json::json!({"file_path": outside.path().join("notes.txt")});

        sub_session.set_permission_mode(PermissionMode::AcceptEdits);
        assert!(matches!(
            check("Write", write.clone()).await,
            PermissionOutcome::Permit
        ));
        assert!(matches!(
            check("Read", read_outside.clone()).await,
            PermissionOutcome::Permit
        ));

        sub_session.set_permission_mode(PermissionMode::Plan);
        match check("Write", write.clone()).await {
            PermissionOutcome::Deny { reason } => assert!(reason.contains("plan mode"), "{reason}"),
            other => panic!("a plan-mode sub-agent wrote: {other:?}"),
        }

        sub_session.set_permission_mode(PermissionMode::Default);
        assert!(
            matches!(
                check("Read", read_outside.clone()).await,
                PermissionOutcome::Deny { .. }
            ),
            "a default-mode sub-agent is asked about a read outside; with no channel, refused"
        );

        sub_session.set_permission_mode(PermissionMode::BypassPermissions);
        assert!(
            matches!(
                check("Bash", serde_json::json!({"command": "touch made"})).await,
                PermissionOutcome::Permit
            ),
            "a bypassing sub-agent is not asked what its acceptEdits parent would be"
        );

        parent_session.set_permission_mode(PermissionMode::Plan);
        sub_session.set_permission_mode(PermissionMode::AcceptEdits);
        match check("Write", write.clone()).await {
            PermissionOutcome::Deny { reason } => assert!(reason.contains("plan mode"), "{reason}"),
            other => panic!("a sub-agent wrote while its parent planned: {other:?}"),
        }
    }

    /// A team member is held to the parent's rules, not given a rule set of
    /// its own.
    #[tokio::test]
    async fn a_team_member_is_judged_by_the_parents_rules() {
        use base::interface::permission::PermissionOutcome;
        use base::permission::{PermissionMode, PermissionRule, RuleBehavior, RuleSource};
        let project = tempfile::tempdir().unwrap();
        let registry = Arc::new(InMemoryToolRegistry::new());
        tools::register_builtin_tools(&registry);
        let parent = Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
            Arc::new(permissions::gate::PermissionGate::new(
                permissions::ruleset::RuleSet::new(vec![PermissionRule {
                    source: RuleSource::ProjectSettings,
                    behavior: RuleBehavior::Deny,
                    tool_name: "Write".into(),
                    rule_content: Some("**/locked/**".into()),
                }]),
            )),
            registry.clone(),
            PermissionMode::AcceptEdits,
        ));
        let agent_tool = test_agent_tool();
        agent_tool.set_parent_permission(parent);
        let member = agent_tool.team_member_permission(
            &registry,
            base::interface::settings::PermissionMode::BypassPermissions,
        );
        member.bind_tool_registry(registry);
        member.bind_session_state(Arc::new(
            base::context::SessionState::new(project.path().to_path_buf())
                .with_permission_mode(PermissionMode::AcceptEdits),
        ));
        let outcome = member
            .check(
                "Write",
                &serde_json::json!({"file_path": project.path().join("locked/a.txt"), "content": "x"}),
                project.path(),
                "member",
            )
            .await;
        assert!(
            matches!(outcome, PermissionOutcome::Deny { .. }),
            "{outcome:?}"
        );
    }

    struct PromptsForEverything;
    #[async_trait]
    impl Permission for PromptsForEverything {
        async fn check(
            &self,
            _t: &str,
            _i: &Value,
            _c: &Path,
            _s: &str,
        ) -> base::interface::permission::PermissionOutcome {
            base::interface::permission::PermissionOutcome::Prompt {
                prompt_id: base::id::Id::new().to_string(),
                message: "run the probe?".into(),
                paths: vec![],
            }
        }
    }

    /// A wired `AgentTool` in an attended scene whose parent asks about
    /// every call, with the events it sends up.
    fn asking_agent_tool() -> (
        Arc<AgentTool>,
        Arc<std::sync::atomic::AtomicUsize>,
        std::path::PathBuf,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (agent_tool, _tools, runs, dir) = wired_agent_tool("code");
        agent_tool.set_scene(Arc::new(scene::code::CodeScene::default()));
        agent_tool.set_parent_permission(Arc::new(PromptsForEverything));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        agent_tool.set_event_sender(runtime::event_bus::EventBus::new(tx));
        agent_tool.set_parent_pending_permissions(Arc::default());
        (Arc::new(agent_tool), runs, dir, rx)
    }

    fn agent_call_ctx(cwd: std::path::PathBuf, tool_use_id: &str, from_code: bool) -> ToolContext {
        let mut ctx = ToolContext::for_test(cwd);
        ctx.tool_use_id = tool_use_id.into();
        ctx.from_code = from_code;
        ctx
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sub_agent_a_program_started_is_refused_what_it_would_ask_about() {
        let (agent_tool, runs, dir, mut rx) = asking_agent_tool();
        let result = base::tool::Tool::call(
            agent_tool.as_ref(),
            serde_json::json!({"prompt": "go", "description": "probe"}),
            agent_call_ctx(dir.clone(), "toolu_code.1", true),
            ProgressSender::noop("t"),
        )
        .await
        .expect("the sub-agent runs to its answer");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the call that needed approval did not run"
        );
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, AgentEvent::PermissionPrompt { .. }),
                "a program's sub-agent asked the host: {event:?}"
            );
        }

        let (agent_tool, _runs, dir, mut rx) = asking_agent_tool();
        let direct = tokio::spawn(async move {
            base::tool::Tool::call(
                agent_tool.as_ref(),
                serde_json::json!({"prompt": "go", "description": "probe"}),
                agent_call_ctx(dir, "toolu_direct", false),
                ProgressSender::noop("t"),
            )
            .await
        });
        let asked = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = rx.recv().await {
                if matches!(event, AgentEvent::PermissionPrompt { .. }) {
                    return true;
                }
            }
            false
        })
        .await;
        direct.abort();
        assert_eq!(
            asked,
            Ok(true),
            "the same sub-agent started directly asks the host"
        );
    }

    /// Answers with text after a pause, counting how many answer at once.
    struct SlowTextModel {
        running: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Model for SlowTextModel {
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
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.running.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            self.running.fetch_sub(1, SeqCst);
            let events: Vec<Result<ModelEvent, base::interface::model::ModelError>> = vec![
                Ok(ModelEvent::TextDelta {
                    text: "done".into(),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_programs_fan_out_past_the_cap_waits_for_a_slot_instead_of_failing() {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let agent_tool = agent_tool_with_model(Arc::new(SlowTextModel {
            running: Arc::default(),
            peak: peak.clone(),
        }))
        .with_subagent_slots(SubagentSlots::new(2));
        let call = |id: &str, from_code: bool| {
            base::tool::Tool::call(
                &agent_tool,
                serde_json::json!({"prompt": "go", "description": id}),
                agent_call_ctx(std::env::temp_dir(), id, from_code),
                ProgressSender::noop("t"),
            )
        };

        let (a, b, c) = futures::join!(call("a", true), call("b", true), call("c", true));
        for r in [a, b, c] {
            let r = r.unwrap();
            assert!(!r.is_error, "{:?}", r.content);
        }
        assert!(peak.load(std::sync::atomic::Ordering::SeqCst) <= 2);

        let (d, e, f) = futures::join!(call("d", false), call("e", false), call("f", false));
        let refused: Vec<_> = [d, e, f]
            .into_iter()
            .map(Result::unwrap)
            .filter(|r| r.is_error)
            .collect();
        assert_eq!(
            refused.len(),
            1,
            "a direct call past the cap is refused at once"
        );
        assert!(
            format!("{:?}", refused[0].content).contains("max_concurrent_subagents"),
            "{:?}",
            refused[0].content
        );
    }

    #[tokio::test]
    async fn a_program_cannot_start_a_background_team_or_worktree_sub_agent() {
        let agent_tool = test_agent_tool();
        for extra in [
            serde_json::json!({"background": true}),
            serde_json::json!({"team_name": "t", "name": "n"}),
            serde_json::json!({"worktree": "w"}),
        ] {
            let mut input = serde_json::json!({"prompt": "go", "description": "d"});
            input
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let err = base::tool::Tool::call(
                &agent_tool,
                input,
                agent_call_ctx(std::env::temp_dir(), "toolu_code.1", true),
                ProgressSender::noop("t"),
            )
            .await
            .expect_err("refused");
            assert!(
                err.to_string().contains("not available from a program"),
                "{extra}: {err}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_background_sub_agent_ends_its_task_failed_and_says_so() {
        let agent_tool = agent_tool_with_model(Arc::new(PanickingModel));
        let ctx = ToolContext::for_test(std::env::temp_dir());
        let session = ctx.session.clone();
        let started = base::tool::Tool::call(
            &agent_tool,
            serde_json::json!({"prompt": "go", "description": "bg", "background": true}),
            ctx,
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
        let ToolResultContent::Text(text) = &started.content else {
            panic!("{:?}", started.content);
        };
        let task_id = text
            .split("task_id: ")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .expect("a task id");
        let task = session.find_running_task(task_id).expect("registered");
        tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
            .await
            .expect("the task ends");
        assert!(
            matches!(
                &*task.status.lock().unwrap(),
                base::context::RunningStatus::Failed(m) if m.contains("model exploded")
            ),
            "{:?}",
            task.status.lock().unwrap()
        );
        assert!(
            session
                .take_notices()
                .iter()
                .any(|n| n.contains("model exploded")),
            "the parent is told"
        );
    }

    /// Without wiring, a sub-agent's permission is allow-all.
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

    /// The sub-agent inherits the parent's scene rather than always
    /// being handed `CodeScene`, and an agent type's own `scene:` wins.
    #[test]
    fn subagent_scene_inherits_parent_and_agent_type_override_wins() {
        let agent_tool = test_agent_tool();
        let harnix: Arc<dyn Scene> = Arc::new(scene::harnix::HarnixScene);
        agent_tool.set_scene(harnix);
        let settings = Settings::defaults_for("test-model");

        assert_eq!(
            agent_tool.inner.scene_for_subagent(None, &settings).id(),
            "harnix",
            "a harnix-scene parent must not hand its sub-agent the code scene"
        );

        // An agent type may *narrow* the scene, never widen it.
        // `.agix/agents/*.md` is repository content; letting it name
        // `scene: code` (whose empty allow-list means "every registered
        // tool") would let a file in the repo hand a harnix session tools its
        // scene leaves out.
        let widening = AgentTypeDefinition {
            name: "coder".into(),
            description: "d".into(),
            scene: Some("code".into()),
            ..Default::default()
        };
        assert_eq!(
            agent_tool
                .inner
                .scene_for_subagent(Some(&widening), &settings)
                .id(),
            "harnix",
            "an agent type must not be able to widen the parent scene's tool surface"
        );

        // Narrowing is fine: mini's allow-list is a strict subset of harnix's.
        let narrowing = AgentTypeDefinition {
            name: "minimal".into(),
            description: "d".into(),
            scene: Some("mini".into()),
            ..Default::default()
        };
        assert_eq!(
            agent_tool
                .inner
                .scene_for_subagent(Some(&narrowing), &settings)
                .id(),
            "mini",
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
            "harnix",
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

        // Reusing the parent's scene under a plugin-style id keeps this test
        // about *resolution*, not about the narrowing rule above.
        let mini: Arc<dyn Scene> = Arc::new(scene::mini::MiniScene);
        agent_tool.set_scene(mini);

        assert_eq!(
            agent_tool
                .inner
                .scene_for_subagent(Some(&named), &settings)
                .id(),
            "mini",
            "without a registry, a non-builtin id is unresolvable and the parent scene is kept"
        );

        let mut registry = scene::SceneRegistry::new();
        registry.register_builtin();
        registry.register(Arc::new(AliasScene {
            id: "plugin:demo",
            inner: Arc::new(scene::mini::MiniScene),
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
        inner: Arc<dyn Scene>,
    }

    impl Scene for AliasScene {
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
    /// degrades to `CodeScene`.
    #[test]
    fn subagent_scene_falls_back_to_settings_scope_then_code() {
        let agent_tool = test_agent_tool();
        let mut settings = Settings::defaults_for("test-model");

        settings.paths.scope = "mini".into();
        assert_eq!(
            agent_tool.inner.scene_for_subagent(None, &settings).id(),
            "mini"
        );

        settings.paths.scope = "nonexistent".into();
        assert_eq!(
            agent_tool.inner.scene_for_subagent(None, &settings).id(),
            "code"
        );
    }

    /// A sub-agent must be able to see AGENTS.md / CLAUDE.md, both when
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
            "agix-agent-scene-front-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_agent_md(
            &dir,
            "digger.md",
            "---\nname: digger\ndescription: Source digger\nscene: harnix\n---\nBody.",
        );
        let types = load_agent_types_from_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(types[0].scene.as_deref(), Some("harnix"));
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
            parent_tool_use_id: Some("toolu_0".into()),
            task_id: Some("ag-1".into()),
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

    #[test]
    fn a_background_agents_answer_is_its_whole_output_by_offset() {
        let session = base::context::SessionState::new(std::env::temp_dir());
        let task = session.register_task(
            "ag-answer",
            base::context::TaskType::Agent,
            tokio_util::sync::CancellationToken::new(),
            serde_json::Value::Null,
        );
        let answer = "the answer, \u{4e2d}".to_string();
        super::conclude_background_task(&session, &task, &Ok(answer.clone()), false);
        let read = task.output_from(0).unwrap();
        assert_eq!(read.text, answer);
        assert_eq!(read.next_byte, answer.len() as u64);
        assert!(read.closed);
    }
}
