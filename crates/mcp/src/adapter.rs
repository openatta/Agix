//! `McpToolAdapter` —— 把一条 MCP server 提供的工具包装成 agix `Tool`。
//!
//! 工具名按 `mcp__<server>__<tool>` 命名，与权限规则匹配语义一致
//! （见 agix-permissions::ruleset）。

use crate::client::{McpCallResult, McpClientHandle, McpContent, McpToolHints, McpToolMeta};
use crate::error::McpError;
use crate::manager::ElicitationCallback;
use crate::output_cache::McpOutputCache;
use async_trait::async_trait;
use base::{
    error::ToolError,
    tool::PermissionDecision,
    tool::ProgressSender,
    tool::Tool,
    tool::ToolResult,
    tool::ToolResultBlock,
    tool::ToolResultContent,
    tool::ValidationResult,
    tool::{PromptContext, ToolContext},
};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Per-request cap on a server-supplied description. Some servers put pages of
/// documentation there; the full text stays reachable through `prompt()`, which
/// `ToolSearch` serves as the tool's guide whenever the two differ.
const MAX_ADVERTISED_DESCRIPTION_CHARS: usize = 2048;

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

pub struct McpToolAdapter {
    full_name: String,
    tool_name: String,
    server_name: String,
    full_description: String,
    description: String,
    input_schema: Value,
    hints: McpToolHints,
    client: McpClientHandle,
    /// Shared output cache across all MCP adapters.
    cache: Arc<Mutex<McpOutputCache>>,
    /// Callback for MCP elicitation URLs (mcp:// or elicitation:// protocols).
    ///
    /// This is the *same* `Arc<Mutex<..>>` cell the owning `McpManager` holds
    /// (see `with_elicitation_callback`), not a private snapshot. `McpManager`
    /// rebuilds its adapter list on every `refresh_tools()` call, which
    /// commonly runs before an embedder has had a chance to call
    /// `McpManager::set_elicitation_callback` (the daemon connects MCP
    /// servers and refreshes tools before handing the manager to
    /// `runtime::agent::Builder`, which is where the callback gets wired to
    /// the session's real `HookRunner`). A plain `Option<ElicitationCallback>`
    /// captured at construction time would freeze at `None` forever in that
    /// ordering — sharing the cell means a callback set *after* adapters
    /// already exist is still visible to them on their next call.
    elicitation_callback: Arc<Mutex<Option<ElicitationCallback>>>,
}

impl McpToolAdapter {
    /// Construct a new instance.
    pub fn new(client: McpClientHandle, meta: McpToolMeta) -> Self {
        Self::with_cache(client, meta, Arc::new(Mutex::new(McpOutputCache::new())))
    }

    /// Construct with a shared output cache (all adapters from the same manager
    /// should share the same cache instance).
    pub fn with_cache(
        client: McpClientHandle,
        meta: McpToolMeta,
        cache: Arc<Mutex<McpOutputCache>>,
    ) -> Self {
        let server_name = client.server_name().to_string();
        let tool_name = meta.name.clone();
        let full_name = format!("mcp__{server_name}__{tool_name}");
        let full_description = meta
            .description
            .unwrap_or_else(|| format!("MCP tool: {tool_name}"));
        let description = truncate_chars(&full_description, MAX_ADVERTISED_DESCRIPTION_CHARS);
        Self {
            full_name,
            tool_name,
            server_name,
            full_description,
            description,
            input_schema: meta.input_schema,
            hints: meta.hints,
            client,
            cache,
            elicitation_callback: Arc::new(Mutex::new(None)),
        }
    }

    /// Server-qualified tool name, e.g. `{server}__{tool}`.
    pub fn full_name(&self) -> &str {
        &self.full_name
    }
    /// Server name this tool belongs to.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
    /// Tool name (without server prefix).
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// A repeated identical call may be answered from the cache only when the
    /// server declares the tool both read-only and idempotent.
    fn is_cacheable(&self) -> bool {
        self.hints.read_only == Some(true) && self.hints.idempotent == Some(true)
    }

