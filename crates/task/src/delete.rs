use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, PromptContext, Tool, ToolContext, ToolResult, ValidationResult,
};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamDeleteInput {}

pub struct TeamDeleteTool;

#[async_trait]
impl Tool for TeamDeleteTool {
    fn name(&self) -> &str {
        "TeamDelete"
    }

    fn description(&self) -> &str {
        "Clean up team and task directories when the swarm is complete"
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TeamDeleteInput)).expect("schema")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        "# TeamDelete\n\
         \n\
         Remove team and task directories when the swarm work is complete.\n\
         \n\
         This operation:\n\
         - Removes the team directory (`<project>/.agix/teams/{team-name}/`)\n\
         - Removes the task directory (`~/.agix/tasks/{team-name}/`)\n\
         - Clears team context from the current session\n\
         \n\
         **IMPORTANT**: TeamDelete will fail if the team still has active members. \
         Gracefully terminate teammates first, then call TeamDelete after all \
         teammates have shut down.\n\
         \n\
         Use this when all teammates have finished their work and you want to clean \
         up the team resources. The team name is automatically determined from the \
         current session's team context."
            .into()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    fn is_destructive(&self, _: &Value) -> bool {
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
        _ctx: ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text("Team context cleaned up.".into()),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }
}

// Old bridge impl removed — unified on base::tool::Tool.

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn team_delete_returns_success() {
        let tool = TeamDeleteTool;
        let r = tool
            .call(
                serde_json::json!({}),
                base::tool::ToolContext::for_test("/tmp".into()),
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
    }
}
