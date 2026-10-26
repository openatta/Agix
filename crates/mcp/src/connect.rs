//! 真实 MCP server 连接：用 rmcp 1.6 的 client API 起 stdio / streamable-http。
//!
//! 注：rmcp 把 RunningService 暴露成 generic over transport；我们用 trait
//! object 屏蔽，让上层只看 `dyn McpClient`。
//!
//! ## 重连策略
//!
//! Server 子进程 crash 或管道断了之后：
//! - **检测**：list_tools / call_tool 拿到 transport-shape 的错误（rmcp 报错文本
//!   含 `closed` / `eof` / `broken` 或我们自己看到 inner=None）
//! - **重连**：drop 旧 service → 用原 config 重 spawn 子进程 → 重做 MCP 握手
//! - **节流**：两次 reconnect 至少隔 [`MIN_RECONNECT_INTERVAL`]；连续失败超
//!   [`MAX_CONSECUTIVE_FAILURES`] 之后回到 NotConnected，留给下次显式调用再试
//! - **不做主动 keepalive**：纯 lazy 检测；下个请求触发恢复
//!
//! 见 agix-mcp/AGIX.md。

use crate::client::{
    McpCallResult, McpClient, McpContent, McpPromptArg, McpPromptMeta, McpResourceMeta, McpToolMeta,
};
use crate::config::McpServerConfig;
use crate::error::McpError;
use async_trait::async_trait;
use futures::StreamExt;
use rmcp::{model::CallToolRequestParams, service::ServiceExt};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// 两次自动 reconnect 之间至少这么久 —— 防 server 启动慢时被洪水重连击垮
const MIN_RECONNECT_INTERVAL: Duration = Duration::from_millis(500);
/// 连续 reconnect 失败超过这个数就放弃，转 NotConnected
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const MAX_MCP_DESCRIPTION_LENGTH: usize = 10_000;

/// Clamp what a single server may hand us at initialize time.
///
/// `String::truncate` **panics** when the byte offset lands inside a
/// multi-byte character, which is exactly what a >10 KB non-ASCII instruction
/// string does ~2/3 of the time — so this used to be a server-controlled panic
/// on the connect path. `truncate_at_char_boundary` is the canonical
/// UTF-8-safe helper. Note this is only the per-connection clamp; the
/// system-prompt assembly is bounded separately in `manager.rs`.
fn truncate_mcp_instructions(instructions: String) -> String {
    if instructions.len() <= MAX_MCP_DESCRIPTION_LENGTH {
        return instructions;
    }
    let kept = base::text::truncate_at_char_boundary(&instructions, MAX_MCP_DESCRIPTION_LENGTH);
    format!("{kept}... [truncated]")
}

/// stdio 子进程连接的 MCP client。`Arc<Self>` 共享给多个 adapter。
///
/// 内部存 rmcp 的 `RunningService` —— 真正干活的 RPC 引擎。ClientHandler 用
/// [`crate::notify::NotifyingHandler`]：除了我们会处理的几条 notification，
/// 其余全部保持 trait 的默认行为（也就是这里原先用 `()` 时的行为）。
pub struct StdioMcpClient {
    server_name: String,
    config: McpServerConfig,
    instructions: Option<String>,
    inner: Mutex<
        Option<rmcp::service::RunningService<rmcp::RoleClient, crate::notify::NotifyingHandler>>,
    >,
    last_reconnect_at: Mutex<Option<Instant>>,
    consecutive_failures: AtomicU32,
    /// Kept so a reconnect rebuilds the same notification wiring — a server
    /// that came back after a crash is exactly one whose tool list may have
    /// changed, so losing the sink here would silence the case that matters
    /// most.
    sink: Option<crate::notify::NotificationSink>,
    /// The environment policy a stdio server's process starts under, kept
    /// for the same reason: a reconnect must not start it under another.
    env_policy: base::interface::exec::local::SubprocessEnv,
    /// The owning pool's OAuth state, for servers with an `oauth_provider`.
    oauth: Option<Arc<crate::oauth::McpOAuth>>,
}

impl StdioMcpClient {
    /// 启动子进程并完成 MCP 握手。
    pub async fn connect(
        server_name: &str,
        config: &McpServerConfig,
    ) -> Result<Arc<Self>, McpError> {
        Self::connect_with_sink(server_name, config, None).await
    }

    /// Connect, forwarding this server's notifications to `sink`.
    ///
    /// `None` reproduces the pre-existing behavior exactly: the handler is
    /// still real, it simply has nowhere to report to.
    pub async fn connect_with_sink(
        server_name: &str,
        config: &McpServerConfig,
        sink: Option<crate::notify::NotificationSink>,
    ) -> Result<Arc<Self>, McpError> {
        Self::connect_with_env(
            server_name,
            config,
            sink,
            base::interface::exec::local::SubprocessEnv::default(),
            None,
        )
        .await
    }

    /// Connect, starting a stdio server's process under `env_policy` — the
    /// `sandbox.subprocess_env` of the settings the server was declared in —
    /// and taking bearer tokens for an `oauth_provider` from `oauth`.
    pub async fn connect_with_env(
        server_name: &str,
        config: &McpServerConfig,
        sink: Option<crate::notify::NotificationSink>,
        env_policy: base::interface::exec::local::SubprocessEnv,
        oauth: Option<Arc<crate::oauth::McpOAuth>>,
    ) -> Result<Arc<Self>, McpError> {
        let service = spawn_service(
            server_name,
            config,
            sink.clone(),
            &env_policy,
            oauth.as_deref(),
        )
        .await?;
        let instructions = service
            .peer_info()
            .and_then(|info| info.instructions.clone())
            .map(truncate_mcp_instructions);
        Ok(Arc::new(Self {
            server_name: server_name.into(),
            config: config.clone(),
            instructions,
            inner: Mutex::new(Some(service)),
            last_reconnect_at: Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink,
            env_policy,
            oauth,
        }))
    }

