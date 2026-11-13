use async_trait::async_trait;
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::{
    PathStyle, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ToolSpec, ValidationResult,
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

/// A string field whose text is itself a result the schema accepts: the model
/// serialized its answer into one argument, which the schema alone lets through
/// whenever that field is typed as a string.
fn packed_field(input: &Value, validator: &jsonschema::Validator) -> Option<String> {
    input.as_object()?.iter().find_map(|(name, value)| {
        let inner: Value = serde_json::from_str(value.as_str()?).ok()?;
        (inner.is_object() && validator.is_valid(&inner)).then(|| name.clone())
    })
}

#[async_trait]
impl Tool for StructuredOutputTool {
    fn name(&self) -> &str {
        "StructuredOutput"
    }

    fn description(&self) -> &str {
        "Hand in the result in the requested format. The arguments are the result itself: put \
         each field's actual value in it (the text, the number), not code that would compute it \
         and not the whole result as one string."
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        match &self.schema {
            Some(schema) => schema.clone(),
            None => crate::input_schema::<StructuredOutputInput>(),
        }
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("prompts/coding/structured_output.prompt.md").to_string())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
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
            match packed_field(input, &validator) {
                Some(field) => ValidationResult::err(
                    format!(
                        "`{field}` holds the whole result as a string. Pass each field as its \
                         own argument with its own value instead."
                    ),
                    1,
                ),
                None => ValidationResult::Ok,
            }
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

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        ctx.turn.set_structured_output(input.clone());
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
