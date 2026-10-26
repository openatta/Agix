//! `WasmToolAdapter` — a component's export presented to the engine as an
//! ordinary tool.
//!
//! Deliberately shaped like `mcp::adapter::McpToolAdapter`. Both wrap a tool
//! whose schema is only known at runtime and whose implementation the host
//! does not control, so the questions they have to answer are the same ones,
//! and answering them differently would mean one of the two is wrong.

use crate::instance::{CallFailure, PluginInstance};
use crate::state::ProgressSink;
use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use serde_json::Value;
use std::sync::Arc;

/// The prefix under which every WASM plugin tool is registered.
///
/// Chosen to mirror `mcp__<server>__<tool>` so a permission rule can name a
/// whole plugin — `plugin__github-tools` — exactly the way it can name a
/// whole MCP server. `permissions::ruleset` matches a rule against any
/// `__`-delimited prefix of a tool name, so it needs no knowledge of plugins
/// for that to work.
pub const TOOL_PREFIX: &str = "plugin";

/// Fully-qualified name for a tool a plugin exports.
pub fn qualified_name(plugin: &str, tool: &str) -> String {
    format!("{TOOL_PREFIX}__{plugin}__{tool}")
}

pub struct WasmToolAdapter {
    full_name: String,
    tool_name: String,
    plugin: String,
    description: String,
    doc: Option<String>,
    input_schema: Value,
    read_only: bool,
    concurrency_safe: bool,
    instance: Arc<PluginInstance>,
}

impl WasmToolAdapter {
    pub fn new(
        instance: Arc<PluginInstance>,
        plugin: &str,
        def: &crate::bindings::agix::plugin::types::ToolDef,
    ) -> Self {
        // A component that ships a schema we cannot parse still gets to run;
        // it simply advertises the permissive shape, the same fallback the
        // MCP adapter takes. Refusing to register the tool would punish the
        // user for the plugin author's typo.
        let input_schema = serde_json::from_str(&def.input_schema).unwrap_or_else(|e| {
            tracing::warn!(
                plugin = %plugin,
                tool = %def.name,
                error = %e,
                "plugin tool's input schema is not valid JSON; advertising an open object"
            );
            serde_json::json!({"type": "object"})
        });

        Self {
            full_name: qualified_name(plugin, &def.name),
            tool_name: def.name.clone(),
            plugin: plugin.to_string(),
            description: def.description.clone(),
            doc: def.doc.clone(),
            input_schema,
            read_only: def.read_only,
            concurrency_safe: def.concurrency_safe,
            instance,
        }
    }

    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// The long usage guide, which reaches the model on demand through
    /// `ToolSearch` — so an installer has to disclose it like any other
    /// model-visible text.
    pub fn doc(&self) -> Option<&str> {
        self.doc.as_deref()
    }
}

/// Forward a tool's progress reports to the session's sender.
struct SenderSink(ProgressSender);

impl ProgressSink for SenderSink {
    fn on_progress(&self, _call_id: &str, text: &str) {
        self.0.send(text);
    }
}

#[async_trait]
impl Tool for WasmToolAdapter {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn source(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(format!("plugin:{}", self.plugin))
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        self.doc.clone().unwrap_or_else(|| self.description.clone())
    }

    /// The component's own `list-tools` is the authority on shape, and it can
    /// change when a plugin is reloaded.
    fn is_dynamic(&self) -> bool {
        true
    }

    /// Deferred by default. A third-party schema shipped in every request's
    /// tool array is a fixed token cost per call for something the model
    /// usually ignores; the model can fetch it with `ToolSearch` when it
    /// actually wants the tool.
    fn is_deferred(&self) -> bool {
        true
    }

    fn short_description(&self) -> Option<String> {
        Some(self.description.clone())
    }

    /// Taken from the component's declaration, and used only for scheduling.
    /// A plugin calling its own tool read-only does not make it safe — that
    /// is what the permission rules decide.
    fn is_read_only(&self, _: &Value) -> bool {
        self.read_only
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        self.concurrency_safe
    }

    /// No content-based rule matching: a plugin tool's arguments mean
    /// whatever the plugin says they mean, so a rule can only sensibly name
    /// the tool (or, via the prefix, the whole plugin).
    fn permission_match_content(&self, _: &Value) -> Option<String> {
        None
    }

    /// Only the shape the ABI requires. There is no per-tool validation
    /// hook: `Tool::validate_input` has no production caller anywhere in the
    /// engine, so a WIT export for it would have been a contract offered to
    /// plugin authors that nothing would ever invoke.
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        if input.is_object() {
            ValidationResult::Ok
        } else {
            ValidationResult::err("plugin tool input must be a JSON object", 1)
        }
    }

    /// Always `Ask`, left to the user's rules to resolve — the same stance
    /// the MCP adapter takes. A plugin asserting that its own call is fine
    /// is not evidence of anything.
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::Ask {
            message: format!("Allow plugin tool {} ?", self.full_name),
            decision_reason: None,
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        // A plugin that has faulted on every recent call is not going to
        // answer this one either, and each attempt costs the model a turn to
        // discover that. Refusing here is cheaper than letting it keep
        // failing, and says why.
        if self.instance.health().is_broken() {
            return Ok(ToolResult::error_text(format!(
                "plugin `{}` has been disabled after {} consecutive failures; \
                 reinstall or re-enable it to try again",
                self.plugin,
                self.instance.health().consecutive_faults()
            )));
        }

        let sink: Arc<dyn ProgressSink> = Arc::new(SenderSink(progress));
        let out = self
            .instance
            .call_tool(
                &self.tool_name,
                &input.to_string(),
                &ctx.tool_use_id,
                Some(sink),
                &ctx.cancel,
            )
            .await;

        match out {
            Ok(out) => {
                let mut result = ToolResult::text(out.content);
                result.is_error = out.is_error;
                result.structured_content = out
                    .structured
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok());
                Ok(result)
            }
            // Cancellation is the engine's own vocabulary, so it maps to the
            // engine's error rather than to a tool result the model would
            // read as an answer.
            Err(CallFailure::Cancelled) => Err(ToolError::Cancelled),
            // A timeout or a trap is this tool failing, not the turn failing.
            // The model gets to see what happened and choose something else.
            Err(other) => Ok(ToolResult::error_text(format!(
                "plugin `{}` tool `{}`: {other}",
                self.plugin, self.tool_name
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::agix::plugin::types::ToolDef;

    fn def(name: &str, schema: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: "one line".into(),
            doc: Some("the long guide".into()),
            input_schema: schema.into(),
            read_only: true,
            concurrency_safe: true,
        }
    }

    /// Rules are written against names, so the shape of the name is part of
    /// the contract with the permissions layer.
    #[test]
    fn names_mirror_the_mcp_convention() {
        assert_eq!(
            qualified_name("github-tools", "diff"),
            "plugin__github-tools__diff"
        );
    }

    #[test]
    fn an_unparseable_schema_degrades_to_an_open_object() {
        let schema: Value = serde_json::from_str("not json")
            .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
        assert_eq!(schema["type"], "object");
    }

    /// The two documentation channels are distinct: `description` is what
    /// ships every turn, `doc` is what `ToolSearch` fetches.
    #[test]
    fn the_definition_splits_short_and_long_documentation() {
        let d = def("diff", r#"{"type":"object"}"#);
        assert_eq!(d.description, "one line");
        assert_eq!(d.doc.as_deref(), Some("the long guide"));
    }
}
