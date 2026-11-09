//! 真实 MCP server 连接：用 rmcp 3.x 的 client API 起 stdio / streamable-http。
//!
//! 注：rmcp 把 RunningService 暴露成 generic over transport；我们用 trait
//! object 屏蔽，让上层只看 `dyn McpClient`。
//!
//! ## 时限
//!
//! 起 server 加握手、以及工具调用以外的每个请求，受 server 的启动时限约束；
//! `tools/call` 受工具时限约束（见 [`crate::config::McpTimeouts`]）。超时的请求在
//! server 端被取消（rmcp 发 `notifications/cancelled`），连接本身留着。
//!
//! ## 重连策略
//!
//! Server 子进程 crash 或管道断了之后：
//! - **检测**：请求在途时连接断开（rmcp 报 `TransportClosed` / `TransportSend`）
//!   → 丢掉这条连接，报 [`McpError::ConnectionLost`]；或发请求前就看到连接已关
//! - **重连**：下一个请求发出前用原 config 重 spawn 子进程、重做 MCP 握手
//! - **不重放工具调用**：断在途中的 `tools/call` 可能已经执行过，只报错；
//!   只读的 list / read / get 断在途中会在新连接上再问一次
//! - **节流**：两次 reconnect 至少隔 [`MIN_RECONNECT_INTERVAL`]；连续失败到
//!   [`MAX_CONSECUTIVE_FAILURES`] 次后报 NotConnected，过 [`FAILURE_COOLDOWN`]
//!   再试
//! - **不做主动 keepalive**：纯 lazy 检测；下个请求触发恢复

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
/// 连续 reconnect 失败到这个数就先不再试，转 NotConnected
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// 放弃之后隔这么久，下一个请求再试一次
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_MCP_DESCRIPTION_LENGTH: usize = 10_000;

/// Clamp what a single server may hand us at initialize time.
///
/// `String::truncate` **panics** when the byte offset lands inside a
/// multi-byte character, which is exactly what a >10 KB non-ASCII instruction
/// string does ~2/3 of the time — a server-controlled panic on the connect
/// path. `truncate_at_char_boundary` is the canonical
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
/// 其余全部保持 trait 的默认行为。
pub struct StdioMcpClient {
    server_name: String,
    config: McpServerConfig,
    instructions: Option<String>,
    /// Shared out per request rather than held across it, so one slow call
    /// does not queue every other request to the same server behind it.
    inner: Mutex<Option<Arc<Service>>>,
    /// Also what serializes reconnects: two requests that find the
    /// connection gone make one new one between them.
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
    /// The owning pool's authorization state, for remote servers that use
    /// OAuth.
    auth: Option<Arc<crate::oauth::McpAuth>>,
    /// Whether the connection carries an OAuth token, so a 401 means that
    /// token was refused rather than that there never was one.
    authorized: std::sync::atomic::AtomicBool,
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
    /// and authorizing a remote server through `auth`.
    pub async fn connect_with_env(
        server_name: &str,
        config: &McpServerConfig,
        sink: Option<crate::notify::NotificationSink>,
        env_policy: base::interface::exec::local::SubprocessEnv,
        auth: Option<Arc<crate::oauth::McpAuth>>,
    ) -> Result<Arc<Self>, McpError> {
        let spawned = spawn_service_within(
            server_name,
            config,
            sink.clone(),
            &env_policy,
            auth.as_deref(),
        )
        .await?;
        let instructions = spawned
            .service
            .peer_info()
            .and_then(|info| info.instructions.clone())
            .map(truncate_mcp_instructions);
        Ok(Arc::new(Self {
            server_name: server_name.into(),
            config: config.clone(),
            instructions,
            inner: Mutex::new(Some(Arc::new(spawned.service))),
            last_reconnect_at: Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink,
            env_policy,
            auth,
            authorized: spawned.authorized.into(),
        }))
    }

    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    /// 主动关连接 —— shutdown 子进程。
    pub async fn shutdown(&self) {
        if let Some(svc) = self.inner.lock().await.take() {
            close(svc).await;
        }
    }

    /// Whether the connection is up as far as this client last saw: a
    /// server that died since is found out here only once its pipe closed.
    pub fn is_connected(&self) -> bool {
        match self.inner.try_lock() {
            Ok(guard) => guard.as_deref().is_some_and(is_open),
            Err(_) => true,
        }
    }

