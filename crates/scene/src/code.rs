//! Code scene — the full-capability coding agent: every registered tool but
//! a few in [`NOT_OFFERED`] is allowed, and everything outside
//! [`RESIDENT_TOOLS`] is advertised by name until `ToolSearch` fetches its
//! schema. Created [`Interaction::Interactive`] (someone answers questions
//! and permission prompts) or [`Interaction::Unattended`] (nobody does: the
//! tools, prompt lines and skills that wait on a reply are left out).
//!
//! System prompt assembly uses a section cache keyed by content fingerprint
//! (see `SECTION_CACHE` below) so unchanged sections are not rebuilt on
//! every turn.

use crate::common;
use base::interface::scene::{
    Reminder, ScenePrompt, ScenePromptContext, SceneSpec, TokenBudget, ToolSurface,
};
use base::prompt::{CacheStrategy, PromptBlock};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

/// Tools whose full JSON schema stays in every request for this scene: what a
/// coding turn reaches for without deliberating, plus the two that reach
/// everything else (`Skill`, `ToolSearch`).
///
/// `Glob`/`Grep` stay resident because deferring them would push the model
/// toward shelling out to `find`/`grep`. `Write` stays resident: deferring it
/// would put a round trip in front of the first write of nearly every session.
pub const RESIDENT_TOOLS: &[&str] = &[
    "Agent",
    "AskUserQuestion",
    "Bash",
    "Edit",
    "Glob",
    "Grep",
    "Read",
    "RunCode",
    "Skill",
    "ToolSearch",
    "Write",
];

/// Every tool this scene may see, resident or not.
///
/// Spelled out rather than read from the registry: the deferred list is built
/// before the session's registry is assembled and must give the same answer
/// whichever optional tools a deployment wired up. A tool missing here is
/// never deferred — `every_registered_tool_has_a_deferral_policy` in
/// `crates/runtime` fails when one is added without an entry.
const ALL_DEFERRABLE_TOOLS: &[&str] = &[
    "Agent",
    "AskUserQuestion",
    "Bash",
    "CronCreate",
    "CronDelete",
    "CronList",
    "Edit",
    "EnterPlanMode",
    "ExitPlanMode",
    "Glob",
    "GoalCreate",
    "GoalGet",
    "GoalUpdate",
    "Grep",
    "Monitor",
    "Read",
    "RunCode",
    "Skill",
    "StructuredOutput",
    "TaskMessage",
    "TaskOutput",
    "TaskStop",
    "TaskWait",
    "TeamCreate",
    "TeamDelete",
    "TeamList",
    "TodoWrite",
    "ToolSearch",
    "WebFetch",
    "WebSearch",
    "Write",
];

/// Registered tools this scene leaves out: ones with no counterpart in the
/// harnesses it is measured against, each a schema the model would otherwise
/// carry on every request.
const NOT_OFFERED: &[&str] = &[
    "EnterWorktree",
    "ExitWorktree",
    "NotebookEdit",
    "Ping",
    "PushNotification",
    "Sleep",
];

/// Built-in skills for coding work that runs to the end on its own.
const CODING_SKILLS: &[&str] = &["skillify", "simplify", "verify", "debug", "security-review"];

