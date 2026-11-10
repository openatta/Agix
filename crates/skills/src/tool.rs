//! SkillTool — invokes user-invocable skills by name, dispatching skill files
//! as expanded prompts.
//! v4: Returns expanded skill content via new_messages (injected as user messages),
//!     checks disable_model_invocation, uses full expand_skill_vars substitution.
//!     Supports forked execution via AgentSpawner when skill.context = "fork".

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::agent_spawner::{AgentSpawner, SpawnMode, SpawnRequest};
use base::interface::skill_catalog::SkillExpansion;
use base::tool::{
    ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolResultContent, ToolSpec,
};
use serde_json::Value;
use std::sync::Arc;

/// A tool that invokes skills loaded from disk.
/// Skills are .md files in `~/.agix/skills/` (global default),
/// `~/.agix/scenes/<scene>/skills/` (scene override), and
/// `<project>/.agents/skills/` (project-level).
///
/// Any skill the `SkillManager` loaded is invocable — the only gate is each
/// skill's own `disable_model_invocation` frontmatter flag (checked in
/// `call()` below). There is deliberately no scene-level allow-list on top:
/// `disable_model_invocation` already covers "should the model be allowed
/// to call this," per-skill, which is the right granularity, and a coarser
/// list would block a project's own `.agents/skills/*.md`.
pub struct SkillTool {
    /// Reference to the skill manager for looking up and expanding skills.
    skill_manager: std::sync::Arc<crate::manager::SkillManager>,
    /// The built-in skills this session's scene lists; the others are not
    /// found here either.
    admission: crate::catalog::BundledAdmission,
    /// Agent spawner for forked skill execution (context: "fork").
    spawner: Option<Arc<dyn AgentSpawner>>,
    /// Real permission checker — held here (not read off `ToolContext`,
    /// which has no `permission` field and would mean threading it through
    /// all ~30 other `ToolContext` construction sites workspace-wide for a
    /// field only this one tool needs) so `allowed_tools` can inject
    /// temporary Allow rules. `None` when not wired up (mirrors `spawner`'s
    /// `None` default) — `allowed_tools` injection is simply skipped, same
    /// as `context: fork` silently staying inline without a spawner.
    permission: Option<Arc<dyn base::interface::permission::Permission>>,
    /// Per-session set of already-injected `(skill_name, rendered content)`
    /// hashes — when a re-invocation's rendered content exactly matches one
    /// already in this session, a short note is returned instead of a
    /// duplicate full copy. Keyed by `session_id`
    /// (from `ToolContext`) since one `SkillTool` instance is shared across
    /// however many sessions the process serves — a `SkillTool` isn't
    /// per-session itself, so this can't just be a bare `HashSet`.
    already_injected:
        std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<u64>>>,
}

impl SkillTool {
    pub fn new(skill_manager: std::sync::Arc<crate::manager::SkillManager>) -> Self {
        Self {
            skill_manager,
            admission: Default::default(),
            spawner: None,
            permission: None,
            already_injected: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn with_admission(mut self, admission: crate::catalog::BundledAdmission) -> Self {
        self.admission = admission;
        self
    }

    /// Set the agent spawner for forked skill execution.
    pub fn with_spawner(mut self, spawner: Arc<dyn AgentSpawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }

    /// Set the real permission checker, for `allowed_tools` rule injection.
    pub fn with_permission(
        mut self,
        permission: Arc<dyn base::interface::permission::Permission>,
    ) -> Self {
        self.permission = Some(permission);
        self
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }
    fn description(&self) -> &str {
        "Execute a skill within the main conversation. Skills are loaded from ~/.agix/skills/ (global), ~/.agix/scenes/<scene>/skills/ (scene override), and <project>/.agents/skills/ (project-level). Only skills listed in the user-invocable section are available."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The name of the skill to invoke"
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments to pass to the skill"
                }
            },
            "required": ["skill"]
        })
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/skill_tool.prompt.md").to_string()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let skill_name = input["skill"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("skill name required".into()))?;
        let args = input["args"].as_str().unwrap_or("");
        let not_found = || ToolError::NotFound(format!("Skill '{skill_name}' not found."));

        let info = self
            .skill_manager
            .get(skill_name)
            .filter(|s| self.admission.admits(&s.name, &s.path))
            .ok_or_else(not_found)?;
        if info.disable_model_invocation {
            return Err(ToolError::Denied(format!(
                "Skill '{skill_name}' has disable_model_invocation set. \
                 It can only be invoked by the user via slash command."
            )));
        }

        let expansion = match expand_for_invocation(
            &self.skill_manager,
            &info,
            args,
            &ctx,
            self.permission.as_deref(),
        )
        .await
        {
            Ok(expansion) => expansion,
            Err(ExpandError::NotFound) => return Err(not_found()),
            Err(ExpandError::Rejected(msg)) => {
                return Ok(ToolResult {
                    content: ToolResultContent::Text(msg),
                    is_error: true,
                    ..Default::default()
                });
            }
        };

        // A forked skill's sub-agent gets its own `Permission`, never this
        // one, so its `allowed-tools` grant nothing there; they only narrow
        // the tools it is offered.
        if info.context.as_deref() == Some("fork") {
            if let Some(ref spawner) = self.spawner {
                let request =
                    SpawnRequest::new(expansion.text, ctx.cwd.clone(), ctx.cancel.clone())
                        .agent_type(info.agent.clone())
                        .allowed_tools(allowed_tool_names(&info));
                if info.background == Some(true) {
                    let request = request.mode(SpawnMode::Background {
                        session: ctx.session.clone(),
                    });
                    return match spawner.spawn(request).await {
                        Ok(task_id) => {
                            self.skill_manager.record_invocation(skill_name);
                            Ok(ToolResult {
                                content: ToolResultContent::Text(format!(
                                    "Skill '{skill_name}' forked in the background \
                                     (task_id: {task_id}, status: spawned). Use TaskOutput \
                                     to check on it."
                                )),
                                is_error: false,
                                ..Default::default()
                            })
                        }
                        Err(e) => Ok(ToolResult {
                            content: ToolResultContent::Text(format!(
                                "Skill '{skill_name}' background fork failed: {e}"
                            )),
                            is_error: true,
                            ..Default::default()
                        }),
                    };
                }

                return match spawner.spawn(request).await {
                    Ok(output) => {
                        self.skill_manager.record_invocation(skill_name);
                        Ok(ToolResult {
                            content: ToolResultContent::Text(format!(
                                "Skill '{skill_name}' executed in forked agent.\n\n{output}"
                            )),
                            is_error: false,
                            ..Default::default()
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        content: ToolResultContent::Text(format!(
                            "Skill '{skill_name}' fork failed: {e}"
                        )),
                        is_error: true,
                        ..Default::default()
                    }),
                };
            }
        }

        // Wrapped in command tags so the model reads it as new instructions,
        // not as tool output.
        let skill_msg = format!(
            "<command-name>{skill_name}</command-name>\n<command-args>{args}</command-args>\n\n{}",
            expansion.text
        );

        self.skill_manager.record_invocation(skill_name);

        if let Some(permission) = &self.permission {
            for (tool_name, rule_content) in &expansion.grants {
                permission.add_temporary_allow(tool_name, rule_content.as_deref());
            }
        }

        let content_hash = hash_skill_content(&skill_msg);
        let already_loaded = {
            let mut sessions = self
                .already_injected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let seen = sessions.entry(ctx.session_id.clone()).or_default();
            !seen.insert(content_hash)
        };
        if already_loaded {
            return Ok(ToolResult {
                content: ToolResultContent::Text(format!(
                    "Skill '{skill_name}' is already loaded in this session with the same content — not re-injected."
                )),
                is_error: false,
                ..Default::default()
            });
        }

        Ok(ToolResult {
            content: ToolResultContent::Text(format!(
                "Skill '{skill_name}' invoked. The skill instructions have been loaded."
            )),
            is_error: false,
            new_messages: Some(vec![serde_json::json!({
                "role": "user",
                "content": skill_msg,
            })]),
            ..Default::default()
        })
    }
}

