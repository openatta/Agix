//! `CronList` tool — list all scheduled cron jobs.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::store::CronStore;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};

pub struct CronListTool {
    store: Arc<CronStore>,
}

impl CronListTool {
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        "CronList"
    }

    fn description(&self) -> &str {
        "List scheduled cron jobs"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("../prompts/coding/cron_list.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        _input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let jobs = self.store.list_for(&ctx.session_id);
        if jobs.is_empty() {
            return Ok(ToolResult {
                content: base::tool::ToolResultContent::Text("No scheduled jobs.".into()),
                is_error: false,
                structured_content: Some(json!({"jobs": []})),
                mcp_meta: None,
                new_messages: Some(vec![]),
            });
        }
        let lines: Vec<String> = jobs
            .iter()
            .map(|j| {
                let kind = if j.recurring { "recurring" } else { "one-shot" };
                let durable = if j.durable {
                    " [persistent]"
                } else {
                    " [session]"
                };
                format!(
                    "  · {} — cron: {} ({}{}): {}",
                    j.id, j.cron, kind, durable, j.prompt
                )
            })
            .collect();
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(format!(
                "{} job(s):\n{}",
                jobs.len(),
                lines.join("\n")
            )),
            is_error: false,
            structured_content: Some(json!({"jobs": jobs})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::{ProgressSender, ToolContext};
    use serde_json::json;

    #[test]
    fn cron_list_empty() {
        let store = Arc::new(CronStore::new());
        let tool = CronListTool::new(store);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(async {
            tool.call(
                json!({}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop(""),
            )
            .await
            .unwrap()
        });
        assert!(!r.is_error);
        match &r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("No scheduled jobs"));
            }
            _ => panic!("expected text"),
        }
    }
}
