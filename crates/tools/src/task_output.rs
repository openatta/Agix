//! TaskOutputTool — retrieve output from a running or completed background task.
//!
//! Accepts `task_id` (required), `block` (default: true), and `timeout` (default: 30000ms,
//! max: 600000ms). When `block=true`, polls the running task store until the task completes
//! or the timeout expires.
//!
//! Returns structured content with `retrieval_status`, `task_id`, optional `output` and `status`.
//!
//! Uses `ToolContext.running_tasks` (RunningTasksCallback) to find the task and poll for
//! completion via repeated `find()` calls.

use async_trait::async_trait;
use base::context::RunningStatus;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, Tool, ToolContext, ToolResult, ToolSpec, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskOutputInput {
    /// The ID of the background task to retrieve output for
    pub task_id: String,
    /// Whether to block until the task completes (default: true)
    #[serde(default = "default_true")]
    pub block: bool,
    /// Maximum time to wait in milliseconds (default: 30000, max: 600000)
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    30_000
}

/// Retrieve output from a running or completed background task.
///
/// When `block=true` (default), the tool polls until the task finishes or the
/// timeout expires. Returns structured content with `retrieval_status`,
/// `task_id`, `output`, and `status`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn description(&self) -> &str {
        "Retrieve output from a running or completed background task"
    }

    fn name(&self) -> &str {
        "TaskOutput"
    }

    fn input_schema(&self) -> Value {
        crate::input_schema::<TaskOutputInput>()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<TaskOutputInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.task_id.trim().is_empty() => {
                ValidationResult::err("Missing required parameter: task_id", 1)
            }
            Ok(p) if p.timeout > 600_000 => {
                ValidationResult::err("timeout must not exceed 600000 ms", 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TaskOutputInput = serde_json::from_value(input)?;
        let result = self.retrieve(&input, &ctx).await;
        if let Ok(r) = &result {
            let ended = r
                .structured_content
                .as_ref()
                .and_then(|c| c.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| s != "running");
            if ended {
                ctx.session.retract_notice(&input.task_id);
            }
        }
        result
    }
}

impl TaskOutputTool {
    async fn retrieve(
        &self,
        input: &TaskOutputInput,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        // Look up the task in the running task store
        let (output, status) = match ctx
            .running_tasks
            .as_ref()
            .and_then(|rt| rt.find(&input.task_id))
        {
            None => {
                // Task not found
                return Ok(ToolResult {
                    content: base::tool::ToolResultContent::Text("task not found".to_string()),
                    is_error: true,
                    structured_content: Some(json!({
                        "retrieval_status": "not_found",
                        "task_id": input.task_id,
                    })),
                    mcp_meta: None,
                    new_messages: None,
                });
            }
            Some((output, _, status)) => (output, status),
        };

        // If running and block=true, wait for completion (poll up to timeout)
        if status == RunningStatus::Running && input.block {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(input.timeout);
            let mut current_output = output;

            while tokio::time::Instant::now() < deadline {
                sleep(Duration::from_millis(500)).await;

                if let Some((new_output, _, new_status)) = ctx
                    .running_tasks
                    .as_ref()
                    .and_then(|rt| rt.find(&input.task_id))
                {
                    current_output = new_output;
                    if new_status != RunningStatus::Running {
                        // Task completed (naturally, cancelled, or failed)
                        return Ok(ToolResult {
                            content: base::tool::ToolResultContent::Text(current_output.clone()),
                            is_error: false,
                            structured_content: Some(json!({
                                "retrieval_status": "success",
                                "task_id": input.task_id,
                                "output": current_output,
                                "status": status_to_str(&new_status),
                            })),
                            mcp_meta: None,
                            new_messages: None,
                        });
                    }
                } else {
                    // Task was removed during wait
                    return Ok(ToolResult {
                        content: base::tool::ToolResultContent::Text(
                            "task disappeared during wait".to_string(),
                        ),
                        is_error: true,
                        structured_content: Some(json!({
                            "retrieval_status": "not_found",
                            "task_id": input.task_id,
                        })),
                        mcp_meta: None,
                        new_messages: None,
                    });
                }
            }

            // Timeout expired, task still running
            return Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(current_output.clone()),
                is_error: false,
                structured_content: Some(json!({
                    "retrieval_status": "not_ready",
                    "task_id": input.task_id,
                    "output": current_output,
                    "status": "running",
                })),
                mcp_meta: None,
                new_messages: None,
            });
        }

        // Task is not running (completed/cancelled/failed), or block=false — return current state
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(output.clone()),
            is_error: false,
            structured_content: Some(json!({
                "retrieval_status": "success",
                "task_id": input.task_id,
                "output": output,
                "status": status_to_str(&status),
            })),
            mcp_meta: None,
            new_messages: None,
        })
    }
}