    async fn current(&self) -> Option<Arc<Service>> {
        self.inner
            .lock()
            .await
            .as_ref()
            .filter(|svc| is_open(svc))
            .cloned()
    }

    /// The connection to send a request on, made anew first when there is
    /// none or it has closed — a request that has not gone out yet is the
    /// one kind a new connection may take without anything running twice.
    async fn service(&self) -> Result<Arc<Service>, McpError> {
        if let Some(svc) = self.current().await {
            return Ok(svc);
        }
        self.try_reconnect().await?;
        self.current().await.ok_or_else(|| McpError::NotConnected {
            name: self.server_name.clone(),
        })
    }

    /// Stop using `svc`, unless a reconnect already replaced it.
    async fn lose(&self, svc: &Arc<Service>) {
        let mut guard = self.inner.lock().await;
        if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, svc)) {
            if let Some(old) = guard.take() {
                drop(guard);
                close(old).await;
            }
        }
    }

    /// Await one request on `svc` within `limit`. A connection that drops
    /// under it is given up, so the next request makes a new one.
    async fn request<T>(
        &self,
        op: &'static str,
        limit: Duration,
        svc: &Arc<Service>,
        fut: impl std::future::Future<Output = Result<T, rmcp::ServiceError>>,
    ) -> Result<T, McpError> {
        match tokio::time::timeout(limit, fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                let dropped = connection_dropped(&e);
                match service_error(&self.server_name, op, e) {
                    McpError::RmcpService(_) if dropped => {
                        self.lose(svc).await;
                        Err(McpError::ConnectionLost {
                            name: self.server_name.clone(),
                            op,
                        })
                    }
                    other => Err(other),
                }
            }
            Err(_) => Err(McpError::Timeout {
                name: self.server_name.clone(),
                op,
                after: limit,
            }),
        }
    }

    fn request_limit(&self) -> Duration {
        self.config.timeouts().startup()
    }

    /// A read that may be asked again: one whose connection dropped under
    /// it is, on a new connection.
    async fn read_again_if_dropped<T, F, Fut>(&self, op: F) -> Result<T, McpError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        match self.authorized(&op).await {
            Err(McpError::ConnectionLost { .. }) => {
                warn!(server = %self.server_name, "MCP connection dropped during a read; asking again");
                self.authorized(op).await
            }
            other => other,
        }
    }

    /// 用原 config 重 spawn 一份连接，替掉已经关了的那份。
    /// 节流 + 失败计数；失败到阈值后先报 NotConnected，冷却过后再试。
    async fn try_reconnect(&self) -> Result<(), McpError> {
        let mut last = self.last_reconnect_at.lock().await;
        if self.current().await.is_some() {
            return Ok(());
        }
        let mut fails = self.consecutive_failures.load(Ordering::SeqCst);
        if fails >= MAX_CONSECUTIVE_FAILURES {
            if last.is_some_and(|t| t.elapsed() >= FAILURE_COOLDOWN) {
                fails = 0;
                self.consecutive_failures.store(0, Ordering::SeqCst);
            } else {
                return Err(McpError::NotConnected {
                    name: self.server_name.clone(),
                });
            }
        }

        if let Some(t) = *last {
            let since = t.elapsed();
            if since < MIN_RECONNECT_INTERVAL {
                tokio::time::sleep(MIN_RECONNECT_INTERVAL - since).await;
            }
        }
        *last = Some(Instant::now());

        if let Some(old) = self.inner.lock().await.take() {
            close(old).await;
        }

        match spawn_service_within(
            &self.server_name,
            &self.config,
            self.sink.clone(),
            &self.env_policy,
            self.auth.as_deref(),
        )
        .await
        {
            Ok(spawned) => {
                *self.inner.lock().await = Some(Arc::new(spawned.service));
                self.authorized.store(spawned.authorized, Ordering::SeqCst);
                self.consecutive_failures.store(0, Ordering::SeqCst);
                debug!(server = %self.server_name, "MCP server reconnected");
                if let Some(sink) = &self.sink {
                    let server = self.server_name.clone();
                    sink(crate::manager::McpNotification::ToolListChanged {
                        server: server.clone(),
                    });
                    sink(crate::manager::McpNotification::PromptListChanged { server });
                }
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

    fn unattended(
        &self,
    ) -> Option<(
        &crate::oauth::McpAuth,
        String,
        &crate::config::McpOAuthConfig,
    )> {
        let oauth = self.config.oauth().filter(|o| o.grant.is_unattended())?;
        let auth = self
            .auth
            .as_deref()
            .filter(|_| self.config.oauth_capable())?;
        Some((auth, crate::oauth::server_url(&self.config)?, oauth))
    }

    /// Fetch an unattended server's token again when it is about to expire,
    /// or always when `renew`.
    async fn keep_token(&self, renew: bool) -> Result<(), McpError> {
        match self.unattended() {
            Some((auth, url, oauth)) => {
                auth.renew_unattended(&self.server_name, &url, oauth, renew)
                    .await
            }
            None => Ok(()),
        }
    }

    /// Run `op` with a current token; an unattended server that refuses
    /// one gets a newly fetched one and `op` once more.
    async fn authorized<T, F, Fut>(&self, op: F) -> Result<T, McpError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        self.keep_token(false).await?;
        let first = op().await;
        if matches!(first, Err(McpError::AuthRequired { .. })) && self.unattended().is_some() {
            self.keep_token(true).await?;
            return self.noted(op().await).await;
        }
        self.noted(first).await
    }

    /// Report an authorization refusal to the pool's authorization state —
    /// a 401 takes the server's tokens away, a 403 asks for more scope.
    async fn noted<T>(&self, result: Result<T, McpError>) -> Result<T, McpError> {
        let Err(e) = result else {
            return result;
        };
        if let (Some(auth), Some(url)) = (&self.auth, crate::oauth::server_url(&self.config)) {
            match &e {
                McpError::AuthRequired { challenge, .. } => {
                    auth.note_unauthorized(
                        &self.server_name,
                        &url,
                        challenge.as_deref(),
                        self.authorized.load(Ordering::SeqCst),
                    )
                    .await
                }
                McpError::InsufficientScope { challenge, .. } => {
                    auth.note_insufficient_scope(&self.server_name, &url, challenge)
                }
                _ => {}
            }
        }
        Err(e)
    }

    async fn list_tools_inner(&self) -> Result<Vec<McpToolMeta>, McpError> {
        let svc = self.service().await?;
        let list = self
            .request(
                "list_tools",
                self.request_limit(),
                &svc,
                svc.list_tools(Default::default()),
            )
            .await?;
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
        let svc = self.service().await?;
        let req = CallToolRequestParams::new(tool_name.to_string()).with_arguments(args);
        let result = self
            .request(
                "call_tool",
                self.config.timeouts().tool(),
                &svc,
                svc.call_tool(req),
            )
            .await?;

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

    async fn list_resources_inner(&self) -> Result<Vec<McpResourceMeta>, McpError> {
        let svc = self.service().await?;
        // rmcp 的 list_all_resources 自动分页直到拿完
        let resources = self
            .request(
                "list_resources",
                self.request_limit(),
                &svc,
                svc.list_all_resources(),
            )
            .await?;
        Ok(resources
            .into_iter()
            .map(|r| McpResourceMeta {
                uri: r.uri,
                name: r.name,
                description: r.description,
                mime_type: r.mime_type,
            })
            .collect())
    }

    async fn read_resource_inner(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
        use rmcp::model::ReadResourceRequestParams;
        let svc = self.service().await?;
        let result = self
            .request(
                "read_resource",
                self.request_limit(),
                &svc,
                svc.read_resource(ReadResourceRequestParams::new(uri)),
            )
            .await?;
        Ok(result.contents.into_iter().map(resource_content).collect())
    }

    async fn list_prompts_inner(&self) -> Result<Vec<McpPromptMeta>, McpError> {
        let svc = self.service().await?;
        let prompts = self
            .request(
                "list_prompts",
                self.request_limit(),
                &svc,
                svc.list_all_prompts(),
            )
            .await?;
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

    async fn get_prompt_inner(
        &self,
        prompt_name: &str,
        args: &std::collections::HashMap<String, String>,
    ) -> Result<String, McpError> {
        use rmcp::model::GetPromptRequestParams;

        let svc = self.service().await?;
        let mut params = GetPromptRequestParams::new(prompt_name);
        if !args.is_empty() {
            let mut json_args = rmcp::model::JsonObject::new();
            for (k, v) in args {
                json_args.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
            params = params.with_arguments(json_args);
        }

        let result = self
            .request(
                "get_prompt",
                self.request_limit(),
                &svc,
                svc.get_prompt(params),
            )
            .await?;

        Ok(render_prompt_result(result))
    }
}

#[async_trait]
impl McpClient for StdioMcpClient {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    fn transport_kind(&self) -> &'static str {
        self.config.transport_kind()
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    fn is_connected(&self) -> bool {
        StdioMcpClient::is_connected(self)
    }

    async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError> {
        self.read_again_if_dropped(|| self.list_tools_inner()).await
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<McpCallResult, McpError> {
        let result = self
            .authorized(|| self.call_tool_inner(tool_name, args.clone()))
            .await;
        if let Err(e @ (McpError::ConnectionLost { .. } | McpError::Timeout { .. })) = &result {
            warn!(server = %self.server_name, tool = tool_name, error = %e, "MCP tool call failed; not repeated");
        }
        result
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceMeta>, McpError> {
        self.read_again_if_dropped(|| self.list_resources_inner())
            .await
    }

    async fn read_resource(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
        self.read_again_if_dropped(|| self.read_resource_inner(uri))
            .await
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptMeta>, McpError> {
        self.read_again_if_dropped(|| self.list_prompts_inner())
            .await
    }

    async fn get_prompt(
        &self,
        prompt_name: &str,
        args: &std::collections::HashMap<String, String>,
    ) -> Result<String, McpError> {
        self.read_again_if_dropped(|| self.get_prompt_inner(prompt_name, args))
            .await
    }
}

/// 把 `prompts/get` 的结果渲染成给 slash command 用的纯文本：拼接所有消息的
/// 文本内容，非文本 content（图片/资源）用占位说明——`execute_prompt()` 的调
/// 用方期望一个可以直接当 prompt body 用的字符串，不是结构化多模态内容。
/// 消息为空时退回 `description`（服务器可能只填了这一个字段）。
fn render_prompt_result(result: rmcp::model::GetPromptResult) -> String {
    use rmcp::model::ContentBlock;

    let rendered = result
        .messages
        .iter()
        .map(|m| match &m.content {
            ContentBlock::Text(t) => t.text.clone(),
            ContentBlock::Image(_) => "[image content omitted]".to_string(),
            ContentBlock::Audio(_) => "[audio content omitted]".to_string(),
            ContentBlock::Resource(_) => "[embedded resource omitted]".to_string(),
            ContentBlock::ResourceLink(link) => format!("[resource link: {}]", link.uri),
            _ => "[content omitted]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    if rendered.is_empty() {
        result.description.unwrap_or_default()
    } else {
        rendered
    }
}

type Service = rmcp::service::RunningService<rmcp::RoleClient, crate::notify::NotifyingHandler>;

fn is_open(svc: &Service) -> bool {
    !svc.is_closed() && !svc.peer().is_transport_closed()
}

async fn close(svc: Arc<Service>) {
    match Arc::try_unwrap(svc) {
        Ok(svc) => {
            let _ = svc.cancel().await;
        }
        Err(shared) => shared.cancellation_token().cancel(),
    }
}

/// The connection went away with the request on it, so whether the server
/// got the request cannot be told.
fn connection_dropped(e: &rmcp::ServiceError) -> bool {
    matches!(
        e,
        rmcp::ServiceError::TransportClosed | rmcp::ServiceError::TransportSend(_)
    )
}

struct Spawned {
    service: Service,
    /// The connection sends an OAuth token.
    authorized: bool,
}

impl From<Service> for Spawned {
    fn from(service: Service) -> Self {
        Self {
            service,
            authorized: false,
        }
    }
}

/// [`spawn_service`] within the server's startup limit. A server that does
/// not finish its handshake in time is stopped: dropping the half-made
/// connection kills a stdio server's process group.
async fn spawn_service_within(
    server_name: &str,
    config: &McpServerConfig,
    sink: Option<crate::notify::NotificationSink>,
    env_policy: &base::interface::exec::local::SubprocessEnv,
    auth: Option<&crate::oauth::McpAuth>,
) -> Result<Spawned, McpError> {
    let limit = config.timeouts().startup();
    tokio::time::timeout(
        limit,
        spawn_service(server_name, config, sink, env_policy, auth),
    )
    .await
    .unwrap_or(Err(McpError::Timeout {
        name: server_name.into(),
        op: "initialize",
        after: limit,
    }))
}

/// 根据 server config 起一个新 rmcp service（连过 + 握手完成）。
async fn spawn_service(
    server_name: &str,
    config: &McpServerConfig,
    sink: Option<crate::notify::NotificationSink>,
    env_policy: &base::interface::exec::local::SubprocessEnv,
    auth: Option<&crate::oauth::McpAuth>,
) -> Result<Spawned, McpError> {
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
            spawn_stdio_service(server_name, &command, &args, &env, sink, env_policy)
                .await
                .map(Spawned::from)
        }
        McpServerConfig::StreamableHttp { url, headers, .. }
        | McpServerConfig::Sse { url, headers, .. } => {
            let url = parsed_url(server_name, &crate::config::expand_env_vars(url))?;
            let headers: std::collections::HashMap<String, String> = headers
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            let auth = auth
                .filter(|_| config.oauth_capable())
                .map(|auth| (auth, config.oauth()));
            spawn_streamable_http_service(server_name, &url, &headers, auth, sink).await
        }
        McpServerConfig::WebSocket { url, headers, .. } => {
            let url = parsed_url(server_name, &crate::config::expand_env_vars(url))?;
            let headers: std::collections::HashMap<String, String> = headers
                .iter()
                .map(|(k, v)| (k.clone(), crate::config::expand_env_vars(v)))
                .collect();
            spawn_websocket_service(server_name, &url, &headers, sink)
                .await
                .map(Spawned::from)
        }
    }
}

/// `url`, unless it does not parse — which no retry would change.
fn parsed_url(server_name: &str, url: &str) -> Result<String, McpError> {
    match url::Url::parse(url) {
        Ok(_) => Ok(url.to_string()),
        Err(e) => Err(McpError::ConnectFailed {
            name: server_name.into(),
            source: anyhow::Error::new(e).context(format!("invalid url {url:?}")),
        }),
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

    let handler = crate::notify::NotifyingHandler::new(server_name, sink);
    #[cfg(unix)]
    let handler = match transport.id() {
        Some(pid) => handler.keeping(base::interface::exec::local::orphans::list(pid as i32)),
        None => handler,
    };
    handler
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
) -> rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig {
    rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url)
        .custom_headers(wire_headers(server_name, headers))
}

/// StreamableHttp transport over rmcp's reqwest client; configured headers go
/// on every request. A server that can use OAuth goes through rmcp's
/// `AuthClient`, which attaches the stored token, refreshes it when it
/// expires and retries a 401 once after a refresh. Without a token the
/// request goes out bare, so a server that does not ask for authorization
/// works as before, and one that does surfaces as [`McpError::AuthRequired`].
async fn spawn_streamable_http_service(
    server_name: &str,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    auth: Option<(
        &crate::oauth::McpAuth,
        Option<&crate::config::McpOAuthConfig>,
    )>,
    sink: Option<crate::notify::NotificationSink>,
) -> Result<Spawned, McpError> {
    use rmcp::transport::StreamableHttpClientTransport;

    let http = crate::oauth::http::transport_client(url);
    let transport_config = http_transport_config(server_name, url, headers);
    let handler = crate::notify::NotifyingHandler::new(server_name, sink);
    let Some((auth, oauth)) = auth else {
        let transport = StreamableHttpClientTransport::with_client(http, transport_config);
        return handler
            .serve(transport)
            .await
            .map(Spawned::from)
            .map_err(|e| init_error(server_name, e));
    };

    let (manager, authorized) = auth.transport_manager(server_name, url, oauth).await?;
    let client = rmcp::transport::AuthClient::new(http, manager);
    let transport = StreamableHttpClientTransport::with_client(client, transport_config);
    match handler.serve(transport).await {
        Ok(service) => {
            auth.connected(server_name, url, authorized);
            Ok(Spawned {
                service,
                authorized,
            })
        }
        Err(e) => {
            let e = init_error(server_name, e);
            match &e {
                McpError::AuthRequired { challenge, .. } => {
                    auth.note_unauthorized(server_name, url, challenge.as_deref(), authorized)
                        .await
                }
                McpError::InsufficientScope { challenge, .. } => {
                    auth.note_insufficient_scope(server_name, url, challenge)
                }
                _ => {}
            }
            Err(e)
        }
    }
}

/// The 401 / 403 `insufficient_scope` somewhere in an rmcp error's chain.
fn auth_failure(server_name: &str, top: &(dyn std::error::Error + 'static)) -> Option<McpError> {
    use rmcp::transport::auth::AuthError;
    use rmcp::transport::streamable_http_client::{AuthRequiredError, InsufficientScopeError};
    let mut source = Some(top);
    while let Some(e) = source {
        if let Some(required) = e.downcast_ref::<AuthRequiredError>() {
            return Some(McpError::AuthRequired {
                name: server_name.into(),
                challenge: Some(required.www_authenticate_header.clone()),
            });
        }
        if let Some(scope) = e.downcast_ref::<InsufficientScopeError>() {
            return Some(McpError::InsufficientScope {
                name: server_name.into(),
                challenge: scope.www_authenticate_header.clone(),
                scope: scope.required_scope.clone(),
            });
        }
        if matches!(
            e.downcast_ref::<AuthError>(),
            Some(AuthError::AuthorizationRequired)
        ) {
            return Some(McpError::AuthRequired {
                name: server_name.into(),
                challenge: None,
            });
        }
        source = e.source();
    }
    None
}

fn init_error(server_name: &str, e: rmcp::service::ClientInitializeError) -> McpError {
    use rmcp::service::ClientInitializeError;
    if let ClientInitializeError::TransportError { error, .. } = &e {
        if let Some(auth) = auth_failure(server_name, error.error.as_ref()) {
            return auth;
        }
    }
    if e.is_authorization_required() {
        return McpError::AuthRequired {
            name: server_name.into(),
            challenge: e.auth_challenge().map(str::to_string),
        };
    }
    McpError::ConnectFailed {
        name: server_name.into(),
        source: anyhow::anyhow!("{e}"),
    }
}

fn service_error(server_name: &str, op: &str, e: rmcp::ServiceError) -> McpError {
    if let rmcp::ServiceError::TransportSend(transport) = &e {
        if let Some(auth) = auth_failure(server_name, transport.error.as_ref()) {
            return auth;
        }
    }
    McpError::RmcpService(format!("{op}: {e}"))
}

/// 把 rmcp::model::ContentBlock 转成我们的 McpContent。
/// A blob is an image only when its MIME type says so; any other is named,
/// not carried, since nothing downstream can show it.
fn resource_content(c: rmcp::model::ResourceContents) -> McpContent {
    match c {
        rmcp::model::ResourceContents::TextResourceContents { text, .. } => McpContent::Text(text),
        rmcp::model::ResourceContents::BlobResourceContents {
            blob, mime_type, ..
        } => match mime_type {
            Some(mime) if mime.to_ascii_lowercase().starts_with("image/") => McpContent::Image {
                data: blob,
                media_type: mime,
            },
            mime => {
                let padding = blob.bytes().rev().take_while(|b| *b == b'=').count();
                let bytes = (blob.len() / 4 * 3).saturating_sub(padding);
                McpContent::Text(format!(
                    "[binary resource: {}, {bytes} bytes]",
                    mime.as_deref().unwrap_or("application/octet-stream")
                ))
            }
        },
        other => McpContent::Other(serde_json::to_value(other).unwrap_or(serde_json::Value::Null)),
    }
}

fn rmcp_content_to_ours(c: &rmcp::model::ContentBlock) -> McpContent {
    match c {
        rmcp::model::ContentBlock::Text(t) => McpContent::Text(t.text.to_string()),
        rmcp::model::ContentBlock::Image(i) => McpContent::Image {
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
    spawn_streamable_http_service(server_name, &http_url, headers, None, sink)
        .await
        .map(|spawned| spawned.service)
}

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

    /// `/bin/sh` speaking just enough MCP: it answers the handshake and
    /// `tools/list`, answers a call to `ping`, and never answers a call to
    /// anything else.
    #[cfg(unix)]
    fn sh_server(dir: &std::path::Path, timeouts: crate::config::McpTimeouts) -> McpServerConfig {
        let script = dir.join("server.sh");
        std::fs::write(
            &script,
            r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"sh","version":"0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[]}}\n' "$id" ;;
    *'"name":"ping"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id" ;;
  esac
done
"#,
        )
        .unwrap();
        McpServerConfig::Stdio {
            command: "/bin/sh".into(),
            args: vec![script.to_string_lossy().into_owned()],
            env: HashMap::new(),
            scope: None,
            timeouts,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_never_finishes_its_handshake_is_given_up_on_in_time() {
        let cfg = McpServerConfig::Stdio {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "while IFS= read -r line; do :; done".into()],
            env: HashMap::new(),
            scope: None,
            timeouts: crate::config::McpTimeouts {
                startup_timeout_ms: Some(300),
                ..Default::default()
            },
        };
        let started = Instant::now();
        let err = StdioMcpClient::connect("mute", &cfg).await.err().unwrap();
        assert!(
            matches!(
                err,
                McpError::Timeout {
                    op: "initialize",
                    ..
                }
            ),
            "{err}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_tool_call_past_its_limit_fails_and_the_connection_stays() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = sh_server(
            dir.path(),
            crate::config::McpTimeouts {
                tool_timeout_ms: Some(300),
                ..Default::default()
            },
        );
        let client = StdioMcpClient::connect("sh", &cfg).await.unwrap();
        let err = client
            .call_tool("hang", Default::default())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                McpError::Timeout {
                    op: "call_tool",
                    ..
                }
            ),
            "{err}"
        );
        assert!(client.is_connected());
        let pong = client.call_tool("ping", Default::default()).await.unwrap();
        assert!(matches!(&pong.content[..], [McpContent::Text(t)] if t == "pong"));
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_died_is_seen_as_disconnected_and_reconnected_for_the_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = sh_server(dir.path(), Default::default());
        let client = StdioMcpClient::connect("sh", &cfg).await.unwrap();
        let svc = client.current().await.unwrap();
        svc.cancellation_token().cancel();
        for _ in 0..100 {
            if !client.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!client.is_connected());
        let pong = client.call_tool("ping", Default::default()).await.unwrap();
        assert!(matches!(&pong.content[..], [McpContent::Text(t)] if t == "pong"));
        assert!(client.is_connected());
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_reconnected_server_is_announced_as_changed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = sh_server(dir.path(), Default::default());
        let announcements = crate::notify::Announcements::new();
        let client = StdioMcpClient::connect_with_sink("sh", &cfg, Some(announcements.sink()))
            .await
            .unwrap();
        assert_eq!(announcements.of("sh"), Default::default());

        client
            .current()
            .await
            .unwrap()
            .cancellation_token()
            .cancel();
        for _ in 0..100 {
            if !client.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        client.call_tool("ping", Default::default()).await.unwrap();
        let after = announcements.of("sh");
        assert_eq!((after.tools, after.prompts), (1, 1));
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stdio_servers_process_group_is_listed_while_it_runs() {
        use base::interface::exec::local::orphans;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let McpServerConfig::Stdio { args, .. } = sh_server(dir.path(), Default::default()) else {
            unreachable!()
        };
        let cfg = McpServerConfig::Stdio {
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "echo $$$$ > '{}'; exec /bin/sh '{}'",
                    pid_file.display(),
                    args[0]
                ),
            ],
            env: HashMap::new(),
            scope: None,
            timeouts: Default::default(),
        };
        let client = StdioMcpClient::connect("sh", &cfg).await.unwrap();
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(orphans::listed().contains(&pid), "{:?}", orphans::listed());

        client.shutdown().await;
        for _ in 0..100 {
            if !orphans::listed().contains(&pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!orphans::listed().contains(&pid), "{:?}", orphans::listed());
    }

    #[test]
    fn only_an_image_blob_is_an_image() {
        let blob = |mime: Option<&str>| {
            serde_json::from_value::<rmcp::model::ResourceContents>(serde_json::json!({
                "uri": "x://y", "blob": "aGVsbG8=", "mimeType": mime,
            }))
            .unwrap()
        };
        assert!(matches!(
            resource_content(blob(Some("image/png"))),
            McpContent::Image { media_type, .. } if media_type == "image/png"
        ));
        assert!(matches!(
            resource_content(blob(Some("application/pdf"))),
            McpContent::Text(t) if t == "[binary resource: application/pdf, 5 bytes]"
        ));
        assert!(matches!(
            resource_content(blob(None)),
            McpContent::Text(t) if t == "[binary resource: application/octet-stream, 5 bytes]"
        ));
    }

    #[tokio::test]
    async fn a_command_that_is_not_there_or_a_url_that_does_not_parse_is_not_retried() {
        let missing = McpServerConfig::Stdio {
            command: "/nonexistent/agix-mcp-server".into(),
            args: Vec::new(),
            env: HashMap::new(),
            scope: None,
            timeouts: Default::default(),
        };
        let bad_url = McpServerConfig::StreamableHttp {
            url: "not a url".into(),
            headers: HashMap::new(),
            oauth: None,
            scope: None,
            timeouts: Default::default(),
        };
        let started = Instant::now();
        let manager = crate::manager::McpManager::connect_all(HashMap::from([
            ("missing".to_string(), missing),
            ("bad_url".to_string(), bad_url),
        ]))
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "retried: took {:?}",
            started.elapsed()
        );
        let mut failed: Vec<&str> = manager.failed().iter().map(|f| f.name.as_str()).collect();
        failed.sort();
        assert_eq!(failed, ["bad_url", "missing"]);
    }

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
            timeouts: Default::default(),
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
    fn only_a_connection_that_went_away_counts_as_dropped() {
        assert!(connection_dropped(&rmcp::ServiceError::TransportClosed));
        assert!(!connection_dropped(&rmcp::ServiceError::Timeout {
            timeout: Duration::from_secs(1)
        }));
        assert!(!connection_dropped(&rmcp::ServiceError::UnexpectedResponse));
        assert!(!connection_dropped(&rmcp::ServiceError::Cancelled {
            reason: None
        }));
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
                timeouts: Default::default(),
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            auth: None,
            authorized: false.into(),
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
                oauth: None,
                scope: None,
                timeouts: Default::default(),
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            auth: None,
            authorized: false.into(),
        };
        assert_eq!(client.transport_kind(), "streamable_http");
    }

    #[tokio::test]
    async fn get_prompt_errors_when_the_server_cannot_be_reached() {
        // `get_prompt` 和其他真实方法（`list_prompts`/`read_resource`）一样，
        // 连不上时报错，而不是伪装成功。
        let client = StdioMcpClient {
            server_name: "x".into(),
            config: McpServerConfig::Stdio {
                command: "echo".into(),
                args: Vec::new(),
                env: HashMap::new(),
                scope: None,
                timeouts: Default::default(),
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            auth: None,
            authorized: false.into(),
        };
        let err = client
            .get_prompt("some-prompt", &HashMap::new())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                McpError::NotConnected { .. } | McpError::ConnectFailed { .. }
            ),
            "err: {err:?}"
        );
    }

    fn text_message(role: rmcp::model::Role, text: &str) -> rmcp::model::PromptMessage {
        rmcp::model::PromptMessage::new_text(role, text)
    }

    #[test]
    fn render_prompt_result_joins_text_messages() {
        use rmcp::model::Role;
        let result = rmcp::model::GetPromptResult::new(vec![
            text_message(Role::User, "first"),
            text_message(Role::Assistant, "second"),
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
        use rmcp::model::{ContentBlock, PromptMessage, Resource, Role};
        let resource = Resource::new("file:///tmp/x.txt", "x.txt");
        let result = rmcp::model::GetPromptResult::new(vec![PromptMessage::new(
            Role::User,
            ContentBlock::resource_link(resource),
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
                timeouts: Default::default(),
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            auth: None,
            authorized: false.into(),
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
                timeouts: Default::default(),
            },
            instructions: None,
            inner: tokio::sync::Mutex::new(None),
            last_reconnect_at: tokio::sync::Mutex::new(None),
            consecutive_failures: AtomicU32::new(0),
            sink: None,
            env_policy: Default::default(),
            auth: None,
            authorized: false.into(),
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
                    timeouts: Default::default(),
                },
                instructions: None,
                inner: tokio::sync::Mutex::new(None),
                last_reconnect_at: tokio::sync::Mutex::new(None),
                consecutive_failures: AtomicU32::new(0),
                sink: None,
                env_policy: Default::default(),
                auth: None,
                authorized: false.into(),
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
                    timeouts: Default::default(),
                },
                instructions: Some("test".into()),
                inner: tokio::sync::Mutex::new(None),
                last_reconnect_at: tokio::sync::Mutex::new(None),
                consecutive_failures: AtomicU32::new(0),
                sink: None,
                env_policy: Default::default(),
                auth: None,
                authorized: false.into(),
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
        let config = http_transport_config("s", "http://h/mcp", &headers);
        assert_eq!(
            header(&config.custom_headers, "authorization"),
            "Bearer static"
        );
        assert_eq!(header(&config.custom_headers, "x-tenant"), "acme");
        assert!(
            config.auth_header.is_none(),
            "rmcp would prefix it with Bearer again"
        );
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
