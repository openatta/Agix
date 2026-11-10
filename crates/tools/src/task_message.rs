//! TaskMessage — send a message to a background sub-agent while it works.
//!
//! The message joins the sub-agent's running turn at its next step boundary,
//! the way a user's message sent mid-turn joins the main agent's. Work that
//! takes no messages (a shell command) has no inbox and is refused.

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::{
    PathStyle, PermissionDecision, ProgressSender, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskMessageInput {
    /// The background task running the sub-agent.
    pub task_id: String,
    /// What to tell it: a correction, an extra constraint, a request to wrap up.
    pub message: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TaskMessageTool;

/// Deliver `message` to task `task_id` of `session`; `Err` says why not.
pub fn deliver(
    session: &base::context::SessionState,
    task_id: &str,
    message: String,
) -> Result<(), String> {
    let task = session
        .find_running_task(task_id)
        .ok_or_else(|| format!("no such task: {task_id}"))?;
    if task.has_ended() {
        return Err(format!("task {task_id} has already ended"));
    }
    let inbox = task
        .inbox
        .get()
        .ok_or_else(|| format!("task {task_id} takes no messages"))?;
    inbox.deliver(message)
}

#[async_trait]
impl Tool for TaskMessageTool {
    fn description(&self) -> &str {
        "Send a message to a running background sub-agent"
    }

    fn name(&self) -> &str {
        "TaskMessage"
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<TaskMessageInput>()
    }

    async fn guide(&self, _: &base::tool::PromptContext) -> Option<String> {
        Some(
            "Send a message to a background sub-agent while it is still working: a correction, \
         a constraint it did not have, or a request to stop and report what it has. It \
         reads the message at its next step, as you read a message the user sends while \
         you work. Refused for tasks that have ended or that are not sub-agents."
                .to_string(),
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .concurrency_safe(true)
            .self_allow_overrides_mode(true)
    }

    // Talking to one's own sub-agent touches no file, shell or network; a
    // plan-mode agent must still be able to steer the agents it started.

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<TaskMessageInput>(input.clone()) {
            Ok(i) if i.message.trim().is_empty() => ValidationResult::err("message is empty", 1),
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TaskMessageInput = serde_json::from_value(input)?;
        Ok(match deliver(&ctx.session, &input.task_id, input.message) {
            Ok(()) => ToolResult::text(format!("delivered to {}", input.task_id)),
            Err(why) => ToolResult::error_text(why),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::context::{RunningStatus, SessionState, TaskInbox, TaskType};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    struct Recorder(Mutex<Vec<String>>);
    impl TaskInbox for Recorder {
        fn deliver(&self, message: String) -> Result<(), String> {
            self.0.lock().unwrap().push(message);
            Ok(())
        }
    }

    #[test]
    fn a_message_reaches_the_inbox_of_a_running_task_only() {
        let session = SessionState::new(std::env::temp_dir());
        let task = session.register_task("ag-1", TaskType::Agent, Default::default(), Value::Null);
        session.register_task("sh-1", TaskType::Shell, Default::default(), Value::Null);
        let recorder = Arc::new(Recorder::default());
        task.inbox.set(recorder.clone()).unwrap();

        deliver(&session, "ag-1", "focus on the parser".into()).unwrap();
        assert_eq!(*recorder.0.lock().unwrap(), vec!["focus on the parser"]);
        assert!(deliver(&session, "sh-1", "x".into())
            .unwrap_err()
            .contains("takes no messages"));
        assert!(deliver(&session, "nope", "x".into())
            .unwrap_err()
            .contains("no such task"));
        session.finish_task("ag-1", RunningStatus::Completed);
        assert!(deliver(&session, "ag-1", "x".into())
            .unwrap_err()
            .contains("already ended"));
    }
}
