//! Tools that historically required Anthropic SaaS / IDE infrastructure but in
//! agix have either (a) a real local implementation or (b) been retired
//! because there is no sensible local equivalent.
//!
//! **D-2 收敛状态：**
//! - `SendMessage` / `RemoteTrigger` / `TeamCreate` / `TeamDelete`: removed.
//!   Bash + slack-cli/curl/mail covers messaging; AgentTool with `remote=true`
//!   covers remote-trigger; multi-agent orchestration now lives in
//!   `agent::TeamCreateTool`.
//! - `LspTool`: removed. The registered instance was always
//!   `LspTool::ephemeral()`, which spawns a language server per request and
//!   drops it; the pooling half (`LspManager`) had no caller outside its own
//!   tests. Code intelligence is better served by the language's own CLI via
//!   `Bash` than by a tool that only ever ran its fallback path.
//! - `McpAuthTool`: real OAuth flow implementation. Starts a browser-based
//!   PKCE flow and returns the authorization URL. Completion exchanges the
//!   code for a token via `mcp::oauth`.
//! - `DispatchMcpTool`: a **real** introspection helper. **D1 **: moved
//!   to `agix_mcp::tools::DispatchMcpTool` so this crate no longer needs
//!   an `agix-mcp` dependency.

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

/// 公共 helper：返回固定的 not-in-scope 错误。
#[allow(dead_code)]
fn not_in_scope(tool: &str, alt: &str) -> ToolResult {
    ToolResult {
        content: base::tool::ToolResultContent::Text(format!(
            "{tool} is not part of agix's local-only scope (Anthropic SaaS / IDE \
             integration required). Alternative for this use case: {alt}"
        )),
        is_error: true,
        structured_content: Some(json!({"tool": tool, "scope": "out_of_scope"})),
        mcp_meta: None,
        new_messages: Some(vec![]),
    }
}

// ============ McpAuth ============

#[derive(Debug, Deserialize, JsonSchema)]
pub struct McpAuthInput {
    /// MCP server name. If omitted or empty, the first unauthenticated server
    /// with OAuth configured is used.
    #[serde(default)]
    pub server: Option<String>,
    /// OAuth flow: 'start' (default) begins the flow and returns a URL;
    ///              'complete' finishes with the authorization code.
    #[serde(default)]
    pub action: Option<String>,
    /// Authorization code (for action='complete' when the user already has one).
    #[serde(default)]
    pub code: Option<String>,
    /// Seconds to wait for the browser callback (default: 120; minimum: 30).
    /// Only used when action='complete' and no `code` is provided.
    #[serde(default = "default_callback_timeout")]
    pub timeout_secs: u64,
}

fn default_callback_timeout() -> u64 {
    120
}

pub struct McpAuthTool;

#[async_trait]
impl Tool for McpAuthTool {
    fn description(&self) -> &str {
        "Authenticate with an MCP server via OAuth"
    }
    fn name(&self) -> &str {
        "McpAuth"
    }

    fn is_deferred(&self) -> bool {
        false
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(McpAuthInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "The `{serverName}` MCP server ({transport}) is installed but requires \
         authentication. Call this tool to start the OAuth flow -- you'll receive \
         an authorization URL to share with the user. Once the user completes \
         authorization in their browser, the server's real tools will become \
         available automatically."
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
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
        input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: McpAuthInput = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("Invalid McpAuth input: {e}")))?;

        // Determine the action
        let action = input.action.as_deref().unwrap_or("start");

        match action {
            "complete" => {
                // Complete a previously started OAuth flow by exchanging the code
                match mcp::oauth::complete_oauth_flow(
                    input.code.as_deref(),
                    input.timeout_secs.max(30),
                )
                .await
                {
                    Ok(msg) => Ok(ToolResult {
                        content: base::tool::ToolResultContent::Text(msg),
                        is_error: false,
                        structured_content: None,
                        mcp_meta: None,
                        new_messages: Some(vec![]),
                    }),
                    Err(e) => Ok(ToolResult {
                        content: base::tool::ToolResultContent::Text(format!(
                            "OAuth completion failed: {e}"
                        )),
                        is_error: true,
                        structured_content: None,
                        mcp_meta: None,
                        new_messages: Some(vec![]),
                    }),
                }
            }
            _ => {
                // Start a new OAuth flow
                let server_name = match input.server {
                    Some(ref name) if !name.is_empty() => name.clone(),
                    _ => {
                        // No server specified — find the first unauthenticated one
                        match mcp::oauth::find_first_unauthenticated_server().await {
                            Some(name) => name,
                            None => {
                                return Ok(ToolResult {
                                    content: base::tool::ToolResultContent::Text(
                                        "All MCP servers are already authenticated.".into(),
                                    ),
                                    is_error: false,
                                    structured_content: None,
                                    mcp_meta: None,
                                    new_messages: Some(vec![]),
                                });
                            }
                        }
                    }
                };

                // Check if the server has OAuth configured
                if mcp::oauth::get_server_oauth_provider(&server_name).is_none() {
                    return Ok(ToolResult {
                        content: base::tool::ToolResultContent::Text(format!(
                            "MCP server '{server_name}' does not support OAuth \
                                 authentication.\n\
                                 Use the Authorization header in settings.json for \
                                 static tokens."
                        )),
                        is_error: false,
                        structured_content: None,
                        mcp_meta: None,
                        new_messages: Some(vec![]),
                    });
                }

                // Start the OAuth flow
                match mcp::oauth::start_oauth_flow(&server_name).await {
                    Ok(url) => Ok(ToolResult {
                        content: base::tool::ToolResultContent::Text(format!(
                            "Open this URL to authenticate '{server_name}':\n\n{url}"
                        )),
                        is_error: false,
                        structured_content: Some(json!({
                            "server": server_name,
                            "authorize_url": url,
                            "action": "complete"
                        })),
                        mcp_meta: None,
                        new_messages: Some(vec![]),
                    }),
                    Err(e) => Ok(ToolResult {
                        content: base::tool::ToolResultContent::Text(format!(
                            "Failed to start OAuth flow: {e}"
                        )),
                        is_error: true,
                        structured_content: None,
                        mcp_meta: None,
                        new_messages: Some(vec![]),
                    }),
                }
            }
        }
    }
}

// **D1 **: DispatchMcpTool was moved to agix-mcp::tools::DispatchMcpTool
// — it required McpClientHandle so it stayed coupled to the mcp crate; making
// the move is what lets agix-tools drop its agix-mcp dependency.

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mcp_auth_fails_gracefully_without_config() {
        let tool = McpAuthTool;
        let r = tool
            .call(
                json!({"server": "x"}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        // Without registered configs, the tool should return an error message
        // explaining that no MCP server configurations are loaded.
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(
                    s.contains("No MCP server configurations")
                        || s.contains("does not support OAuth")
                        || s.contains("Failed to start"),
                    "expected config-related error, got: {s}"
                );
            }
            _ => panic!(),
        }
    }
}