/// Built-in skills that hand something back to the user mid-task.
const USER_SKILLS: &[&str] = &[
    "batch",
    "stuck",
    "remember",
    "init",
    "import",
    "loop",
    "rename",
    "updateConfig",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Interaction {
    #[default]
    Interactive,
    Unattended,
}

pub fn spec(interaction: Interaction) -> SceneSpec {
    let interactive = interaction == Interaction::Interactive;
    let mut s = SceneSpec::new("code", "Agix Code", Arc::new(CodePrompt { interaction }));
    s.description = "编程场景 — 完整工具权限的通用编程 Agent".into();
    s.requires_project = true;
    s.supports_team = true;
    s.interactive = interactive;
    let deferred: Vec<&str> = ALL_DEFERRABLE_TOOLS
        .iter()
        .copied()
        .filter(|name| !RESIDENT_TOOLS.contains(name))
        .collect();
    s.tools = ToolSurface::denying(NOT_OFFERED).deferring(&deferred);
    let user: &[&str] = if interactive { USER_SKILLS } else { &[] };
    s.bundled_skills = CODING_SKILLS
        .iter()
        .chain(user)
        .map(|s| s.to_string())
        .collect();
    s.token_budget = TokenBudget::new(150_000, 20);
    s.reminders = vec![Reminder::GitStatus];
    s
}

/// The code scene's prompt: sections rebuilt only when their input changed
/// (see `SECTION_CACHE`).
#[derive(Debug, Clone, Copy)]
pub struct CodePrompt {
    pub interaction: Interaction,
}

impl ScenePrompt for CodePrompt {
    fn system(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        let sections = build_section_registry(ctx, self.interaction == Interaction::Interactive);
        render_prompt_blocks(resolve_sections(sections))
    }

    fn session_context(&self, ctx: &ScenePromptContext) -> String {
        common::session_context(ctx)
    }
}

#[derive(Debug, Clone)]
struct CachedSection {
    key: u64,
    value: String,
}

static SECTION_CACHE: OnceLock<Mutex<HashMap<&'static str, CachedSection>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionPhase {
    Static,
    Dynamic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionCachePolicy {
    StaticPrefix,
    Memoized { key: u64 },
}

struct SystemPromptSection<'a> {
    name: &'static str,
    phase: SectionPhase,
    cache: SectionCachePolicy,
    compute: Box<dyn FnOnce() -> Option<String> + 'a>,
}

struct ResolvedSection {
    name: &'static str,
    phase: SectionPhase,
    text: String,
}

fn cached_section(name: &'static str, key: u64, compute: impl FnOnce() -> String) -> String {
    let cache = SECTION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = guard.get(name) {
            if entry.key == key {
                return entry.value.clone();
            }
        }
    }
    let value = compute();
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(
        name,
        CachedSection {
            key,
            value: value.clone(),
        },
    );
    value
}

fn fingerprint<T: Hash>(value: &T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

fn static_section<'a>(
    name: &'static str,
    compute: impl FnOnce() -> String + 'a,
) -> SystemPromptSection<'a> {
    SystemPromptSection {
        name,
        phase: SectionPhase::Static,
        cache: SectionCachePolicy::StaticPrefix,
        compute: Box::new(move || Some(compute())),
    }
}

fn memoized_section<'a>(
    name: &'static str,
    key: u64,
    compute: impl FnOnce() -> Option<String> + 'a,
) -> SystemPromptSection<'a> {
    SystemPromptSection {
        name,
        phase: SectionPhase::Dynamic,
        cache: SectionCachePolicy::Memoized { key },
        compute: Box::new(compute),
    }
}

fn resolve_sections(sections: Vec<SystemPromptSection<'_>>) -> Vec<ResolvedSection> {
    let mut out = Vec::with_capacity(sections.len());
    for s in sections {
        let text = match s.cache {
            SectionCachePolicy::StaticPrefix => (s.compute)(),
            SectionCachePolicy::Memoized { key } => {
                let compute = s.compute;
                Some(cached_section(s.name, key, || {
                    compute().unwrap_or_default()
                }))
            }
        };
        if let Some(t) = text {
            if !t.trim().is_empty() {
                out.push(ResolvedSection {
                    name: s.name,
                    phase: s.phase,
                    text: t,
                });
            }
        }
    }
    out
}