/// Convert a RunningStatus to a serializable &'static str.
fn status_to_str(status: &RunningStatus) -> &'static str {
    match status {
        RunningStatus::Running => "running",
        RunningStatus::Completed => "completed",
        RunningStatus::Cancelled => "cancelled",
        RunningStatus::Failed(_) => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::RunningTasksCallback;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    /// Mock RunningTasksCallback for testing
    #[derive(Debug)]
    struct MockTasks {
        tasks: HashMap<String, (String, Vec<String>, RunningStatus)>,
    }

    impl RunningTasksCallback for MockTasks {
        fn find(&self, tid: &str) -> Option<(String, Vec<String>, RunningStatus)> {
            self.tasks.get(tid).cloned()
        }
        fn cancel(&self, _tid: &str) -> bool {
            true
        }
    }

    fn ctx_with_tasks(tasks: MockTasks) -> ToolContext {
        let mut ctx = platform::tool_context_for_test(Path::new("/").to_path_buf());
        ctx.running_tasks = Some(Arc::new(tasks) as Arc<dyn RunningTasksCallback>);
        ctx
    }

    #[tokio::test]
    async fn name_is_task_output() {
        let tool = TaskOutputTool;
        assert_eq!(tool.name(), "TaskOutput");
        assert!(tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).concurrency_safe);
    }

    #[tokio::test]
    async fn requires_task_id() {
        let tool = TaskOutputTool;
        let r = tool
            .validate_input(
                &json!({}),
                &ctx_with_tasks(MockTasks {
                    tasks: HashMap::new(),
                }),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn empty_task_id_rejected() {
        let tool = TaskOutputTool;
        let r = tool
            .validate_input(
                &json!({"task_id": ""}),
                &ctx_with_tasks(MockTasks {
                    tasks: HashMap::new(),
                }),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn timeout_cap_enforced() {
        let tool = TaskOutputTool;
        let r = tool
            .validate_input(
                &json!({"task_id": "t1", "timeout": 999999}),
                &ctx_with_tasks(MockTasks {
                    tasks: HashMap::new(),
                }),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn not_found_returns_error() {
        let ctx = ctx_with_tasks(MockTasks {
            tasks: HashMap::new(),
        });
        let tool = TaskOutputTool;
        let r = tool
            .call(
                json!({"task_id": "nonexistent"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "not_found");
        assert_eq!(sc["task_id"], "nonexistent");
    }

    #[tokio::test]
    async fn completed_task_returns_output() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "ag-done".into(),
            ("output text".into(), vec![], RunningStatus::Completed),
        );
        let ctx = ctx_with_tasks(MockTasks { tasks });
        let tool = TaskOutputTool;
        let r = tool
            .call(
                json!({"task_id": "ag-done", "block": false}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "success");
        assert_eq!(sc["output"], "output text");
        assert_eq!(sc["status"], "completed");
    }

    #[tokio::test]
    async fn running_task_without_block_returns_not_ready() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "ag-running".into(),
            ("partial output".into(), vec![], RunningStatus::Running),
        );
        let ctx = ctx_with_tasks(MockTasks { tasks });
        let tool = TaskOutputTool;
        let r = tool
            .call(
                json!({"task_id": "ag-running", "block": false}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "success");
        assert_eq!(sc["output"], "partial output");
        assert_eq!(sc["status"], "running");
    }

    #[tokio::test]
    async fn failed_task_returns_status() {
        let mut tasks = HashMap::new();
        tasks.insert(
            "ag-fail".into(),
            (
                "error log".into(),
                vec![],
                RunningStatus::Failed("oops".into()),
            ),
        );
        let ctx = ctx_with_tasks(MockTasks { tasks });
        let tool = TaskOutputTool;
        let r = tool
            .call(
                json!({"task_id": "ag-fail", "block": false}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "success");
        assert_eq!(sc["output"], "error log");
        assert_eq!(sc["status"], "failed");
    }

    #[tokio::test]
    async fn running_with_block_waits_for_completion() {
        // Simulate a task that starts running and then completes
        let tasks = Arc::new(std::sync::Mutex::new(HashMap::new()));
        {
            let mut t = tasks.lock().unwrap();
            t.insert(
                "ag-blocking".into(),
                ("initial output".into(), vec![], RunningStatus::Running),
            );
        }

        struct BlockingMock {
            #[allow(clippy::type_complexity)]
            tasks: Arc<std::sync::Mutex<HashMap<String, (String, Vec<String>, RunningStatus)>>>,
            poll_count: std::sync::atomic::AtomicU32,
        }
        impl std::fmt::Debug for BlockingMock {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "BlockingMock")
            }
        }
        impl RunningTasksCallback for BlockingMock {
            fn find(&self, tid: &str) -> Option<(String, Vec<String>, RunningStatus)> {
                let prev = self
                    .poll_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let t = self.tasks.lock().unwrap();
                if prev >= 2 {
                    // After 2+ polls, mark as completed
                    if let Some((output, events, _)) = t.get(tid) {
                        return Some((
                            format!("{output} [final]"),
                            events.clone(),
                            RunningStatus::Completed,
                        ));
                    }
                }
                t.get(tid).cloned()
            }
            fn cancel(&self, _tid: &str) -> bool {
                true
            }
        }

        let mock = BlockingMock {
            tasks: tasks.clone(),
            poll_count: std::sync::atomic::AtomicU32::new(0),
        };

        let mut ctx = platform::tool_context_for_test(Path::new("/").to_path_buf());
        ctx.running_tasks = Some(Arc::new(mock));

        let tool = TaskOutputTool;
        let r = tool
            .call(
                json!({"task_id": "ag-blocking", "block": true, "timeout": 10000}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "success");
        assert_eq!(sc["output"], "initial output [final]");
        assert_eq!(sc["status"], "completed");
    }
}
