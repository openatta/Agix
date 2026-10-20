use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StructuredOutputInput {
    /// Any data to return as structured output. All properties are echoed back.
    #[serde(flatten)]
    pub payload: Value,
}

/// With a host schema, the model is shown that schema and a call that does
/// not match it is refused, so the model can correct it and try again. A
/// call that is accepted is handed to the session as the turn's result.
#[derive(Default)]
pub struct StructuredOutputTool {
    schema: Option<Value>,
}

impl StructuredOutputTool {
    pub fn with_schema(schema: Value) -> Self {
        Self {
            schema: Some(schema),
        }
    }
}

#[async_trait]
impl Tool for StructuredOutputTool {
    fn name(&self) -> &str {
        "StructuredOutput"
    }

    fn description(&self) -> &str {
        "Return structured output in the requested format"
    }

    fn input_schema(&self) -> Value {
        match &self.schema {
            Some(schema) => schema.clone(),
            None => {
                serde_json::to_value(schemars::schema_for!(StructuredOutputInput)).expect("schema")
            }
        }
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/structured_output.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let Some(schema) = &self.schema else {
            return ValidationResult::Ok;
        };
        let validator = match jsonschema::validator_for(schema) {
            Ok(v) => v,
            Err(e) => {
                return ValidationResult::err(
                    format!("the host's output schema is not a valid JSON Schema: {e}"),
                    1,
                )
            }
        };
        let problems: Vec<String> = validator
            .iter_errors(input)
            .map(|e| {
                let at = e.instance_path().to_string();
                if at.is_empty() {
                    e.to_string()
                } else {
                    format!("{at}: {e}")
                }
            })
            .collect();
        if problems.is_empty() {
            ValidationResult::Ok
        } else {
            ValidationResult::err(
                format!(
                    "Output does not match the required schema:\n- {}",
                    problems.join("\n- ")
                ),
                1,
            )
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
        ctx.session.set_structured_output(input.clone());
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(
                "Structured output provided successfully".into(),
            ),
            is_error: false,
            structured_content: Some(json!({"structured_output": input})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}