    /// 主动关连接 —— shutdown 子进程。
    pub async fn shutdown(&self) {
        if let Some(svc) = self.inner.lock().await.take() {
            let _ = svc.cancel().await;
        }
    }

    /// 把 inner 里残留的死 service 丢掉，用原 config 重 spawn 一份。
    /// 节流 + 失败计数；超阈值返回 NotConnected。
    ///
    /// 如果 config 有 oauth_provider，先清除缓存的 OAuth token 以确保
    /// 重连时重新走 OAuth 流程（适用 401 / token 过期场景）。
    async fn try_reconnect(&self) -> Result<(), McpError> {
        let fails = self.consecutive_failures.load(Ordering::SeqCst);
        if fails >= MAX_CONSECUTIVE_FAILURES {
            return Err(McpError::NotConnected {
                name: self.server_name.clone(),
            });
        }

        // 节流：上次 reconnect 还不到 MIN_RECONNECT_INTERVAL 的话先等
        {
            let mut last = self.last_reconnect_at.lock().await;
            if let Some(t) = *last {
                let since = t.elapsed();
                if since < MIN_RECONNECT_INTERVAL {
                    tokio::time::sleep(MIN_RECONNECT_INTERVAL - since).await;
                }
            }
            *last = Some(Instant::now());
        }

        // 如果 config 有 oauth_provider，清除缓存的 OAuth token 以确保
        // 重连时重新 resolve 一次（处理 401 / token 过期）。
        if let (Some(provider), Some(oauth)) = (self.config.oauth_provider(), &self.oauth) {
            oauth.clear_token(provider).await;
        }

        // 先把旧 service 清掉（cancel 会 kill 子进程）
        if let Some(old) = self.inner.lock().await.take() {
            let _ = old.cancel().await;
        }

        match spawn_service(
            &self.server_name,
            &self.config,
            self.sink.clone(),
            &self.env_policy,
            self.oauth.as_deref(),
        )
        .await
        {
            Ok(new_svc) => {
                *self.inner.lock().await = Some(new_svc);
                self.consecutive_failures.store(0, Ordering::SeqCst);
                debug!(server = %self.server_name, "MCP server reconnected");
                Ok(())
            }
            Err(e) => {
                self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
                warn!(
                    server = %self.server_name,
                    fails = fails + 1,
                    error = %e,
                    "MCP reconnect attempt failed"
                );
                Err(e)
            }
        }
    }

    async fn list_tools_inner(&self) -> Result<Vec<McpToolMeta>, McpError> {
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;
        let list = svc
            .list_tools(Default::default())
            .await
            .map_err(|e| McpError::RmcpService(format!("list_tools: {e}")))?;
        Ok(list
            .tools
            .into_iter()
            .map(|t| McpToolMeta {
                name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()),
                input_schema: serde_json::Value::Object((*t.input_schema).clone()),
                hints: t
                    .annotations
                    .map(|a| crate::client::McpToolHints {
                        read_only: a.read_only_hint,
                        idempotent: a.idempotent_hint,
                        destructive: a.destructive_hint,
                    })
                    .unwrap_or_default(),
            })
            .collect())
    }

    async fn call_tool_inner(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<McpCallResult, McpError> {
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;

        let req = CallToolRequestParams::new(tool_name.to_string()).with_arguments(args);
        let result = svc
            .call_tool(req)
            .await
            .map_err(|e| McpError::RmcpService(format!("call_tool: {e}")))?;

        let mut content_blocks = Vec::new();
        for c in result.content.iter() {
            content_blocks.push(rmcp_content_to_ours(c));
        }

        Ok(McpCallResult {
            content: content_blocks,
            is_error: result.is_error.unwrap_or(false),
            meta: None,
        })
    }
}

