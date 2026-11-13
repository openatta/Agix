//! Agent-type catalog: built-in types, loading `.md` definitions from disk, merging across
//! sources and the shared hot-reloaded catalog, plus the built-in type prompts.

use super::*;

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
/// Returns all successfully parsed definitions, tagged with `source`.
/// Malformed files are skipped with a `tracing::warn!` message.
///
/// Sync (`std::fs`), not async — matches `skills::manager::SkillManager::load_dir`,
/// which is called from the same sync `Builder::build()` context this
/// function is meant to be called from (see `merge_agent_types`).
pub fn load_agent_types_from_dir(
    dir: &Path,
    source: AgentTypeSource,
    files: &PromptFiles,
) -> Vec<AgentTypeDefinition> {
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
        let content = match files.read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "agent type: failed to read file");
                continue;
            }
        };
        match parse_agent_type_file(&content, &path) {
            Some(def) => types.push(AgentTypeDefinition { source, ..def }),
            None => {
                tracing::warn!(path = %path.display(), "agent type: failed to parse frontmatter");
            }
        }
    }
    types
}

/// The `agents/` directories a session with these settings reads, lowest
/// first: global, scene, then the project's `.agix/agents` tagged with
/// whether the project is trusted. A session with no project tier of its own
/// has no third.
pub fn agent_type_dirs(settings: &LoadedSettings) -> Vec<(std::path::PathBuf, AgentTypeSource)> {
    use base::project_trust::ProjectTrust;
    let mut dirs = vec![
        (
            settings.paths.global_data_dir.join("agents"),
            AgentTypeSource::User,
        ),
        (
            settings.paths.user_data_dir.join("agents"),
            AgentTypeSource::User,
        ),
    ];
    let trusted = match settings.trust() {
        ProjectTrust::NoProject => None,
        ProjectTrust::Trusted => Some(true),
        ProjectTrust::Untrusted => Some(false),
    };
    if let (Some(trusted), Some(project)) = (trusted, settings.paths.project_root()) {
        dirs.push((
            project.join(".agix").join("agents"),
            AgentTypeSource::Project { trusted },
        ));
    }
    dirs
}

/// Merge the built-in agent types with those of `plugin_types` and the
/// `agents/` directories in `dirs`, lowest priority first.
///
/// Precedence low → high: built-in defaults < `plugin_types` (plugin-declared
/// `[[agents]]`) < each directory in order — same override order as skills
/// (`crates/core/src/frozen/skill.rs`). A definition replaces a lower one of
/// the same `name` entirely (not field-merged — a type definition is one
/// atomic unit, same convention as `task_models` overrides in the
/// multi-provider LLM config), when its source may shadow
/// ([`AgentTypeSource::may_shadow`]): an untrusted project's type only adds
/// names nothing else defines.
/// How the [`agent_type_dirs`] of a session with these settings are read.
pub fn agent_type_files(settings: &LoadedSettings) -> PromptFiles {
    PromptFiles::for_project(&base::paths::ConfigPaths::from_settings(&settings.paths))
}

pub fn merge_agent_types(
    dirs: &[(std::path::PathBuf, AgentTypeSource)],
    plugin_types: &[AgentTypeDefinition],
    files: &PromptFiles,
) -> std::collections::HashMap<String, AgentTypeDefinition> {
    let mut merged: std::collections::HashMap<String, AgentTypeDefinition> = builtin_agent_types()
        .into_iter()
        .map(|d| (d.name.clone(), d))
        .collect();
    for def in plugin_types {
        merged.insert(def.name.clone(), def.clone());
    }
    for (dir, source) in dirs {
        for def in load_agent_types_from_dir(dir, *source, files) {
            if def.source.may_shadow() || !merged.contains_key(&def.name) {
                merged.insert(def.name.clone(), def);
            } else {
                tracing::warn!(agent_type = %def.name, dir = %dir.display(), "the project is not trusted, so its agent type does not replace the one already defined");
            }
        }
    }
    merged
}

/// The project agent types that behave differently once the project is
/// trusted: the ones that replace a type of the same name from elsewhere, or
/// may hand their sub-agent a looser mode or a higher call cap than
/// `settings` give the session. As `agents/<name>`, for `project.status`.
pub fn project_agent_types_requiring_trust(
    settings: &LoadedSettings,
    plugin_types: &[AgentTypeDefinition],
) -> Vec<String> {
    let dirs = agent_type_dirs(settings);
    let Some((project_dir, AgentTypeSource::Project { .. })) = dirs.last() else {
        return Vec::new();
    };
    let files = agent_type_files(settings);
    let below = merge_agent_types(&dirs[..dirs.len() - 1], plugin_types, &files);
    let mut out: Vec<String> = load_agent_types_from_dir(
        project_dir,
        AgentTypeSource::Project { trusted: true },
        &files,
    )
    .into_iter()
    .filter(|def| {
        below.contains_key(&def.name)
            || def
                .permission_mode
                .is_some_and(|m| m.permissiveness() > settings.permission_mode.permissiveness())
            || def
                .max_turns
                .is_some_and(|n| n > settings.execution.max_api_calls_per_turn)
    })
    .map(|def| format!("agents/{}", def.name))
    .collect();
    out.sort();
    out
}