    /// Alias this adapter's elicitation cell to the owning `McpManager`'s
    /// shared cell — see the field doc comment for why sharing (not copying
    /// a snapshot) matters. When the adapter detects an elicitation URL
    /// (`mcp://` or `elicitation://`) in a tool result, it looks up whatever
    /// callback the cell holds *at that moment*. Returns Self for
    /// builder-style chaining.
    pub fn with_elicitation_callback(
        mut self,
        cell: Arc<Mutex<Option<ElicitationCallback>>>,
    ) -> Self {
        self.elicitation_callback = cell;
        self
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn source(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(format!("mcp:{}", self.server_name))
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        self.full_description.clone()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    /// Taken from the server's `readOnlyHint`; a server that says nothing is
    /// assumed to write.
    fn is_read_only(&self, _: &Value) -> bool {
        self.hints.read_only == Some(true)
    }

    fn is_destructive(&self, _: &Value) -> bool {
        self.hints.destructive == Some(true)
    }

    fn permission_match_content(&self, _: &Value) -> Option<String> {
        None // MCP 工具的"内容"不可解释；只按 tool_name 匹配规则
    }

    /// MCP tools are dynamic — their schemas and prompts come from live server
    /// connections and may change between turns (e.g., after reconnection).
    fn is_dynamic(&self) -> bool {
        true
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        // 接受任意 object 输入 —— 实际 schema 校验由 server 端做
        if input.is_object() {
            ValidationResult::Ok
        } else {
            ValidationResult::err("MCP tool input must be a JSON object", 1)
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        // 默认 Ask —— 上层 PermissionGate 用规则（如 `mcp__github` allow）放行
        PermissionDecision::Ask {
            message: format!("Allow MCP tool {} ?", self.full_name),
            decision_reason: None,
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let args = input.as_object().cloned().unwrap_or_default();
        let cacheable = self.is_cacheable();

        if cacheable {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.get(&self.server_name, &self.tool_name, &args) {
                return Ok(into_tool_result(cached));
            }
        }

        let result = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = self.client.call_tool(&self.tool_name, args.clone()) => r,
        };

        let result = result.map_err(|e: McpError| ToolError::Execution(anyhow::Error::new(e)))?;

        if cacheable {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.put(&self.server_name, &self.tool_name, &args, result.clone());
        }

        // P2-4: Check for elicitation URLs in the result. If found, fire
        // the elicitation callback to notify the hook system. Locked and
        // cloned rather than held across the `if` so a callback registered
        // after this adapter was built (see the field doc comment) is
        // picked up, and so the lock isn't held while `cb` runs.
        let cb = self
            .elicitation_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(cb) = cb {
            if let Some(url) = find_elicitation_url(&result) {
                cb(self.server_name.clone(), url);
            }
        }

        Ok(into_tool_result(result))
    }
}

/// 把 MCP `CallToolResult` 转成我们的 `ToolResult`。
///
/// 单个 **文本** 内容块 → `Text`；其它一律 → `Blocks` 逐块保留。
/// `is_error` 一并传播。
///
/// A lone image block used to collapse to the literal string `"[image]"` here,
/// which threw the pixels away before anything downstream could see them — a
/// single-image result is the *common* shape for screenshot/diagram MCP servers,
/// so that fast path silently defeated image support for exactly the servers
/// that need it. Only a lone text block still takes the `Text` shortcut; any
/// result containing an image goes through `Blocks` so the runtime can lift it
/// into a real `ModelContentBlock::Image`.
fn into_tool_result(result: McpCallResult) -> ToolResult {
    let is_error = result.is_error;
    let has_image = result
        .content
        .iter()
        .any(|b| matches!(b, McpContent::Image { .. }));
    if !has_image && result.content.len() <= 1 {
        let text = result
            .content
            .iter()
            .map(|b| match b {
                McpContent::Text(t) => t.clone(),
                McpContent::Image { .. } => "[image]".to_string(),
                McpContent::Other(v) => format!("[mcp content: {v}]"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.is_empty() {
            "(empty response)".to_string()
        } else {
            text
        };
        let mut r = ToolResult::text(text);
        r.is_error = is_error;
        return r;
    }
    let blocks = into_tool_result_blocks(result.content);
    ToolResult {
        content: ToolResultContent::Blocks(blocks),
        is_error,
        structured_content: None,
        mcp_meta: None,
        new_messages: None,
    }
}

/// Scan an MCP tool result for an elicitation URL (`mcp://` or `elicitation://`
/// protocol). Returns the first such URL found, or None if none exists.
///
/// Elicitation URLs indicate that the MCP server requires user interaction
/// (e.g. authorization consent, form fill, confirmation dialog) — the content
/// of the URL encodes the elicitation request.
fn find_elicitation_url(result: &McpCallResult) -> Option<String> {
    for block in &result.content {
        if let McpContent::Text(text) = block {
            for word in text.split_whitespace() {
                let clean = word.trim_matches(|c: char| c.is_ascii_punctuation() && c != ':');
                if clean.starts_with("mcp://") || clean.starts_with("elicitation://") {
                    return Some(clean.to_string());
                }
            }
        }
    }
    None
}

fn into_tool_result_blocks(blocks: Vec<McpContent>) -> Vec<ToolResultBlock> {
    blocks
        .into_iter()
        .map(|b| match b {
            McpContent::Text(t) => ToolResultBlock::text(t),
            McpContent::Image { data, media_type } => ToolResultBlock::image(media_type, data),
            McpContent::Other(v) => ToolResultBlock::text(format!("[mcp content: {v}]")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MockMcpClient;
    use base::tool::ToolResultContent;
    use serde_json::json;
    use std::sync::Arc;

    fn meta(name: &str) -> McpToolMeta {
        McpToolMeta {
            name: name.into(),
            description: Some(format!("desc for {name}")),
            input_schema: json!({"type": "object"}),
            hints: Default::default(),
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::for_test(std::path::PathBuf::from("/tmp"))
    }

    #[tokio::test]
    async fn name_is_namespaced_with_server() {
        let client = Arc::new(MockMcpClient::new("github", vec![]));
        let adapter = McpToolAdapter::new(client, meta("create_issue"));
        assert_eq!(adapter.name(), "mcp__github__create_issue");
        assert_eq!(adapter.server_name(), "github");
        assert_eq!(adapter.tool_name(), "create_issue");
    }

    /// The `mcp__` prefix already implies the answer, but a naming convention
    /// is not a declaration — it holds only until someone registers a builtin
    /// whose name starts that way. The adapter says which server it fronts.
    #[tokio::test]
    async fn the_server_description_is_what_the_model_is_shown() {
        let client = Arc::new(MockMcpClient::new("github", vec![]));
        let adapter = McpToolAdapter::new(client, meta("create_issue"));
        assert_eq!(adapter.description(), "desc for create_issue");
        assert!(adapter
            .detailed_prompt(&PromptContext::default())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn an_oversized_description_is_cut_and_the_whole_text_becomes_the_guide() {
        let client = Arc::new(MockMcpClient::new("docs", vec![]));
        let mut m = meta("search");
        let long = "x".repeat(MAX_ADVERTISED_DESCRIPTION_CHARS + 10);
        m.description = Some(long.clone());
        let adapter = McpToolAdapter::new(client, m);
        assert_eq!(
            adapter.description().chars().count(),
            MAX_ADVERTISED_DESCRIPTION_CHARS + 1
        );
        assert_eq!(
            adapter.detailed_prompt(&PromptContext::default()).await,
            Some(long)
        );
    }

    #[tokio::test]
    async fn source_names_the_server_it_fronts() {
        let client = Arc::new(MockMcpClient::new("github", vec![]));
        let adapter = McpToolAdapter::new(client, meta("create_issue"));
        assert_eq!(adapter.source(), "mcp:github");
    }

    #[tokio::test]
    async fn input_schema_passes_through() {
        let client = Arc::new(MockMcpClient::new("github", vec![]));
        let mut m = meta("x");
        m.input_schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        let adapter = McpToolAdapter::new(client, m);
        let s = adapter.input_schema();
        assert_eq!(s["properties"]["q"]["type"], "string");
    }

    #[tokio::test]
    async fn calls_underlying_client_and_converts_text_result() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("echo")]));
        client.push_response(
            "echo",
            McpCallResult {
                content: vec![McpContent::Text("hi".into())],
                is_error: false,
                meta: None,
            },
        );
        let adapter = McpToolAdapter::new(client.clone(), meta("echo"));
        let r = adapter
            .call(json!({"text": "hi"}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            ToolResultContent::Text(s) => assert_eq!(s, "hi"),
            _ => panic!("expected Text"),
        }
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "echo");
        assert_eq!(calls[0].1.get("text"), Some(&json!("hi")));
    }

    #[tokio::test]
    async fn is_error_propagates() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("fail")]));
        client.push_response(
            "fail",
            McpCallResult {
                content: vec![McpContent::Text("oops".into())],
                is_error: true,
                meta: None,
            },
        );
        let adapter = McpToolAdapter::new(client, meta("fail"));
        let r = adapter
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(r.is_error);
    }

    fn text(t: &str, is_error: bool) -> McpCallResult {
        McpCallResult {
            content: vec![McpContent::Text(t.into())],
            is_error,
            meta: None,
        }
    }

    async fn call_twice(hints: McpToolHints, first: McpCallResult) -> (Vec<ToolResult>, usize) {
        let mut m = meta("state");
        m.hints = hints;
        let client = Arc::new(MockMcpClient::new("s", vec![m.clone()]));
        client.push_response("state", first);
        client.push_response("state", text("second", false));
        let adapter = McpToolAdapter::new(client.clone(), m);
        let mut results = Vec::new();
        for _ in 0..2 {
            results.push(
                adapter
                    .call(json!({"id": 1}), ctx(), ProgressSender::noop("t"))
                    .await
                    .unwrap(),
            );
        }
        let calls = client.calls.lock().unwrap().len();
        (results, calls)
    }

    #[tokio::test]
    async fn a_tool_without_hints_is_called_every_time() {
        let (results, calls) = call_twice(McpToolHints::default(), text("first", false)).await;
        assert_eq!(calls, 2, "a state-reading tool must not get a stale answer");
        assert!(matches!(&results[1].content, ToolResultContent::Text(s) if s == "second"));
    }

    #[tokio::test]
    async fn a_read_only_idempotent_tool_is_served_from_the_cache() {
        let hints = McpToolHints {
            read_only: Some(true),
            idempotent: Some(true),
            destructive: None,
        };
        let (results, calls) = call_twice(hints, text("first", false)).await;
        assert_eq!(calls, 1);
        assert!(matches!(&results[1].content, ToolResultContent::Text(s) if s == "first"));
    }

    #[tokio::test]
    async fn an_error_result_is_not_replayed_from_the_cache() {
        let hints = McpToolHints {
            read_only: Some(true),
            idempotent: Some(true),
            destructive: None,
        };
        let (results, calls) = call_twice(hints, text("failed", true)).await;
        assert_eq!(calls, 2);
        assert!(!results[1].is_error);
    }

    #[tokio::test]
    async fn read_only_comes_from_the_servers_hint() {
        let client = Arc::new(MockMcpClient::new("s", vec![]));
        assert!(!McpToolAdapter::new(client.clone(), meta("x")).is_read_only(&json!({})));
        let mut m = meta("x");
        m.hints.read_only = Some(true);
        assert!(McpToolAdapter::new(client, m).is_read_only(&json!({})));
    }

    #[tokio::test]
    async fn multi_block_result_keeps_blocks() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("multi")]));
        client.push_response(
            "multi",
            McpCallResult {
                content: vec![
                    McpContent::Text("part1".into()),
                    McpContent::Text("part2".into()),
                ],
                is_error: false,
                meta: None,
            },
        );
        let adapter = McpToolAdapter::new(client, meta("multi"));
        let r = adapter
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Blocks(b) => assert_eq!(b.len(), 2),
            _ => panic!("expected Blocks"),
        }
    }

    #[tokio::test]
    async fn image_content_converts_to_image_block() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("img")]));
        client.push_response(
            "img",
            McpCallResult {
                content: vec![McpContent::Image {
                    data: "base64data".into(),
                    media_type: "image/png".into(),
                }],
                is_error: false,
                meta: None,
            },
        );
        let adapter = McpToolAdapter::new(client, meta("img"));
        let r = adapter
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        // A lone image block must survive as a real image block. This used to
        // assert the opposite (the whole image collapsing to the placeholder
        // string `[image]`), which is precisely the data loss that made a
        // single-image MCP result — the common screenshot/diagram shape —
        // invisible to the model.
        match r.content {
            ToolResultContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                let (media_type, data) = blocks[0].as_image().expect("expected an image block");
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "base64data");
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_aborts() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("hang")]));
        // 故意不 push_response → call_tool 永不返回？不对，mock 是 sync 直接返回错误
        // 所以我们改成：cancel 已触发后 select! biased 命中 cancel 分支
        let adapter = McpToolAdapter::new(client, meta("hang"));
        let ctx = ToolContext::for_test(std::path::PathBuf::from("/tmp"));
        ctx.cancel.cancel();
        let err = adapter
            .call(json!({}), ctx, ProgressSender::noop("t"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn non_object_input_validates_err() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("x")]));
        let adapter = McpToolAdapter::new(client, meta("x"));
        let r = adapter.validate_input(&json!("string"), &ctx()).await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn default_check_permissions_is_ask() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("x")]));
        let adapter = McpToolAdapter::new(client, meta("x"));
        let d = adapter.check_permissions(&json!({}), &ctx()).await;
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }
}
