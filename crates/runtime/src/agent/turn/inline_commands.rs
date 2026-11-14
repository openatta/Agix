//! A skill's inline command, `` !`cmd` ``, as the session's hooks see it: a
//! `Bash` call, without the turn's tool call around it — whether it runs
//! inside the `Skill` tool's call or while a `/name` command expands.

use super::process::hook_permission_mode;
use super::tool_permission::answer_of_pre_tool_use;
use base::interface::permission::HookAnswer;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const BASH: &str = "Bash";

pub(super) struct SessionInlineCommands {
    pub(super) hooks: Arc<hooks::HookRunner>,
    pub(super) session_id: String,
    pub(super) cwd: std::path::PathBuf,
    pub(super) session_state: Arc<base::context::SessionState>,
}

impl SessionInlineCommands {
    fn payload(&self, event: &str, input: &Value) -> hooks::HookInput {
        hooks::HookInput::lifecycle(
            event,
            self.session_id.clone(),
            self.cwd.display().to_string(),
            hook_permission_mode(self.session_state.permission_mode()),
        )
        .with_tool(BASH, input.clone())
    }
}

#[async_trait::async_trait]
impl base::interface::inline_command::InlineCommandHooks for SessionInlineCommands {
    async fn pre_tool_use(&self, input: Value, cancel: &CancellationToken) -> (Value, HookAnswer) {
        if !self.hooks.has_hooks_for(hooks::HookEvent::PreToolUse) {
            return (input, HookAnswer::None);
        }
        let payload = self.payload("PreToolUse", &input);
        answer_of_pre_tool_use(&self.hooks, &payload, input, cancel).await
    }
}