/// Why a skill could not be expanded.
#[derive(Debug)]
pub enum ExpandError {
    NotFound,
    /// Told to the model as the invocation's result.
    Rejected(String),
}

/// Character cap on a skill's expanded body; its `files:` have their own
/// budget on top.
const SKILL_BODY_CHAR_CAP: usize = 8000;

/// Expand a skill for one invocation — the expansion the `Skill` tool, a
/// forked skill and a `/name` command all share.
///
/// The body (without its frontmatter) has its `` !`cmd` `` placeholders
/// replaced first, over the raw text, before argument substitution; then it
/// is capped, and the `files:` it bundles are appended. The grants are its
/// `allowed-tools`, less any blanket grant its source may not make: a bare
/// tool name switches off confirmation for every call to that tool, which is
/// only safe from a skill the user put there. `<project>/.agents/skills/` is
/// repository content — attacker-supplied on a cloned repo — and a plugin
/// package is someone else's, so those only get grants that narrow the tool
/// to a content pattern (`Bash(cargo test:*)`) — not one that reaches as far
/// as the bare tool or out of the project (`Bash(b*)`, `Write(../**)`, see
/// `permissions::dangerous::is_unscoped_grant_in`). A project skill gets none at all while
/// the project is not trusted (`ctx.config.project_untrusted`).
pub async fn expand_for_invocation(
    manager: &crate::manager::SkillManager,
    info: &crate::manager::SkillInfo,
    args: &str,
    ctx: &ToolContext,
    permission: Option<&dyn base::interface::permission::Permission>,
) -> Result<SkillExpansion, ExpandError> {
    let bundled_files_text = build_bundled_files_injection(
        &info.path,
        &info.name,
        &*ctx.exec.filesystem,
        manager.prompt_files(),
    )
    .await
    .map_err(ExpandError::Rejected)?;
    let body = manager
        .get_skill_content(&info.name)
        .ok_or(ExpandError::NotFound)?;
    // Before the grants below take effect, on purpose: a skill's own
    // frontmatter does not get to approve its own commands.
    let body = expand_dynamic_context(&body, ctx, permission, info.source).await;
    let expanded = base::frozen::skill::expand_skill_vars_named(
        &body,
        args,
        info.arguments.as_deref().unwrap_or(&[]),
    );
    let capped = match expanded.char_indices().nth(SKILL_BODY_CHAR_CAP) {
        Some((byte_idx, _)) => format!("{}...[truncated]", &expanded[..byte_idx]),
        None => expanded,
    };

    let restricted = matches!(
        info.source,
        crate::manager::SkillSource::Project | crate::manager::SkillSource::Plugin
    );
    let withheld =
        info.source == crate::manager::SkillSource::Project && ctx.config.project_untrusted;
    if withheld && info.allowed_tools.as_ref().is_some_and(|t| !t.is_empty()) {
        tracing::warn!(
            skill = %info.name,
            "the project is not trusted; its skill's `allowed-tools` grant nothing until it is"
        );
    }
    let grants = info
        .allowed_tools
        .iter()
        .filter(|_| !withheld)
        .flatten()
        .filter_map(|entry| {
            let (tool_name, rule_content) = split_allowed_tool_entry(entry);
            if restricted
                && permissions::dangerous::is_unscoped_grant_in(tool_name, rule_content, &ctx.cwd)
            {
                tracing::warn!(
                    skill = %info.name,
                    source = info.source.as_str(),
                    tool = %tool_name,
                    "ignoring a blanket `allowed-tools` grant from a project or plugin skill; \
                     scope it as `Tool(pattern)` to a path inside the project, a program \
                     (`Bash(cargo test:*)`) or a registrable domain to have it honored"
                );
                return None;
            }
            Some((tool_name.to_string(), rule_content.map(str::to_string)))
        })
        .collect();

    Ok(SkillExpansion {
        text: format!("{capped}{bundled_files_text}"),
        grants,
    })
}

/// Per-file cap for bundled reference material (`files:` frontmatter) — see
/// `build_bundled_files_injection`.
const BUNDLED_FILE_CHAR_CAP: usize = 2000;
/// Total cap across all of a skill's bundled files combined, separate from
/// (and on top of) the skill body's own 8000-char cap.
const BUNDLED_TOTAL_CHAR_CAP: usize = 6000;

/// Resolves, safety-validates, reads, and budget-caps a skill's `files:`
/// frontmatter (reference material bundled alongside SKILL.md), returning
/// ready-to-append injection text on success.
///
/// `Err(message)` is returned — naming both the offending skill and the
/// declared path — when a declared file escapes the skill's own folder
/// (e.g. via `..`) or doesn't exist / can't be read. These are surfaced as
/// visible tool errors by the caller, never silently skipped.
///
/// `skill_path` is the skill's SKILL.md path (`SkillInfo::path`). This
/// re-reads and re-parses that file directly rather than widening
/// `SkillManager::list()`'s `SkillInfo` projection with a `files` field:
/// `SkillInfo` is a runtime-facing view of `SkillEntry` used across the
/// whole `skills` crate, and `SkillEntry` (which already carries `files`,
/// see `crates/core/src/frozen/skill.rs`) is cheaply reconstructed here with
/// a single read-plus-parse, cheaper than a cross-crate struct change for
/// one field only this call site needs. Bundled/MCP sentinel skills
/// (paths like `(bundled:name)` or `(mcp:...)`) simply fail this read and
/// fall through to "no bundled files," which is correct — those skills
/// have no real folder to bundle files alongside.
async fn build_bundled_files_injection(
    skill_path: &std::path::Path,
    skill_name: &str,
    fs: &dyn base::interface::exec::FileSystem,
    files: &base::paths::PromptFiles,
) -> Result<String, String> {
    let Ok(real_skill_path) = fs.canonicalize(skill_path).await else {
        return Ok(String::new());
    };
    if files.admit(skill_path, &real_skill_path).is_err() {
        return Ok(String::new());
    }
    let Ok(content) = fs.read_to_string(&real_skill_path).await else {
        return Ok(String::new());
    };
    let dir_name = skill_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(entry) = base::frozen::frontmatter::parse_skill_file(
        &content,
        dir_name,
        skill_path,
        base::frozen::skill::SkillSource::Project,
    ) else {
        return Ok(String::new());
    };

    let declared = entry.files.clone().unwrap_or_default();
    if declared.is_empty() {
        return Ok(String::new());
    }
    let candidates = entry.resolve_bundled_files();

    let Some(skill_dir) = skill_path.parent() else {
        return Ok(String::new());
    };
    // Canonicalize once: the root every candidate must stay inside.
    let Ok(canonical_skill_dir) = fs.canonicalize(skill_dir).await else {
        return Ok(String::new());
    };

    let mut out = String::new();
    let mut total_used = 0usize;
    for (relative, absolute) in declared.iter().zip(candidates.iter()) {
        if total_used >= BUNDLED_TOTAL_CHAR_CAP {
            out.push_str(&format!(
                "\n\n---\n## Bundled file: {relative}\n\n\
                 [skipped: total bundled-files budget ({BUNDLED_TOTAL_CHAR_CAP} chars) \
                 already reached by earlier files]\n"
            ));
            continue;
        }

        // Safety: the resolved real path must still be inside the skill's
        // own (canonicalized) folder — this is what rejects `..` traversal.
        // A failed canonicalize (file doesn't exist / not readable) is
        // reported the same way: a named, visible error, not a silent skip.
        let real_path = match fs.canonicalize(absolute).await {
            Ok(p) => p,
            Err(e) => {
                return Err(format!(
                    "Skill '{skill_name}' declares bundled file `{relative}` \
                     (resolved to {}) which does not exist or could not be read: {e}",
                    absolute.display()
                ));
            }
        };
        if !permissions::path_rule::is_within_root_lexical(&real_path, &canonical_skill_dir) {
            return Err(format!(
                "Skill '{skill_name}' declares bundled file `{relative}` which resolves \
                 outside the skill's own folder ({}): {} — rejected.",
                canonical_skill_dir.display(),
                real_path.display()
            ));
        }
        if let Err(why) = files.admit(absolute, &real_path) {
            return Err(format!(
                "Skill '{skill_name}' declares bundled file `{relative}`: {why}"
            ));
        }

        let file_content = match fs.read_to_string(&real_path).await {
            Ok(c) => c,
            Err(e) => {
                return Err(format!(
                    "Skill '{skill_name}' declares bundled file `{relative}` \
                     which could not be read: {e}"
                ));
            }
        };

        // Caps are char counts, not byte counts — slicing by raw byte index
        // (`file_content.len()` / `&file_content[..n]`) panics on non-ASCII
        // content whose cut point doesn't land on a UTF-8 char boundary
        // (very reachable here: skill reference docs in this repo are
        // routinely Chinese). `char_indices().nth(n)` finds the byte offset
        // of the n-th character, giving a boundary-safe cut.
        let per_file_cap = BUNDLED_FILE_CHAR_CAP.min(BUNDLED_TOTAL_CHAR_CAP - total_used);
        let (chunk, was_truncated) = match file_content.char_indices().nth(per_file_cap) {
            Some((byte_idx, _)) => (&file_content[..byte_idx], true),
            None => (file_content.as_str(), false),
        };
        total_used += chunk.chars().count();

        out.push_str(&format!("\n\n---\n## Bundled file: {relative}\n\n{chunk}"));
        if was_truncated {
            out.push_str("...[truncated]");
        }
        out.push('\n');
    }
    Ok(out)
}

