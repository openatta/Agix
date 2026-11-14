//! Mini scene — the smallest working coding agent: nine tools, all resident,
//! and a prompt of a few lines. Unattended, like harnix.

use crate::common;
use base::interface::scene::{
    Reminder, ScenePrompt, ScenePromptContext, SceneSpec, TokenBudget, ToolSurface,
};
use base::prompt::PromptBlock;
use std::sync::Arc;

pub const TOOLS: &[&str] = &[
    "Agent",
    "Bash",
    "Edit",
    "Glob",
    "Grep",
    "Read",
    "Skill",
    "ToolSearch",
    "Write",
];

pub fn spec() -> SceneSpec {
    let mut s = SceneSpec::new("mini", "Agix Mini", Arc::new(MiniPrompt));
    s.description = "最小编码场景 — 九个常驻工具与几行提示词".into();
    s.tools = ToolSurface::allowing(TOOLS);
    s.token_budget = TokenBudget::new(150_000, 10);
    s.reminders = vec![Reminder::GitStatus];
    s
}

pub struct MiniPrompt;

impl ScenePrompt for MiniPrompt {
    fn system(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        let mut identity =
            PromptBlock::system_cached(format!("{PROMPT}\n{}", common::SAFETY_RULES));
        identity.name = Some("identity".into());
        vec![identity, common::session_block(ctx)]
    }

    fn session_context(&self, ctx: &ScenePromptContext) -> String {
        common::session_context(ctx)
    }
}

const PROMPT: &str = "\
You are Agix, a software engineering agent.

- Read a file before editing it; change only what was asked.
- Locate with Glob or Grep before reading; put independent tool calls in one message.
- Verify the finished change, not every edit, and report failures as they are.
- Nobody answers mid-task: take the most reasonable reading where the task leaves a choice open.
- Destructive or outward-facing actions only when the task asks for them; otherwise leave them undone and say so.
- If a tool call is denied, don't repeat it unchanged; report what you could not do.
- End with a brief report: what changed, what you verified, what is left.
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_ctx;
    use base::interface::tool::ToolOrigin;

    #[test]
    fn nine_tools_none_deferred() {
        let mini = spec();
        assert_eq!(mini.tools.allow.as_ref().map(Vec::len), Some(9));
        assert!(!mini.interactive);
        assert!(mini.tools.deferred.is_empty());
        assert!(!mini.permits_tool("WebFetch", &ToolOrigin::Builtin));
        assert!(!mini.permits_tool("mcp__github__search", &ToolOrigin::mcp("github")));
    }

    #[test]
    fn prompt_is_short() {
        let blocks = spec().system_prompt(&test_ctx());
        assert!(blocks[0].content.len() < 1_500);
        assert!(!blocks[1].content.contains("/home/user/project"));
        assert!(spec()
            .session_context(&test_ctx())
            .contains("/home/user/project"));
    }
}
