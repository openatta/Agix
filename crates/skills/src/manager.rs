//! Skill manager — runtime loading, listing, and reloading of skills.
//!
//! v2: delegates YAML frontmatter parsing to `base::frozen::frontmatter::parse_skill_file`
//! for full field coverage (all 15+ fields). Uses `base::frozen::skill::SkillEntry` as the canonical
//! parsed type; `SkillInfo` is the runtime wrapper with cached description for prompt assembly.

use base::frozen::frontmatter::parse_skill_file;
use base::frozen::skill::{SkillEntry, SkillSource as FrozenSkillSource};
use base::interface::skill_provider::{SkillPrecedence, SkillProvider};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Metadata for a loaded skill at runtime.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// Additional trigger context appended to `description` in the model-
    /// facing listing (e.g. example phrasings) — was parsed into
    /// `SkillEntry` but never carried through to this runtime-facing struct
    /// until the listing actually needed it.
    pub when_to_use: Option<String>,
    pub source: SkillSource,
    pub path: PathBuf,
    // -- Extended fields --
    /// Restrict tools this skill can invoke (whitelist of tool names)
    pub allowed_tools: Option<Vec<String>>,
    /// Tools removed from the pool while this skill is active (denylist)
    pub disallowed_tools: Option<Vec<String>>,
    /// Named positional arguments for `$name` substitution
    pub arguments: Option<Vec<String>>,
    /// Override model for this skill
    pub model: Option<String>,
    /// Effort/thinking-mode override while this skill is active
    pub effort: Option<String>,
    /// Execution context: "fork" (sub-agent in worktree) or "inline" (default)
    pub context: Option<String>,
    /// Which agent type to run as when `context: "fork"` is set
    pub agent: Option<String>,
    /// `false` waits for the forked result in the invoking turn; default `true`
    pub background: Option<bool>,
    /// Hint shown to user for arguments (e.g. "commit message")
    pub argument_hint: Option<String>,
    /// Glob patterns for conditional activation
    pub paths: Option<Vec<String>>,
    /// If true, model cannot invoke this skill directly (only user via slash)
    pub disable_model_invocation: bool,
    /// If false, skill is hidden from user-facing slash command list
    pub user_invocable: bool,
    /// Version string
    pub version: Option<String>,
}

impl From<SkillEntry> for SkillInfo {
    fn from(e: SkillEntry) -> Self {
        SkillInfo {
            name: e.name,
            description: e.description,
            when_to_use: e.when_to_use,
            source: SkillSource::from(e.source),
            path: e.path,
            allowed_tools: e.allowed_tools,
            disallowed_tools: e.disallowed_tools,
            arguments: e.arguments,
            model: e.model,
            effort: e.effort,
            context: e.context,
            agent: e.agent,
            background: e.background,
            argument_hint: e.argument_hint,
            paths: e.paths,
            disable_model_invocation: e.disable_model_invocation,
            user_invocable: e.user_invocable,
            version: e.version,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    User,
    Project,
    Plugin,
}

impl SkillSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillSource::User => "user",
            SkillSource::Project => "project",
            SkillSource::Plugin => "plugin",
        }
    }
}

impl From<FrozenSkillSource> for SkillSource {
    fn from(s: FrozenSkillSource) -> Self {
        match s {
            FrozenSkillSource::User => SkillSource::User,
            FrozenSkillSource::Project => SkillSource::Project,
            FrozenSkillSource::Plugin => SkillSource::Plugin,
        }
    }
}

/// A skill's invocation history within this `SkillManager`'s lifetime
/// (i.e. session-scoped, not persisted across process restarts — the
/// listing budget this feeds is itself a per-session heuristic, not a
/// cross-session preference). `last_seq` is a monotonic
/// counter rather than a wall-clock timestamp deliberately: recency only
/// needs a total order among invocations, and a counter is exact + free of
/// the timestamp-based replay non-determinism this session already hit once.
#[derive(Debug, Clone, Copy, Default)]
pub struct InvocationStats {
    pub count: u32,
    pub last_seq: u64,
}

/// Manages loaded skills at runtime. Skills are .md files with YAML frontmatter.
pub struct SkillManager {
    skills: RwLock<HashMap<String, SkillInfo>>,
    watcher: RwLock<Option<std::sync::Arc<crate::watcher::SkillWatcher>>>,
    /// This manager's read cursor into `watcher`'s change log — see
    /// `SkillWatcher::check_and_reload_since`. Only meaningful when
    /// `watcher` is `Some`; irrelevant (and unused) otherwise.
    watcher_last_seen: std::sync::atomic::AtomicU64,
    /// Keyed by skill name (not tied to a particular `SkillInfo` instance,
    /// so a reload — which replaces the `SkillInfo` wholesale — doesn't
    /// wipe the count for a skill whose content just changed).
    invocation_stats: RwLock<HashMap<String, InvocationStats>>,
    invocation_seq: std::sync::atomic::AtomicU64,
    /// The sources this manager loaded from, kept so a skill whose content
    /// lives in its source rather than on disk can still be expanded — see
    /// `get_skill_content`.
    providers: RwLock<Vec<std::sync::Arc<dyn SkillProvider>>>,
}