/// Each static section is its own globally cached block, so editing one does
/// not invalidate the cache for the others; the dynamic tail is merged into a
/// single ephemeral block.
fn render_prompt_blocks(sections: Vec<ResolvedSection>) -> Vec<PromptBlock> {
    let mut out = Vec::new();
    let mut dynamic_texts = Vec::new();
    let mut dynamic_names = Vec::new();
    for s in sections {
        match s.phase {
            SectionPhase::Static => {
                out.push(PromptBlock {
                    role: base::prompt::BlockRole::System,
                    content: s.text,
                    cache_strategy: Some(CacheStrategy::Global),
                    name: Some(s.name.to_string()),
                    origin: base::prompt::BlockOrigin::Kernel,
                });
            }
            SectionPhase::Dynamic => {
                dynamic_texts.push(s.text);
                dynamic_names.push(s.name);
            }
        }
    }
    let d = dynamic_texts.join("\n\n");
    if !d.is_empty() {
        out.push(PromptBlock {
            role: base::prompt::BlockRole::System,
            content: d,
            cache_strategy: Some(CacheStrategy::Ephemeral),
            name: Some(dynamic_names.join(",")),
            origin: base::prompt::BlockOrigin::Kernel,
        });
    }
    out
}

fn build_section_registry<'a>(
    ctx: &'a ScenePromptContext,
    interactive: bool,
) -> Vec<SystemPromptSection<'a>> {
    let mut sections = Vec::with_capacity(12);
    let pick = |attended: &str, unattended: &str| match interactive {
        true => attended.to_string(),
        false => unattended.to_string(),
    };

    sections.push(static_section("identity", identity_block));
    let system_info = pick(SYSTEM_INFO_BLOCK, SYSTEM_INFO_UNATTENDED);
    sections.push(static_section("system_info", move || system_info));
    sections.push(static_section("working", || {
        format!("# Working\n\n{}", common::WORKING_RULES)
    }));
    sections.push(static_section("doing_tasks", || {
        DOING_TASKS_BLOCK.to_string()
    }));
    let actions = pick(ACTIONS_BLOCK, ACTIONS_UNATTENDED);
    sections.push(static_section("actions", move || actions));
    sections.push(static_section("tool_usage", || {
        TOOL_USAGE_BLOCK.to_string()
    }));
    let batch_tools = ctx.available_tools.clone().map(|s| s.into_owned());
    sections.push(SystemPromptSection {
        name: "batch_work",
        phase: SectionPhase::Static,
        cache: SectionCachePolicy::StaticPrefix,
        compute: Box::new(move || common::batch_work(batch_tools.as_deref())),
    });
    let tone = pick(TONE_STYLE_BLOCK, TONE_STYLE_UNATTENDED);
    sections.push(static_section("tone_style", move || tone));

    let model_name = ctx.model_name.to_string();
    let model_key = fingerprint(&model_name);
    sections.push(memoized_section("model", model_key, move || {
        Some(render_model(&model_name))
    }));

    if ctx.language.is_some() {
        let lang = ctx.language.clone().map(|s| s.into_owned());
        let lang_key = fingerprint(&lang);
        sections.push(memoized_section("language", lang_key, move || {
            Some(common::language_section(lang.as_deref().unwrap_or("en")))
        }));
    }

    if ctx.tool_results_ever_cleared {
        sections.push(memoized_section("function_result_clearing", 1, || {
            Some(common::FUNCTION_RESULTS_CLEARED.to_string())
        }));
    }

    if let Some(style) = ctx.output_style_content.clone().map(|s| s.into_owned()) {
        let style_key = fingerprint(&style);
        sections.push(memoized_section("output_style", style_key, move || {
            Some(format!("# Output style\n\n{style}\n"))
        }));
    }

    let tools_list = ctx.available_tools.clone().map(|s| s.into_owned());
    let tools_key = fingerprint(&(&tools_list, interactive));
    sections.push(memoized_section("session_guidance", tools_key, move || {
        Some(render_session_guidance(tools_list.as_deref(), interactive))
    }));

    sections
}

fn identity_block() -> String {
    format!(
        "You are Agix, OpenAtta's agent for software engineering tasks. Use the instructions \
         below and your tools to help the user.\n\n{}",
        common::SAFETY_RULES
    )
}

const SYSTEM_INFO_BLOCK: &str = "\
# System

