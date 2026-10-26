//! MCP 协议特性的工具包装：`ListMcpResources` / `ReadMcpResource`。
//!
//! **D1 **：原 `agix-tools/src/mcp_tools.rs`，搬到这里以解除 tools →
//! mcp 的反向依赖。CLI 直接从 `agix_mcp::tools::{...}` 装配。
//!
//! 设计取舍：
//! - **不绑定具体 server name**：列资源时枚举所有 connected server 并合并
//! - **uri 是全局 key**：read 时直接按 uri 匹配；多 server 同名 uri → 第一个赢
//! - **不缓存**：每次 list 实时调 server（list_resources 通常很快）
//!
//! 不实装：McpAuthTool（需要 OAuth client 基础设施，不在 范围）

use crate::client::{McpClientHandle, McpContent};
use async_trait::async_trait;
use base::{
    error::ToolError, tool::PermissionDecision, tool::ProgressSender, tool::PromptContext,
    tool::Tool, tool::ToolContext, tool::ToolResult, tool::ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct ListMcpResourcesInput {
    /// Optional server name to filter by; None = all connected servers
    #[serde(default)]
    pub server: Option<String>,
}

pub struct ListMcpResourcesTool {
    clients: Vec<McpClientHandle>,
}

impl ListMcpResourcesTool {
    /// Construct a new instance.
    pub fn new(clients: Vec<McpClientHandle>) -> Self {
        Self { clients }
    }
}

#[async_trait]
impl Tool for ListMcpResourcesTool {
    fn name(&self) -> &str {
        "ListMcpResources"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn is_deferred(&self) -> bool {
        false
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ListMcpResourcesInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "List resources exposed by connected MCP servers (files, URIs, db schemas, \
         etc — non-tool readable objects). Pass `server` to filter to one server. \
         Returns array of {server, uri, name, description, mime_type}."
            .into()
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
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ListMcpResourcesInput = serde_json::from_value(input).unwrap_or_default();
        let mut all = Vec::new();
        for client in &self.clients {
            if let Some(filter) = &input.server {
                if client.server_name() != filter {
                    continue;
                }
            }
            match client.list_resources().await {
                Ok(resources) => {
                    for r in resources {
                        all.push(json!({
                            "server": client.server_name(),
                            "uri": r.uri,
                            "name": r.name,
                            "description": r.description,
                            "mime_type": r.mime_type,
                        }));
                    }
                }
                Err(e) => {
                    // 某个 server 失败不阻塞整体；继续
                    tracing::warn!(server = %client.server_name(), error = %e, "list_resources failed");
                }
            }
        }
        let body = if all.is_empty() {
            "(no MCP resources available — either no servers connected or none expose resources)"
                .to_string()
        } else {
            format!(
                "{} resource(s):\n{}",
                all.len(),
                all.iter()
                    .filter_map(|r| {
                        let server = r.get("server")?.as_str()?;
                        let uri = r.get("uri")?.as_str()?;
                        let name = r.get("name")?.as_str()?;
                        Some(format!("  · [{server}] {uri} — {name}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: false,
            structured_content: Some(json!({"resources": all})),
            mcp_meta: None,
            new_messages: None,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMcpResourceInput {
    /// Resource URI to read (from ListMcpResources output)
    pub uri: String,
    /// Optional server name (defaults to first matching server)
    #[serde(default)]
    pub server: Option<String>,
}

pub struct ReadMcpResourceTool {
    clients: Vec<McpClientHandle>,
}

impl ReadMcpResourceTool {
    /// Construct a new instance.
    pub fn new(clients: Vec<McpClientHandle>) -> Self {
        Self { clients }
    }
}

#[async_trait]
impl Tool for ReadMcpResourceTool {
    fn name(&self) -> &str {
        "ReadMcpResource"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn is_deferred(&self) -> bool {
        false
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ReadMcpResourceInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "Read a single MCP resource by URI. Returns text content (or base64 blob for \
         binary). Use ListMcpResources first to find available URIs."
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<ReadMcpResourceInput>(input.clone()) {
            Ok(p) if p.uri.trim().is_empty() => ValidationResult::err("uri must not be empty", 1),
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        // MCP resource 是 server 暴露的；可能很大但本身是只读
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ReadMcpResourceInput = serde_json::from_value(input)?;
        // 找 client：指定 server 优先；否则第一个能 read 出来的
        let candidates: Vec<&McpClientHandle> = self
            .clients
            .iter()
            .filter(|c| {
                input
                    .server
                    .as_ref()
                    .map(|s| c.server_name() == s)
                    .unwrap_or(true)
            })
            .collect();

        if candidates.is_empty() {
            return Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "No matching MCP server (filter: {:?})",
                    input.server
                )),
                is_error: true,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            });
        }

        let mut last_err = None;
        for client in candidates {
            match client.read_resource(&input.uri).await {
                Ok(contents) => {
                    let text = contents
                        .iter()
                        .filter_map(|c| match c {
                            McpContent::Text(t) => Some(t.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let body = if text.is_empty() {
                        format!("[binary or empty resource: {} blocks]", contents.len())
                    } else {
                        text
                    };
                    return Ok(ToolResult {
                        content: base::tool::ToolResultContent::Text(body),
                        is_error: false,
                        structured_content: Some(
                            json!({"server": client.server_name(), "uri": input.uri}),
                        ),
                        mcp_meta: None,
                        new_messages: None,
                    });
                }
                Err(e) => last_err = Some(format!("{}", e)),
            }
        }
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(format!(
                "could not read resource '{}': {}",
                input.uri,
                last_err.unwrap_or_else(|| "no client succeeded".into())
            )),
            is_error: true,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }
}

// 辅助：让 ListMcpResourcesTool::new 在没 client 时也能实例化（便于测试）
impl Default for ListMcpResourcesTool {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Default for ReadMcpResourceTool {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

// 让 mcp clients 字段可以被 cli 看到（虽然是给构造用的，所以这里不暴露 getter）
// 但为了让 ListMcpResources / ReadMcpResource 都共享同一份 clients，
// CLI 用 `Arc<Vec<McpClientHandle>>`（避免重复 .clone() 列表）
/// Build the MCP tool wrappers for the engine's tool registry.
pub fn make_mcp_tools(
    clients: Vec<McpClientHandle>,
) -> (ListMcpResourcesTool, ReadMcpResourceTool) {
    (
        ListMcpResourcesTool::new(clients.clone()),
        ReadMcpResourceTool::new(clients),
    )
}

// ============ DispatchMcpTool ============
//
// **D1 **: moved from `agix-tools/src/saas_stubs.rs`.
//
// Introspection tool: lists which MCP servers have registered which tools.
// Because Agix flat-mounts each server's tools under `mcp__<server>__<tool>`
// names, this tool is used for discovery rather than dispatching calls.

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct DispatchMcpInput {
    /// Optional server name to filter
    #[serde(default)]
    pub server: Option<String>,
}

pub struct DispatchMcpTool {
    clients: Vec<McpClientHandle>,
}

impl DispatchMcpTool {
    /// Construct a new instance.
    pub fn new(clients: Vec<McpClientHandle>) -> Self {
        Self { clients }
    }
}

#[async_trait]
impl Tool for DispatchMcpTool {
    fn name(&self) -> &str {
        "MCP"
    }

    fn is_deferred(&self) -> bool {
        false
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(DispatchMcpInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "Introspect MCP server tools. Agix flat-mounts each MCP server's tools \
         under `mcp__<server>__<tool>` names — call them directly, no dispatcher \
         needed. This tool just lists what's available so you know the names."
            .into()
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
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: DispatchMcpInput = serde_json::from_value(input).unwrap_or_default();
        let mut all = Vec::new();
        for client in &self.clients {
            if let Some(filter) = &input.server {
                if client.server_name() != filter {
                    continue;
                }
            }
            match client.list_tools().await {
                Ok(tools) => {
                    for t in tools {
                        all.push(json!({
                            "server": client.server_name(),
                            "tool_name": format!("mcp__{}__{}", client.server_name(), t.name),
                            "raw_name": t.name,
                            "description": t.description,
                        }));
                    }
                }
                Err(e) => {
                    tracing::warn!(server = %client.server_name(), error = %e, "list_tools failed");
                }
            }
        }
        let body = if all.is_empty() {
            "(no MCP tools available)".to_string()
        } else {
            format!(
                "{} MCP tool(s):\n{}",
                all.len(),
                all.iter()
                    .filter_map(|t| {
                        let name = t.get("tool_name")?.as_str()?;
                        let desc = t.get("description")?.as_str().unwrap_or("");
                        Some(format!("  · {name} — {desc}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: false,
            structured_content: Some(json!({"tools": all})),
            mcp_meta: None,
            new_messages: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{McpCallResult, McpClient, McpContent, McpResourceMeta, McpToolMeta};
    use crate::error::McpError;
    use std::sync::Arc;

    /// 测试用 mock client
    struct MockClient {
        name: String,
        resources: Vec<McpResourceMeta>,
        read_responses: std::sync::Mutex<std::collections::HashMap<String, Vec<McpContent>>>,
    }

    #[async_trait]
    impl McpClient for MockClient {
        fn server_name(&self) -> &str {
            &self.name
        }
        async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError> {
            Ok(Vec::new())
        }
        async fn call_tool(
            &self,
            _: &str,
            _: serde_json::Map<String, Value>,
        ) -> Result<McpCallResult, McpError> {
            Ok(McpCallResult::default())
        }
        async fn list_resources(&self) -> Result<Vec<McpResourceMeta>, McpError> {
            Ok(self.resources.clone())
        }
        async fn read_resource(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
            self.read_responses
                .lock()
                .unwrap()
                .get(uri)
                .cloned()
                .ok_or_else(|| McpError::Transport(anyhow::anyhow!("no such uri: {uri}")))
        }
    }

    fn mk(name: &str, resources: Vec<(&str, &str, &str)>) -> Arc<dyn McpClient> {
        let resources = resources
            .into_iter()
            .map(|(uri, name, desc)| McpResourceMeta {
                uri: uri.into(),
                name: name.into(),
                description: Some(desc.into()),
                mime_type: None,
            })
            .collect();
        Arc::new(MockClient {
            name: name.into(),
            resources,
            read_responses: std::sync::Mutex::new(std::collections::HashMap::new()),
        }) as Arc<dyn McpClient>
    }

    #[tokio::test]
    async fn list_aggregates_across_servers() {
        let c1 = mk("fs", vec![("file:///a", "a.txt", "desc a")]);
        let c2 = mk(
            "github",
            vec![("github://issues/1", "issue 1", "first issue")],
        );
        let tool = ListMcpResourcesTool::new(vec![c1, c2]);
        let r = tool
            .call(
                json!({}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("2 resource"));
                assert!(s.contains("[fs]"));
                assert!(s.contains("[github]"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn list_filters_by_server() {
        let c1 = mk("fs", vec![("file:///a", "a", "x")]);
        let c2 = mk("github", vec![("github://1", "1", "y")]);
        let tool = ListMcpResourcesTool::new(vec![c1, c2]);
        let r = tool
            .call(
                json!({"server": "fs"}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("[fs]"));
                assert!(!s.contains("[github]"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn list_with_no_servers_returns_friendly_message() {
        let tool = ListMcpResourcesTool::new(vec![]);
        let r = tool
            .call(
                json!({}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("no MCP resources"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn read_unknown_uri_returns_error() {
        let c1 = mk("fs", vec![]);
        let tool = ReadMcpResourceTool::new(vec![c1]);
        let r = tool
            .call(
                json!({"uri": "file:///nowhere"}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn read_validates_empty_uri() {
        let tool = ReadMcpResourceTool::new(vec![]);
        let r = tool
            .validate_input(&json!({"uri": "  "}), &ToolContext::for_test("/tmp".into()))
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    /// **D1 **: DispatchMcpTool migrated here from `agix-tools/src/saas_stubs.rs`;
    /// keep the no-servers smoke test alongside the implementation.
    #[tokio::test]
    async fn mcp_dispatch_lists_zero_when_no_servers() {
        let tool = DispatchMcpTool::new(vec![]);
        let r = tool
            .call(
                json!({}),
                ToolContext::for_test("/tmp".into()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("(no MCP tools available)"));
            }
            _ => panic!(),
        }
    }
}