/// Cheap, non-cryptographic hash of rendered skill content for the
/// already-injected dedup check — collision risk is irrelevant here (worst
/// case: a false "already loaded" skip, immediately visible and harmless,
/// not a security boundary).
fn hash_skill_content(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Split one `allowed-tools` frontmatter entry into `(tool name, content
/// pattern)`.
///
/// Accepts the same `Tool(pattern)` shape `settings.json`'s
/// `permission_rules` uses (`permissions::rule::parse_rule_string`), so a
/// skill can say `Bash(cargo test:*)` and get exactly that call approved
/// rather than every `Bash` call. A bare `Bash` yields `None` for the
/// pattern, which the caller treats as a blanket grant — see the call site
/// for why that is refused for project-supplied skills.
///
/// Deliberately lenient about malformed input: an entry with an unbalanced
/// paren is treated as a plain tool name rather than dropped, matching how
/// the rest of the skill frontmatter parser degrades. The caller's
/// project-source check is what makes that safe.
/// The tool names a skill's `allowed-tools` lists, without their content
/// patterns: `Bash(cargo test:*)` offers a fork `Bash`.
fn allowed_tool_names(info: &crate::manager::SkillInfo) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for entry in info.allowed_tools.iter().flatten() {
        let (name, _) = split_allowed_tool_entry(entry);
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn split_allowed_tool_entry(entry: &str) -> (&str, Option<&str>) {
    let entry = entry.trim();
    match (entry.find('('), entry.strip_suffix(')')) {
        (Some(open), Some(without_close)) if open > 0 => {
            let content = &without_close[open + 1..];
            if content.trim().is_empty() {
                (entry[..open].trim(), None)
            } else {
                (entry[..open].trim(), Some(content))
            }
        }
        _ => (entry, None),
    }
}

/// Whether a skill body holds a command its expansion would run.
pub(crate) fn runs_commands(body: &str) -> bool {
    base::frozen::skill::body_runs_commands(body)
}

const DISABLED_PLACEHOLDER: &str = "[shell command execution disabled by policy]";
const UNTRUSTED_PLACEHOLDER: &str = "[shell command not run: this project is not trusted]";

/// Dynamic context injection: `` !`command` `` (inline, `!` at start of
/// line or after whitespace only) and fenced ` ```!\n...\n``` ` blocks in a
/// skill body get replaced with the command's real output *before* the
/// model ever sees the skill content — preprocessing, not something the
/// model executes. Single pass: substituted output is not re-scanned for further
/// placeholders, so a command's own output can't trigger a second round of
/// injection.
///
/// Security posture: a skill body is untrusted input the same way a
/// model-generated Bash command is — a project skill is whatever the
/// repository says — so each command is held to what a model's `Bash` call
/// with that command would be: the session's `Permission` decides first,
/// then it runs through the real `BashTool::call()` with this invocation's
/// `ToolContext` (same sandbox, cwd, cancellation). A deny rule refuses it;
/// so does a call that would need the user's approval, because
/// preprocessing has no one to ask — only what the rules allow, or what
/// `Bash` itself lets through silently (a read-only command inside the
/// project), runs. The skill's own `allowed-tools` are not applied yet at
/// this point. With no `Permission` wired nothing runs.
///
/// Respects `ctx.config.disable_skill_shell_execution` — when set, every
/// placeholder is replaced with a fixed disabled-notice string instead of
/// running anything — and does the same for a project skill while the
/// project is not trusted (`ctx.config.project_untrusted`).
async fn expand_dynamic_context(
    body: &str,
    ctx: &ToolContext,
    permission: Option<&dyn base::interface::permission::Permission>,
    source: crate::manager::SkillSource,
) -> String {
    if !runs_commands(body) {
        return body.to_string();
    }

    #[allow(clippy::unwrap_used)]
    static FENCED: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?s)```!\n(.*?)\n```").unwrap());
    #[allow(clippy::unwrap_used)]
    static INLINE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // `!` at start-of-line or preceded by a space/tab only —
        // `KEY=!\`cmd\`` (no separating whitespace) is left as literal text.
        regex::Regex::new(r"(?m)(^|[ \t])!`([^`]*)`").unwrap()
    });

    let refusal = if ctx.config.disable_skill_shell_execution {
        Some(DISABLED_PLACEHOLDER)
    } else if source == crate::manager::SkillSource::Project && ctx.config.project_untrusted {
        Some(UNTRUSTED_PLACEHOLDER)
    } else {
        None
    };

    // Pass 1: fenced multi-line blocks.
    let mut out = String::with_capacity(body.len());
    let mut last_end = 0;
    for caps in FENCED.captures_iter(body) {
        let Some(m) = caps.get(0) else { continue };
        out.push_str(&body[last_end..m.start()]);
        let command = caps.get(1).map_or("", |c| c.as_str());
        out.push_str(&run_skill_shell_command(command, ctx, refusal, permission).await);
        last_end = m.end();
    }
    out.push_str(&body[last_end..]);
    let body = out;

    // Pass 2: inline `` !`cmd` `` — operates on pass 1's output, so an
    // inline placeholder that happened to be embedded in a fenced block's
    // *output* isn't re-expanded (single pass, per the doc comment above).
    let mut out = String::with_capacity(body.len());
    let mut last_end = 0;
    for caps in INLINE.captures_iter(&body) {
        let Some(m) = caps.get(0) else { continue };
        out.push_str(&body[last_end..m.start()]);
        out.push_str(caps.get(1).map_or("", |c| c.as_str())); // preserve the leading space/tab (or nothing at line start)
        let command = caps.get(2).map_or("", |c| c.as_str());
        out.push_str(&run_skill_shell_command(command, ctx, refusal, permission).await);
        last_end = m.end();
    }
    out.push_str(&body[last_end..]);
    out
}

