//! Harnix scene — the default scene of sessions hosted by Harnix: a common
//! working tool set and a short prompt, which a Harnix scene starts from to add
//! its own tools and instructions. Unattended: nobody answers mid-task, so the
//! tools and prompt lines that would wait on a reply are not here.

use crate::common;
use base::interface::scene::{
    Reminder, ScenePrompt, ScenePromptContext, SceneSpec, TokenBudget, ToolSurface,
};
use base::interface::tool::PathStyle;
use base::prompt::PromptBlock;
use std::sync::Arc;

pub const TOOLS: &[&str] = &[
    "Agent",
    "Bash",
    "Edit",
    "Glob",
    "GoalCreate",
    "GoalGet",
    "GoalUpdate",
    "Grep",
    "Read",
    "RunCode",
    "Skill",
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
    "mcp__*",
];

/// Reached for occasionally, so advertised by name until `ToolSearch` fetches
/// their schemas.
const DEFERRED: &[&str] = &[
    "TaskMessage",
    "TaskOutput",
    "TaskStop",
    "TaskWait",
    "TeamDelete",
    "TeamList",
];

pub fn spec() -> SceneSpec {
    let mut s = SceneSpec::new("harnix", "Harnix", Arc::new(HarnixPrompt));
    s.description = "Harnix 宿主会话 — 常用工具集与精简提示词，宿主在其上加自己的工具".into();
    s.supports_team = true;
    s.tools = ToolSurface::allowing(TOOLS).deferring(DEFERRED);
    s.path_style = PathStyle::WorkspaceRelative;
    s.token_budget = TokenBudget::new(150_000, 15);
    s.reminders = vec![Reminder::GitStatus];
    s.session_naming =
        Some("用 3-5 个词概括以下对话的主题，只输出标题不要任何解释：\n{first_message}".into());
    s
}

pub struct HarnixPrompt;

impl ScenePrompt for HarnixPrompt {
    fn system(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        let mut identity =
            PromptBlock::system_cached(format!("{IDENTITY}\n\n{}", common::SAFETY_RULES));
        identity.name = Some("identity".into());
        let mut text = format!("# Working\n\n{}{RULES}", common::WORKING_RULES);
        if let Some(batch) = common::batch_work(ctx.available_tools.as_deref()) {
            text.push_str(&format!("\n{batch}"));
        }
        let mut rules = PromptBlock::system_cached(text);
        rules.name = Some("rules".into());
        vec![identity, rules, common::session_block(ctx)]
    }

    fn session_context(&self, ctx: &ScenePromptContext) -> String {
        common::session_context(ctx)
    }
}

const IDENTITY: &str = "You are Agix, an agent hosted by Harnix. You work with code, files and \
information in the working directory; the host may add tools and instructions of its own.";

const RULES: &str = "\
- Nobody answers mid-task. Find facts by inspecting the workspace and your tools; where the task leaves a choice open, take the most reasonable reading and say which you took.
- Actions that are hard to undo or reach outside the workspace — deleting, overwriting, pushing, sending, publishing — only when the task asks for them in so many words; otherwise leave them undone and say so.
- A call the permission policy refuses was not put to anyone: work within the policy, and report what it kept you from doing.
- Track work of 3+ steps with TodoWrite: exactly one item in_progress, each marked completed as soon as it is done.
- Hand self-contained work to a sub-agent (Agent) with a brief that stands on its own — file paths, what to change, what to return; independent agents go in one message. For long work use `background: true` and wait for the completion notice instead of polling; TaskOutput, TaskWait and TaskStop reach background work.
- Search the web (WebSearch) for current facts and documentation rather than answering from memory or guessing URLs; WebFetch reads a given page.
- Use Skill for a matching skill or a `/<skill-name>` in the task. ToolSearch fetches the schema of a tool listed by name only, and a tool's detailed guide.
- For an objective one turn won't finish, set a goal with GoalCreate: rounds then continue automatically until you mark it complete with GoalUpdate. Mark complete only when the whole objective is achieved, and blocked only when the same blocker has held for several rounds.
- End with a short report: the answer or what changed, what you verified, what is left. No emojis. Reference code as `file_path:line_number`.
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_ctx;
    use base::interface::tool::ToolOrigin;

    #[test]
    fn every_deferred_tool_is_also_allowed() {
        for name in DEFERRED {
            assert!(TOOLS.contains(name), "{name} is deferred but not allowed");
        }
    }

    #[test]
    fn tools_admit_mcp_and_leave_out_the_rest() {
        let scene = spec();
        let builtin = ToolOrigin::Builtin;
        assert!(scene.permits_tool("mcp__github__search", &ToolOrigin::mcp("github")));
        assert!(scene.permits_tool("Agent", &builtin));
        assert!(!scene.interactive);
        assert_eq!(scene.path_style, PathStyle::WorkspaceRelative);
        for excluded in [
            "AskUserQuestion",
            "EnterPlanMode",
            "CronCreate",
            "NotebookEdit",
            "EnterWorktree",
        ] {
            assert!(!scene.permits_tool(excluded, &builtin), "{excluded}");
        }
    }

    #[test]
    fn session_facts_stay_out_of_the_prompt() {
        let blocks = spec().system_prompt(&test_ctx());
        assert_eq!(blocks.len(), 3);
        assert!(blocks[0].content.contains("Harnix"));
        assert!(!blocks[2].content.contains("/home/user/project"));
        assert!(blocks[2].content.contains("claude-sonnet-4-6"));
        assert!(spec()
            .session_context(&test_ctx())
            .contains("/home/user/project"));
    }
}
