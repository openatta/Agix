//! `CronDelete` tool — cancel a previously-scheduled cron job.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::store::CronStore;
use base::error::ToolError;
use base::tool::{
    PathStyle, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ToolSpec, ValidationResult,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CronDeleteInput {
    /// Job ID returned by CronCreate.
    pub id: String,
}

pub struct CronDeleteTool {
    store: Arc<CronStore>,
}

impl CronDeleteTool {
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        "CronDelete"
    }

    fn description(&self) -> &str {
        "Cancel a scheduled cron job by ID"
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<CronDeleteInput>()
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("../prompts/coding/cron_delete.prompt.md").to_string())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<CronDeleteInput>(input.clone()) {
            Ok(p) if p.id.trim().is_empty() => ValidationResult::err("id must not be empty", 1),
            Ok(_) => {
                // Validate the job exists
                let exists = self
                    .store
                    .list()
                    .iter()
                    .any(|j| j.id == input.get("id").and_then(|v| v.as_str()).unwrap_or(""));
                if !exists {
                    return ValidationResult::err("No scheduled job with that id", 2);
                }
                ValidationResult::Ok
            }
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
        let input: CronDeleteInput = serde_json::from_value(input)?;
        if self.store.remove_for(&ctx.session_id, &input.id) {
            Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "Cancelled job {}.",
                    input.id
                )),
                is_error: false,
                structured_content: Some(json!({"id": input.id})),
                mcp_meta: None,
                new_messages: Some(vec![]),
            })
        } else {
            Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "No job found with id '{}'",
                    input.id
                )),
                is_error: true,
                structured_content: None,
                mcp_meta: None,
                new_messages: Some(vec![]),
            })
        }
    }
}
