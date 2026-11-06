//! Prompt pieces the built-in scenes share: the safety rules, the per-session
//! environment block, and the reminder wrapping.

use base::interface::prompt::PromptBlock;
use base::interface::scene::{ReminderContext, ScenePromptContext};

pub(crate) const SAFETY_RULES: &str = "\
IMPORTANT: Assist with authorized security testing, defensive security, CTF challenges, and \
educational contexts. Refuse requests for destructive attacks, DoS attacks, mass targeting, \
supply chain compromise, or detection evasion for malicious purposes. Dual-use security tools \
(C2 frameworks, credential testing, exploit development) require clear authorization context: \
pentesting engagements, CTF competitions, security research, or defensive use cases.

IMPORTANT: Never generate or guess URLs unless you are confident they help the user with \
programming. You may use URLs the user provides or that appear in local files; for documentation, \
search for the current URL instead of guessing.";

pub(crate) const FUNCTION_RESULTS_CLEARED: &str = "\
IMPORTANT: Function results have been cleared. You do not have access to previous tool call \
results; re-read files or re-run searches if you need that information.
";

pub(crate) fn language_section(lang: &str) -> String {
    format!(
        "# Language\n\nThe user prefers responses in {lang}. Always respond in {lang} unless they \
         ask for another language; code, technical terms and identifiers stay as they are.\n"
    )
}

pub(crate) fn scratchpad_section(dir: Option<&str>) -> String {
    let location = match dir {
        Some(dir) => format!("the session scratchpad at `{dir}`"),
        None => "the session scratchpad".to_string(),
    };
    format!(
        "# Scratchpad directory\n\nPut temporary files — intermediate results, throwaway \
         scripts or configs, outputs that don't belong in the project — in {location}, not \
         `/tmp` or other system temp directories. It is isolated from the project and needs no \
         permission prompts. Use `/tmp` only when explicitly told to.\n"
    )
}

pub(crate) fn reminder(ctx: &ReminderContext) -> String {
    let mut r = String::new();
    if let Some(ref git) = ctx.git_status {
        r.push_str(&format!("\n<system-reminder>\n{git}\n</system-reminder>"));
    }
    if let Some(ref mem) = ctx.memory_summary {
        r.push_str(&format!("\n<system-reminder>\n{mem}\n</system-reminder>"));
    }
    r
}

/// The per-session tail of a short scene prompt: environment, language,
/// scratchpad and the cleared-results notice, as one uncached block.
pub(crate) fn session_block(ctx: &ScenePromptContext) -> PromptBlock {
    let mut env = format!(
        "<env>\nWorking directory: {}\nOS: {}\nShell: {}\nDate: {}\n",
        ctx.cwd, ctx.os, ctx.shell, ctx.date
    );
    if ctx.is_git {
        env.push_str("Git repo: yes\n");
        if let Some(branch) = &ctx.git_branch {
            env.push_str(&format!("Git branch: {branch}\n"));
        }
        if ctx.is_worktree {
            env.push_str("This is a git worktree; changes here do not affect other worktrees.\n");
        }
        if let Some(status) = &ctx.git_status {
            env.push_str(&format!("gitStatus: {status}\n"));
        }
    }
    env.push_str(&format!(
        "</env>\nYou are powered by the model {}.",
        ctx.model_name
    ));

    let mut parts = vec![env];
    if let Some(lang) = &ctx.language {
        parts.push(language_section(lang));
    }
    if ctx.scratchpad_dir.is_some() {
        parts.push(scratchpad_section(ctx.scratchpad_dir.as_deref()));
    }
    if let Some(style) = &ctx.output_style_content {
        parts.push(format!("# Output style\n\n{style}\n"));
    }
    if ctx.tool_results_ever_cleared {
        parts.push(FUNCTION_RESULTS_CLEARED.to_string());
    }
    let mut block = PromptBlock::system(parts.join("\n\n"));
    block.name = Some("session".into());
    block
}

#[cfg(test)]
pub(crate) fn test_ctx() -> ScenePromptContext<'static> {
    use std::borrow::Cow;
    ScenePromptContext {
        cwd: Cow::Borrowed("/home/user/project"),
        os: Cow::Borrowed("linux"),
        shell: Cow::Borrowed("bash"),
        home_dir: Cow::Borrowed("/home/user"),
        date: Cow::Borrowed("2026-06-10"),
        model_name: Cow::Borrowed("claude-sonnet-4-6"),
        skills_text: None,
        mcp_instructions: None,
        session_memory: None,
        is_git: true,
        git_branch: Some(Cow::Borrowed("main")),
        is_worktree: false,
        git_status: None,
        language: None,
        scratchpad_dir: None,
        output_style_content: None,
        available_tools: None,
        tool_results_ever_cleared: false,
    }
}
