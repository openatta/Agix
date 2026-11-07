//! Mini scene — the smallest working coding agent: ten tools, all resident,
//! and a prompt of a few lines.

use crate::common;
use base::interface::prompt::PromptBlock;
use base::interface::scene::{ReminderContext, Scene, ScenePromptContext, TokenBudget};

pub const TOOLS: &[&str] = &[
    "Agent",
    "AskUserQuestion",
    "Bash",
    "Edit",
    "Glob",
    "Grep",
    "Read",
    "Skill",
    "ToolSearch",
    "Write",
];

pub struct MiniScene;

impl Scene for MiniScene {
    fn id(&self) -> &str {
        "mini"
    }
    fn name(&self) -> &str {
        "Agix Mini"
    }
    fn description(&self) -> &str {
        "最小编码场景 — 十个常驻工具与几行提示词"
    }

    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        let mut identity =
            PromptBlock::system_cached(format!("{PROMPT}\n{}", common::SAFETY_RULES));
        identity.name = Some("identity".into());
        vec![identity, common::session_block(ctx)]
    }

    fn tools(&self) -> Vec<String> {
        TOOLS.iter().map(|s| s.to_string()).collect()
    }

    fn token_budget(&self) -> TokenBudget {
        TokenBudget {
            compact_threshold: 150_000,
            compact_keep_recent: 10,
        }
    }

    fn build_system_reminder(&self, ctx: &ReminderContext) -> String {
        common::reminder(ctx)
    }

    fn build_session_context(&self, ctx: &ScenePromptContext) -> String {
        common::session_context(ctx)
    }
}

const PROMPT: &str = "\
You are Agix, a software engineering agent.

- Read a file before editing it; change only what was asked.
- Verify your work before saying it is done, and report failures as they are.
- Confirm before destructive or outward-facing actions.
- If a tool call is denied, don't repeat it unchanged.
- Be brief.
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_ctx;

    #[test]
    fn ten_tools_none_deferred() {
        assert_eq!(MiniScene.tools().len(), 10);
        assert!(MiniScene.deferred_tools().is_empty());
        assert!(!MiniScene.permits_tool("WebFetch"));
        assert!(!MiniScene.permits_tool("mcp__github__search"));
    }

    #[test]
    fn prompt_is_short() {
        let blocks = MiniScene.build_system_prompt(&test_ctx());
        assert!(blocks[0].content.len() < 1_500);
        assert!(!blocks[1].content.contains("/home/user/project"));
        assert!(MiniScene
            .build_session_context(&test_ctx())
            .contains("/home/user/project"));
    }
}