- Text outside tool calls is shown to the user as GitHub-flavored markdown.
- Tools run under a permission mode; a call it does not allow goes to the user to approve or deny.
- Hooks may run around tool calls, the end of a turn and compaction. If one blocks you, adjust; if you can't, ask the user to check their hooks.
";

const SYSTEM_INFO_UNATTENDED: &str = "\
# System

- Nobody answers mid-task: text outside tool calls is your report to whoever started it, as GitHub-flavored markdown.
- Tools run under a permission policy; a call it does not allow is refused, not put to anyone. Work within it, and say in your report what it kept you from doing.
- Hooks may run around tool calls, the end of a turn and compaction. If one blocks you, adjust; if you can't, report it.
";

const DOING_TASKS_BLOCK: &str = "\
# Doing tasks

- Stay in scope: no cleanup, configurability or files (README, docs, tests) nobody asked for. Prefer editing an existing file.
- Don't over-engineer: no handling for cases that can't happen, validation only at system boundaries, no abstraction for a one-off; three similar lines beat a premature abstraction.
- Comments only for a non-obvious why. Match the surrounding code's naming, idiom and comment density.
- Don't change public APIs or data formats unless asked; no compatibility shims; delete what is unused.
- Write secure code: sanitize input, parameterize queries, never log secrets. Fix insecure code you notice.
- Diagnose failures from the actual error and fix the root cause. Don't blindly retry, and don't drop a viable approach after one failure.
- Ambiguous requests: take the most reasonable reading for software engineering in this directory. Never estimate how long something will take.
";

const ACTIONS_BLOCK: &str = "\
# Executing actions with care

Local, reversible actions such as editing files or running tests are fine. Check with the user before anything hard to undo or visible to others:
- destructive: deleting files or branches, dropping tables, killing processes, rm -rf, overwriting uncommitted changes;
- hard to reverse: force-pushing, git reset --hard, amending published commits, removing packages, changing CI/CD;
- visible to others: pushing, creating or commenting on PRs and issues, sending messages, posting to external services — an upload may stay cached or indexed after it is deleted.

Don't bypass safety checks (e.g. --no-verify) to get past an obstacle; fix the cause. Investigate unfamiliar files, branches or configuration before deleting or overwriting them — they may be someone's work in progress. Resolve merge conflicts rather than discarding changes.
";

const ACTIONS_UNATTENDED: &str = "\
# Executing actions with care

Local, reversible actions such as editing files or running tests are fine. Nobody is there to confirm anything else, so do it only when the task asks for it in so many words — otherwise leave it undone and say so in your report:
- destructive: deleting files or branches, dropping tables, killing processes, rm -rf, overwriting uncommitted changes;
- hard to reverse: force-pushing, git reset --hard, amending published commits, removing packages, changing CI/CD;
- visible to others: pushing, creating or commenting on PRs and issues, sending messages, posting to external services — an upload may stay cached or indexed after it is deleted.

Don't bypass safety checks (e.g. --no-verify) to get past an obstacle; fix the cause. Investigate unfamiliar files, branches or configuration before deleting or overwriting them — they may be someone's work in progress. Resolve merge conflicts rather than discarding changes.
";

const TOOL_USAGE_BLOCK: &str = "\
# Sub-agents and tracking

- Track multi-step work with TodoWrite.
- Agent runs a sub-agent with its own context: an explore agent for broad research (3+ searches), several in one message for independent questions; a general-purpose agent for multi-file changes once the research is done; `background: true` for long independent work — you are notified when it ends, so don't poll. Give it file paths, line numbers and exactly what to change, not \"fix it based on your findings\".
";

const TONE_STYLE_BLOCK: &str = "\
# Tone and output

- Be brief and direct: lead with the answer or action; no preamble, filler or restating the request. Code and tool calls are exempt.
- Spend text on decisions that need the user, status at milestones, and errors that change the plan. Summarize long tool output as its key points. No emojis unless asked.
- Reference code as `file_path:line_number`, and issues or pull requests as owner/repo#123.
- No colon before a tool call — write \"Let me read the file.\" rather than \"Let me read the file:\".
";