async fn run_skill_shell_command(
    command: &str,
    ctx: &ToolContext,
    refusal: Option<&str>,
    permission: Option<&dyn base::interface::permission::Permission>,
) -> String {
    use base::interface::permission::PermissionOutcome;
    if let Some(refusal) = refusal {
        return refusal.to_string();
    }
    let input = serde_json::json!({"command": command});
    let refused = match permission {
        None => Some("no permission check is wired for this skill".to_string()),
        Some(permission) => match permission
            .check("Bash", &input, &ctx.cwd, &ctx.session_id)
            .await
        {
            PermissionOutcome::Permit => None,
            PermissionOutcome::Deny { reason } => Some(reason),
            PermissionOutcome::Prompt { message, .. } => Some(format!(
                "it needs the user's approval, which a skill's inline command cannot ask for \
                 ({message}); run it with the Bash tool instead"
            )),
        },
    };
    if let Some(reason) = refused {
        tracing::warn!(command = %command, reason = %reason, "skill inline command not run");
        return format!("[`{command}` not run: {reason}]");
    }
    let result = tools::bash::BashTool
        .call(input, ctx.clone(), ProgressSender::noop(""))
        .await;
    match result {
        // Trim exactly one trailing newline, matching shell `$(...)` command
        // substitution semantics — without this, every inline placeholder
        // splice leaves a stray blank line/line-break before whatever
        // follows it in the skill body, since `echo`-style commands always
        // end their output in `\n`.
        Ok(r) => match r.content {
            ToolResultContent::Text(t) => t.strip_suffix('\n').unwrap_or(&t).to_string(),
            ToolResultContent::Blocks(_) => String::new(),
        },
        Err(e) => format!("[error running `{command}`: {e}]"),
    }
}

/// Register the `Skill` tool over `m` in `r`.
pub fn register(
    r: &base::tool::InMemoryToolRegistry,
    m: std::sync::Arc<crate::manager::SkillManager>,
    admission: crate::catalog::BundledAdmission,
    spawner: Option<std::sync::Arc<dyn base::interface::agent_spawner::AgentSpawner>>,
    permission: std::sync::Arc<dyn base::interface::permission::Permission>,
) {
    let mut tool = SkillTool::new(m)
        .with_admission(admission)
        .with_permission(permission);
    if let Some(spawner) = spawner {
        tool = tool.with_spawner(spawner);
    }
    r.register(std::sync::Arc::new(tool));
}

#[cfg(test)]
mod dynamic_context_tests {
    use super::*;
    use base::interface::permission::{Permission, PermissionOutcome};

    struct Permits;

    #[async_trait]
    impl Permission for Permits {
        async fn check(
            &self,
            _: &str,
            _: &Value,
            _: &std::path::Path,
            _: &str,
        ) -> PermissionOutcome {
            PermissionOutcome::Permit
        }
    }

    async fn expand(body: &str, ctx: &ToolContext) -> String {
        expand_dynamic_context(body, ctx, Some(&Permits), crate::manager::SkillSource::User).await
    }

    /// The session's real gate over the real `Bash`, under `rules`.
    fn gate(
        rules: Vec<base::permission::PermissionRule>,
        mode: base::permission::PermissionMode,
    ) -> permissions::rule_set_permission::RuleSetPermission {
        let registry = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools::register_builtin_tools(&registry);
        permissions::rule_set_permission::RuleSetPermission::new(
            Arc::new(permissions::gate::PermissionGate::new(
                permissions::ruleset::RuleSet::new(rules),
            )),
            registry,
            mode,
        )
    }

    fn bash_rule(
        behavior: base::permission::RuleBehavior,
        content: Option<&str>,
    ) -> base::permission::PermissionRule {
        base::permission::PermissionRule {
            source: base::permission::RuleSource::ProjectSettings,
            behavior,
            tool_name: "Bash".into(),
            rule_content: content.map(Into::into),
        }
    }

    #[tokio::test]
    async fn an_inline_command_is_held_to_the_rules_a_bash_call_would_be() {
        use base::permission::{PermissionMode, RuleBehavior};
        let dir = tempfile::tempdir().unwrap();
        let ctx = platform::tool_context_for_test(dir.path().to_path_buf());
        let body = "!`echo stamped > stamp.txt`";

        let denied = gate(
            vec![bash_rule(RuleBehavior::Deny, None)],
            PermissionMode::AcceptEdits,
        );
        let out =
            expand_dynamic_context(body, &ctx, Some(&denied), crate::manager::SkillSource::User)
                .await;
        assert!(
            out.contains("not run") && out.contains("denied by rule"),
            "{out}"
        );
        assert!(!dir.path().join("stamp.txt").exists());

        let asks = gate(vec![], PermissionMode::Default);
        let out =
            expand_dynamic_context(body, &ctx, Some(&asks), crate::manager::SkillSource::User)
                .await;
        assert!(out.contains("needs the user's approval"), "{out}");
        assert!(!dir.path().join("stamp.txt").exists());

        let out = expand_dynamic_context(body, &ctx, None, crate::manager::SkillSource::User).await;
        assert!(out.contains("not run"), "{out}");
        assert!(!dir.path().join("stamp.txt").exists());

        let allowed = gate(
            vec![bash_rule(RuleBehavior::Allow, Some("echo:*"))],
            PermissionMode::Default,
        );
        expand_dynamic_context(
            body,
            &ctx,
            Some(&allowed),
            crate::manager::SkillSource::User,
        )
        .await;
        assert!(dir.path().join("stamp.txt").exists());
    }

    #[tokio::test]
    async fn a_read_only_inline_command_runs_as_a_bash_call_would() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = platform::tool_context_for_test(dir.path().to_path_buf());
        let asks = gate(vec![], base::permission::PermissionMode::Default);
        let out = expand_dynamic_context(
            "!`echo hello`",
            &ctx,
            Some(&asks),
            crate::manager::SkillSource::User,
        )
        .await;
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn inline_placeholder_at_line_start_is_substituted() {
        let body = "## Info\n!`echo hello-world`\n\nDone.";
        let out = expand(body, &platform::tool_context_for_test(std::env::temp_dir())).await;
        assert!(out.contains("hello-world"));
        assert!(!out.contains("!`echo"));
    }

    #[tokio::test]
    async fn inline_placeholder_after_whitespace_is_substituted() {
        let body = "Value: !`echo 42`.";
        let out = expand(body, &platform::tool_context_for_test(std::env::temp_dir())).await;
        assert!(out.contains("Value: 42."));
    }

    #[tokio::test]
    async fn inline_placeholder_without_separating_whitespace_is_left_literal() {
        // `!` must be at line start or after whitespace — `KEY=!\`cmd\`` has
        // no separator, so it's untouched.
        let body = "KEY=!`echo should-not-run`";
        let out = expand(body, &platform::tool_context_for_test(std::env::temp_dir())).await;
        assert_eq!(out, body);
    }

