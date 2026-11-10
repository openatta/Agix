//! TaskStopTool — stop a running background task by ID.
//!
//! Accepts `task_id` (required) and `shell_id` (optional, deprecated alias for
//! backward compatibility with the removed KillShell tool).
//!
//! Uses `ToolContext.running_tasks` (RunningTasksCallback) to find and cancel
//! the task via its CancellationToken.

use async_trait::async_trait;
use base::context::RunningStatus;
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::{
    PathStyle, PermissionDecision, ProgressSender, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskStopInput {
    /// The ID of the background task to stop
    #[serde(default)]
    pub task_id: Option<String>,
    /// Deprecated: use task_id instead
    #[serde(default)]
    pub shell_id: Option<String>,
}

/// Stop a running background task by ID.
///
/// Resolves the effective task ID from `task_id ?? shell_id` (shell_id is a
/// deprecated alias for KillShell backward compatibility). Checks that the
/// task exists and is running before sending the cancel signal.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn description(&self) -> &str {
        "Stop a running background task by ID"
    }

    fn name(&self) -> &str {
        "TaskStop"
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<TaskStopInput>()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<TaskStopInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) => {
                let id = p.task_id.or(p.shell_id);
                match id {
                    Some(id) if id.trim().is_empty() => {
                        ValidationResult::err("Missing required parameter: task_id", 1)
                    }
                    Some(_) => ValidationResult::Ok,
                    None => ValidationResult::err("Missing required parameter: task_id", 1),
                }
            }
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
        let input: TaskStopInput = serde_json::from_value(input)?;
        let id = input.task_id.or(input.shell_id).unwrap_or_default();

        // Locate the task via RunningTasksCallback
        let running_task = ctx.running_tasks.as_ref().and_then(|rt| rt.find(&id));

        match running_task {
            Some((_, _, RunningStatus::Running)) => {
                // Task exists and is running — cancel it
                let cancelled = ctx
                    .running_tasks
                    .as_ref()
                    .map(|rt| rt.cancel(&id))
                    .unwrap_or(false);

                if cancelled {
                    Ok(ToolResult {
                        content: base::tool::ToolResultContent::Text(format!(
                            "Successfully stopped task: {id}"
                        )),
                        is_error: false,
                        structured_content: Some(json!({"task_id": id})),
                        mcp_meta: None,
                        new_messages: None,
                    })
                } else {
                    Ok(ToolResult::error_text(format!("Failed to stop task: {id}")))
                }
            }
            Some((_, _, status)) => {
                // Task exists but is not running
                Ok(ToolResult::error_text(format!(
                    "Task {id} is not running (status: {status:?})"
                )))
            }
            None => Ok(ToolResult::error_text(format!(
                "No task found with ID: {id}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::RunningTasksCallback;
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;

    /// Mock RunningTasksCallback for testing
    #[derive(Debug)]
    struct MockTasks {
        tasks: std::collections::HashMap<String, (String, Vec<String>, RunningStatus)>,
    }

    impl RunningTasksCallback for MockTasks {
        fn find(&self, tid: &str) -> Option<(String, Vec<String>, RunningStatus)> {
            self.tasks.get(tid).cloned()
        }
        fn cancel(&self, tid: &str) -> bool {
            self.tasks.contains_key(tid)
        }
    }

    fn ctx_with_tasks(tasks: MockTasks) -> ToolContext {
        let mut ctx = platform::tool_context_for_test(Path::new("/").to_path_buf());
        ctx.running_tasks = Some(Arc::new(tasks) as Arc<dyn RunningTasksCallback>);
        ctx
    }

    #[tokio::test]
    async fn name_is_task_stop() {
        let tool = TaskStopTool;
        assert_eq!(tool.name(), "TaskStop");
        assert!(!tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).concurrency_safe);
    }

    #[tokio::test]
    async fn requires_at_least_one_id() {
        let tool = TaskStopTool;
        let r = tool
            .validate_input(
                &json!({}),
                &ctx_with_tasks(MockTasks {
                    tasks: std::collections::HashMap::new(),
                }),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn empty_task_id_is_rejected() {
        let tool = TaskStopTool;
        let r = tool
            .validate_input(
                &json!({"task_id": ""}),
                &ctx_with_tasks(MockTasks {
                    tasks: std::collections::HashMap::new(),
                }),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn shell_id_backward_compat() {
        let tool = TaskStopTool;
        let r = tool
            .validate_input(
                &json!({"shell_id": "sh-abc123"}),
                &ctx_with_tasks(MockTasks {
                    tasks: std::collections::HashMap::new(),
                }),
            )
            .await;
        assert!(matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn stops_running_task() {
        let mut tasks = std::collections::HashMap::new();
        tasks.insert(
            "ag-test".into(),
            ("ag-test".into(), vec![], RunningStatus::Running),
        );
        let ctx = ctx_with_tasks(MockTasks { tasks });
        let tool = TaskStopTool;
        let r = tool
            .call(
                json!({"task_id": "ag-test"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(ref t) => {
                assert!(t.contains("Successfully stopped"), "got: {t}");
            }
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn errors_on_not_found() {
        let ctx = ctx_with_tasks(MockTasks {
            tasks: std::collections::HashMap::new(),
        });
        let tool = TaskStopTool;
        let r = tool
            .call(
                json!({"task_id": "nonexistent"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(ref t) => {
                assert!(t.contains("No task found"), "got: {t}");
            }
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn errors_on_not_running() {
        let mut tasks = std::collections::HashMap::new();
        tasks.insert(
            "ag-completed".into(),
            ("ag-completed".into(), vec![], RunningStatus::Completed),
        );
        let ctx = ctx_with_tasks(MockTasks { tasks });
        let tool = TaskStopTool;
        let r = tool
            .call(
                json!({"task_id": "ag-completed"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(ref t) => {
                assert!(t.contains("not running"), "got: {t}");
            }
            _ => panic!("expected Text content"),
        }
    }
}