/// A live-reloaded agent-type catalog meant to be built **once** and shared
/// by every session a multi-session caller (a daemon's `SessionPool`) hands
/// out: a session that scans the directories itself starts a file-watcher
/// thread of its own, N redundant threads for N sessions.
/// Shared, live-reloaded agent-type map: `subagent_type name -> AgentTypeDefinition`.
/// The inner `Arc<HashMap>` is swapped wholesale on reload (never mutated in
/// place), so a reader that clones it out under a brief read lock never sees
/// a half-updated map — see `SharedAgentTypeCatalog`/`AgentCatalog`.
pub(crate) type SharedAgentTypeMap =
    Arc<std::sync::RwLock<Arc<std::collections::HashMap<String, AgentTypeDefinition>>>>;

pub struct SharedAgentTypeCatalog {
    pub(super) agent_types: SharedAgentTypeMap,
    // Kept alive only for its background thread.
    pub(super) _watcher: Option<Arc<crate::type_watcher::AgentTypeWatcher>>,
}

impl SharedAgentTypeCatalog {
    /// Merge `dirs`/`plugin_types` into the initial catalog and start
    /// watching `dirs` for `.md` changes. `notify` setup failing degrades to
    /// a build-time-only catalog (logged, non-fatal), as a session's own
    /// catalog does.
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
    pub fn build(
        dirs: &[(std::path::PathBuf, AgentTypeSource)],
        plugin_types: &[AgentTypeDefinition],
        files: &PromptFiles,
    ) -> Self {
        Self::build_with(dirs, plugin_types, files, true)
    }

    /// [`build`](Self::build) without the watcher — for a host that already
    /// watches these directories and calls [`refresh`](Self::refresh)
    /// itself. One `notify` stream per catalog is fine for one catalog; a
    /// host building one per project reaches the backend's per-process
    /// ceiling, and on macOS that is an abort rather than an error.
    pub fn unwatched(
        dirs: &[(std::path::PathBuf, AgentTypeSource)],
        plugin_types: &[AgentTypeDefinition],
        files: &PromptFiles,
    ) -> Self {
        Self::build_with(dirs, plugin_types, files, false)
    }

    pub(super) fn build_with(
        dirs: &[(std::path::PathBuf, AgentTypeSource)],
        plugin_types: &[AgentTypeDefinition],
        files: &PromptFiles,
        watch: bool,
    ) -> Self {
        let merged = merge_agent_types(dirs, plugin_types, files);
        let agent_types = Arc::new(std::sync::RwLock::new(Arc::new(merged)));
        if !watch {
            return Self {
                agent_types,
                _watcher: None,
            };
        }
        let _watcher = crate::type_watcher::AgentTypeWatcher::watch(
            dirs.to_vec(),
            plugin_types.to_vec(),
            files.clone(),
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
    pub fn refresh(
        &self,
        dirs: &[(std::path::PathBuf, AgentTypeSource)],
        plugin_types: &[AgentTypeDefinition],
        files: &PromptFiles,
    ) {
        *self
            .agent_types
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Arc::new(merge_agent_types(dirs, plugin_types, files));
    }

    /// Handle to pass to `AgentsModule::shared_agent_types` for each session.
    pub fn handle(&self) -> SharedAgentTypeMap {
        self.agent_types.clone()
    }
}

/// A session's agent types: a catalog that may be swapped in place by a
/// watcher, with this session's plugins' types over it.
pub(crate) struct AgentCatalog {
    types: SharedAgentTypeMap,
    /// Kept alive only so the watcher's thread keeps running.
    _watcher: Option<Arc<crate::type_watcher::AgentTypeWatcher>>,
    session_types: std::collections::HashMap<String, AgentTypeDefinition>,
    /// Computed once: `Tool::description` hands out a borrowed `&str`, which
    /// cannot come from behind the catalog's lock. A type added on disk later
    /// is usable at once but not listed until the next rebuild.
    description: Arc<str>,
}

impl AgentCatalog {
    /// Scanned from `dirs` (low → high priority) over the built-in types and
    /// `plugin_types`, and watched for changes. A watcher that cannot start
    /// leaves the catalog as it was built, as `SkillManager::enable_watching`
    /// does.
    pub(crate) fn watched(
        dirs: &[(std::path::PathBuf, AgentTypeSource)],
        plugin_types: &[AgentTypeDefinition],
        files: &PromptFiles,
    ) -> Self {
        let types: SharedAgentTypeMap = Arc::new(std::sync::RwLock::new(Arc::new(
            merge_agent_types(dirs, plugin_types, files),
        )));
        let watcher = crate::type_watcher::AgentTypeWatcher::watch(
            dirs.to_vec(),
            plugin_types.to_vec(),
            files.clone(),
            types.clone(),
        )
        .map(Arc::new)
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to enable agent-type file watching; live reload disabled for this session");
        })
        .ok();
        Self::over(types, watcher, &[])
    }

