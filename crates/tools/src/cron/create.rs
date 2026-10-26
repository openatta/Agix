//! `CronCreate` tool — schedule a prompt for future execution.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::parser::cron_expression_valid;
use super::store::CronStore;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CronCreateInput {
    /// Standard 5-field cron expression in local time:
    /// "M H DoM Mon DoW" (e.g. "*/5 * * * *" = every 5 minutes,
    /// "30 14 28 2 *" = Feb 28 at 2:30pm local once).
    pub cron: String,
    /// The prompt to enqueue at each fire time.
    pub prompt: String,
    /// true (default) = fire on every cron match until deleted or
    /// auto-expired after 7 days. false = fire once at the next
    /// match, then auto-delete.
    #[serde(default = "default_true")]
    pub recurring: bool,
    /// true = persist to ~/.agix/scenes/<scope>/scheduled_tasks.json and survive
    /// restarts. false (default) = in-memory only, dies when this
    /// session ends. Use true only when the user explicitly asks
    /// the task to survive across sessions.
    #[serde(default)]
    pub durable: bool,
}

fn default_true() -> bool {
    true
}

pub struct CronCreateTool {
    store: Arc<CronStore>,
}

impl CronCreateTool {
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        "CronCreate"
    }

    fn description(&self) -> &str {
        "Schedule a prompt to be enqueued at a future time. Use for both \
         recurring schedules and one-shot reminders."
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(CronCreateInput)).expect("schema")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("../prompts/coding/cron_create.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<CronCreateInput>(input.clone()) {
            Ok(p) => {
                if p.cron.trim().is_empty() {
                    return ValidationResult::err("cron must not be empty", 1);
                }
                // Relaxed validation: accept name-based ranges like MON-FRI
                if !cron_expression_valid(&p.cron) {
                    return ValidationResult::err(
                        "Invalid cron expression. Expected 5 fields: M H DoM Mon DoW",
                        2,
                    );
                }
                if p.prompt.trim().is_empty() {
                    return ValidationResult::err("prompt must not be empty", 3);
                }
                ValidationResult::Ok
            }
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 4),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: CronCreateInput = serde_json::from_value(input)?;
        let id = self
            .store
            .add(input.cron, input.prompt, input.recurring, input.durable);
        let where_str = if input.durable {
            "Persisted to ~/.agix/scenes/<scope>/scheduled_tasks.json"
        } else {
            "Session-only (dies when this session exits)"
        };
        let kind = if input.recurring {
            "recurring job"
        } else {
            "one-shot task"
        };
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(format!(
                "Scheduled {kind} {id}. {where_str}. Auto-expires after 7 days. \
                 Use CronDelete to cancel sooner."
            )),
            is_error: false,
            structured_content: Some(json!({
                "id": id,
                "recurring": input.recurring,
                "durable": input.durable})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::delete::CronDeleteTool;
    use base::tool::{ProgressSender, ToolContext};
    use serde_json::json;

    #[test]
    fn cron_tool_create_and_delete() {
        let store = Arc::new(CronStore::new());
        let create = CronCreateTool::new(store.clone());
        let delete = CronDeleteTool::new(store.clone());

        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(async {
            create
                .call(
                    json!({"cron": "0 9 * * *", "prompt": "test", "recurring": true}),
                    ToolContext::for_test("/tmp".into()),
                    ProgressSender::noop(""),
                )
                .await
                .unwrap()
        });
        assert!(!r.is_error);
        // Extract ID from content
        let id = match &r.content {
            base::tool::ToolResultContent::Text(s) => {
                // Format: "Scheduled recurring job abc12345. ..."
                let token = s.split_whitespace().nth(3).unwrap();
                token.trim_end_matches('.').to_string()
            }
            _ => panic!("expected text"),
        };
        assert_eq!(id.len(), 8, "id '{id}' should be 8 chars");

        // Delete it
        let r2 = rt.block_on(async {
            delete
                .call(
                    json!({"id": id}),
                    ToolContext::for_test("/tmp".into()),
                    ProgressSender::noop(""),
                )
                .await
                .unwrap()
        });
        assert!(!r2.is_error);
        assert!(store.list().is_empty());
    }
}
