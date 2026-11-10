//! What `StructuredOutput` accepts under a host's schema: the fields
//! themselves, not a call that misses the schema and not the whole result
//! serialized into one string field.

use base::interface::tool::{Tool, ValidationResult};
use serde_json::json;
use tools::structured_output::StructuredOutputTool;

fn tool() -> StructuredOutputTool {
    StructuredOutputTool::with_schema(json!({
        "type": "object",
        "required": ["code", "priority"],
        "properties": {
            "code": { "type": "string" },
            "priority": { "type": "integer" }
        }
    }))
}

async fn check(input: serde_json::Value) -> Result<(), String> {
    let ctx = platform::tool_context_for_test(std::env::temp_dir());
    match tool().validate_input(&input, &ctx).await {
        ValidationResult::Ok => Ok(()),
        ValidationResult::Err(msg, _) => Err(msg),
    }
}

#[tokio::test]
async fn the_fields_themselves_are_accepted() {
    check(json!({ "code": "KESTREL-1", "priority": 4 }))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_call_that_misses_the_schema_is_refused() {
    let err = check(json!({ "code": "KESTREL-1" })).await.unwrap_err();
    assert!(err.contains("does not match the required schema"), "{err}");
}

#[tokio::test]
async fn the_whole_result_packed_into_one_field_is_refused() {
    let packed = json!({ "code": "KESTREL-1", "priority": 4 }).to_string();
    let err = check(json!({ "code": packed, "priority": 4 }))
        .await
        .unwrap_err();
    assert!(err.contains("`code` holds the whole result"), "{err}");
}