    #[tokio::test]
    async fn fenced_block_is_substituted() {
        let body = "## Environment\n```!\necho line-one\necho line-two\n```\n";
        let out = expand(body, &platform::tool_context_for_test(std::env::temp_dir())).await;
        assert!(out.contains("line-one"));
        assert!(out.contains("line-two"));
        assert!(!out.contains("```!"));
    }

    #[tokio::test]
    async fn disable_skill_shell_execution_replaces_with_fixed_placeholder_not_running_anything() {
        let mut ctx = platform::tool_context_for_test(std::env::temp_dir());
        ctx.config = Arc::new({
            let mut c = base::context::EngineConfig::defaults_for("test-model");
            c.disable_skill_shell_execution = true;
            c
        });
        let body = "!`echo should-not-run`";
        let out = expand(body, &ctx).await;
        assert_eq!(out, DISABLED_PLACEHOLDER);
        assert!(!out.contains("should-not-run"));
    }

    #[tokio::test]
    async fn project_skill_commands_wait_for_trust() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = platform::tool_context_for_test(dir.path().to_path_buf());
        ctx.config = Arc::new({
            let mut c = base::context::EngineConfig::defaults_for("test-model");
            c.project_untrusted = true;
            c
        });
        let body = "!`echo ran > ran.txt`";
        let expand_from = |source| expand_dynamic_context(body, &ctx, Some(&Permits), source);

        assert_eq!(
            expand_from(crate::manager::SkillSource::Project).await,
            UNTRUSTED_PLACEHOLDER
        );
        assert!(!dir.path().join("ran.txt").exists());