const TONE_STYLE_UNATTENDED: &str = "\
# Tone and output

- Be brief and direct: no preamble, filler or restating the task. Code and tool calls are exempt.
- End with the report: what changed, what you verified and how, and what is left or could not be done. Summarize long tool output as its key points. No emojis.
- Reference code as `file_path:line_number`, and issues or pull requests as owner/repo#123.
";

/// The Claude model list is advice for building on Claude, which only a
/// Claude model is in a position to give.
fn render_model(model_name: &str) -> String {
    match get_knowledge_cutoff(model_name) {
        Some(cutoff) => format!(
            "You are powered by the model {model_name}. Knowledge cutoff: {cutoff}.\n\n{}",
            build_model_recommendations(model_name)
        ),
        None => format!("You are powered by the model {model_name}."),
    }
}

fn get_knowledge_cutoff(model_name: &str) -> Option<&'static str> {
    let lower = model_name.to_lowercase();
    if lower.contains("claude-sonnet-4-6") {
        Some("August 2025")
    } else if lower.contains("claude-sonnet-4-5") {
        Some("June 2025")
    } else if lower.contains("claude-opus-4-8")
        || lower.contains("claude-opus-4-7")
        || lower.contains("claude-opus-4-6")
        || lower.contains("claude-opus-4-5")
    {
        Some("May 2025")
    } else if lower.contains("claude-haiku-4-5") {
        Some("February 2025")
    } else if lower.contains("claude-opus-4") || lower.contains("claude-sonnet-4") {
        Some("January 2025")
    } else if lower.contains("claude-haiku-4") {
        Some("December 2024")
    } else if lower.contains("claude-fable-5") {
        Some("May 2025")
    } else if lower.contains("claude-3-5") {
        Some("April 2025")
    } else if lower.contains("claude-3")
        && (lower.contains("opus") || lower.contains("sonnet") || lower.contains("haiku"))
    {
        Some("August 2024")
    } else if lower.contains("claude") {
        Some("early 2025")
    } else {
        None
    }
}

fn build_model_recommendations(model_name: &str) -> &'static str {
    let lower = model_name.to_lowercase();
    if lower.contains("claude-opus-4-8")
        || lower.contains("claude-sonnet-4-6")
        || lower.contains("claude-haiku-4-5")
    {
        "Latest Claude model IDs: Fable 5 'claude-fable-5', Opus 4.8 'claude-opus-4-8', \
         Sonnet 4.6 'claude-sonnet-4-6', Haiku 4.5 'claude-haiku-4-5-20251001'. When building \
         AI applications, default to the latest and most capable Claude models."
    } else if lower.contains("claude-4") || lower.contains("claude-3-5") {
        "Newer Claude models are available: Opus 4.8, Sonnet 4.6, Haiku 4.5, and Fable 5."
    } else {
        "Available Claude models include Opus 4.8 (most capable), Sonnet 4.6 (balanced), \
         Haiku 4.5 (fastest), and Fable 5 (latest). When building AI applications, default \
         to the latest and most capable Claude models."
    }
}

