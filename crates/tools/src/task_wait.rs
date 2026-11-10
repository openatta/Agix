//! TaskWait — wait for any or all of several background tasks to end.
//!
//! The counterpart of TaskOutput for more than one task: the agent started a
//! few sub-agents in the background and wants to continue when the first
//! (`any`) or the last (`all`) of them is done, without polling each in turn.

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, Tool, ToolContext, ToolResult, ToolResultContent, ToolSpec,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

const MAX_TASKS: usize = 16;
const MAX_TIMEOUT_MS: u64 = 600_000;
const OUTPUT_TAIL_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WaitMode {
    /// Return as soon as one of the tasks ends.
    #[default]
    Any,
    /// Return once every task has ended.
    All,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskWaitInput {
    /// The background tasks to wait for (at most 16).
    pub task_ids: Vec<String>,
    /// `any` (default) or `all`.
    #[serde(default)]
    pub mode: WaitMode,
    /// How long to wait at most, in milliseconds (default 60000, max 600000).
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    60_000
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TaskWaitTool;

#[async_trait]
impl Tool for TaskWaitTool {
    fn description(&self) -> &str {
        "Wait for any or all of several background tasks to end"
    }

    fn name(&self) -> &str {
        "TaskWait"
    }

    fn input_schema(&self) -> Value {
        crate::input_schema::<TaskWaitInput>()
    }

    async fn prompt(&self, _: &base::tool::PromptContext) -> String {
        "Wait for background tasks (sub-agents started with `background: true`, shell \
         commands, team members' replies) to end.\n\n\
         - `mode: \"any\"` returns as soon as one ends; `\"all\"` once every one has.\n\
         - Returns each task's status and the tail of its output; tasks still running \
         when the wait ends are listed as running.\n\
         - You are told about a finished background sub-agent anyway, so wait only when \
         you have nothing else to do until it is done."
            .to_string()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            concurrency_safe: true,
            read_only: true,
            ..ToolSpec::defaults_for(self)
        }
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<TaskWaitInput>(input.clone()) {
            Ok(i) if i.task_ids.is_empty() => ValidationResult::err("task_ids is empty", 1),
            Ok(i) if i.task_ids.len() > MAX_TASKS => {
                ValidationResult::err(format!("at most {MAX_TASKS} task_ids"), 2)
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
        let input: TaskWaitInput = serde_json::from_value(input)?;
        let mut tasks = Vec::new();
        let mut missing = Vec::new();
        for id in &input.task_ids {
            match ctx.session.find_running_task(id) {
                Some(t) => tasks.push(t),
                None => missing.push(id.clone()),
            }
        }
        if !missing.is_empty() {
            return Ok(ToolResult::error_text(format!(
                "no such task: {}",
                missing.join(", ")
            )));
        }

        let timeout = std::time::Duration::from_millis(input.timeout_ms.min(MAX_TIMEOUT_MS));
        let ends = tasks.iter().map(|t| {
            let t = Arc::clone(t);
            Box::pin(async move { t.wait_ended().await })
        });
        let waited = async {
            match input.mode {
                WaitMode::Any => {
                    futures::future::select_all(ends).await;
                }
                WaitMode::All => {
                    futures::future::join_all(ends).await;
                }
            }
        };
        let timed_out = tokio::select! {
            _ = waited => false,
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            _ = tokio::time::sleep(timeout) => true,
        };

        let mut report = Vec::new();
        let mut text = String::new();
        for t in &tasks {
            let info = t.info();
            let status = info["status"].as_str().unwrap_or("running").to_string();
            let mut entry = json!({"task_id": t.task_id, "status": status});
            text.push_str(&format!("## {} — {status}\n", t.task_id));
            if t.has_ended() {
                ctx.session.retract_notice(&t.task_id);
                let tail = t.output_tail(OUTPUT_TAIL_BYTES);
                if let Some(e) = info["error"].as_str() {
                    text.push_str(&format!("error: {e}\n"));
                    entry["error"] = json!(e);
                }
                if !tail.text.is_empty() {
                    if tail.from_byte > 0 {
                        text.push_str("(the start is cut; TaskOutput has what is kept)\n");
                    }
                    text.push_str(&tail.text);
                    text.push('\n');
                }
                entry["output"] = json!(tail.text);
            }
            text.push('\n');
            report.push(entry);
        }
        if timed_out {
            text.push_str("(the wait timed out)\n");
        }
        Ok(ToolResult {
            content: ToolResultContent::Text(text),
            is_error: false,
            structured_content: Some(json!({"tasks": report, "timed_out": timed_out})),
            mcp_meta: None,
            new_messages: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::context::{RunningStatus, SessionState, TaskType};
    use tokio_util::sync::CancellationToken;

    fn ctx_with(session: Arc<SessionState>) -> ToolContext {
        let mut ctx = ToolContext::for_test(std::env::temp_dir());
        ctx.session = session;
        ctx
    }

    fn register(session: &SessionState, id: &str) {
        session.register_task(id, TaskType::Agent, CancellationToken::new(), Value::Null);
    }

    async fn wait(session: &Arc<SessionState>, input: Value) -> Value {
        let result = TaskWaitTool
            .call(input, ctx_with(session.clone()), ProgressSender::noop("t"))
            .await
            .unwrap();
        result.structured_content.unwrap()
    }

    #[tokio::test]
    async fn any_returns_when_the_first_task_ends() {
        let session = Arc::new(SessionState::new(std::env::temp_dir()));
        register(&session, "a");
        register(&session, "b");
        let s = session.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            s.find_running_task("b")
                .unwrap()
                .append_output("b's answer");
            s.finish_task("b", RunningStatus::Completed);
        });
        let out = wait(
            &session,
            json!({"task_ids": ["a", "b"], "timeout_ms": 5000}),
        )
        .await;
        assert_eq!(out["timed_out"], false);
        assert_eq!(out["tasks"][0]["status"], "running");
        assert_eq!(out["tasks"][1]["status"], "done");
        assert_eq!(out["tasks"][1]["output"], "b's answer");
    }

    #[tokio::test]
    async fn all_waits_for_every_task_and_withdraws_their_notices() {
        let session = Arc::new(SessionState::new(std::env::temp_dir()));
        register(&session, "a");
        register(&session, "b");
        session.finish_task("a", RunningStatus::Completed);
        session.post_notice(Some("a".into()), "a finished");
        let s = session.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            s.finish_task("b", RunningStatus::Failed("boom".into()));
        });
        let out = wait(
            &session,
            json!({"task_ids": ["a", "b"], "mode": "all", "timeout_ms": 5000}),
        )
        .await;
        assert_eq!(out["timed_out"], false);
        assert_eq!(out["tasks"][1]["status"], "failed");
        assert_eq!(out["tasks"][1]["error"], "boom");
        assert!(session.take_notices().is_empty());
    }

    #[tokio::test]
    async fn a_wait_that_runs_out_says_so() {
        let session = Arc::new(SessionState::new(std::env::temp_dir()));
        register(&session, "a");
        let out = wait(&session, json!({"task_ids": ["a"], "timeout_ms": 20})).await;
        assert_eq!(out["timed_out"], true);
        assert_eq!(out["tasks"][0]["status"], "running");
    }
}