impl SkillManager {
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
            watcher: RwLock::new(None),
            watcher_last_seen: std::sync::atomic::AtomicU64::new(0),
            invocation_stats: RwLock::new(HashMap::new()),
            invocation_seq: std::sync::atomic::AtomicU64::new(0),
            providers: RwLock::new(Vec::new()),
        }
    }

    /// Record that `name` was just invoked (via the Skill tool, a slash
    /// command, or a subagent preload) — call on success only, not on a
    /// failed/not-found lookup. Feeds `recall_priority` and, transitively,
    /// `build_skills_text`'s budget-overflow drop order.
    pub fn record_invocation(&self, name: &str) {
        let seq = self
            .invocation_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let mut stats = self.invocation_stats.write().unwrap();
        let entry = stats.entry(name.to_string()).or_default();
        entry.count += 1;
        entry.last_seq = seq;
    }

    /// Total times `name` has been invoked this session. `0` for a
    /// never-invoked skill. Deliberately **count**, not recency — the
    /// listing-overflow drop order is by invocation frequency, a different
    /// metric from the most-recent-invocation ordering compaction
    /// reattachment uses (see `last_invoked_seq`). Feeds
    /// `build_skills_text`'s drop order.
    pub fn invocation_count(&self, name: &str) -> u32 {
        self.invocation_stats
            .read()
            .unwrap()
            .get(name)
            .map(|s| s.count)
            .unwrap_or(0)
    }

    /// Monotonic sequence number of `name`'s most recent invocation, `0` if
    /// never invoked. Higher = more recent; used to order compaction
    /// reattachment (most-recently-invoked skill's content restored first,
    /// within the shared token budget), a different axis from
    /// `invocation_count`'s frequency-based listing-drop order.
    pub fn last_invoked_seq(&self, name: &str) -> u64 {
        self.invocation_stats
            .read()
            .unwrap()
            .get(name)
            .map(|s| s.last_seq)
            .unwrap_or(0)
    }

    /// Take everything a source has, and keep the source for the bodies it
    /// may hold. Returns how many skills this call put in the catalog — a
    /// source that only fills gaps does not count the names it deferred on.
    pub fn load_from(&self, provider: std::sync::Arc<dyn SkillProvider>) -> usize {
        let entries = provider.skills();
        let overrides = provider.precedence() == SkillPrecedence::Override;
        let mut count = 0;
        {
            let mut skills = self.skills.write().unwrap();
            for entry in entries {
                let name = entry.name.clone();
                match skills.entry(name) {
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(SkillInfo::from(entry));
                        count += 1;
                    }
                    std::collections::hash_map::Entry::Occupied(mut e) if overrides => {
                        e.insert(SkillInfo::from(entry));
                        count += 1;
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {}
                }
            }
        }
        self.providers.write().unwrap().push(provider);
        count
    }

    /// Load skills from a directory (user skills: ~/.agix/scenes/<scope>/skills/;
    /// project skills: <cwd>/.agents/skills/).
    /// Each .md file is a skill; filename (without .md) is the skill name.
    pub fn load_dir(&self, dir: &Path, source: SkillSource) -> std::io::Result<usize> {
        Ok(
            self.load_from(std::sync::Arc::new(crate::sources::SkillDirectory::flat(
                dir, source,
            ))),
        )
    }

    /// Load skills from a directory using SKILL.md subdirectory format.
    /// Each subdirectory containing a SKILL.md becomes a skill.
    pub fn load_dir_subdirs(&self, dir: &Path, source: SkillSource) -> std::io::Result<usize> {
        Ok(
            self.load_from(std::sync::Arc::new(crate::sources::SkillDirectory::tier(
                dir, source,
            ))),
        )
    }

    /// Take `fresh`'s catalog and sources in place of this one's, keeping
    /// the invocation history. Everything holding this manager sees the new
    /// catalog at once.
    pub fn replace_catalog(&self, fresh: SkillManager) {
        let SkillManager {
            skills, providers, ..
        } = fresh;
        *self.skills.write().unwrap() = skills.into_inner().unwrap();
        *self.providers.write().unwrap() = providers.into_inner().unwrap();
    }

    /// Register a bundled (in-memory) skill. Used for built-in skills.
    /// Disk-loaded skills with the same name take priority (skip).
    pub fn register_bundled(&self, entry: SkillEntry) {
        let mut skills = self.skills.write().unwrap();
        let name = entry.name.clone();
        // Disk skills take priority — only register if not already loaded
        skills.entry(name).or_insert_with(|| SkillInfo::from(entry));
    }

    /// Discover skills by walking up directory tree from given file paths.
    /// For each path, walks up to find `skills/` directories
    /// containing SKILL.md files relevant to the path.
    pub fn discover_for_paths(&self, paths: &[PathBuf]) -> Vec<SkillInfo> {
        if paths.is_empty() {
            return Vec::new();
        }
        let mut discovered = Vec::new();
        let mut seen_dirs = HashSet::new();
        for path in paths {
            let mut current = if path.is_dir() {
                path.clone()
            } else {
                path.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            };
            // Walk up to filesystem root
            loop {
                let candidate = current.join("skills");
                if candidate.is_dir() && seen_dirs.insert(candidate.clone()) {
                    if let Ok(entries) = std::fs::read_dir(&candidate) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            if p.is_dir() {
                                let skill_md = p.join("SKILL.md");
                                if skill_md.is_file() {
                                    let dir_name = p
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("unknown")
                                        .to_string();
                                    let info = Self::load_skill_at_path(
                                        &skill_md,
                                        &dir_name,
                                        SkillSource::Project,
                                    );
                                    if let Some(info) = info {
                                        discovered.push(info);
                                    }
                                }
                            }
                        }
                    }
                }
                match current.parent() {
                    Some(parent) if parent != current => current = parent.to_path_buf(),
                    _ => break,
                }
            }
        }
        discovered
    }

    /// List all loaded skills, sorted by name.
    ///
    /// Sorted, not insertion/hash order: the backing store is a `HashMap`,
    /// whose iteration order is randomized per-process (different every run,
    /// same skill set or not) — every consumer of this list renders it
    /// straight into user/model-facing text (`turn.rs::build_skills_text`
    /// injects it verbatim into the system prompt, `agent.rs`'s `/skills`
    /// command, the `SkillTool` lookup). An unsorted list meant the system
    /// prompt's "## Available Skills" section came out in a different order
    /// every single process start — which, since the request carries
    /// the full prompt text, silently changed the very first
    /// turn between any two runs of the exact same test case.
    /// Provider-side
    /// prompt caching (most APIs cache by exact prefix match) would have the
    /// same problem in production, independent of testing.
    pub fn list(&self) -> Vec<SkillInfo> {
        let mut skills: Vec<SkillInfo> = self.skills.read().unwrap().values().cloned().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str) -> Option<SkillInfo> {
        self.skills.read().unwrap().get(name).cloned()
    }

    /// Get the full content of a skill file (for prompt expansion at invocation time).
    ///
    /// Bundled and MCP-derived skills use sentinel `path` values —
    /// `(bundled:name)` / `(mcp:server:tool)` — not real filesystem paths;
    /// their bodies live in-memory instead (see `crate::bundled::bundled_body`
    /// / `crate::mcp_builder::mcp_skill_body`). Mirrors the same special-
    /// casing `runtime::commands::expand_skill_for_command` already does for
    /// the slash-command path — this is the counterpart for the model-facing
    /// `Skill` tool, which was previously always `None` for both cases.
    pub fn get_skill_content(&self, name: &str) -> Option<String> {
        let info = self.get(name)?;
        let path_str = info.path.to_string_lossy();
        if path_str.starts_with("(bundled:") {
            return crate::bundled::bundled_body(name).map(|s| s.to_string());
        }
        if path_str.starts_with("(mcp:") {
            return crate::mcp_builder::mcp_skill_body(name);
        }
        // Newest source first: a source registered later is the one whose
        // entry is in the catalog if two of them claimed the same name.
        for provider in self.providers.read().unwrap().iter().rev() {
            if let Some(body) = provider.body(name) {
                return Some(body);
            }
        }
        std::fs::read_to_string(&info.path).ok()
    }

    /// Generate a prompt for a skill invocation with arguments.
    /// Substitutes `{args}` placeholder in the skill content.
    pub fn expand_skill(&self, name: &str, args: &str) -> Option<String> {
        let content = self.get_skill_content(name)?;
        let expanded = content.replace("{args}", args);
        Some(expanded)
    }

    /// Activate skills whose `paths` patterns match any of the given `file_paths`.
    ///
    /// For each loaded skill with a non-empty `paths` field, checks whether any
    /// of the supplied file paths match the gitignore-style glob patterns.
    /// Returns the matching skills so the caller can inject them into context.
    ///
    /// Pattern rules (same as `.gitignore`):
    /// - `*`  matches any sequence of characters except `/`
    /// - `**` matches any sequence including `/`
    /// - `?`  matches any single character except `/`
    /// - Leading `/` anchors the pattern to the root of the path
    /// - Trailing `/` is ignored for file-path matching
    pub fn activate_conditional_skills_for_paths(&self, file_paths: &[PathBuf]) -> Vec<SkillInfo> {
        if file_paths.is_empty() {
            return Vec::new();
        }

        let skills = self.skills.read().unwrap();
        let mut activated = Vec::new();

        'skill: for info in skills.values() {
            let Some(patterns) = &info.paths else {
                continue;
            };
            if patterns.is_empty() {
                continue;
            }

            // Build a GlobSet once per skill using GlobSetBuilder.
            let mut gb = globset::GlobSetBuilder::new();
            for p in patterns {
                // Strip leading '/' for anchored gitignore patterns; globset
                // handles the rest with literal_separator = true.
                let trimmed = p.strip_prefix('/').unwrap_or(p);
                if let Ok(glob) = globset::GlobBuilder::new(trimmed)
                    .literal_separator(true)
                    .build()
                {
                    gb.add(glob);
                }
            }
            let Ok(glob_set) = gb.build() else { continue };

            for fp in file_paths {
                let p_str = fp.to_string_lossy();
                if glob_set.is_match(p_str.as_ref()) {
                    activated.push(info.clone());
                    continue 'skill;
                }
            }
        }

        activated
    }

    /// Build a prompt block listing all loaded skills with their descriptions.
    pub fn build_skills_prompt(&self) -> Option<String> {
        let skills = self.list();
        if skills.is_empty() {
            return None;
        }
        let mut prompt = String::from("## Available Skills\n\n");
        for s in &skills {
            prompt.push_str(&format!("- **{}**: {}\n", s.name, s.description));
        }
        Some(prompt)
    }

    /// Parse a SKILL.md file at the given path, using the core frontmatter parser.
    fn load_skill_at_path(path: &Path, dir_name: &str, source: SkillSource) -> Option<SkillInfo> {
        let content = std::fs::read_to_string(path).ok()?;
        let frozen_source = match source {
            SkillSource::User => FrozenSkillSource::User,
            SkillSource::Project => FrozenSkillSource::Project,
            SkillSource::Plugin => FrozenSkillSource::Plugin,
        };
        let entry = parse_skill_file(&content, dir_name.to_string(), path, frozen_source)?;
        Some(SkillInfo::from(entry))
    }

    // ── File-watching support ──

    /// Enable file watching on the given directories.
    ///
    /// Starts a background [`SkillWatcher`](crate::watcher::SkillWatcher) that
    /// monitors the provided paths for SKILL.md / *.md changes. Use
    /// [`check_for_changes`](Self::check_for_changes) periodically to apply
    /// any pending reloads.
    ///
    /// Can be called multiple times — only the most recent set of watch paths
    /// is active; previous watcher is replaced.
    ///
    /// Starts a `SkillWatcher` owned solely by this manager, i.e. its own
    /// dedicated `notify` background thread. Fine for a single standalone
    /// manager; when many `SkillManager`s watch the same directories (e.g.
    /// one per session in a daemon, all watching the same global/scene/
    /// project skill dirs), prefer [`attach_watcher`](Self::attach_watcher)
    /// with one watcher shared across all of them instead of paying for a
    /// thread per manager.
    pub fn enable_watching(&self, paths: &[std::path::PathBuf]) -> Result<(), String> {
        let mut watcher = crate::watcher::SkillWatcher::new();
        watcher.watch_skills(paths)?;
        self.watcher_last_seen
            .store(0, std::sync::atomic::Ordering::Release);
        *self.watcher.write().unwrap() = Some(std::sync::Arc::new(watcher));
        Ok(())
    }

    /// Attach to a `SkillWatcher` owned and started elsewhere (typically a
    /// pool-level singleton watching the same directories this manager was
    /// loaded from), instead of starting a private one. This manager's read
    /// cursor starts at the watcher's *current* generation, not `0` — this
    /// manager's skills were just loaded from a fresh disk scan, so it has
    /// no need to replay change history that predates the attachment.
    pub fn attach_watcher(&self, watcher: std::sync::Arc<crate::watcher::SkillWatcher>) {
        self.watcher_last_seen.store(
            watcher.current_generation(),
            std::sync::atomic::Ordering::Release,
        );
        *self.watcher.write().unwrap() = Some(watcher);
    }

    /// Poll the file watcher and reload any skills whose files have changed.
    ///
    /// This is a lightweight call (essentially a Mutex lock + iteration) safe
    /// to run at the start of each turn. Returns the number of skills reloaded.
    pub fn check_for_changes(&self) -> usize {
        let guard = self.watcher.read().unwrap();
        match guard.as_ref() {
            Some(w) => w.check_and_reload_since(self, &self.watcher_last_seen),
            None => 0,
        }
    }

    /// Register MCP-derived skills for a server's tools.
    ///
    /// Each MCP tool becomes a skill named `mcp__{server}__{tool}` that
    /// users can invoke via slash command. Skills are registered with
    /// `user_invocable: true` so they appear in `/skills`.
    ///
    /// Returns the number of skills that were actually registered (not
    /// counting duplicates already present from disk).
    pub fn register_mcp_skills(
        &self,
        server_name: &str,
        tools: &[base::interface::model::ToolDef],
    ) -> usize {
        self.load_from(std::sync::Arc::new(crate::sources::McpSkills::new(
            server_name,
            tools.to_vec(),
        )))
    }

    /// Reload a single skill from its file path.
    ///
    /// Handles both subdirectory format (`skills/<name>/SKILL.md`) and flat
    /// format (`<name>.md`). If the file no longer exists on disk, the skill
    /// is removed from the cache. Returns an error if the path is not a valid
    /// skill file or parsing fails.
    pub fn reload_skill(&self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            let target = canonicalize_lossy(path);
            let mut skills = self.skills.write().unwrap();
            skills.retain(|_, info| canonicalize_lossy(&info.path) != target);
            return Ok(());
        }

        // Determine skill name from path
        let (name, _file_name) = if path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
            // Subdirectory format: skills/<name>/SKILL.md
            let name = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .ok_or_else(|| format!("Cannot determine skill name: {}", path.display()))?
                .to_string();
            (name, "SKILL.md".to_string())
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            // Flat format: <name>.md
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| format!("Cannot determine skill name: {}", path.display()))?
                .to_string();
            let fname = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown.md")
                .to_string();
            (name, fname)
        } else {
            return Err(format!("Not a skill file: {}", path.display()));
        };

        // Determine source from existing entry, default to User
        let source = {
            let skills = self.skills.read().unwrap();
            skills
                .get(&name)
                .map(|s| s.source)
                .unwrap_or(SkillSource::User)
        };
        let frozen_source = match source {
            SkillSource::User => FrozenSkillSource::User,
            SkillSource::Project => FrozenSkillSource::Project,
            SkillSource::Plugin => FrozenSkillSource::Plugin,
        };

        let content =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read skill: {e}"))?;
        let entry = parse_skill_file(&content, name.clone(), path, frozen_source)
            .ok_or_else(|| format!("Failed to parse skill file: {}", path.display()))?;

        let mut skills = self.skills.write().unwrap();
        skills.insert(name, SkillInfo::from(entry));
        Ok(())
    }
}