fn render_session_guidance(available_tools: Option<&str>, interactive: bool) -> String {
    let has = |name: &str| available_tools.map(|t| t.contains(name)).unwrap_or(true);

    let mut items = vec![];
    if interactive {
        items.push(
            "If you need the user to run a shell command themselves, suggest they type `! <command>` in the prompt.",
        );
    }
    if has("Skill") {
        items.push("`/<skill-name>` (e.g. /commit) is shorthand for a user-invocable skill; run it with the Skill tool.");
    }
    if interactive && has("AskUserQuestion") {
        items.push(
            "If you don't understand why the user denied a tool call, ask with AskUserQuestion.",
        );
    }

    let body: String = items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("# Session-specific guidance\n\n{body}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_ctx as ctx;
    use base::interface::scene::ReminderContext;
    use std::borrow::Cow;

    fn code() -> SceneSpec {
        spec(Interaction::Interactive)
    }

    fn text(blocks: &[PromptBlock]) -> String {
        blocks
            .iter()
            .map(|b| b.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn knowledge_cutoff_claude_3_5_not_shadowed_by_claude_3() {
        assert_eq!(
            get_knowledge_cutoff("claude-3-5-sonnet-20241022"),
            Some("April 2025")
        );
        assert_eq!(
            get_knowledge_cutoff("claude-3-5-haiku-20241022"),
            Some("April 2025")
        );
        assert_eq!(
            get_knowledge_cutoff("claude-3-opus-20240229"),
            Some("August 2024")
        );
    }

    #[test]
    fn only_a_claude_model_is_given_the_claude_model_list() {
        let mut c = ctx();
        c.model_name = Cow::Borrowed("deepseek-flash");
        let deepseek = text(&code().system_prompt(&c));
        assert!(deepseek.contains("You are powered by the model deepseek-flash."));
        assert!(!deepseek.contains("Claude"), "{deepseek}");

        let claude = text(&code().system_prompt(&ctx()));
        assert!(claude.contains("Knowledge cutoff: August 2025"));
        assert!(claude.contains("Latest Claude model IDs"));
    }

    #[test]
    fn code_id() {
        assert_eq!(code().id, "code");
    }

    #[test]
    fn prompt_contains_identity_and_behavior() {
        let text = text(&code().system_prompt(&ctx()));
        assert!(text.contains("Agix"));
        assert!(text.contains("software engineering"));
        assert!(text.contains("# Doing tasks"));
        assert!(text.contains("run in parallel"));
        assert!(!text.contains("say in one sentence what you're about to do"));
    }

    #[test]
    fn session_facts_are_in_the_context_not_the_prompt() {
        let mut c = ctx();
        c.scratchpad_dir = Some(Cow::Borrowed("/home/user/project/.agix/scratchpad"));
        c.git_status = Some(Cow::Borrowed("M src/main.rs"));
        let prompt = text(&code().system_prompt(&c));
        assert!(!prompt.contains("/home/user/project"), "{prompt}");
        assert!(!prompt.contains("2026-06-10"));
        assert!(!prompt.contains("M src/main.rs"));

        let context = code().session_context(&c);
        assert!(context.contains("Working directory: /home/user/project"));
        assert!(context.contains("Git branch: main"));
        assert!(context.contains(".agix/scratchpad"));
        assert!(
            !context.contains("M src/main.rs"),
            "the per-turn reminder has git status"
        );
    }

    #[test]
    fn prompt_is_identical_across_sessions() {
        let mut a = ctx();
        a.cwd = Cow::Borrowed("/a");
        a.date = Cow::Borrowed("2026-01-01");
        a.scratchpad_dir = Some(Cow::Borrowed("/a/.agix/scratchpad"));
        let mut b = ctx();
        b.cwd = Cow::Borrowed("/b");
        b.date = Cow::Borrowed("2026-02-02");
        b.git_branch = Some(Cow::Borrowed("dev"));
        b.scratchpad_dir = Some(Cow::Borrowed("/b/.agix/scratchpad"));
        assert_eq!(code().system_prompt(&a), code().system_prompt(&b));
    }

    #[test]
    fn static_sections_have_global_cache() {
        let blocks = code().system_prompt(&ctx());
        assert_eq!(blocks[0].cache_strategy, Some(CacheStrategy::Global));
    }

    #[test]
    fn reminder_includes_git() {
        let rctx = ReminderContext::new("/tmp").git_status("On branch main");
        let r = code().reminder(&rctx);
        assert!(r.contains("On branch main"));
        assert!(r.contains("system-reminder"));
    }

    #[test]
    fn reminder_leaves_the_memory_index_to_the_prompt() {
        let rctx = ReminderContext::new("/tmp").memory_summary("- [deploy](deploy.md)");
        assert_eq!(code().reminder(&rctx), "");
    }

    #[test]
    fn section_cache_is_reusable() {
        let b1 = code().system_prompt(&ctx());
        let b2 = code().system_prompt(&ctx());
        assert_eq!(b1.len(), b2.len());
        for (a, b) in b1.iter().zip(b2.iter()) {
            assert_eq!(a.content, b.content);
        }
    }

    #[test]
    fn function_result_clearing_only_appears_after_something_was_actually_cleared() {
        let mut ctx_fresh = ctx();
        ctx_fresh.tool_results_ever_cleared = false;
        let blocks_fresh = code().system_prompt(&ctx_fresh);
        let dynamic_fresh = &blocks_fresh.last().expect("dynamic block").content;
        assert!(
            !dynamic_fresh.contains("Function results have been cleared"),
            "should not claim results were cleared when nothing has been: {dynamic_fresh}"
        );

        let mut ctx_cleared = ctx();
        ctx_cleared.tool_results_ever_cleared = true;
        let blocks_cleared = code().system_prompt(&ctx_cleared);
        let dynamic_cleared = &blocks_cleared.last().expect("dynamic block").content;
        assert!(dynamic_cleared.contains("Function results have been cleared"));
    }

    /// `SECTION_CACHE` is process-wide: a key that ignored its input would
    /// hand one session's section to every other session.
    #[test]
    fn output_style_section_reflects_changed_content_not_stale_cache() {
        let mut ctx_a = ctx();
        ctx_a.output_style_content = Some(Cow::Borrowed("Style A: be terse."));
        let blocks_a = code().system_prompt(&ctx_a);
        assert!(blocks_a
            .last()
            .expect("dynamic block")
            .content
            .contains("Style A: be terse."));

        let mut ctx_b = ctx();
        ctx_b.output_style_content = Some(Cow::Borrowed("Style B: be verbose."));
        let blocks_b = code().system_prompt(&ctx_b);
        let dynamic_b = &blocks_b.last().expect("dynamic block").content;
        assert!(
            dynamic_b.contains("Style B: be verbose."),
            "got: {dynamic_b}"
        );
        assert!(!dynamic_b.contains("Style A: be terse."));
    }

    #[test]
    fn every_block_names_its_section() {
        let blocks = code().system_prompt(&ctx());
        assert!(blocks.iter().all(|b| b.name.is_some()));
        assert_eq!(blocks[0].name.as_deref(), Some("identity"));

        let tail = blocks.last().unwrap();
        assert_eq!(tail.cache_strategy, Some(CacheStrategy::Ephemeral));
        assert!(
            tail.name.as_deref().unwrap().contains(','),
            "the merged tail should list its sections, got {:?}",
            tail.name
        );
    }

    #[test]
    fn session_guidance_drops_lines_for_absent_tools() {
        let guidance = render_session_guidance(Some("Read,Bash"), true);
        assert!(!guidance.contains("Skill tool"));
        assert!(!guidance.contains("AskUserQuestion"));
        assert!(!guidance.contains("sub-agent"));
        assert!(guidance.contains("! <command>"));
    }

    #[test]
    fn an_unattended_prompt_does_not_count_on_a_reply() {
        let unattended = spec(Interaction::Unattended);
        let interactive = code();
        assert!(interactive.interactive && !unattended.interactive);

        let prompt = text(&unattended.system_prompt(&ctx()));
        for line in ["Check with the user", "`! <command>`", "AskUserQuestion"] {
            assert!(!prompt.contains(line), "{line}");
        }
        assert!(prompt.contains("Nobody is there to confirm"));
        assert!(text(&interactive.system_prompt(&ctx())).contains("Check with the user"));
    }

    #[test]
    fn only_an_interactive_session_gets_the_skills_that_hand_back_to_the_user() {
        let unattended = spec(Interaction::Unattended).bundled_skills;
        let interactive = code().bundled_skills;
        assert!(unattended
            .iter()
            .all(|s| CODING_SKILLS.contains(&s.as_str())));
        for skill in USER_SKILLS {
            assert!(interactive.iter().any(|s| s == skill), "{skill}");
            assert!(!unattended.iter().any(|s| s == skill), "{skill}");
        }
    }
}