#[async_trait]
impl McpClient for StdioMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn transport_kind(&self) -> &'static str {
        match &self.config {
            McpServerConfig::Stdio { .. } => "stdio",
            McpServerConfig::StreamableHttp { .. } => "streamable_http",
            McpServerConfig::Sse { .. } => "sse",
            McpServerConfig::InProcess { .. } => "in_process",
            McpServerConfig::WebSocket { .. } => "web_socket",
        }
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError> {
        match self.list_tools_inner().await {
            Ok(r) => Ok(r),
            Err(e) if is_auth_error(&e) => {
                warn!(
                    server = %self.server_name,
                    error = %e,
                    "MCP list_tools auth error; attempting reconnect with fresh token"
                );
                self.try_reconnect().await?;
                self.list_tools_inner().await
            }
            Err(e) if is_transport_error(&e) => {
                warn!(
                    server = %self.server_name,
                    error = %e,
                    "MCP list_tools transport error; attempting reconnect"
                );
                self.try_reconnect().await?;
                self.list_tools_inner().await
            }
            Err(e) => Err(e),
        }
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<McpCallResult, McpError> {
        match self.call_tool_inner(tool_name, args.clone()).await {
            Ok(r) => Ok(r),
            Err(e) if is_auth_error(&e) => {
                warn!(
                    server = %self.server_name,
                    tool = tool_name,
                    error = %e,
                    "MCP call_tool auth error; attempting reconnect with fresh token"
                );
                self.try_reconnect().await?;
                self.call_tool_inner(tool_name, args).await
            }
            Err(e) if is_transport_error(&e) => {
                warn!(
                    server = %self.server_name,
                    tool = tool_name,
                    error = %e,
                    "MCP call_tool transport error; attempting reconnect"
                );
                self.try_reconnect().await?;
                self.call_tool_inner(tool_name, args).await
            }
            Err(e) => Err(e),
        }
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceMeta>, McpError> {
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;
        // rmcp 的 list_all_resources 自动分页直到拿完
        let resources = svc
            .list_all_resources()
            .await
            .map_err(|e| McpError::RmcpService(format!("list_resources: {e}")))?;
        Ok(resources
            .into_iter()
            .map(|r| {
                // Annotated<RawResource> derefs to RawResource; clone fields out
                let raw = &r.raw;
                McpResourceMeta {
                    uri: raw.uri.clone(),
                    name: raw.name.clone(),
                    description: raw.description.clone(),
                    mime_type: raw.mime_type.clone(),
                }
            })
            .collect())
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
        use rmcp::model::ReadResourceRequestParams;
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;
        let result = svc
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(|e| McpError::RmcpService(format!("read_resource: {e}")))?;
        // rmcp ReadResourceResult.contents 是 Vec<ResourceContents>；转成我们的 McpContent
        let mut out = Vec::new();
        for c in result.contents {
            match c {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    out.push(McpContent::Text(text));
                }
                rmcp::model::ResourceContents::BlobResourceContents {
                    blob, mime_type, ..
                } => {
                    out.push(McpContent::Image {
                        data: blob,
                        media_type: mime_type.unwrap_or_else(|| "application/octet-stream".into()),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptMeta>, McpError> {
        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;
        let prompts = svc
            .list_all_prompts()
            .await
            .map_err(|e| McpError::RmcpService(format!("list_prompts: {e}")))?;
        Ok(prompts
            .into_iter()
            .map(|p| McpPromptMeta {
                name: p.name.to_string(),
                description: p.description.map(|d| d.to_string()),
                arguments: p
                    .arguments
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| McpPromptArg {
                        name: a.name.to_string(),
                        description: a.description.map(|d| d.to_string()),
                        required: a.required,
                    })
                    .collect(),
            })
            .collect())
    }

    async fn get_prompt(
        &self,
        prompt_name: &str,
        args: &std::collections::HashMap<String, String>,
    ) -> Result<String, McpError> {
        use rmcp::model::GetPromptRequestParams;

        let guard = self.inner.lock().await;
        let svc = guard.as_ref().ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })?;

        let mut params = GetPromptRequestParams::new(prompt_name);
        if !args.is_empty() {
            let mut json_args = rmcp::model::JsonObject::new();
            for (k, v) in args {
                json_args.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
            params = params.with_arguments(json_args);
        }

        let result = svc
            .get_prompt(params)
            .await
            .map_err(|e| McpError::RmcpService(format!("get_prompt: {e}")))?;

        Ok(render_prompt_result(result))
    }
}

/// 把 `prompts/get` 的结果渲染成给 slash command 用的纯文本：拼接所有消息的
/// 文本内容，非文本 content（图片/资源）用占位说明——`execute_prompt()` 的调
/// 用方期望一个可以直接当 prompt body 用的字符串，不是结构化多模态内容。
/// 消息为空时退回 `description`（服务器可能只填了这一个字段）。
fn render_prompt_result(result: rmcp::model::GetPromptResult) -> String {
    use rmcp::model::PromptMessageContent;

    let rendered = result
        .messages
        .iter()
        .map(|m| match &m.content {
            PromptMessageContent::Text { text } => text.clone(),
            PromptMessageContent::Image { .. } => "[image content omitted]".to_string(),
            PromptMessageContent::Resource { .. } => "[embedded resource omitted]".to_string(),
            PromptMessageContent::ResourceLink { link } => {
                format!("[resource link: {}]", link.uri)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    if rendered.is_empty() {
        result.description.unwrap_or_default()
    } else {
        rendered
    }
}

/// 根据 server config 起一个新 rmcp service（连过 + 握手完成）。
async fn spawn_service(
    server_name: &str,
    config: &McpServerConfig,
    sink: Option<crate::notify::NotificationSink>,
    env_policy: &base::interface::exec::local::SubprocessEnv,
    oauth: Option<&crate::oauth::McpOAuth>,
) -> Result<
    rmcp::service::RunningService<rmcp::RoleClient, crate::notify::NotifyingHandler>,
    McpError,
> {
    match config {
        McpServerConfig::Stdio {
            command, args, env, ..
        } => {
            // Expand $VAR/${VAR}/${VAR:-default}/$$ in command/args/env values
            // before spawning.
            let command = crate::config::expand_env_vars(command);
            let args: Vec<String> = args
                .iter()
                .map(|a| crate::config::expand_env_vars(a))
                .collect();
            let env: std::collections::HashMap<String, String> = env
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            spawn_stdio_service(server_name, &command, &args, &env, sink, env_policy).await
        }
        McpServerConfig::StreamableHttp {
            url,
            headers,
            oauth_provider,
            ..
        } => {
            let url = crate::config::expand_env_vars(url);
            let headers: std::collections::HashMap<String, String> = headers
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            spawn_streamable_http_service(
                server_name,
                &url,
                &headers,
                oauth_provider.as_deref(),
                oauth,
                sink,
            )
            .await
        }
        McpServerConfig::Sse {
            url,
            headers,
            oauth_provider,
            ..
        } => {
            let url = crate::config::expand_env_vars(url);
            let headers: std::collections::HashMap<String, String> = headers
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            tracing::info!(
                server = %server_name,
                url = %url,
                "connecting to MCP server via SSE transport"
            );
            spawn_sse_service(
                server_name,
                &url,
                &headers,
                oauth_provider.as_deref(),
                oauth,
                sink,
            )
            .await
        }
        McpServerConfig::InProcess { name, .. } => {
            // In-process servers are handled by InProcessMcpClient via
            // the process-local registry. StdioMcpClient cannot spawn them.
            Err(McpError::ConnectFailed {
                name: server_name.into(),
                source: anyhow::anyhow!(
                    "in-process MCP server '{}' must be connected via \
                     InProcessMcpClient::connect, not spawned as a subprocess",
                    name
                ),
            })
        }
        McpServerConfig::WebSocket { url, headers, .. } => {
            let url = crate::config::expand_env_vars(url);
            let headers: std::collections::HashMap<String, String> = headers
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            spawn_websocket_service(server_name, &url, &headers, sink).await
        }
    }
}

async fn spawn_stdio_service(
    server_name: &str,
    command: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
    sink: Option<crate::notify::NotificationSink>,
    env_policy: &base::interface::exec::local::SubprocessEnv,
) -> Result<
    rmcp::service::RunningService<rmcp::RoleClient, crate::notify::NotifyingHandler>,
    McpError,
> {
    let mut cmd = tokio::process::Command::new(command);
    for a in args {
        cmd.arg(a);
    }
    env_policy.apply(&mut cmd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut cmd = process_wrap::tokio::CommandWrap::from(cmd);
    #[cfg(unix)]
    cmd.wrap(process_wrap::tokio::ProcessGroup::leader());
    cmd.wrap(process_wrap::tokio::KillOnDrop);

    let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd).map_err(|e| {
        McpError::ConnectFailed {
            name: server_name.into(),
            source: anyhow::Error::new(e),
        }
    })?;

    crate::notify::NotifyingHandler::new(server_name, sink)
        .serve(transport)
        .await
        .map_err(|e| McpError::ConnectFailed {
            name: server_name.into(),
            source: anyhow::anyhow!("{e}"),
        })
}

/// Headers the transport sets itself; rmcp rejects a request that also carries
/// one of these as a custom header.
const TRANSPORT_OWNED_HEADERS: &[&str] = &[
    "accept",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// Turn configured headers into what goes on the wire, every one of them,
/// verbatim. The exception is an `Authorization` value with no scheme: rmcp's
/// own auth slot always prefixed `Bearer `, so configs written for it hold a
/// bare token, and that meaning is kept.
fn wire_headers(
    server_name: &str,
    headers: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue> {
    use reqwest::header::{HeaderName, HeaderValue};
    let mut out = std::collections::HashMap::new();
    for (k, v) in headers {
        if TRANSPORT_OWNED_HEADERS
            .iter()
            .any(|h| k.eq_ignore_ascii_case(h))
        {
            warn!(server = %server_name, header = %k, "header is set by the MCP transport itself; ignored");
            continue;
        }
        let value = if k.eq_ignore_ascii_case("authorization") && !v.trim().contains(' ') {
            format!("Bearer {}", v.trim())
        } else {
            v.clone()
        };
        match (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            (Ok(name), Ok(value)) => {
                out.insert(name, value);
            }
            _ => warn!(server = %server_name, header = %k, "invalid HTTP header; ignored"),
        }
    }
    out
}

fn http_transport_config(
    server_name: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    oauth_authorization: Option<String>,
) -> rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig {
    let mut headers = headers.clone();
    if let Some(a) = oauth_authorization {
        headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
        headers.insert("Authorization".into(), a);
    }
    rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url)
        .custom_headers(wire_headers(server_name, &headers))
}

/// StreamableHttp transport (rmcp's reqwest client). Configured headers go on
/// every request; with `oauth_provider`, a stored (refreshed if needed) token
/// replaces any configured `Authorization`. Headers are fixed when the
/// transport is built, so a changed token takes a reconnect.
async fn spawn_streamable_http_service(
    server_name: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    oauth_provider: Option<&str>,
    oauth: Option<&crate::oauth::McpOAuth>,
    sink: Option<crate::notify::NotificationSink>,
) -> Result<
    rmcp::service::RunningService<rmcp::RoleClient, crate::notify::NotifyingHandler>,
    McpError,
> {
    use rmcp::transport::StreamableHttpClientTransport;

    let resolved_auth: Option<String> = if let Some(provider) = oauth_provider {
        let resolved = match oauth {
            Some(oauth) => oauth.resolve_bearer(provider).await,
            None => Err("this connection has no OAuth state".to_string()),
        };
        match resolved {
            Ok(tok) => Some(format!("Bearer {tok}")),
            Err(e) => {
                warn!(
                    server = %server_name,
                    provider = %provider,
                    error = %e,
                    "OAuth provider lookup failed; falling back to static headers"
                );
                None
            }
        }
    } else {
        None
    };
    let config = http_transport_config(server_name, url, headers, resolved_auth);
    let transport = StreamableHttpClientTransport::from_config(config);

    crate::notify::NotifyingHandler::new(server_name, sink)
        .serve(transport)
        .await
        .map_err(|e| McpError::ConnectFailed {
            name: server_name.into(),
            source: anyhow::anyhow!("{e}"),
        })
}

/// 启发式判断：错误是不是 transport / connectivity 类（值得重连）vs
/// 真正的 RPC / schema / business 错（不该重连）。
///
/// 假阳性的代价：多 spawn 一次子进程（轻）。假阴性的代价：用户错过自动恢复
/// （重）。所以这里偏宽松。
pub(crate) fn is_transport_error(e: &McpError) -> bool {
    match e {
        McpError::NotConnected { .. } => true,
        McpError::RmcpService(msg) => {
            let m = msg.to_ascii_lowercase();
            [
                "closed",
                "eof",
                "broken pipe",
                "connection",
                "channel",
                "timeout",
            ]
            .iter()
            .any(|k| m.contains(k))
        }
        McpError::Transport(_) => true,
        _ => false,
    }
}

/// 启发式判断：错误是不是 auth / 401 / token 过期类（值得重连 + 重新 resolve OAuth token）
/// vs 真正不可恢复的授权错。
///
/// 与 is_transport_error 的关系：auth 错也会触发 reconnect，区别在于 reconnect 前
/// 会重新 resolve OAuth bearer token（StreamableHttp transport 场景）。Stdio transport
/// 不存在 auth 错。
pub(crate) fn is_auth_error(e: &McpError) -> bool {
    match e {
        McpError::NotConnected { .. } => false,
        McpError::RmcpService(msg) => {
            let m = msg.to_ascii_lowercase();
            [
                "401",
                "403",
                "unauthorized",
                "forbidden",
                "token expired",
                "access denied",
                "auth failure",
                "authentication",
            ]
            .iter()
            .any(|k| m.contains(k))
        }
        _ => false,
    }
}

/// 把 rmcp::model::Content 转成我们的 McpContent。
fn rmcp_content_to_ours(c: &rmcp::model::Annotated<rmcp::model::RawContent>) -> McpContent {
    match &c.raw {
        rmcp::model::RawContent::Text(t) => McpContent::Text(t.text.to_string()),
        rmcp::model::RawContent::Image(i) => McpContent::Image {
            data: i.data.to_string(),
            media_type: i.mime_type.to_string(),
        },
        other => {
            // 其它 content 类型：序列化成 JSON 留给上层
            let v = serde_json::to_value(other).unwrap_or(serde_json::Value::Null);
            McpContent::Other(v)
        }
    }
}

// ── WebSocket transport ──

/// Type alias for the tokio_tungstenite WebSocket stream (with optional TLS).
type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Concrete error type for the WebSocket transport.
///
/// Required because `Box<dyn Error + Send + Sync>` does not satisfy
/// the `Sized` bound baked into rmcp's `Transport::Error` supertrait.
#[derive(Debug, thiserror::Error)]
enum WsTransportError {
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("websocket: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
}

/// rmcp `Transport` adapter for WebSocket connections.
///
/// Each WebSocket text frame carries one JSON-RPC message (`serde_json`
/// serialized/deserialized). Binary frames are also accepted on the
/// receive side. Ping/pong frames are handled internally by tungstenite.
///
/// The write half is behind `Arc<Mutex<…>>` so that `send()` can return a
/// `'static + Send` future per the `Transport` trait contract.
struct WebSocketTransport {
    write:
        Arc<Mutex<futures::stream::SplitSink<WsStream, tokio_tungstenite::tungstenite::Message>>>,
    read: futures::stream::SplitStream<WsStream>,
}

impl rmcp::transport::Transport<rmcp::RoleClient> for WebSocketTransport {
    type Error = WsTransportError;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let json = serde_json::to_string(&item);
        let write = self.write.clone();
        Box::pin(async move {
            let msg = tokio_tungstenite::tungstenite::Message::text(json?);
            let mut guard = write.lock().await;
            futures::SinkExt::send(&mut *guard, msg).await?;
            Ok(())
        })
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleClient>>>
           + Send {
        let read = &mut self.read;
        Box::pin(async move {
            loop {
                match futures::StreamExt::next(read).await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if let Ok(msg) = serde_json::from_str(&text) {
                            return Some(msg);
                        }
                        tracing::warn!("WebSocket: ignoring non-JSON-RPC text frame");
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
                        if let Ok(msg) = serde_json::from_slice(&data) {
                            return Some(msg);
                        }
                    }
                    // Close frame or stream end => connection closed
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                        return None;
                    }
                    // Ping/Pong handled internally by tungstenite
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        tracing::error!("WebSocket receive error: {e}");
                        return None;
                    }
                }
            }
        })
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        let write = self.write.clone();
        Box::pin(async move {
            let mut guard = write.lock().await;
            futures::SinkExt::close(&mut *guard).await?;
            Ok(())
        })
    }
}

/// Try to establish a raw WebSocket connection for MCP, sending the configured
/// headers on the upgrade request.
async fn try_ws_connect(
    server_name: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
) -> Result<WebSocketTransport, anyhow::Error> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    url::Url::parse(url).map_err(|e| anyhow::anyhow!("Invalid WebSocket URL '{url}': {e}"))?;
    let mut request = url
        .into_client_request()
        .map_err(|e| anyhow::anyhow!("Invalid WebSocket URL '{url}': {e}"))?;
    request
        .headers_mut()
        .extend(wire_headers(server_name, headers));

    let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| anyhow::anyhow!("WebSocket connection to '{url}' failed: {e}"))?;

    let (write, read) = ws_stream.split();
    Ok(WebSocketTransport {
        write: Arc::new(Mutex::new(write)),
        read,
    })
}

/// Convert a `ws://` or `wss://` URL to the equivalent HTTP scheme for
/// StreamableHTTP fallback.
fn ws_url_to_http(url: &str) -> String {
    if url.starts_with("ws://") {
        url.replacen("ws://", "http://", 1)
    } else if url.starts_with("wss://") {
        url.replacen("wss://", "https://", 1)
    } else {
        url.to_string()
    }
}

/// WebSocket MCP transport with transparent StreamableHTTP fallback.
///
/// Strategy:
/// 1. Attempt a direct WebSocket connection via `tokio_tungstenite`.
/// 2. On success, wrap the stream in `WebSocketTransport` and serve via rmcp.
/// 3. On failure (connection refused, upgrade not supported, DNS error, …),
///    convert `ws://` → `http://` (or `wss://` → `https://`) and fall back
///    to the StreamableHTTP transport.
///
/// Configured headers go on the upgrade request and on the fallback alike.
async fn spawn_websocket_service(
    server_name: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    sink: Option<crate::notify::NotificationSink>,
) -> Result<
    rmcp::service::RunningService<rmcp::RoleClient, crate::notify::NotifyingHandler>,
    McpError,
> {
    // 1. Try WebSocket first
    match try_ws_connect(server_name, url, headers).await {
        Ok(transport) => {
            info!(
                server = %server_name,
                url = %url,
                "Connected to MCP server via WebSocket transport"
            );
            return crate::notify::NotifyingHandler::new(server_name, sink)
                .serve(transport)
                .await
                .map_err(|e| McpError::ConnectFailed {
                    name: server_name.into(),
                    source: anyhow::anyhow!("{e}"),
                });
        }
        Err(e) => {
            warn!(
                server = %server_name,
                url = %url,
                error = %e,
                "WebSocket connection failed; falling back to StreamableHTTP"
            );
        }
    }

    // 2. Fall back to StreamableHTTP
    let http_url = ws_url_to_http(url);
    info!(
        server = %server_name,
        ws_url = %url,
        http_url = %http_url,
        "WebSocket unavailable; falling back to StreamableHTTP transport"
    );
    spawn_streamable_http_service(server_name, &http_url, headers, None, None, sink).await
}

// ── SSE (Server-Sent Events) transport ──
// P2-7: Pure SSE transport. Delegates to streamable HTTP as fallback.
async fn spawn_sse_service(
    server_name: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    oauth_provider: Option<&str>,
    oauth: Option<&crate::oauth::McpOAuth>,
    sink: Option<crate::notify::NotificationSink>,
) -> Result<
    rmcp::service::RunningService<rmcp::RoleClient, crate::notify::NotifyingHandler>,
    McpError,
> {
    tracing::info!(server = %server_name, url = %url, "SSE transport: delegating to streamable HTTP");
    spawn_streamable_http_service(server_name, url, headers, oauth_provider, oauth, sink).await
}

// ── WebSocketMcpClient ──

/// MCP client connected via WebSocket transport (with StreamableHTTP fallback).
///
/// This is a thin convenience wrapper. `connect()` attempts a WebSocket
/// connection first; if the server does not support the WebSocket upgrade,
/// it falls back to StreamableHTTP. All `McpClient` methods delegate to the
/// underlying connection.
///
/// The `transport_kind()` always reports `"web_socket"` regardless of
/// whether the actual transport is native WebSocket or the StreamableHTTP
/// fallback.
pub struct WebSocketMcpClient {
    inner: Arc<StdioMcpClient>,
}

impl WebSocketMcpClient {
    /// Connect to an MCP server configured with WebSocket transport.
    ///
    /// Attempts WebSocket first, then StreamableHTTP on failure.
    pub async fn connect(
        server_name: &str,
        config: &McpServerConfig,
    ) -> Result<Arc<Self>, McpError> {
        let inner = StdioMcpClient::connect(server_name, config).await?;
        Ok(Arc::new(Self { inner }))
    }

    /// Shutdown the WebSocket connection.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

#[async_trait]
impl McpClient for WebSocketMcpClient {
    fn server_name(&self) -> &str {
        self.inner.server_name()
    }

    fn transport_kind(&self) -> &'static str {
        "web_socket"
    }

    fn instructions(&self) -> Option<&str> {
        self.inner.instructions()
    }

    async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError> {
        self.inner.list_tools().await
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<McpCallResult, McpError> {
        self.inner.call_tool(tool_name, args).await
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceMeta>, McpError> {
        self.inner.list_resources().await
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
        self.inner.read_resource(uri).await
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptMeta>, McpError> {
        self.inner.list_prompts().await
    }

    async fn get_prompt(
        &self,
        prompt_name: &str,
        args: &std::collections::HashMap<String, String>,
    ) -> Result<String, McpError> {
        self.inner.get_prompt(prompt_name, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A stdio server is a child process like any other, and starts under the
    /// environment policy of the settings it was declared in.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stdio_server_starts_under_the_env_policy_it_was_given() {
        use base::interface::exec::local::{SubprocessEnv, SubprocessEnvMode};
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("env.txt");
        let cfg = McpServerConfig::Stdio {
            command: "sh".into(),
            args: vec!["-c".into(), format!("env > '{}'", out.display())],
            env: HashMap::new(),
            scope: None,
        };
        let policy = SubprocessEnv {
            mode: SubprocessEnvMode::Allowlist,
            allow: Vec::new(),
        };
        // The script answers no handshake, so the connect fails — after the
        // process has run, which is all this needs.
        let _ = StdioMcpClient::connect_with_env("probe", &cfg, None, policy, None).await;

        let mut seen = String::new();
        for _ in 0..100 {
            seen = std::fs::read_to_string(&out).unwrap_or_default();
            if seen.contains("PATH=") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(seen.contains("PATH="), "the server saw: {seen}");
        assert!(
            !seen.contains("CARGO_MANIFEST_DIR="),
            "an allowlist policy passed the host's environment through: {seen}"
        );
    }

    #[test]
    fn is_transport_error_for_not_connected() {
        let e = McpError::NotConnected { name: "x".into() };
        assert!(is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_for_transport_variant() {
        let e = McpError::Transport(anyhow::anyhow!("any"));
        assert!(is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_for_rmcp_with_eof_msg() {
        let e = McpError::RmcpService("connection closed by peer".into());
        assert!(is_transport_error(&e));
        let e = McpError::RmcpService("read EOF unexpectedly".into());
        assert!(is_transport_error(&e));
        let e = McpError::RmcpService("broken pipe writing to child".into());
        assert!(is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_skips_genuine_rpc_errors() {
        // 业务错误：tool 抛 ValidationError —— 不该重连
        let e = McpError::RmcpService("tool returned validation error: bad arg".into());
        assert!(!is_transport_error(&e));
    }

    #[test]
    fn is_transport_error_skips_unknown_tool_and_schema() {
        let e = McpError::UnknownTool {
            name: "x".into(),
            tool: "y".into(),
        };
        assert!(!is_transport_error(&e));
        let e = McpError::Schema(serde_json::from_str::<()>("not json").unwrap_err());
        assert!(!is_transport_error(&e));
    }

    // ---- is_auth_error tests ----

    #[test]
    fn is_auth_error_for_401() {
        let e = McpError::RmcpService("HTTP 401 Unauthorized".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_for_forbidden() {
        let e = McpError::RmcpService("403 Forbidden: access denied".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_for_token_expired() {
        let e = McpError::RmcpService("token expired, please re-authenticate".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_skips_transport_errors() {
        // transport 错不该被误判为 auth
        let e = McpError::RmcpService("connection closed by peer".into());
        assert!(!is_auth_error(&e));
        let e = McpError::RmcpService("broken pipe".into());
        assert!(!is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_skips_unknown_tool_and_not_connected() {
        let e = McpError::UnknownTool {
            name: "x".into(),
            tool: "y".into(),
        };
        assert!(!is_auth_error(&e));
        let e = McpError::NotConnected { name: "x".into() };
        assert!(!is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_for_rmcp_auth_failure_message() {
        let e = McpError::RmcpService("authentication failure: invalid token".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn is_auth_error_for_rmcp_access_denied() {
        let e = McpError::RmcpService("access denied: insufficient permissions".into());
        assert!(is_auth_error(&e));
    }

    #[test]
    fn transport_kind_reports_stdio_for_stdio_config() {
        let client = StdioMcpClient {
            server_name: "x".into(),
            config: McpServerConfig::Stdio {
                command: "echo".into(),
                args: Vec::new(),
                env: HashMap::new(),
                scope: None,
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            oauth: None,
        };
        assert_eq!(client.transport_kind(), "stdio");
    }

    #[test]
    fn transport_kind_reports_streamable_http_for_http_config() {
        let client = StdioMcpClient {
            server_name: "x".into(),
            config: McpServerConfig::StreamableHttp {
                url: "http://example.com/mcp".into(),
                headers: HashMap::new(),
                oauth_provider: None,
                scope: None,
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            oauth: None,
        };
        assert_eq!(client.transport_kind(), "streamable_http");
    }

    // ---- get_prompt ----

    #[tokio::test]
    async fn get_prompt_errors_not_connected_when_no_session() {
        // 之前的实现无视连接状态，永远返回一句硬编码占位文案——这个测试确保
        // `get_prompt` 现在和其他真实方法（`list_prompts`/`read_resource`）一样，
        // 未连接时报 `NotConnected`，而不是伪装成功。
        let client = StdioMcpClient {
            server_name: "x".into(),
            config: McpServerConfig::Stdio {
                command: "echo".into(),
                args: Vec::new(),
                env: HashMap::new(),
                scope: None,
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            oauth: None,
        };
        let err = client
            .get_prompt("some-prompt", &HashMap::new())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::NotConnected { .. }), "err: {err:?}");
    }

    fn text_message(
        role: rmcp::model::PromptMessageRole,
        text: &str,
    ) -> rmcp::model::PromptMessage {
        rmcp::model::PromptMessage::new_text(role, text)
    }

    #[test]
    fn render_prompt_result_joins_text_messages() {
        use rmcp::model::PromptMessageRole;
        let result = rmcp::model::GetPromptResult::new(vec![
            text_message(PromptMessageRole::User, "first"),
            text_message(PromptMessageRole::Assistant, "second"),
        ]);
        assert_eq!(render_prompt_result(result), "first\n\nsecond");
    }

    #[test]
    fn render_prompt_result_falls_back_to_description_when_no_messages() {
        let mut result = rmcp::model::GetPromptResult::new(Vec::new());
        result.description = Some("fallback description".into());
        assert_eq!(render_prompt_result(result), "fallback description");
    }

    #[test]
    fn render_prompt_result_empty_when_no_messages_and_no_description() {
        let result = rmcp::model::GetPromptResult::new(Vec::new());
        assert_eq!(render_prompt_result(result), "");
    }

    #[test]
    fn render_prompt_result_placeholders_non_text_content() {
        use rmcp::model::{PromptMessage, PromptMessageContent, PromptMessageRole};
        let resource = rmcp::model::Annotated::new(
            rmcp::model::RawResource::new("file:///tmp/x.txt", "x.txt"),
            None,
        );
        let result = rmcp::model::GetPromptResult::new(vec![PromptMessage::new(
            PromptMessageRole::User,
            PromptMessageContent::resource_link(resource),
        )]);
        let rendered = render_prompt_result(result);
        assert!(
            rendered.contains("file:///tmp/x.txt"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn reconnect_with_failing_command_stops_after_max_failures() {
        // 用一个"启动后立即退出"的 fake 命令模拟 server 永远连不上
        // 这里直接构造一个 StdioMcpClient 而非走真 connect（避开 rmcp 握手）
        let client = StdioMcpClient {
            server_name: "fake".into(),
            config: McpServerConfig::Stdio {
                command: "/nonexistent/binary/that/should/fail".into(),
                args: Vec::new(),
                env: HashMap::new(),
                scope: None,
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            oauth: None,
        };
        // 三次失败后 try_reconnect 应当 short-circuit NotConnected，不再 spawn
        let mut last_err = None;
        for _ in 0..MAX_CONSECUTIVE_FAILURES + 2 {
            last_err = Some(client.try_reconnect().await.unwrap_err());
        }
        // 最后一次错误是 NotConnected（短路返回）
        assert!(matches!(last_err.unwrap(), McpError::NotConnected { .. }));
        // failures 计数应当卡在 MAX_CONSECUTIVE_FAILURES（之后 short-circuit 不再加）
        assert_eq!(
            client.consecutive_failures.load(Ordering::SeqCst),
            MAX_CONSECUTIVE_FAILURES
        );
    }

    // ── WebSocket tests ──

    #[test]
    fn ws_url_to_http_converts_ws_to_http() {
        assert_eq!(
            ws_url_to_http("ws://localhost:8080/mcp"),
            "http://localhost:8080/mcp"
        );
    }

    #[test]
    fn ws_url_to_http_converts_wss_to_https() {
        assert_eq!(
            ws_url_to_http("wss://example.com/mcp"),
            "https://example.com/mcp"
        );
    }

    #[test]
    fn ws_url_to_http_preserves_non_ws() {
        assert_eq!(ws_url_to_http("http://example.com"), "http://example.com");
        assert_eq!(ws_url_to_http("https://example.com"), "https://example.com");
        assert_eq!(ws_url_to_http(""), "");
    }

    #[test]
    fn transport_kind_reports_web_socket_for_ws_config_via_stdio_client() {
        let client = StdioMcpClient {
            server_name: "ws-server".into(),
            config: McpServerConfig::WebSocket {
                url: "ws://localhost:8080/mcp".into(),
                headers: HashMap::new(),
                scope: None,
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            oauth: None,
        };
        assert_eq!(client.transport_kind(), "web_socket");
    }

    #[test]
    fn websocket_mcp_client_transport_kind_is_always_web_socket() {
        let client = WebSocketMcpClient {
            inner: Arc::new(StdioMcpClient {
                server_name: "ws-server".into(),
                config: McpServerConfig::WebSocket {
                    url: "ws://localhost:8080/mcp".into(),
                    headers: HashMap::new(),
                    scope: None,
                },
                instructions: None,
                inner: tokio::sync::Mutex::new(None),
                last_reconnect_at: tokio::sync::Mutex::new(None),
                consecutive_failures: AtomicU32::new(0),
                sink: None,
                env_policy: Default::default(),
                oauth: None,
            }),
        };
        assert_eq!(client.transport_kind(), "web_socket");
        assert_eq!(client.server_name(), "ws-server");
    }

    #[test]
    fn websocket_mcp_client_delegates_transport_kind_override() {
        // Verify that WebSocketMcpClient always reports "web_socket"
        // even when the inner client would report something different.
        let client = WebSocketMcpClient {
            inner: Arc::new(StdioMcpClient {
                server_name: "mock".into(),
                config: McpServerConfig::Stdio {
                    command: "echo".into(),
                    args: vec![],
                    env: HashMap::new(),
                    scope: None,
                },
                instructions: Some("test".into()),
                inner: tokio::sync::Mutex::new(None),
                last_reconnect_at: tokio::sync::Mutex::new(None),
                consecutive_failures: AtomicU32::new(0),
                sink: None,
                env_policy: Default::default(),
                oauth: None,
            }),
        };
        // The inner StdioMcpClient would report "stdio",
        // but WebSocketMcpClient's override should still report "web_socket"
        assert_eq!(client.transport_kind(), "web_socket");
        assert_eq!(client.server_name(), "mock");
        assert_eq!(client.instructions(), Some("test"));
    }

    fn header<'a>(
        headers: &'a HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
        name: &'static str,
    ) -> &'a str {
        headers[&reqwest::header::HeaderName::from_static(name)]
            .to_str()
            .unwrap()
    }

    #[test]
    fn wire_headers_keep_every_header_and_only_complete_a_bare_token() {
        let headers = HashMap::from([
            ("Authorization".to_string(), "tok".to_string()),
            ("X-Tenant".to_string(), "acme".to_string()),
            ("Accept".to_string(), "text/plain".to_string()),
        ]);
        let wire = wire_headers("s", &headers);
        assert_eq!(header(&wire, "authorization"), "Bearer tok");
        assert_eq!(header(&wire, "x-tenant"), "acme");
        assert!(
            !wire.contains_key(&reqwest::header::HeaderName::from_static("accept")),
            "the transport owns Accept"
        );

        let explicit = HashMap::from([("authorization".to_string(), "Basic dXNlcg==".to_string())]);
        assert_eq!(
            header(&wire_headers("s", &explicit), "authorization"),
            "Basic dXNlcg=="
        );
    }

    /// Accepts one connection, returns its request head, and answers 500 so the
    /// client gives up.
    async fn capture_one_request() -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let _ = sock
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
            String::from_utf8_lossy(&buf).to_ascii_lowercase()
        });
        (addr.to_string(), handle)
    }

    /// Checked on the transport config rather than on the wire: reqwest honors
    /// the system proxy, so a loopback listener is not reliably reached.
    #[test]
    fn streamable_http_config_carries_the_configured_headers() {
        let headers = HashMap::from([
            ("Authorization".to_string(), "Bearer static".to_string()),
            ("X-Tenant".to_string(), "acme".to_string()),
        ]);
        let config = http_transport_config("s", "http://h/mcp", &headers, None);
        assert_eq!(
            header(&config.custom_headers, "authorization"),
            "Bearer static"
        );
        assert_eq!(header(&config.custom_headers, "x-tenant"), "acme");
        assert!(
            config.auth_header.is_none(),
            "rmcp would prefix it with Bearer again"
        );

        let oauth =
            http_transport_config("s", "http://h/mcp", &headers, Some("Bearer fresh".into()));
        assert_eq!(
            header(&oauth.custom_headers, "authorization"),
            "Bearer fresh"
        );
        assert_eq!(header(&oauth.custom_headers, "x-tenant"), "acme");
    }

    #[tokio::test]
    async fn the_websocket_upgrade_carries_the_configured_headers() {
        let (addr, server) = capture_one_request().await;
        let headers = HashMap::from([("X-Tenant".to_string(), "acme".to_string())]);
        assert!(try_ws_connect("s", &format!("ws://{addr}/mcp"), &headers)
            .await
            .is_err());
        let head = server.await.unwrap();
        assert!(head.contains("upgrade: websocket"), "{head}");
        assert!(head.contains("x-tenant: acme\r\n"), "{head}");
    }
}

// ── SSE (Server-Sent Events) transport ──
// P2-7: Pure SSE transport. Currently delegates to streamable HTTP as fallback.