/// Resolve `p` to a comparable canonical form even when `p` itself no longer
/// exists on disk (the delete case: `fs::canonicalize` requires the full
/// path to exist). Walks up to the nearest existing ancestor, canonicalizes
/// that, then reattaches the (possibly-gone) suffix. Falls back to `p`
/// unchanged if no ancestor exists (e.g. already deleted down to the root).
///
/// Needed because on macOS `/tmp` is a symlink to `/private/tmp`, and the
/// `notify` crate's FSEvents backend reports delete events using the
/// resolved `/private/tmp/...` form regardless of which form was passed to
/// `watch()` — while the initial directory scan (`load_dir`/
/// `load_dir_subdirs`) stores whatever unresolved form the caller's project
/// root was built from. Without normalizing both sides, a deleted skill's
/// watcher event never string-matches its stored `SkillInfo::path`, so
/// `reload_skill`'s "file's gone, evict it" branch silently no-ops.
fn canonicalize_lossy(p: &Path) -> PathBuf {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(canon) = cur.canonicalize() {
            let mut result = canon;
            for part in suffix.iter().rev() {
                result.push(part);
            }
            return result;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                suffix.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

impl Default for SkillManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Regression test for a real production bug: every startup skill scan
    /// (`runtime::agent::Builder::build()`'s three call sites,
    /// `Agent::warmup()`'s three, and `daemon::session_pool::build_shared_commands`'s
    /// three — nine call sites total) used to call `load_dir`, which only
    /// recognizes flat `<name>.md` files. The `<name>/SKILL.md` subdirectory
    /// convention — what this project's own template fixture and
    /// `discover_for_paths`/`reload_skill` already supported — was silently
    /// invisible at startup: such a skill only
    /// became invocable after a live-reload watcher event fired for it once
    /// (see `reload_skill`'s doc comment, which already handled both
    /// formats). Found via replay of a real test case: the fixture
    /// project's `code-review/SKILL.md` skill never appeared in any recorded
    /// system prompt. `load_dir_subdirs` is the fix — it handles both
    /// formats in one pass — and is now what every production call site uses.
    #[test]
    fn load_dir_subdirs_finds_skill_md_in_a_named_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("code-review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut f = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        writeln!(
            f,
            "---\nname: code-review\ndescription: Review code changes.\n---\n\nBody."
        )
        .unwrap();

        let mgr = SkillManager::new();
        // The flat-only loader must NOT find it — this is the exact bug.
        mgr.load_dir(dir.path(), SkillSource::Project).unwrap();
        assert!(
            mgr.get("code-review").is_none(),
            "sanity: load_dir is flat-only and should miss the subdirectory-format skill"
        );

        let count = mgr
            .load_dir_subdirs(dir.path(), SkillSource::Project)
            .unwrap();
        assert_eq!(count, 1);
        let info = mgr
            .get("code-review")
            .expect("load_dir_subdirs must find skills/<name>/SKILL.md");
        assert_eq!(info.description, "Review code changes.");
    }

    #[test]
    fn invocation_count_and_last_invoked_seq_start_at_zero() {
        let mgr = SkillManager::new();
        assert_eq!(mgr.invocation_count("never-called"), 0);
        assert_eq!(mgr.last_invoked_seq("never-called"), 0);
    }

    #[test]
    fn record_invocation_increments_count_and_advances_seq() {
        let mgr = SkillManager::new();
        mgr.record_invocation("a");
        mgr.record_invocation("a");
        mgr.record_invocation("b");
        assert_eq!(mgr.invocation_count("a"), 2);
        assert_eq!(mgr.invocation_count("b"), 1);
        // "b" was recorded after "a"'s second call, so it's more recent —
        // count and recency are independent axes (frequency vs recency),
        // this is the case that distinguishes them: "a" has a higher count
        // but "b" has a higher (more recent) seq.
        assert!(mgr.last_invoked_seq("b") > mgr.last_invoked_seq("a"));
    }

    #[test]
    fn invocation_stats_survive_a_reload_of_the_same_named_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("code-review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: v1\n---\nBody v1.",
        )
        .unwrap();
        let mgr = SkillManager::new();
        mgr.load_dir_subdirs(dir.path(), SkillSource::Project)
            .unwrap();
        mgr.record_invocation("code-review");
        assert_eq!(mgr.invocation_count("code-review"), 1);

        // Reload (as the live-reload watcher's `reload_skill` would do)
        // replaces the `SkillInfo` instance wholesale — invocation stats
        // are keyed by name, not tied to that instance, so they survive.
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: v2\n---\nBody v2.",
        )
        .unwrap();
        mgr.reload_skill(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(mgr.get("code-review").unwrap().description, "v2");
        assert_eq!(mgr.invocation_count("code-review"), 1);
    }

    #[test]
    fn get_skill_content_resolves_bundled_sentinel_path() {
        // Regression test: a bundled skill's `path` is a `(bundled:name)`
        // sentinel, not a real filesystem path. `get_skill_content` used to
        // blindly `fs::read_to_string` it, which always failed — meaning
        // the model-facing Skill tool could never actually invoke any
        // bundled skill (e.g. "simplify"), even though it's explicitly
        // allowlisted as model-invocable.
        let mgr = SkillManager::new();
        for bundled in crate::bundled::bundled_skills() {
            mgr.register_bundled(bundled);
        }
        let content = mgr.get_skill_content("simplify");
        assert!(
            content.is_some(),
            "expected bundled skill content, got None"
        );
        let expected = crate::bundled::bundled_body("simplify").unwrap();
        assert_eq!(content.unwrap(), expected);
    }

    #[test]
    fn get_skill_content_resolves_mcp_sentinel_path() {
        let tool = base::interface::model::ToolDef {
            name: "search".into(),
            description: "search things".into(),
            input_schema: serde_json::json!({"type": "object"}),
            source: None,
        };
        let entries =
            crate::mcp_builder::build_skills_from_mcp("test-server-get-skill-content", &[tool]);
        let mgr = SkillManager::new();
        for entry in entries {
            mgr.register_bundled(entry);
        }
        let content = mgr.get_skill_content("mcp__test-server-get-skill-content__search");
        assert!(content.is_some(), "expected mcp skill content, got None");
        assert!(content.unwrap().contains("search"));
    }

    #[test]
    fn parse_skill_delegates_to_core_parser() {
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("my-skill.md");
        let mut f = std::fs::File::create(&skill_md).unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "description: A test skill for validation").unwrap();
        writeln!(f, "allowed-tools: [Bash, Read]").unwrap();
        writeln!(f, "model: sonnet").unwrap();
        writeln!(f, "argument_hint: file path").unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "# Test Skill Body").unwrap();
        drop(f);

        let info = SkillManager::load_skill_at_path(&skill_md, "my-skill", SkillSource::User);
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.name, "my-skill");
        assert_eq!(info.description, "A test skill for validation");
        assert_eq!(info.allowed_tools, Some(vec!["Bash".into(), "Read".into()]));
        assert_eq!(info.model, Some("sonnet".into()));
        assert_eq!(info.argument_hint, Some("file path".into()));
    }

    #[test]
    fn parse_skill_without_description_returns_none_for_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("empty.md");
        std::fs::write(&skill_md, "").unwrap();
        let info = SkillManager::load_skill_at_path(&skill_md, "empty", SkillSource::User);
        assert!(info.is_none());
    }

    #[test]
    fn parse_skill_body_line_is_fallback_description() {
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("body.md");
        std::fs::write(&skill_md, "# A body-only skill").unwrap();
        let info = SkillManager::load_skill_at_path(&skill_md, "body", SkillSource::User);
        assert!(info.is_some());
        assert_eq!(info.unwrap().description, "A body-only skill");
    }

    #[test]
    fn discover_for_paths_finds_skills() {
        let tmp = tempfile::tempdir().unwrap();
        // Create directory structure: project/skills/my-skill/SKILL.md
        let skills_dir = tmp.path().join("skills").join("my-skill");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let mut f = std::fs::File::create(skills_dir.join("SKILL.md")).unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "description: Discovered skill").unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "Body").unwrap();
        drop(f);

        let mgr = SkillManager::new();
        let discovered = mgr.discover_for_paths(&[tmp.path().to_path_buf()]);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].name, "my-skill");
    }

    #[test]
    fn register_bundled_respects_disk_priority() {
        let mgr = SkillManager::new();
        // Register a bundled skill
        let bundled = SkillEntry {
            name: "test-skill".into(),
            description: "bundled version".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:test-skill)"),
            ..Default::default()
        };
        mgr.register_bundled(bundled);
        assert_eq!(
            mgr.get("test-skill").unwrap().description,
            "bundled version"
        );

        // Load a disk skill with the same name — should override
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("test-skill.md");
        let mut f = std::fs::File::create(&skill_md).unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "description: disk version").unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "Body").unwrap();
        drop(f);

        mgr.load_dir(dir.path(), SkillSource::User).unwrap();
        assert_eq!(mgr.get("test-skill").unwrap().description, "disk version");
    }

    /// Regression: `list()` used to be `HashMap::values().collect()` — same
    /// skill set, different (randomized, per-process) text order every run.
    /// Since `turn.rs::build_skills_text` renders this straight into the
    /// system prompt, that silently changed the request for the very
    /// first turn of every replay. Register in
    /// deliberately unsorted order and assert the output is alphabetical
    /// regardless — a HashMap-order bug wouldn't reliably fail a single
    /// `cargo test` run (order is only *unstable across* runs, not
    /// necessarily "b before a" within one run), so this asserts the actual
    /// contract (`list()` is sorted) rather than trying to catch
    /// nondeterminism directly.
    #[test]
    fn list_is_sorted_by_name_regardless_of_registration_order() {
        let mgr = SkillManager::new();
        for name in ["zebra", "apple", "mango"] {
            mgr.register_bundled(SkillEntry {
                name: name.into(),
                description: format!("{name} skill"),
                source: FrozenSkillSource::User,
                path: PathBuf::from(format!("(bundled:{name})")),
                ..Default::default()
            });
        }
        let names: Vec<String> = mgr.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn activate_conditional_skills_matches_paths() {
        let mgr = SkillManager::new();

        // Register a skill with paths patterns
        let skill_with_paths = SkillEntry {
            name: "rust-helper".into(),
            description: "Rust file helper".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:rust-helper)"),
            paths: Some(vec!["**/*.rs".into(), "**/Cargo.toml".into()]),
            ..Default::default()
        };
        mgr.register_bundled(skill_with_paths);

        // Register a skill without paths — should never activate
        let skill_no_paths = SkillEntry {
            name: "general-helper".into(),
            description: "No path restriction".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:general-helper)"),
            paths: None,
            ..Default::default()
        };
        mgr.register_bundled(skill_no_paths);

        // Register a skill with empty paths — should never activate
        let skill_empty_paths = SkillEntry {
            name: "empty-paths".into(),
            description: "Empty paths".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:empty-paths)"),
            paths: Some(Vec::new()),
            ..Default::default()
        };
        mgr.register_bundled(skill_empty_paths);

        // Matching .rs files
        let result =
            mgr.activate_conditional_skills_for_paths(&[PathBuf::from("/tmp/project/src/main.rs")]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "rust-helper");

        // Matching Cargo.toml files
        let result =
            mgr.activate_conditional_skills_for_paths(&[PathBuf::from("/tmp/project/Cargo.toml")]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "rust-helper");

        // Non-matching paths
        let result =
            mgr.activate_conditional_skills_for_paths(&[PathBuf::from("/tmp/project/README.md")]);
        assert_eq!(result.len(), 0);

        // Empty file_paths returns empty
        let result = mgr.activate_conditional_skills_for_paths(&[]);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn activate_conditional_skills_matches_anchored_pattern() {
        let mgr = SkillManager::new();

        let skill = SkillEntry {
            name: "config-helper".into(),
            description: "Root config helper".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:config-helper)"),
            paths: Some(vec!["/.claude/*".into()]),
            ..Default::default()
        };
        mgr.register_bundled(skill);

        // With anchored pattern /.claude/* (stripped to .claude/*), globset
        // with literal_separator expects .claude as an immediate child of the
        // path root. Paths like /.claude/settings.json match because .claude
        // sits at the root of the path string.
        let result =
            mgr.activate_conditional_skills_for_paths(&[PathBuf::from(".claude/settings.json")]);
        assert_eq!(result.len(), 1);

        // No match: file not under .claude/
        let result =
            mgr.activate_conditional_skills_for_paths(&[PathBuf::from("/project/src/main.rs")]);
        assert_eq!(result.len(), 0);

        // Deep path with .claude is not matched by /.claude/* anchoring
        let result = mgr.activate_conditional_skills_for_paths(&[PathBuf::from(
            "/project/.claude/settings.json",
        )]);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn activate_conditional_skills_multiple_matches() {
        let mgr = SkillManager::new();

        let skill_rs = SkillEntry {
            name: "rust-helper".into(),
            description: "Rust helper".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:rust-helper)"),
            paths: Some(vec!["**/*.rs".into()]),
            ..Default::default()
        };
        let skill_md = SkillEntry {
            name: "md-helper".into(),
            description: "Markdown helper".into(),
            source: FrozenSkillSource::User,
            path: PathBuf::from("(bundled:md-helper)"),
            paths: Some(vec!["**/*.md".into()]),
            ..Default::default()
        };
        mgr.register_bundled(skill_rs);
        mgr.register_bundled(skill_md);

        let result = mgr.activate_conditional_skills_for_paths(&[
            PathBuf::from("/proj/src/lib.rs"),
            PathBuf::from("/proj/README.md"),
        ]);
        assert_eq!(result.len(), 2);
        let names: Vec<&str> = result.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"rust-helper"));
        assert!(names.contains(&"md-helper"));
    }

    /// Regression: `enable_watching`/`check_for_changes`/`SkillWatcher` were
    /// all fully implemented but `enable_watching` had no caller anywhere in
    /// production code, so a skill file edited after a session started was
    /// never picked up without a full session rebuild. This exercises the
    /// real `notify` watcher end to end — not just the reload logic in
    /// isolation — since that's exactly the part that was silently inert.
    #[test]
    fn editing_a_watched_skill_file_is_picked_up_without_a_manual_reload() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("watched-skill.md");
        std::fs::write(
            &skill_path,
            "---\ndescription: original description\n---\nOriginal body.",
        )
        .unwrap();

        let mgr = SkillManager::new();
        mgr.load_dir(dir.path(), SkillSource::User).unwrap();
        assert_eq!(
            mgr.get("watched-skill").unwrap().description,
            "original description"
        );

        mgr.enable_watching(&[dir.path().to_path_buf()])
            .expect("watcher setup should succeed in a test tempdir");

        // Edit the file after watching has started — this is the part that
        // used to have no effect at all on a running session.
        std::fs::write(
            &skill_path,
            "---\ndescription: updated description\n---\nUpdated body.",
        )
        .unwrap();

        // `notify` delivers events asynchronously via a background thread —
        // poll with a bounded timeout instead of a single fixed sleep, to
        // keep this fast on a quiet CI box without being flaky on a loaded one.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut picked_up = false;
        while std::time::Instant::now() < deadline {
            if mgr.check_for_changes() > 0 {
                picked_up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        assert!(
            picked_up,
            "watcher never reported the file change within 5s"
        );
        assert_eq!(
            mgr.get("watched-skill").unwrap().description,
            "updated description",
            "check_for_changes reported a change but the skill content wasn't actually reloaded"
        );
    }

    /// Two `SkillManager`s attached to the *same* shared `SkillWatcher`
    /// (the daemon `SessionPool` scenario `attach_watcher` exists for — one
    /// watcher thread instead of one per session) must both independently
    /// observe a change, in either poll order. A drain-based single-queue
    /// design would let whichever manager calls `check_for_changes` first
    /// consume the event, silently starving the other.
    #[test]
    fn two_managers_sharing_one_watcher_both_see_the_same_change() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("shared-watched-skill.md");
        std::fs::write(
            &skill_path,
            "---\ndescription: original description\n---\nOriginal body.",
        )
        .unwrap();

        let mgr_a = SkillManager::new();
        mgr_a.load_dir(dir.path(), SkillSource::User).unwrap();
        let mgr_b = SkillManager::new();
        mgr_b.load_dir(dir.path(), SkillSource::User).unwrap();

        let mut shared = crate::watcher::SkillWatcher::new();
        shared
            .watch_skills(&[dir.path().to_path_buf()])
            .expect("watcher setup should succeed in a test tempdir");
        let shared = std::sync::Arc::new(shared);
        mgr_a.attach_watcher(shared.clone());
        mgr_b.attach_watcher(shared);

        std::fs::write(
            &skill_path,
            "---\ndescription: updated description\n---\nUpdated body.",
        )
        .unwrap();

        // Poll `mgr_a` to completion first, fully "consuming" its view of the
        // change, *then* poll `mgr_b` — if the two were racing to drain one
        // shared queue, `mgr_b` would see nothing here.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut a_picked_up = false;
        while std::time::Instant::now() < deadline {
            if mgr_a.check_for_changes() > 0 {
                a_picked_up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            a_picked_up,
            "mgr_a never observed the file change within 5s"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut b_picked_up = false;
        while std::time::Instant::now() < deadline {
            if mgr_b.check_for_changes() > 0 {
                b_picked_up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            b_picked_up,
            "mgr_b never observed the file change within 5s — it was starved by mgr_a's poll"
        );

        assert_eq!(
            mgr_a.get("shared-watched-skill").unwrap().description,
            "updated description"
        );
        assert_eq!(
            mgr_b.get("shared-watched-skill").unwrap().description,
            "updated description"
        );
    }

    /// A manager that `attach_watcher`s to an *already-running* shared
    /// watcher must not replay history from before it attached — its own
    /// `load_dir` already reflects current disk state at attach time, so
    /// starting its cursor at `0` would cause a redundant (harmless but
    /// wasteful) reload of every change that watcher ever logged.
    #[test]
    fn attaching_to_a_running_watcher_does_not_replay_old_history() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let path_a = dir_a.path().join("early-skill.md");
        std::fs::write(&path_a, "---\ndescription: a\n---\nBody.").unwrap();
        let path_b = dir_b.path().join("late-skill.md");
        std::fs::write(&path_b, "---\ndescription: b\n---\nBody.").unwrap();

        let mut shared = crate::watcher::SkillWatcher::new();
        shared
            .watch_skills(&[dir_a.path().to_path_buf(), dir_b.path().to_path_buf()])
            .expect("watcher setup should succeed in a test tempdir");
        let shared = std::sync::Arc::new(shared);

        let early_mgr = SkillManager::new();
        early_mgr.load_dir(dir_a.path(), SkillSource::User).unwrap();
        early_mgr.attach_watcher(shared.clone());

        // Generate — and fully consume, via `early_mgr` — one change event
        // before `late_mgr` ever exists.
        std::fs::write(&path_a, "---\ndescription: a-updated\n---\nBody.").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && early_mgr.check_for_changes() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            early_mgr.get("early-skill").unwrap().description,
            "a-updated"
        );

        // `late_mgr` attaches only now — it should start at the *current*
        // generation, not `0`, so it must report zero reloads immediately.
        let late_mgr = SkillManager::new();
        late_mgr.load_dir(dir_b.path(), SkillSource::User).unwrap();
        late_mgr.attach_watcher(shared);
        assert_eq!(
            late_mgr.check_for_changes(),
            0,
            "a freshly-attached manager replayed pre-attachment history instead of starting at the current generation"
        );
    }
}