    /// A catalog the host keeps and watches for all its sessions, with
    /// `session_types` over it: above built-in and package types, below the
    /// ones on disk.
    pub(crate) fn shared(map: SharedAgentTypeMap, session_types: &[AgentTypeDefinition]) -> Self {
        Self::over(map, None, session_types)
    }

    fn over(
        types: SharedAgentTypeMap,
        _watcher: Option<Arc<crate::type_watcher::AgentTypeWatcher>>,
        session_types: &[AgentTypeDefinition],
    ) -> Self {
        let session_types: std::collections::HashMap<_, _> = session_types
            .iter()
            .map(|d| (d.name.clone(), d.clone()))
            .collect();
        let mut visible = types
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .clone();
        for (name, definition) in &session_types {
            if visible.get(name).is_none_or(|d| !d.source.may_shadow()) {
                visible.insert(name.clone(), definition.clone());
            }
        }
        Self {
            types,
            _watcher,
            session_types,
            description: describe_agent_types(&visible).into(),
        }
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    /// One type, cloned out from behind the lock: the watcher may swap the
    /// map at any moment, and a borrow into the old one would dangle.
    pub(crate) fn get(&self, name: &str) -> Option<AgentTypeDefinition> {
        let shared = self
            .types
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned();
        match (shared, self.session_types.get(name)) {
            (Some(on_disk), _) if on_disk.source.may_shadow() => Some(on_disk),
            (_, Some(declared)) => Some(declared.clone()),
            (shared, None) => shared,
        }
    }
}

/// Render the one-line-per-type catalog injected into `AgentTool::description()`
/// so the model can see what `subagent_type` values are actually available
/// (built-in + any custom types loaded from `.agix/agents/`) without a
/// separate discovery call. Sorted by name for stable output.
pub(super) fn describe_agent_types(
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
pub(super) fn parse_agent_type_file(content: &str, path: &Path) -> Option<AgentTypeDefinition> {
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
    let mut permission_mode: Option<base::settings::PermissionMode> = None;
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
        source: AgentTypeSource::User,
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
/// `HostContracts::scene_registry` and `Inner::resolve_scene_id`. This is also how a
/// sub-agent recovers its parent's scene when no explicit
/// `Arc<dyn Scene>` was wired in (the daemon sets `Settings.paths.scope`
/// to `scene.id()`, so the scope *is* the scene id there). Unknown ids return
/// `None` so the caller can fall back rather than guessing.
pub(super) fn builtin_scene_by_id(id: &str) -> Option<Arc<dyn Scene>> {
    match id {
        "code" => Some(Arc::new(scene::code::CodeScene::default())),
        "harnix" => Some(Arc::new(scene::harnix::HarnixScene)),
        "mini" => Some(Arc::new(scene::mini::MiniScene)),
        _ => None,
    }
}

pub(super) const EXPLORE_PROMPT: &str = "\
You are a read-only file search specialist. Your job is to explore, find, and \
report — do NOT edit, write, or delete any files. Use Read/Glob/Grep/WebFetch/\
WebSearch/LSP tools to gather information. Return a concise structured summary \
with file paths and line references.";

pub(super) const PLAN_PROMPT: &str = "\
You are a software architect and planning specialist. Your job is to design \
implementation plans — do NOT write or edit any code. Use FileRead/Glob/Grep \
to explore the codebase. Produce a concrete, step-by-step plan with specific \
file paths, crate names, and implementation approach.";

pub(super) const GENERAL_PURPOSE_PROMPT: &str = "\nYou are a general-purpose AI coding agent. Execute the user's request thoroughly.\nUse tools to read, edit, and search code. Report findings clearly.\nFocus on correctness and completeness.";

pub(super) const CLAUDE_PROMPT: &str = "\nYou are Claude, an AI assistant. Execute the user's request thoroughly.\nUse tools as needed. Report findings clearly and concisely.";

pub(super) const CODE_REVIEWER_PROMPT: &str = "\
You are a code reviewer. Your job is to review code diffs for correctness, \
performance, and style issues. Use Read/Grep/Glob/LSP to examine the codebase \
and Bash for read-only inspection commands (e.g. git diff, cargo check, \
rustfmt --check). Report findings with specific file paths and line references. \
Do NOT make any edits.";

pub(super) const WORKER_PROMPT: &str = "\nYou are a worker agent in a team. Execute the assigned task precisely.\nReport results concisely. Do not deviate from the assigned scope.";
