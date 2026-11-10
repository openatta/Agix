//! `DemoStamp` — `Registrar::tool`. A fingerprint of a piece of text (64-bit
//! FNV-1a, hex), so a model that reports one can only have called the tool.

use std::sync::Arc;

use base::error::ToolError;
use base::interface::tool::{
    PermissionDecision, ProgressSender, Tool, ToolContext, ToolOrigin, ToolResult, ToolSpec,
};
use serde_json::Value;

use crate::activity::Activity;

pub const NAME: &str = "DemoStamp";

pub fn stamp(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("stamp-{hash:016x}")
}

#[derive(Debug)]
pub struct StampTool {
    activity: Arc<Activity>,
}

impl StampTool {
    pub fn new(activity: Arc<Activity>) -> Self {
        Self { activity }
    }
}

#[async_trait::async_trait]
impl Tool for StampTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Return the stamp of `text`: a short fingerprint that identifies exactly that text. \
         Use it when asked to stamp something."
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .origin(ToolOrigin::plugin(crate::NAME))
            .read_only(true)
            .concurrency_safe(true)
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"text": {"type": "string", "description": "The text to stamp, exactly"}},
            "required": ["text"]
        })
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let Some(text) = input.get("text").and_then(Value::as_str) else {
            return Err(ToolError::InvalidInput("`text` must be a string".into()));
        };
        Activity::count(&self.activity.stamps);
        Ok(ToolResult::text(stamp(text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_names_its_text_and_nothing_else() {
        assert_eq!(stamp(""), "stamp-cbf29ce484222325");
        assert_eq!(stamp("a"), "stamp-af63dc4c8601ec8c");
        assert_ne!(stamp("hello"), stamp("hello "));
    }
}
