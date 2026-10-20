//! `McpClient` trait —— 屏蔽 rmcp generic transport 类型，让 adapter 只看
//! list_tools / call_tool 两个能力。
//!
//! 真实现：`RmcpStdioClient`（包装 rmcp::ServiceExt::serve(child_process)）。
//! 测试用：`MockMcpClient`（in-memory 脚本回放）。

use crate::error::McpError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 我们自己用的 MCP 工具元信息（rmcp 类型的子集；保 API 稳定）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema —— 直接对接到 ToolDef::input_schema
    pub input_schema: Value,
    #[serde(default)]
    pub hints: McpToolHints,
}

/// The behavior hints a server attaches to a tool (MCP `ToolAnnotations`).
/// `None` means the server said nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolHints {
    #[serde(default)]
    pub read_only: Option<bool>,
    #[serde(default)]
    pub idempotent: Option<bool>,
    #[serde(default)]
    pub destructive: Option<bool>,
}

/// MCP 资源（server 暴露的非工具型可读对象 —— 如文件、URL、数据库 schema）。
/// 见 https://modelcontextprotocol.io/docs/concepts/resources
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceMeta {
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// MCP prompt 模板（server 暴露的可参数化 prompt）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// 参数定义（可选）
    #[serde(default)]
    pub arguments: Vec<McpPromptArg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptArg {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: Option<bool>,
}

/// MCP `tools/call` 的返回。
#[derive(Debug, Clone, Default)]
pub struct McpCallResult {
    pub content: Vec<McpContent>,
    pub is_error: bool,
    /// 透传 _meta
    pub meta: Option<Value>,
}

/// MCP 协议的 content block。本 crate 只关心 Text / Image；其它（resource_link 等）
/// 暂以 Other 保留 raw json，让上层决定怎么呈现。
#[derive(Debug, Clone)]
pub enum McpContent {
    Text(String),
    Image { data: String, media_type: String },
    Other(Value),
}

#[async_trait]
pub trait McpClient: Send + Sync {
    /// 服务器名（用于命名工具：mcp__<server_name>__<tool_name>）
    fn server_name(&self) -> &str;

    /// transport 标签 —— "stdio" / "streamable_http"。给 /doctor /mcp 显示用。
    /// 默认 "stdio" 兼容旧实现；新实现 override。
    fn transport_kind(&self) -> &'static str {
        "stdio"
    }

    /// Optional human-readable server instructions from MCP initialize.
    fn instructions(&self) -> Option<&str> {
        None
    }

    async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError>;

    async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, Value>,
    ) -> Result<McpCallResult, McpError>;

    /// MCP resources/list —— server 暴露的非工具资源（默认空，子类按需 override）
    async fn list_resources(&self) -> Result<Vec<McpResourceMeta>, McpError> {
        Ok(Vec::new())
    }

    /// resources/read —— 读单个资源；默认报 not implemented
    async fn read_resource(&self, _uri: &str) -> Result<Vec<McpContent>, McpError> {
        Err(McpError::Transport(anyhow::anyhow!(
            "read_resource not implemented for this client"
        )))
    }

    /// MCP prompts/list —— server 暴露的 prompt 模板（默认空）
    async fn list_prompts(&self) -> Result<Vec<McpPromptMeta>, McpError> {
        Ok(Vec::new())
    }

    /// MCP prompts/get —— execute a named prompt and return rendered text.
    async fn get_prompt(
        &self,
        _prompt_name: &str,
        _args: &std::collections::HashMap<String, String>,
    ) -> Result<String, McpError> {
        Err(McpError::Transport(anyhow::anyhow!(
            "get_prompt not implemented for this client"
        )))
    }
}

// -------- mock 实现（in-memory） --------

/// 测试用 mock client。
pub struct MockMcpClient {
    name: String,
    tools: Vec<McpToolMeta>,
    instructions: Option<String>,
    /// 按 (tool_name, args_match) 查找回放
    responses: Mutex<HashMap<String, Vec<McpCallResult>>>,
    /// 每次 call 的请求都记下
    pub calls: Mutex<Vec<(String, serde_json::Map<String, Value>)>>,
    /// `prompts/list` 回放
    prompts: Vec<McpPromptMeta>,
    /// `prompts/get` 回放：prompt name → 渲染文本
    prompt_bodies: HashMap<String, String>,
    /// 每次 `get_prompt` 的 (name, args) 都记下 —— 断言"参数真的传下去了"
    pub prompt_calls: Mutex<Vec<(String, HashMap<String, String>)>>,
    /// `resources/list` 回放
    resources: Vec<McpResourceMeta>,
    /// `resources/read` 回放：uri → content blocks
    resource_bodies: HashMap<String, Vec<McpContent>>,
    /// 模拟"server 掉线"：所有 list/get/read 都返回 `NotConnected`
    down: bool,
}