        expand_from(crate::manager::SkillSource::User).await;
        assert!(
            dir.path().join("ran.txt").exists(),
            "only the project's own skills wait for trust"
        );
    }

    #[tokio::test]
    async fn body_with_no_placeholders_is_returned_unchanged() {
        let body = "Just plain instructions, no shell syntax here.";
        let out = expand(body, &platform::tool_context_for_test(std::env::temp_dir())).await;
        assert_eq!(out, body);
    }

    #[tokio::test]
    async fn command_output_is_not_rescanned_for_further_placeholders() {
        // The command's own output happens to contain inline-placeholder-
        // shaped text — it must NOT be expanded again (single pass). Uses
        // printf's octal escape (\140 = backtick) so the *skill body*
        // itself contains no literal backtick — this simple parser has no
        // escaping support for a backtick inside the command text, so a
        // command that needs to *produce* a backtick has to build it this
        // way rather than embed one directly.
        let body = "!`printf '!\\140echo nested\\140'`";
        let out = expand(body, &platform::tool_context_for_test(std::env::temp_dir())).await;
        assert_eq!(out, "!`echo nested`", "output was: {out}");
    }

    #[derive(Default)]
    struct Forks(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl AgentSpawner for Forks {
        async fn spawn(
            &self,
            request: SpawnRequest,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            assert!(
                matches!(request.mode, SpawnMode::Wait),
                "no skill here forks in the background"
            );
            self.0.lock().unwrap().push(request.prompt);
            Ok("done".into())
        }
    }

    /// The `Skill` tool inline, a forked skill and a `/name` command (which
    /// reaches skills through the catalog) hand the conversation the same
    /// expansion: no frontmatter, `!cmd` output, arguments, the body cap.
    #[tokio::test]
    async fn every_invocation_path_expands_a_skill_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        let skill = |name: &str, extra: &str| {
            let folder = dir.path().join(name);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(
                folder.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: ships\n{extra}---\nShip $ARGUMENTS.\n\
                     !`echo stamped`\n{}",
                    "x".repeat(9000)
                ),
            )
            .unwrap();
        };
        skill("ship", "");
        skill("ship-forked", "context: fork\n");
        let mgr = Arc::new(crate::manager::SkillManager::new());
        mgr.load_dir_subdirs(dir.path(), crate::manager::SkillSource::User)
            .unwrap();
        let ctx = platform::tool_context_for_test(dir.path().to_path_buf());
        let forks = Arc::new(Forks::default());
        let tool = SkillTool::new(mgr.clone())
            .with_permission(Arc::new(Permits))
            .with_spawner(forks.clone());

        let inline = tool
            .call(
                serde_json::json!({"skill": "ship", "args": "v1"}),
                ctx.clone(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let inline = inline.new_messages.unwrap()[0]["content"]
            .as_str()
            .unwrap()
            .split_once("\n\n")
            .unwrap()
            .1
            .to_string();
        tool.call(
            serde_json::json!({"skill": "ship-forked", "args": "v1"}),
            ctx.clone(),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
        let forked = forks.0.lock().unwrap()[0].clone();
        let slash = base::interface::skill_catalog::SkillCatalog::expand(
            &*mgr, "ship", "v1", &ctx, &Permits,
        )
        .await
        .unwrap()
        .unwrap()
        .text;

        assert!(
            inline.starts_with("Ship v1.\nstamped\nxxx") && inline.ends_with("...[truncated]"),
            "{inline:.40}"
        );
        assert_eq!(forked, inline);
        assert_eq!(slash, inline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::frozen::skill::{SkillEntry, SkillSource as FrozenSkillSource};

    fn register_test_skill(
        mgr: &crate::manager::SkillManager,
        name: &str,
        body: &str,
        disable_model_invocation: bool,
    ) {
        register_test_skill_with(mgr, name, body, disable_model_invocation, None, None);
    }

    fn register_test_skill_with(
        mgr: &crate::manager::SkillManager,
        name: &str,
        body: &str,
        disable_model_invocation: bool,
        context: Option<&str>,
        agent: Option<&str>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the file survives past this function — fine for
        // a short-lived test process.
        let path = dir.keep().join(format!("{name}.md"));
        std::fs::write(&path, body).unwrap();
        mgr.register_bundled(SkillEntry {
            name: name.into(),
            description: format!("{name} skill"),
            source: FrozenSkillSource::Project,
            path,
            disable_model_invocation,
            context: context.map(String::from),
            agent: agent.map(String::from),
            ..Default::default()
        });
    }

    /// Records the `agent_type` it was called with, so the fork path's
    /// `agent:` frontmatter wiring can be asserted end to end. Also records
    /// whether the background variant was invoked, for the `background:
    /// true` test.
    struct MockSpawner {
        last_agent_type: std::sync::Mutex<Option<Option<String>>>,
        background_calls: std::sync::Mutex<u32>,
    }
    impl MockSpawner {
        fn new() -> Self {
            Self {
                last_agent_type: std::sync::Mutex::new(None),
                background_calls: std::sync::Mutex::new(0),
            }
        }
    }
    #[async_trait]
    impl base::interface::agent_spawner::AgentSpawner for MockSpawner {
        async fn spawn(
            &self,
            request: SpawnRequest,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            *self.last_agent_type.lock().unwrap() = Some(request.agent_type);
            if matches!(request.mode, SpawnMode::Background { .. }) {
                *self.background_calls.lock().unwrap() += 1;
                return Ok("mock-task-id-1".to_string());
            }
            Ok("forked output".to_string())
        }
    }

    /// A project-defined skill (e.g. loaded from `.agents/skills/`) is
    /// invocable unless its own frontmatter says otherwise.
    #[tokio::test]
    async fn project_skill_without_disable_flag_is_invocable() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "code-review", "Review the changed files.", false);
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "code-review"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        assert!(
            result.new_messages.is_some(),
            "expanded skill content should be injected as a new message"
        );
    }

    /// `allowed_tools` end to end: before the skill runs, `Bash` has no
    /// rule and would `Ask` in `Default` mode (see
    /// `permissions::rule_set_permission`'s own `default_mode_with_no_rule_prompts`
    /// test for that baseline) — invoking a skill that scopes its grant
    /// (`Bash(cargo test)`) must make an immediately-following check for
    /// exactly that call come back `Permit`, via the real `PermissionGate`,
    /// not a test double.
    ///
    /// The grant is *scoped*. A bare `Bash` from a project-supplied
    /// skill is refused outright — see
    /// `project_skill_blanket_allowed_tools_grant_is_refused` below.
    #[tokio::test]
    async fn allowed_tools_injects_a_real_temporary_allow_rule() {
        use base::interface::permission::{Permission, PermissionOutcome};
        use permissions::gate::PermissionGate;
        use permissions::rule_set_permission::RuleSetPermission;

        let mgr = Arc::new(crate::manager::SkillManager::new());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("uses-bash.md");
        std::fs::write(&path, "Run some commands.").unwrap();
        mgr.register_bundled(SkillEntry {
            name: "uses-bash".into(),
            description: "uses-bash skill".into(),
            source: FrozenSkillSource::Project,
            path,
            allowed_tools: Some(vec!["Bash(cargo test)".into()]),
            ..Default::default()
        });

        struct FakeBashTool;
        #[async_trait]
        impl Tool for FakeBashTool {
            fn name(&self) -> &str {
                "Bash"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn prompt(&self, _: &PromptContext) -> String {
                "fake bash".into()
            }
            async fn check_permissions(
                &self,
                _: &Value,
                _: &ToolContext,
            ) -> base::tool::PermissionDecision {
                // Default `Tool::check_permissions` is `allow()`, which would
                // short-circuit `PermissionGate::check` before it ever
                // reaches rule/mode dispatch — defeating the point of this
                // test. A real `Bash`-shaped tool defers to the gate.
                base::tool::PermissionDecision::ask("run?")
            }
            fn permission_match_content(&self, _: &Value) -> Option<String> {
                // What a real `BashTool` derives from its input — the thing a
                // scoped rule is matched against.
                Some("cargo test".to_string())
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

        let tool_registry = base::tool::InMemoryToolRegistry::new();
        tool_registry.register(Arc::new(FakeBashTool));
        let real_permission = Arc::new(RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            Arc::new(tool_registry),
            base::permission::PermissionMode::Default,
        ));

        let sanity = real_permission
            .check(
                "Bash",
                &serde_json::json!({}),
                std::path::Path::new("/tmp"),
                "s1",
            )
            .await;
        assert!(
            matches!(sanity, PermissionOutcome::Prompt { .. }),
            "sanity: Bash should Ask before the skill runs, got {sanity:?}"
        );

        let tool = SkillTool::new(mgr).with_permission(real_permission.clone());
        let result = tool
            .call(
                serde_json::json!({"skill": "uses-bash"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(!result.is_error);

        let after = real_permission
            .check(
                "Bash",
                &serde_json::json!({}),
                std::path::Path::new("/tmp"),
                "s1",
            )
            .await;
        assert!(
            matches!(after, PermissionOutcome::Permit),
            "expected allowed_tools to have injected a real Allow rule, got {after:?}"
        );
    }

    /// `<project>/.agents/skills/` is repository content. On a cloned repo
    /// it is attacker-supplied, so a bare `allowed-tools: [Bash]` there must
    /// not switch off confirmation for every `Bash` call.
    #[tokio::test]
    async fn project_skill_blanket_allowed_tools_grant_is_refused() {
        assert!(
            matches!(
                bash_after_a_blanket_grant_from(FrozenSkillSource::Project).await,
                base::interface::permission::PermissionOutcome::Prompt { .. }
            ),
            "a project skill's blanket grant must not silence the Bash prompt"
        );
    }

    /// Until the project is trusted, its skills grant nothing — not even a
    /// scoped `Bash(python3:*)`, which would otherwise wave through every
    /// `python3 -c …` once the model invoked the skill.
    #[tokio::test]
    async fn an_untrusted_projects_skills_grant_nothing() {
        let grants = |source: FrozenSkillSource, untrusted: bool| async move {
            let mgr = crate::manager::SkillManager::new();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("py.md");
            std::fs::write(&path, "Use python.").unwrap();
            mgr.register_bundled(SkillEntry {
                name: "py".into(),
                description: "py".into(),
                source,
                path,
                allowed_tools: Some(vec!["Bash(python3:*)".into()]),
                ..Default::default()
            });
            let info = mgr.list().into_iter().find(|s| s.name == "py").unwrap();
            let mut ctx = platform::tool_context_for_test(dir.path().to_path_buf());
            ctx.config = Arc::new({
                let mut c = base::context::EngineConfig::defaults_for("test-model");
                c.project_untrusted = untrusted;
                c
            });
            expand_for_invocation(&mgr, &info, "", &ctx, None)
                .await
                .unwrap()
                .grants
        };
        let scoped = vec![("Bash".to_string(), Some("python3:*".to_string()))];

        assert!(grants(FrozenSkillSource::Project, true).await.is_empty());
        assert_eq!(grants(FrozenSkillSource::Project, false).await, scoped);
        assert_eq!(
            grants(FrozenSkillSource::User, true).await,
            scoped,
            "only the project's own skills wait for trust"
        );
    }

    /// `Bash(*)` names content, but its `*` matches every command — the same
    /// blanket grant as a bare `Bash`.
    #[tokio::test]
    async fn a_wildcard_grant_from_a_project_or_plugin_skill_is_refused() {
        let grants = |source: FrozenSkillSource| async move {
            let mgr = crate::manager::SkillManager::new();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("wild.md");
            std::fs::write(&path, "Anything goes.").unwrap();
            mgr.register_bundled(SkillEntry {
                name: "wild".into(),
                description: "wild".into(),
                source,
                path,
                allowed_tools: Some(vec![
                    "Bash(*)".into(),
                    "Write(**)".into(),
                    "Edit(/**)".into(),
                    "Write(x/../../../../../../../../**)".into(),
                    "Edit(/U*/**)".into(),
                    "Bash(b*)".into(),
                    "Bash(bash:*)".into(),
                    "WebFetch(https://*.co.uk/**)".into(),
                    "Bash(cargo test:*)".into(),
                    "Write(src/**)".into(),
                    "WebFetch(https://*.example.com/**)".into(),
                ]),
                ..Default::default()
            });
            let info = mgr.list().into_iter().find(|s| s.name == "wild").unwrap();
            let ctx = platform::tool_context_for_test(dir.path().to_path_buf());
            expand_for_invocation(&mgr, &info, "", &ctx, None)
                .await
                .unwrap()
                .grants
        };
        let scoped: Vec<_> = [
            ("Bash", "cargo test:*"),
            ("Write", "src/**"),
            ("WebFetch", "https://*.example.com/**"),
        ]
        .into_iter()
        .map(|(tool, content)| (tool.to_string(), Some(content.to_string())))
        .collect();
        assert_eq!(grants(FrozenSkillSource::Project).await, scoped);
        assert_eq!(grants(FrozenSkillSource::Plugin).await, scoped);
        assert_eq!(grants(FrozenSkillSource::User).await.len(), 11);
    }

    /// A plugin package is someone else's code just the same.
    #[tokio::test]
    async fn plugin_skill_blanket_allowed_tools_grant_is_refused() {
        assert!(matches!(
            bash_after_a_blanket_grant_from(FrozenSkillSource::Plugin).await,
            base::interface::permission::PermissionOutcome::Prompt { .. }
        ));
    }

    #[tokio::test]
    async fn user_skill_blanket_allowed_tools_grant_is_honored() {
        assert!(matches!(
            bash_after_a_blanket_grant_from(FrozenSkillSource::User).await,
            base::interface::permission::PermissionOutcome::Permit
        ));
    }

    async fn bash_after_a_blanket_grant_from(
        source: FrozenSkillSource,
    ) -> base::interface::permission::PermissionOutcome {
        use base::interface::permission::Permission;
        use permissions::gate::PermissionGate;
        use permissions::rule_set_permission::RuleSetPermission;

        let mgr = Arc::new(crate::manager::SkillManager::new());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("grabby.md");
        std::fs::write(&path, "Run whatever you like.").unwrap();
        mgr.register_bundled(SkillEntry {
            name: "grabby".into(),
            description: "grabby skill".into(),
            source,
            path,
            // Bare tool name = blanket grant.
            allowed_tools: Some(vec!["Bash".into()]),
            ..Default::default()
        });

        struct AskingBash;
        #[async_trait]
        impl Tool for AskingBash {
            fn name(&self) -> &str {
                "Bash"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn prompt(&self, _: &PromptContext) -> String {
                "fake bash".into()
            }
            async fn check_permissions(
                &self,
                _: &Value,
                _: &ToolContext,
            ) -> base::tool::PermissionDecision {
                base::tool::PermissionDecision::ask("run?")
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

        let registry = base::tool::InMemoryToolRegistry::new();
        registry.register(Arc::new(AskingBash));
        let permission = Arc::new(RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            Arc::new(registry),
            base::permission::PermissionMode::Default,
        ));

        let tool = SkillTool::new(mgr).with_permission(permission.clone());
        let result = tool
            .call(
                serde_json::json!({"skill": "grabby"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(!result.is_error, "the skill itself still runs");

        permission
            .check(
                "Bash",
                &serde_json::json!({}),
                std::path::Path::new("/tmp"),
                "s1",
            )
            .await
    }

    /// Feeds `build_skills_text`'s budget-drop order (task tracking
    /// `SkillManager::invocation_count`/`last_invoked_seq`) — a successful
    /// call through the Skill tool must record the invocation.
    #[tokio::test]
    async fn successful_call_records_invocation_on_the_skill_manager() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "code-review", "Review the changed files.", false);
        let tool = SkillTool::new(mgr.clone());

        assert_eq!(mgr.invocation_count("code-review"), 0);
        tool.call(
            serde_json::json!({"skill": "code-review"}),
            platform::tool_context_for_test(std::env::temp_dir()),
            ProgressSender::noop("t"),
        )
        .await
        .expect("call should not error");
        assert_eq!(mgr.invocation_count("code-review"), 1);

        tool.call(
            serde_json::json!({"skill": "code-review"}),
            platform::tool_context_for_test(std::env::temp_dir()),
            ProgressSender::noop("t"),
        )
        .await
        .expect("call should not error");
        assert_eq!(mgr.invocation_count("code-review"), 2);
    }

    /// A denied call (disable_model_invocation) must NOT count as an
    /// invocation — the metric tracks actual use, not attempted use.
    #[tokio::test]
    async fn denied_call_does_not_record_an_invocation() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "manual-only", "Only run via slash command.", true);
        let tool = SkillTool::new(mgr.clone());

        let _ = tool
            .call(
                serde_json::json!({"skill": "manual-only"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await;
        assert_eq!(mgr.invocation_count("manual-only"), 0);
    }

    /// Re-invoking the same skill with identical rendered content in the
    /// same session must not inject a second full copy.
    #[tokio::test]
    async fn identical_reinvocation_in_same_session_skips_duplicate_content() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "code-review", "Review the changed files.", false);
        let tool = SkillTool::new(mgr);

        let ctx1 = platform::tool_context_for_test(std::env::temp_dir());
        let session_id = ctx1.session_id.clone();
        let first = tool
            .call(
                serde_json::json!({"skill": "code-review"}),
                ctx1,
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(first.new_messages.is_some());

        let mut ctx2 = platform::tool_context_for_test(std::env::temp_dir());
        ctx2.session_id = session_id;
        let second = tool
            .call(
                serde_json::json!({"skill": "code-review"}),
                ctx2,
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(
            second.new_messages.is_none(),
            "identical re-invocation must not inject a second full copy"
        );
        match &second.content {
            ToolResultContent::Text(t) => assert!(t.contains("already loaded")),
            other => panic!("expected Text content, got {other:?}"),
        }
    }

    /// A different session must NOT see another session's dedup state —
    /// the same skill+args invoked for the first time in a fresh session
    /// must inject full content, even if another session already has it.
    #[tokio::test]
    async fn identical_content_in_a_different_session_still_injects_full_content() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "code-review", "Review the changed files.", false);
        let tool = SkillTool::new(mgr);

        tool.call(
            serde_json::json!({"skill": "code-review"}),
            platform::tool_context_for_test(std::env::temp_dir()),
            ProgressSender::noop("t"),
        )
        .await
        .expect("call should not error");

        // A fresh `tool_context_for_test` has a different/independent identity
        // from the harness's perspective — but `tool_context_for_test` always uses the
        // same literal session_id ("test"), so exercise the real
        // distinguishing behavior by asserting the dedup is keyed off
        // session_id explicitly instead of relying on `tool_context_for_test`'s default.
        let mut ctx = platform::tool_context_for_test(std::env::temp_dir());
        ctx.session_id = "a-different-session".into();
        let result = tool
            .call(
                serde_json::json!({"skill": "code-review"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(
            result.new_messages.is_some(),
            "a different session must get the full content, not the dedup note"
        );
    }

    /// `context: fork` skills pass their `agent:` frontmatter through to
    /// the spawner as `agent_type` — the field that selects which subagent
    /// type to fork into.
    #[tokio::test]
    async fn fork_context_passes_agent_field_to_spawner() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill_with(
            &mgr,
            "deep-research",
            "Research the topic thoroughly.",
            false,
            Some("fork"),
            Some("Explore"),
        );
        let spawner = Arc::new(MockSpawner::new());
        let tool = SkillTool::new(mgr).with_spawner(spawner.clone());

        let result = tool
            .call(
                serde_json::json!({"skill": "deep-research"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        assert_eq!(
            *spawner.last_agent_type.lock().unwrap(),
            Some(Some("Explore".to_string())),
            "the skill's agent: field must reach the spawner"
        );
    }

    /// `context: fork` + `background: true` must return a task id
    /// immediately via a background `spawn`, not block on
    /// a waiting one's result.
    #[tokio::test]
    async fn fork_context_with_background_true_uses_the_background_spawner_method() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("bg-research.md");
        std::fs::write(&path, "Research the topic thoroughly.").unwrap();
        mgr.register_bundled(SkillEntry {
            name: "bg-research".into(),
            description: "bg-research skill".into(),
            source: FrozenSkillSource::Project,
            path,
            context: Some("fork".into()),
            background: Some(true),
            ..Default::default()
        });
        let spawner = Arc::new(MockSpawner::new());
        let tool = SkillTool::new(mgr).with_spawner(spawner.clone());

        let result = tool
            .call(
                serde_json::json!({"skill": "bg-research"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        assert_eq!(*spawner.background_calls.lock().unwrap(), 1);
        let ToolResultContent::Text(text) = result.content else {
            panic!("expected text content");
        };
        assert!(
            text.contains("mock-task-id-1"),
            "expected the task id in the response, got: {text}"
        );
    }

    /// A `context: fork` skill with no `agent:` field passes `None` through
    /// — the spawner (and ultimately `AgentTool::run_sub`) decides the
    /// default (general-purpose), this tool doesn't hardcode one.
    #[tokio::test]
    async fn fork_context_without_agent_field_passes_none() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill_with(
            &mgr,
            "pr-summary",
            "Summarize this pull request.",
            false,
            Some("fork"),
            None,
        );
        let spawner = Arc::new(MockSpawner::new());
        let tool = SkillTool::new(mgr).with_spawner(spawner.clone());

        tool.call(
            serde_json::json!({"skill": "pr-summary"}),
            platform::tool_context_for_test(std::env::temp_dir()),
            ProgressSender::noop("t"),
        )
        .await
        .expect("call should not error");

        assert_eq!(*spawner.last_agent_type.lock().unwrap(), Some(None));
    }

    /// A skill's own `disable_model_invocation: true` is still respected —
    /// this is the one gate that survived (per-skill, not per-scene).
    #[tokio::test]
    async fn skill_with_disable_model_invocation_is_denied() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "manual-only", "Only run via slash command.", true);
        let tool = SkillTool::new(mgr);

        let err = tool
            .call(
                serde_json::json!({"skill": "manual-only"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect_err("disable_model_invocation should deny the call");

        assert!(matches!(err, ToolError::Denied(_)));
    }

    #[tokio::test]
    async fn unknown_skill_returns_not_found() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        let tool = SkillTool::new(mgr);

        let err = tool
            .call(
                serde_json::json!({"skill": "does-not-exist"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect_err("unknown skill should error");

        assert!(matches!(err, ToolError::NotFound(_)));
    }

    /// A skill with `files: [notes.md]` gets that file's content read and
    /// injected alongside the body, wrapped in a `## Bundled file:` header.
    #[tokio::test]
    async fn bundled_file_content_is_injected() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("bundled-notes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: has bundled notes\nfiles: [notes.md]\n---\nBody text.",
        )
        .unwrap();
        std::fs::write(skill_dir.join("notes.md"), "KNOWN_MARKER_CONTENT_XYZ").unwrap();

        let mgr = Arc::new(crate::manager::SkillManager::new());
        mgr.register_bundled(SkillEntry {
            name: "bundled-notes".into(),
            description: "has bundled notes".into(),
            source: FrozenSkillSource::Project,
            path: skill_dir.join("SKILL.md"),
            files: Some(vec!["notes.md".into()]),
            ..Default::default()
        });
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "bundled-notes"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        let messages = result.new_messages.expect("expected injected message");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(
            content.contains("KNOWN_MARKER_CONTENT_XYZ"),
            "bundled file content missing from injected message: {content}"
        );
        assert!(content.contains("## Bundled file: notes.md"));
    }

    /// A skill body itself (not a bundled file) that's multi-byte UTF-8 and
    /// exceeds the 8000-char cap must be truncated without panicking, at a
    /// valid char boundary.
    #[tokio::test]
    async fn skill_body_truncation_does_not_panic_on_multibyte_content() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        // Each "你" is a 3-byte UTF-8 char; 9000 of them is well over the
        // 8000-char body cap and, at a byte-index cut, lands mid-char for
        // almost any cap value that isn't a multiple of 3.
        let long_cjk_body = "你".repeat(9000);
        register_test_skill(&mgr, "cjk-body", &long_cjk_body, false);
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "cjk-body"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error (must not panic on truncation)");

        assert!(!result.is_error);
        let messages = result.new_messages.expect("expected injected message");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("...[truncated]"));
        // Exactly 8000 valid "你" chars should have survived the cut, no
        // partial/garbled character.
        assert!(content.contains(&"你".repeat(8000)));
        assert!(!content.contains(&"你".repeat(8001)));
    }

    /// A bundled file whose content is multi-byte UTF-8 (Chinese) and
    /// exceeds `BUNDLED_FILE_CHAR_CAP` must be truncated without panicking,
    /// at a valid char boundary.
    #[tokio::test]
    async fn bundled_file_truncation_does_not_panic_on_multibyte_content() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("cjk-notes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: has cjk bundled notes\nfiles: [notes.md]\n---\nBody text.",
        )
        .unwrap();
        // Each "你" is a 3-byte UTF-8 char; 2500 of them is well over the
        // 2000-char per-file cap and, at a byte-index cut, lands mid-char
        // for almost any cap value that isn't a multiple of 3.
        let long_cjk = "你".repeat(2500);
        std::fs::write(skill_dir.join("notes.md"), &long_cjk).unwrap();

        let mgr = Arc::new(crate::manager::SkillManager::new());
        mgr.register_bundled(SkillEntry {
            name: "cjk-notes".into(),
            description: "has cjk bundled notes".into(),
            source: FrozenSkillSource::Project,
            path: skill_dir.join("SKILL.md"),
            files: Some(vec!["notes.md".into()]),
            ..Default::default()
        });
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "cjk-notes"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error (must not panic on truncation)");

        assert!(!result.is_error);
        let messages = result.new_messages.expect("expected injected message");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("## Bundled file: notes.md"));
        assert!(content.contains("...[truncated]"));
        // Exactly 2000 valid "你" chars should have survived the cut, no
        // partial/garbled character.
        assert!(content.contains(&"你".repeat(2000)));
        assert!(!content.contains(&"你".repeat(2001)));
    }

    /// A skill declaring a `files:` entry that escapes its own folder (via
    /// `..`) must be rejected with a visible `is_error: true` result naming
    /// both the skill and the offending path — and none of the outside
    /// file's content may leak into the result.
    #[tokio::test]
    async fn bundled_file_escaping_skill_folder_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("escaping-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        // `outside.md` lives one level above the skill's own folder.
        std::fs::write(dir.path().join("outside.md"), "SECRET_SHOULD_NOT_LEAK").unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: tries to escape\nfiles: [../outside.md]\n---\nBody text.",
        )
        .unwrap();

        let mgr = Arc::new(crate::manager::SkillManager::new());
        mgr.register_bundled(SkillEntry {
            name: "escaping-skill".into(),
            description: "tries to escape".into(),
            source: FrozenSkillSource::Project,
            path: skill_dir.join("SKILL.md"),
            files: Some(vec!["../outside.md".into()]),
            ..Default::default()
        });
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "escaping-skill"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("validation failure surfaces as is_error, not Err");

        assert!(result.is_error);
        let ToolResultContent::Text(text) = result.content else {
            panic!("expected text content");
        };
        assert!(
            text.contains("escaping-skill"),
            "error should name the skill: {text}"
        );
        assert!(
            text.contains("outside.md"),
            "error should name the offending path: {text}"
        );
        assert!(
            !text.contains("SECRET_SHOULD_NOT_LEAK"),
            "content from outside the skill folder must never leak: {text}"
        );
        assert!(
            result.new_messages.is_none(),
            "a rejected bundled file must not still inject a message"
        );
    }

    /// A skill with no `files` field at all gets no `## Bundled file:`
    /// section — just the `<command-name>`/`<command-args>`-wrapped message.
    #[tokio::test]
    async fn skill_without_files_field_is_unaffected() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "no-files", "Just a plain skill body.", false);
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "no-files"}),
                platform::tool_context_for_test(std::env::temp_dir()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        let messages = result.new_messages.expect("expected injected message");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(!content.contains("## Bundled file:"));
        assert_eq!(
            content,
            "<command-name>no-files</command-name>\n<command-args></command-args>\n\nJust a plain skill body."
        );
    }
}
