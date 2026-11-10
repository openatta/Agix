//! `SleepTool` —— 让模型主动 yield N 毫秒后再继续。
//!
//! 用例：polling 模式 / 速率限制 / 让外部进程有时间起来。

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::Tool;
use base::tool::{
    PathStyle, PermissionDecision, ProgressSender, PromptContext, ToolContext, ToolResult,
    ToolResultContent, ToolSpec, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_SLEEP_MS: u64 = 30_000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SleepInput {
    pub duration_ms: u64,
}

pub struct SleepTool;

#[async_trait]
impl Tool for SleepTool {
    fn description(&self) -> &str {
        "Wait for a specified duration"
    }
    fn name(&self) -> &str {
        "Sleep"
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<SleepInput>()
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("prompts/coding/sleep.prompt.md").to_string())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<SleepInput>(input.clone()) {
            Ok(p) if p.duration_ms == 0 => ValidationResult::err("duration_ms must be > 0", 1),
            Ok(p) if p.duration_ms > MAX_SLEEP_MS => {
                ValidationResult::err(format!("duration_ms exceeds {MAX_SLEEP_MS} ms cap"), 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
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
        let input: SleepInput = serde_json::from_value(input)?;
        let dur = std::time::Duration::from_millis(input.duration_ms.min(MAX_SLEEP_MS));
        tokio::select! {
            _ = tokio::time::sleep(dur) => {}
            _ = ctx.cancel.cancelled() => {
                return Ok(ToolResult {
                    content: ToolResultContent::Text("Sleep cancelled".into()),
                    is_error: true,
                    structured_content: None,
                    mcp_meta: None,
                    new_messages: None,
                });
            }
        }
        Ok(ToolResult {
            content: ToolResultContent::Text(format!("Slept {} ms", dur.as_millis())),
            is_error: false,
            structured_content: Some(json!({"duration_ms": dur.as_millis()})),
            mcp_meta: None,
            new_messages: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::Tool;

    #[tokio::test]
    async fn validates_zero_duration() {
        let tool = SleepTool;
        let r = tool
            .validate_input(
                &json!({"duration_ms": 0}),
                &platform::tool_context_for_test("/tmp".into()),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn validates_over_cap() {
        let tool = SleepTool;
        let r = tool
            .validate_input(
                &json!({"duration_ms": 999_999}),
                &platform::tool_context_for_test("/tmp".into()),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn sleeps_short_duration() {
        let tool = SleepTool;
        let start = std::time::Instant::now();
        let r = tool
            .call(
                json!({"duration_ms": 50}),
                platform::tool_context_for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed >= std::time::Duration::from_millis(45));
        assert!(elapsed < std::time::Duration::from_millis(500));
        assert!(!r.is_error);
    }

    #[tokio::test]
    async fn cancellable() {
        let tool = SleepTool;
        let ctx = platform::tool_context_for_test("/tmp".into());
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let start = std::time::Instant::now();
        let r = tool
            .call(json!({"duration_ms": 5000}), ctx, ProgressSender::noop("t"))
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed < std::time::Duration::from_millis(1000));
        assert!(r.is_error);
    }
}
