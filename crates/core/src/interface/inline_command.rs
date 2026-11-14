//! `InlineCommandHooks` — a shell command run outside any tool call of its
//! own (a skill's `` !`cmd` ``), as the session's hooks see it: a `Bash` call.

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::interface::permission::HookAnswer;

#[async_trait::async_trait]
pub trait InlineCommandHooks: Send + Sync {
    /// The session's `PreToolUse` hooks on `input` as a `Bash` call's input:
    /// the input the command runs with, and what they answered.
    async fn pre_tool_use(&self, input: Value, cancel: &CancellationToken) -> (Value, HookAnswer);
}