impl MockMcpClient {
    /// Construct a new instance.
    pub fn new(server_name: impl Into<String>, tools: Vec<McpToolMeta>) -> Self {
        Self {
            name: server_name.into(),
            tools,
            instructions: None,
            responses: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            prompts: Vec::new(),
            prompt_bodies: HashMap::new(),
            prompt_calls: Mutex::new(Vec::new()),
            resources: Vec::new(),
            resource_bodies: HashMap::new(),
            down: false,
        }
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Declare the prompts this server exposes via `prompts/list`.
    pub fn with_prompts(mut self, prompts: Vec<McpPromptMeta>) -> Self {
        self.prompts = prompts;
        self
    }

    /// Canned `prompts/get` rendering for `prompt_name`. A prompt declared via
    /// [`Self::with_prompts`] but without a body here returns an error, which
    /// is how "the prompt itself failed" is simulated.
    pub fn with_prompt_body(mut self, prompt_name: &str, body: impl Into<String>) -> Self {
        self.prompt_bodies.insert(prompt_name.into(), body.into());
        self
    }

    /// Declare the resources this server exposes via `resources/list`.
    pub fn with_resources(mut self, resources: Vec<McpResourceMeta>) -> Self {
        self.resources = resources;
        self
    }

    /// Canned `resources/read` content for `uri`. Any other URI errors, which
    /// is how "bad URI" is simulated.
    pub fn with_resource_text(mut self, uri: &str, text: impl Into<String>) -> Self {
        self.resource_bodies
            .insert(uri.into(), vec![McpContent::Text(text.into())]);
        self
    }

    /// Same as [`Self::with_resource_text`] but with arbitrary content blocks.
    pub fn with_resource_content(mut self, uri: &str, content: Vec<McpContent>) -> Self {
        self.resource_bodies.insert(uri.into(), content);
        self
    }

    /// Simulate an unreachable server: every protocol call fails with
    /// `NotConnected`.
    pub fn down(mut self) -> Self {
        self.down = true;
        self
    }

    fn not_connected(&self) -> McpError {
        McpError::NotConnected {
            name: self.name.clone(),
        }
    }

    /// 给某个 tool name 队列追加一个回放结果（FIFO）
    pub fn push_response(&self, tool_name: &str, result: McpCallResult) {
        let mut r = self.responses.lock().unwrap();
        r.entry(tool_name.to_string()).or_default().push(result);
    }
}

#[async_trait]
impl McpClient for MockMcpClient {
    fn server_name(&self) -> &str {
        &self.name
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError> {
        Ok(self.tools.clone())
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, Value>,
    ) -> Result<McpCallResult, McpError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool_name.to_string(), args));
        let mut r = self.responses.lock().unwrap();
        let queue = r.get_mut(tool_name).ok_or_else(|| McpError::UnknownTool {
            name: self.name.clone(),
            tool: tool_name.to_string(),
        })?;
        if queue.is_empty() {
            return Err(McpError::UnknownTool {
                name: self.name.clone(),
                tool: tool_name.to_string(),
            });
        }
        Ok(queue.remove(0))
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptMeta>, McpError> {
        if self.down {
            return Err(self.not_connected());
        }
        Ok(self.prompts.clone())
    }

    async fn get_prompt(
        &self,
        prompt_name: &str,
        args: &HashMap<String, String>,
    ) -> Result<String, McpError> {
        self.prompt_calls
            .lock()
            .unwrap()
            .push((prompt_name.to_string(), args.clone()));
        if self.down {
            return Err(self.not_connected());
        }
        match self.prompt_bodies.get(prompt_name) {
            Some(body) => {
                // Substitute `{name}` placeholders so tests can assert that
                // the mapped arguments actually reached the server.
                let mut out = body.clone();
                for (k, v) in args {
                    out = out.replace(&format!("{{{k}}}"), v);
                }
                Ok(out)
            }
            None => Err(McpError::RmcpService(format!(
                "get_prompt: unknown prompt '{prompt_name}'"
            ))),
        }
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceMeta>, McpError> {
        if self.down {
            return Err(self.not_connected());
        }
        Ok(self.resources.clone())
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
        if self.down {
            return Err(self.not_connected());
        }
        match self.resource_bodies.get(uri) {
            Some(content) => Ok(content.clone()),
            None => Err(McpError::RmcpService(format!(
                "read_resource: no such resource '{uri}'"
            ))),
        }
    }
}

/// 把同一份 client 多 Arc 共享 —— adapter 们都拿同一个底层 client。
pub type McpClientHandle = Arc<dyn McpClient>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(name: &str) -> McpToolMeta {
        McpToolMeta {
            name: name.into(),
            description: Some(format!("desc for {name}")),
            input_schema: json!({"type": "object"}),
            hints: Default::default(),
        }
    }

    #[tokio::test]
    async fn mock_list_tools() {
        let c = MockMcpClient::new("test", vec![meta("a"), meta("b")]);
        let tools = c.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[tokio::test]
    async fn mock_call_tool_returns_pushed_response() {
        let c = MockMcpClient::new("test", vec![meta("a")]);
        c.push_response(
            "a",
            McpCallResult {
                content: vec![McpContent::Text("hello".into())],
                is_error: false,
                meta: None,
            },
        );
        let result = c.call_tool("a", Default::default()).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        assert!(matches!(&result.content[0], McpContent::Text(s) if s == "hello"));
    }

    #[tokio::test]
    async fn mock_unknown_tool_errors() {
        let c = MockMcpClient::new("test", vec![]);
        let err = c.call_tool("ghost", Default::default()).await.unwrap_err();
        assert!(matches!(err, McpError::UnknownTool { .. }));
    }

    #[tokio::test]
    async fn mock_records_calls() {
        let c = MockMcpClient::new("test", vec![meta("a")]);
        c.push_response("a", McpCallResult::default());
        c.push_response("a", McpCallResult::default());
        let mut args1 = serde_json::Map::new();
        args1.insert("x".into(), json!(1));
        let _ = c.call_tool("a", args1.clone()).await.unwrap();
        let _ = c.call_tool("a", Default::default()).await.unwrap();
        let calls = c.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "a");
        assert_eq!(calls[0].1.get("x"), Some(&json!(1)));
    }
}
