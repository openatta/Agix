//! `McpManager` —— 串联 settings / connect / adapter 给 engine 用的入口。
//!
//! 用法：CLI 启动时把 settings.mcp_servers 喂给 `connect_all`；返回的 manager 把
//! 全部 server 的 tools 摊平成 `Vec<Arc<dyn Tool>>` 加进 ToolRegistry。

use crate::adapter::McpToolAdapter;
use crate::client::{McpClient, McpClientHandle};
use crate::config::McpServerConfig;
use crate::connect::StdioMcpClient;
use crate::error::McpError;
use crate::output_cache::McpOutputCache;
use crate::prompts::{
    parse_prompt_args, render_prompt_failure, render_prompt_success, split_command_name,
    PromptInvocation,
};
use crate::resources::{
    find_resource_refs, flatten_resource_content, render_resource_block, render_resource_error,
    wrap_resource_blocks, MAX_RESOURCE_BYTES, MAX_TOTAL_RESOURCE_BYTES,
};
use base::context::McpServerInstruction;
use base::text::truncate_at_char_boundary;
use base::tool::Tool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::task::JoinSet;
use tracing::{info, warn};

/// Longest instruction text kept from any one MCP server when assembling the
/// system prompt. `connect.rs` already clamps what a server may hand us at
/// initialize time, but nothing bounded the *assembled* section, so N verbose
/// servers could still crowd out the real system prompt.
pub const MAX_SERVER_INSTRUCTION_BYTES: usize = 4_000;

/// Longest combined instruction text across all servers.
pub const MAX_TOTAL_SERVER_INSTRUCTION_BYTES: usize = 12_000;

/// Appended in place of whatever a cap removed, so the model knows the text
/// it is reading is incomplete rather than the server's whole word.
const INSTRUCTION_TRUNCATION_NOTE: &str =
    "\n[... truncated: MCP server instructions exceeded the system-prompt budget]";

/// Callback invoked when an MCP tool result contains an elicitation URL
/// (e.g. `mcp://` or `elicitation://` protocol). The first argument is the
/// server name, the second is the elicitation URL.
pub type ElicitationCallback = Arc<dyn Fn(String, String) + Send + Sync>;

/// An MCP server prompt exposed as a slash command.
///
/// Defined in [`crate::prompts`] alongside the argument mapping/validation it
/// is used with; re-exported here because `McpManager` is where callers
/// already look for it.
pub use crate::prompts::McpPromptEntry;

/// MCP server connection state (`Connected`, `Failed`, `NeedsAuth`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerState {
    Connected,
    Failed,
    NeedsAuth,
    Pending {
        reconnect_attempt: u32,
        max_reconnect_attempts: u32,
    },
    Disabled,
}

/// MCP notification from a server (`notifications/tools/list_changed` etc.).
#[derive(Debug, Clone)]
pub enum McpNotification {
    /// Server's tool list changed (tools added/removed/updated).
    ToolListChanged { server: String },
    /// Server's resource list changed.
    ResourceListChanged { server: String },
    /// Server's prompt list changed.
    PromptListChanged { server: String },
    /// Custom notification (unknown method).
    Custom { server: String, method: String },
}

/// Handler for MCP server notifications.
/// Register with [`McpManager::register_notification_handler`].
#[async_trait::async_trait]
pub trait McpNotificationHandler: Send + Sync {
    /// Called when an MCP server sends a notification.
    async fn on_mcp_notification(&self, notification: McpNotification);
}

pub struct McpManager {
    clients: Vec<McpClientHandle>,
    adapters: Vec<Arc<dyn Tool>>,
    prompts: Vec<McpPromptEntry>,
    /// Per-server connection states (index-aligned with clients).
    #[allow(dead_code)]
    server_states: Vec<(String, McpServerState)>,
    /// Registered notification handlers.
    notification_handlers: Vec<Box<dyn McpNotificationHandler>>,
    /// Notifications servers have sent that nothing has acted on yet — see
    /// `crate::notify::NotificationQueue`. Shared with every connected
    /// client, since the reaction (re-asking for tools) is manager-wide.
    notifications: Arc<crate::notify::NotificationQueue>,
    /// P2-2: MCP notification channel allowlist — only notifications from
    /// servers whose names appear in this set are dispatched to handlers.
    /// Empty = allow all.
    pub notification_allowlist: Option<std::collections::HashSet<String>>,
    /// P2-2: Per-server tool permission overrides. Maps server_name → allowed_tool_names.
    /// If a server has an entry, only listed tools are callable. Empty vec = allow all.
    pub tool_permissions: std::collections::HashMap<String, Vec<String>>,
    /// Shared MCP output cache.
    output_cache: Arc<Mutex<McpOutputCache>>,
    /// Callback invoked when an MCP tool result contains an elicitation URL.
    /// Set by the CLI/engine to wire into the hook system.
    ///
    /// Shared cell, not a plain `Option` — every `McpToolAdapter` this
    /// manager builds aliases the *same* cell (see that struct's field doc
    /// comment). `set_elicitation_callback` therefore takes effect for
    /// already-built adapters too, which matters because `refresh_tools()`
    /// commonly runs before an embedder gets a chance to call it.
    elicitation_callback: Arc<Mutex<Option<ElicitationCallback>>>,
    /// What a stdio server this manager starts gets for an environment.
    subprocess_env: base::interface::exec::local::SubprocessEnv,
    /// The owning pool's OAuth state, handed to every connection.
    oauth: Option<Arc<crate::oauth::McpOAuth>>,
}

impl McpManager {
    /// Empty/default instance with no state.
    pub fn empty() -> Self {
        Self {
            clients: Vec::new(),
            adapters: Vec::new(),
            prompts: Vec::new(),
            server_states: Vec::new(),
            notification_handlers: Vec::new(),
            notifications: crate::notify::NotificationQueue::new(),
            notification_allowlist: None,
            tool_permissions: std::collections::HashMap::new(),
            output_cache: Arc::new(Mutex::new(McpOutputCache::new())),
            elicitation_callback: Arc::new(Mutex::new(None)),
            subprocess_env: base::interface::exec::local::SubprocessEnv::default(),
            oauth: None,
        }
    }

    /// Register a notification handler. Handlers are called in registration order
    /// when any MCP server sends a notification.
    pub fn register_notification_handler(&mut self, handler: Box<dyn McpNotificationHandler>) {
        self.notification_handlers.push(handler);
    }

    /// Dispatch a notification to all registered handlers, respecting the
    /// notification allowlist. Servers not in the allowlist are silently skipped.
    /// Handlers that panic are caught and skipped.
    pub async fn dispatch_notification(&self, notification: McpNotification) {
        // P2-2: Check notification allowlist
        let server = match &notification {
            McpNotification::ToolListChanged { server }
            | McpNotification::ResourceListChanged { server }
            | McpNotification::PromptListChanged { server }
            | McpNotification::Custom { server, .. } => server,
        };
        if let Some(ref allowlist) = self.notification_allowlist {
            if !allowlist.contains(server) {
                tracing::debug!(
                    server = %server,
                    "MCP notification blocked: server not in allowlist"
                );
                return;
            }
        }
        for handler in &self.notification_handlers {
            handler.on_mcp_notification(notification.clone()).await;
        }
    }

    /// P2-2: Check whether a specific MCP tool is allowed based on tool_permissions.
    /// Returns true if the tool is allowed, false if blocked by per-server rules.
    pub fn is_tool_allowed(&self, server: &str, tool_name: &str) -> bool {
        match self.tool_permissions.get(server) {
            None => true,                                // No rules → allow all
            Some(allowed) if allowed.is_empty() => true, // Empty list → allow all
            Some(allowed) => allowed.iter().any(|t| t == tool_name),
        }
    }

    /// P2-2: Set the notification allowlist. Pass None to allow all servers.
    pub fn set_notification_allowlist(&mut self, servers: Option<Vec<String>>) {
        self.notification_allowlist = servers.map(|v| v.into_iter().collect());
    }

    /// P2-2: Set tool permissions for a specific server. Pass empty vec to allow all.
    pub fn set_tool_permissions(&mut self, server: &str, allowed: Vec<String>) {
        self.tool_permissions.insert(server.to_string(), allowed);
    }

    /// Register an elicitation callback. When an MCP tool result contains an
    /// elicitation URL (mcp:// or elicitation://), the adapter calls this
    /// callback which fires the `Elicitation` hook event.
    ///
    /// Writes into the shared cell every current and future adapter reads
    /// from — safe to call before or after `connect_all`/`refresh_tools`.
    pub fn set_elicitation_callback(&mut self, cb: ElicitationCallback) {
        *self
            .elicitation_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(cb);
    }

    /// Fire the Elicitation hook event. Called when an MCP tool returns an
    /// elicitation URL. This notifies the hook system (and any registered
    /// hooks) that user attention is needed.
    pub fn on_elicitation(&self, server_name: &str, url: &str) {
        let cb = self
            .elicitation_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(cb) = cb {
            cb(server_name.to_string(), url.to_string());
        }
    }

    /// 连所有 server，list_tools 完成后构造 adapters。
    /// 使用并发连接——最多 3 个本地 stdio server 同时启动，远程 server 可更多。
    /// 单个 server 失败不整体失败 —— warn + skip。
    pub async fn connect_all(servers: HashMap<String, McpServerConfig>) -> Self {
        Self::connect_all_impl(
            servers,
            None,
            base::interface::exec::local::SubprocessEnv::default(),
            None,
        )
        .await
    }

    /// `connect_all`, with stdio servers started under `env` — the
    /// `sandbox.subprocess_env` of the settings they were declared in.
    pub async fn connect_all_with_env(
        servers: HashMap<String, McpServerConfig>,
        env: base::interface::exec::local::SubprocessEnv,
    ) -> Self {
        Self::connect_all_impl(servers, None, env, None).await
    }

    /// `connect_all_with_env`, taking bearer tokens for servers with an
    /// `oauth_provider` from `oauth` — the owning pool's, so pools in one
    /// process never share tokens.
    pub async fn connect_all_with_oauth(
        servers: HashMap<String, McpServerConfig>,
        env: base::interface::exec::local::SubprocessEnv,
        oauth: Arc<crate::oauth::McpOAuth>,
    ) -> Self {
        oauth.remember_servers(&servers);
        Self::connect_all_impl(servers, None, env, Some(oauth)).await
    }

    /// Start every stdio server this manager adds later under `env`.
    pub fn with_subprocess_env(mut self, env: base::interface::exec::local::SubprocessEnv) -> Self {
        self.subprocess_env = env;
        self
    }

    /// 连所有 server，并传入一个 Optional 的 elicitation callback。
    /// 当 MCP tool 返回 elicitation URL 时，adapter 会调用此 callback。
    pub async fn connect_all_with_callback(
        servers: HashMap<String, McpServerConfig>,
        callback: ElicitationCallback,
    ) -> Self {
        Self::connect_all_impl(
            servers,
            Some(callback),
            base::interface::exec::local::SubprocessEnv::default(),
            None,
        )
        .await
    }

    /// Internal implementation shared by `connect_all` and
    /// `connect_all_with_callback`.
    async fn connect_all_impl(
        servers: HashMap<String, McpServerConfig>,
        elicitation_cb: Option<ElicitationCallback>,
        subprocess_env: base::interface::exec::local::SubprocessEnv,
        oauth: Option<Arc<crate::oauth::McpOAuth>>,
    ) -> Self {
        // Shared cell (see the field doc comment on `elicitation_callback`)
        // threaded through every spawned connect task and every adapter it
        // builds, then reused as-is for this manager's own field — so a
        // later `set_elicitation_callback` call reaches adapters built here.
        let elicitation_cell = Arc::new(Mutex::new(elicitation_cb));
        let cache = Arc::new(Mutex::new(McpOutputCache::new()));
        // Shared with every client this builds, and kept as this manager's
        // own field, so a server that announces a tool change reaches the
        // same queue the manager later drains.
        let notifications = crate::notify::NotificationQueue::new();
        let mut clients: Vec<McpClientHandle> = Vec::new();
        let mut adapters: Vec<Arc<dyn Tool>> = Vec::new();
        let mut set = JoinSet::new();
        let max_concurrent = 3usize; // limit concurrent spawns

        type ConnectOutput =
            Result<(String, McpClientHandle, Vec<Arc<dyn Tool>>), (String, McpError)>;

        // Process a JoinSet result: success → collect into clients/adapters;
        // errors are non-fatal (warn + skip).
        let handle_result =
            |clients: &mut Vec<McpClientHandle>,
             adapters: &mut Vec<Arc<dyn Tool>>,
             result: Result<ConnectOutput, tokio::task::JoinError>| {
                match result {
                    Ok(Ok((_name, client, server_adapters))) => {
                        clients.push(client);
                        adapters.extend(server_adapters);
                    }
                    Ok(Err((name, e))) => {
                        warn!(server = %name, error = %e, "MCP server connect failed; skipping");
                    }
                    Err(join_err) => {
                        warn!(?join_err, "MCP connect task panicked; skipping");
                    }
                }
            };

        for (name, cfg) in servers {
            // Wait if we're at the concurrency limit
            while set.len() >= max_concurrent {
                if let Some(result) = set.join_next().await {
                    handle_result(&mut clients, &mut adapters, result);
                }
            }

            let cache_for_spawn = cache.clone();
            let cell_for_spawn = elicitation_cell.clone();
            let queue_for_spawn = notifications.clone();
            let env_for_spawn = subprocess_env.clone();
            let oauth_for_spawn = oauth.clone();
            set.spawn(async move {
                match Self::connect_with_retry(
                    &name,
                    &cfg,
                    cache_for_spawn,
                    cell_for_spawn,
                    queue_for_spawn,
                    &env_for_spawn,
                    oauth_for_spawn,
                )
                .await
                {
                    Ok((client, server_adapters)) => {
                        info!(server = %name, n_tools = server_adapters.len(), "MCP server connected");
                        Ok((name, client, server_adapters))
                    }
                    Err(e) => Err((name, e)),
                }
            });
        }

        // Drain remaining
        while let Some(result) = set.join_next().await {
            handle_result(&mut clients, &mut adapters, result);
        }

        let manager = Self {
            clients,
            adapters,
            notifications,
            prompts: Vec::new(),
            server_states: Vec::new(),
            notification_handlers: Vec::new(),
            notification_allowlist: None,
            tool_permissions: std::collections::HashMap::new(),
            output_cache: cache,
            elicitation_callback: elicitation_cell,
            subprocess_env,
            oauth,
        };
        // Collect MCP prompts from connected servers (non-blocking: fail silently)
        manager.collect_prompts().await
    }

    async fn collect_prompts(mut self) -> Self {
        self.refresh_prompts().await;
        self
    }

    /// Re-run `prompts/list` on every connected server and rebuild the slash
    /// command catalog. Called once after `connect_all`; also useful after a
    /// `PromptListChanged` notification or when a manager was assembled from
    /// pre-built clients (`from_clients`, which cannot await).
    ///
    /// A server that fails to answer is skipped with a warning — its prompts
    /// simply don't appear as commands, the rest of the session is unaffected.
    pub async fn refresh_prompts(&mut self) {
        let mut prompts = Vec::new();
        for client in &self.clients {
            match client.list_prompts().await {
                Ok(server_prompts) => {
                    let n_prompts = server_prompts.len();
                    for p in server_prompts {
                        prompts.push(McpPromptEntry {
                            server: client.server_name().to_string(),
                            name: p.name.clone(),
                            description: p.description.unwrap_or_default(),
                            arguments: p.arguments,
                        });
                    }
                    info!(
                        server = %client.server_name(),
                        n_prompts,
                        "MCP prompts collected"
                    );
                }
                Err(e) => {
                    warn!(
                        server = %client.server_name(),
                        error = %e,
                        "MCP prompt collection failed; skipping"
                    );
                }
            }
        }
        self.prompts = prompts;
    }

    async fn connect_one(
        name: &str,
        cfg: &McpServerConfig,
        cache: Arc<Mutex<McpOutputCache>>,
        elicitation_cell: Arc<Mutex<Option<ElicitationCallback>>>,
        notifications: Arc<crate::notify::NotificationQueue>,
        subprocess_env: &base::interface::exec::local::SubprocessEnv,
        oauth: Option<Arc<crate::oauth::McpOAuth>>,
    ) -> Result<(McpClientHandle, Vec<Arc<dyn Tool>>), McpError> {
        // StdioMcpClient 现在支持 stdio + streamable_http 两种 config（spawn_service 里分派）
        let client = StdioMcpClient::connect_with_env(
            name,
            cfg,
            Some(notifications.sink()),
            subprocess_env.clone(),
            oauth,
        )
        .await?;
        let tools = client.list_tools().await?;
        let handle: McpClientHandle = client;
        let adapters: Vec<Arc<dyn Tool>> = tools
            .into_iter()
            .map(|meta| {
                let adapter = McpToolAdapter::with_cache(handle.clone(), meta, cache.clone())
                    .with_elicitation_callback(elicitation_cell.clone());
                Arc::new(adapter) as Arc<dyn Tool>
            })
            .collect();
        Ok((handle, adapters))
    }

    /// Connect to an MCP server with exponential backoff retry:
    /// MAX_RETRIES=5, INITIAL_DELAY=1s, MAX_DELAY=30s, ±25% jitter.
    async fn connect_with_retry(
        name: &str,
        cfg: &McpServerConfig,
        cache: Arc<Mutex<McpOutputCache>>,
        elicitation_cell: Arc<Mutex<Option<ElicitationCallback>>>,
        notifications: Arc<crate::notify::NotificationQueue>,
        subprocess_env: &base::interface::exec::local::SubprocessEnv,
        oauth: Option<Arc<crate::oauth::McpOAuth>>,
    ) -> Result<(McpClientHandle, Vec<Arc<dyn Tool>>), McpError> {
        const MAX_RETRIES: u32 = 5;
        const INITIAL_DELAY_MS: u64 = 1_000;
        const MAX_DELAY_MS: u64 = 30_000;

        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            match Self::connect_one(
                name,
                cfg,
                cache.clone(),
                elicitation_cell.clone(),
                notifications.clone(),
                subprocess_env,
                oauth.clone(),
            )
            .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    last_err = Some(e);
                    if attempt + 1 < MAX_RETRIES {
                        let delay = (INITIAL_DELAY_MS * 2u64.pow(attempt)).min(MAX_DELAY_MS);
                        // Deterministic jitter: use attempt+server hash to spread
                        // retries across servers within ±25% of the computed delay.
                        let jitter = delay / 4;
                        let offset = (attempt as u64 * 7 + name.len() as u64) % (2 * jitter + 1);
                        let jittered = delay - jitter + offset;
                        warn!(
                            server = %name,
                            attempt = attempt + 1,
                            max_retries = MAX_RETRIES,
                            delay_ms = jittered,
                            "MCP server connect failed; retrying with backoff"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(jittered)).await;
                    }
                }
            }
        }
        Err(last_err.unwrap())
    }

    fn install_server(
        &mut self,
        name: &str,
        client: McpClientHandle,
        server_adapters: Vec<Arc<dyn Tool>>,
    ) {
        let source = format!("mcp:{name}");
        self.clients.retain(|c| c.server_name() != name);
        self.adapters.retain(|a| a.source() != source);
        self.clients.push(client);
        self.adapters.extend(server_adapters);
    }

    /// Connect an MCP server after construction, replacing any server already
    /// connected under `name` (its tools included). On failure the existing
    /// connection, if any, stays and a warning is logged.
    pub async fn add_server(&mut self, name: &str, cfg: &McpServerConfig) {
        if let Some(oauth) = &self.oauth {
            oauth.remember_servers(&HashMap::from([(name.to_string(), cfg.clone())]));
        }
        let cb = self.elicitation_callback.clone();
        match Self::connect_one(
            name,
            cfg,
            self.output_cache.clone(),
            cb,
            self.notifications.clone(),
            &self.subprocess_env,
            self.oauth.clone(),
        )
        .await
        {
            Ok((client, server_adapters)) => {
                info!(
                    server = %name,
                    n_tools = server_adapters.len(),
                    "plugin MCP server connected"
                );
                self.install_server(name, client, server_adapters);
            }
            Err(e) => {
                warn!(server = %name, error = %e, "plugin MCP server connect failed; skipping");
            }
        }
    }

    /// 用注入测试 client 构造 manager（绕过真 connect）。
    #[doc(hidden)]
    pub fn from_clients(clients: Vec<McpClientHandle>) -> Self {
        // Can't await list_tools here; use empty adapters as placeholder.
        // Real usage should go through connect_all.
        Self {
            clients,
            adapters: Vec::new(),
            prompts: Vec::new(),
            server_states: Vec::new(),
            notification_handlers: Vec::new(),
            notifications: crate::notify::NotificationQueue::new(),
            notification_allowlist: None,
            tool_permissions: std::collections::HashMap::new(),
            output_cache: Arc::new(Mutex::new(McpOutputCache::new())),
            elicitation_callback: Arc::new(Mutex::new(None)),
            subprocess_env: base::interface::exec::local::SubprocessEnv::default(),
            oauth: None,
        }
    }

    /// 给 ToolRegistry 用的 adapter 列表。
    pub fn tool_adapters(&self) -> &[Arc<dyn Tool>] {
        &self.adapters
    }

    pub fn clients(&self) -> &[McpClientHandle] {
        &self.clients
    }

    /// Number of registered servers.
    pub fn server_count(&self) -> usize {
        self.clients.len()
    }

    pub fn tool_count(&self) -> usize {
        self.adapters.len()
    }

    /// MCP prompts collected from connected servers. Each entry maps to a
    /// slash command of the form `mcp__<server>__<prompt>`.
    pub fn all_prompts(&self) -> &[McpPromptEntry] {
        &self.prompts
    }

    /// Seed the prompt catalog without a `prompts/list` round trip. Exists so
    /// tests can build the "command list was collected while the server was
    /// up, then the server went away" state that `from_clients` (which cannot
    /// await) otherwise can't express.
    #[doc(hidden)]
    pub fn set_prompts_for_test(&mut self, prompts: Vec<McpPromptEntry>) {
        self.prompts = prompts;
    }

    /// Return a reference to the official MCP server registry.
    /// The registry provides a curated list of well-known MCP servers
    /// that users can discover and add to their configuration.
    pub fn official_registry() -> &'static crate::registry::OfficialRegistry {
        static REGISTRY: std::sync::OnceLock<crate::registry::OfficialRegistry> =
            std::sync::OnceLock::new();
        REGISTRY.get_or_init(crate::registry::OfficialRegistry::new)
    }

    /// Execute a named MCP prompt on the specified server. Returns the
    /// rendered text from the server's prompt handler, or an error message.
    pub async fn execute_prompt(
        &self,
        server: &str,
        prompt_name: &str,
        args: &std::collections::HashMap<String, String>,
    ) -> String {
        for client in &self.clients {
            if client.server_name() == server {
                match client.get_prompt(prompt_name, args).await {
                    Ok(content) => return content,
                    Err(e) => return format!("MCP prompt error: {e}"),
                }
            }
        }
        format!("MCP server '{server}' not found")
    }

    // ── Prompts as slash commands ──

    /// Look up a collected prompt by its slash command name
    /// (`mcp__<server>__<prompt>`).
    pub fn find_prompt_command(&self, command: &str) -> Option<&McpPromptEntry> {
        let (server, prompt) = split_command_name(command)?;
        self.prompts
            .iter()
            .find(|p| p.server == server && p.name == prompt)
    }

    /// Run an MCP prompt slash command end to end: map + validate the
    /// argument string against the prompt's declared arguments, call
    /// `prompts/get`, and render the result as content to push into the
    /// conversation.
    ///
    /// **Never fails.** An unknown command, a bad argument string, a server
    /// that is down, or a prompt handler that errors all come back as
    /// `is_error: true` with human-readable text — the turn continues and the
    /// model explains the failure.
    pub async fn invoke_prompt_command(&self, command: &str, raw_args: &str) -> PromptInvocation {
        let Some(entry) = self.find_prompt_command(command) else {
            return PromptInvocation {
                text: render_prompt_failure(
                    command,
                    "no connected MCP server exposes this prompt (the server may have \
                     disconnected since the command list was built).",
                ),
                is_error: true,
            };
        };

        let args = match parse_prompt_args(&entry.arguments, raw_args) {
            Ok(args) => args,
            Err(e) => {
                return PromptInvocation {
                    text: render_prompt_failure(command, &e.to_string()),
                    is_error: true,
                }
            }
        };

        let Some(client) = self
            .clients
            .iter()
            .find(|c| c.server_name() == entry.server)
        else {
            return PromptInvocation {
                text: render_prompt_failure(
                    command,
                    &format!("MCP server '{}' is no longer connected.", entry.server),
                ),
                is_error: true,
            };
        };

        match client.get_prompt(&entry.name, &args).await {
            Ok(body) => PromptInvocation {
                text: render_prompt_success(command, raw_args, &body),
                is_error: false,
            },
            Err(e) => {
                warn!(server = %entry.server, prompt = %entry.name, error = %e, "MCP prompts/get failed");
                PromptInvocation {
                    text: render_prompt_failure(command, &format!("{e}")),
                    is_error: true,
                }
            }
        }
    }

    // ── Resources as `@` references ──

    /// Read one resource and flatten it to text. `Err` carries a
    /// human-readable reason; nothing here panics or propagates.
    pub async fn read_resource_text(&self, server: &str, uri: &str) -> Result<String, String> {
        let Some(client) = self.clients.iter().find(|c| c.server_name() == server) else {
            return Err(format!(
                "no connected MCP server named '{server}' (connected: {})",
                if self.clients.is_empty() {
                    "none".to_string()
                } else {
                    self.clients
                        .iter()
                        .map(|c| c.server_name())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ));
        };
        match client.read_resource(uri).await {
            Ok(content) => Ok(flatten_resource_content(&content)),
            Err(e) => Err(format!("{e}")),
        }
    }

    /// Scan a user message for `@server:scheme://path` references (see
    /// [`crate::resources`] for the syntax and why it is shaped that way) and
    /// return a block of resolved contents to append to the message, or `None`
    /// when the message contains no references.
    ///
    /// Unresolvable references become visible error blocks rather than
    /// failing the turn.
    pub async fn resolve_resource_refs(&self, text: &str) -> Option<String> {
        // No servers → `@` can't mean an MCP resource; stay out of the way of
        // sessions that never configured MCP at all.
        if self.clients.is_empty() {
            return None;
        }
        let refs = find_resource_refs(text);
        if refs.is_empty() {
            return None;
        }

        let mut blocks = Vec::with_capacity(refs.len());
        let mut budget = MAX_TOTAL_RESOURCE_BYTES;
        for r in &refs {
            match self.read_resource_text(&r.server, &r.uri).await {
                Ok(body) => {
                    let cap = MAX_RESOURCE_BYTES.min(budget);
                    let shown = truncate_at_char_boundary(&body, cap);
                    budget = budget.saturating_sub(shown.len());
                    let truncated = shown.len() < body.len();
                    blocks.push(render_resource_block(&r.server, &r.uri, shown, truncated));
                }
                Err(reason) => {
                    warn!(server = %r.server, uri = %r.uri, %reason, "MCP resource reference failed");
                    blocks.push(render_resource_error(&r.server, &r.uri, &reason));
                }
            }
        }
        Some(wrap_resource_blocks(&blocks))
    }

    /// Per-server instruction text for the system prompt, bounded both
    /// per-server ([`MAX_SERVER_INSTRUCTION_BYTES`]) and in total
    /// ([`MAX_TOTAL_SERVER_INSTRUCTION_BYTES`]).
    ///
    /// The caller (`runtime`'s `build_mcp_instructions`) concatenates these
    /// straight into the system prompt with no further limit, so the bound has
    /// to live here: a single chatty server used to be able to push the actual
    /// system prompt out of the model's attention, and N of them could do it
    /// even while each stayed under the per-connection clamp in `connect.rs`.
    pub fn server_instructions(&self) -> Vec<McpServerInstruction> {
        let mut budget = MAX_TOTAL_SERVER_INSTRUCTION_BYTES;
        let mut out = Vec::new();
        for c in &self.clients {
            let Some(instructions) = c.instructions().map(str::trim) else {
                continue;
            };
            if instructions.is_empty() {
                continue;
            }
            if budget == 0 {
                warn!(
                    server = %c.server_name(),
                    "MCP server instructions dropped: total system-prompt budget exhausted"
                );
                continue;
            }
            let cap = MAX_SERVER_INSTRUCTION_BYTES.min(budget);
            let shown = truncate_at_char_boundary(instructions, cap);
            budget = budget.saturating_sub(shown.len());
            let mut text = shown.to_string();
            if shown.len() < instructions.len() {
                warn!(
                    server = %c.server_name(),
                    original_bytes = instructions.len(),
                    kept_bytes = shown.len(),
                    "MCP server instructions truncated for the system prompt"
                );
                text.push_str(INSTRUCTION_TRUNCATION_NOTE);
            }
            out.push(McpServerInstruction {
                name: c.server_name().to_string(),
                instructions: text,
            });
        }
        out
    }

    /// 异步取所有 server 的 resources + prompts 摘要 —— /mcp 详细模式用。
    /// 失败的 server 跳过（warn 而非整体 fail）；rmcp 服务器不一定支持这两个能力。
    pub async fn server_inventory(&self) -> Vec<ServerInventory> {
        let mut out = Vec::with_capacity(self.clients.len());
        for c in &self.clients {
            let resources = c.list_resources().await.unwrap_or_default();
            let prompts = c.list_prompts().await.unwrap_or_default();
            out.push(ServerInventory {
                name: c.server_name().to_string(),
                resource_count: resources.len(),
                prompt_count: prompts.len(),
            });
        }
        out
    }

    /// Refresh tools from connected MCP servers. Used between turns to pick up
    /// newly connected servers.
    /// Re-fetches tool lists from all connected clients and updates adapters.
    /// Re-ask every server for its tools when one of them announced a
    /// change.
    ///
    /// Returns whether anything was refreshed, so a caller can log it or
    /// skip work when nothing happened. This is the consumer that makes the
    /// notification path mean something — before it, `McpNotification` was a
    /// type nothing constructed and nothing read.
    pub async fn refresh_tools_if_announced(&mut self) -> bool {
        if !self.notifications.take_tools_changed() {
            return false;
        }
        tracing::info!("an MCP server announced new tools; re-reading every server's list");
        self.refresh_tools().await;
        true
    }

    /// Notifications parked since the last drain, for diagnostics.
    pub fn pending_notifications(&self) -> Vec<McpNotification> {
        self.notifications.drain()
    }

    pub async fn refresh_tools(&mut self) {
        let mut new_adapters: Vec<Arc<dyn Tool>> = Vec::new();
        // Always alias the shared cell (not gated on it currently holding
        // `Some`) — see the field doc comment: a caller may set the callback
        // *after* this runs, and only sharing the cell (not a snapshot of
        // its contents) makes that visible to these adapters.
        let cell = self.elicitation_callback.clone();
        for client in &self.clients {
            match client.list_tools().await {
                Ok(tools) => {
                    for meta in tools {
                        let adapter = McpToolAdapter::with_cache(
                            client.clone(),
                            meta,
                            self.output_cache.clone(),
                        )
                        .with_elicitation_callback(cell.clone());
                        new_adapters.push(Arc::new(adapter) as Arc<dyn Tool>);
                    }
                }
                Err(e) => {
                    warn!(
                        server = %client.server_name(),
                        error = %e,
                        "MCP tool refresh failed; keeping previous adapter set"
                    );
                }
            }
        }
        if !new_adapters.is_empty() {
            self.adapters = new_adapters;
        }
    }

    /// `/doctor` `/mcp` 用的状态摘要：每个 server 的 name / transport / 工具数。
    /// transport 字段：来自 client 的 transport_kind()——stdio / streamable_http / sse /
    /// in_process / web_socket（见 connect.rs）。
    pub fn server_statuses(&self) -> Vec<ServerStatus> {
        // 把每 client 的 server_name 与持有它的 adapter 数关联起来
        // adapter 通过 McpToolAdapter::server_name() 暴露 server 名（已存在）
        let mut counts: HashMap<String, usize> = HashMap::new();
        for a in &self.adapters {
            // adapter 的 name 形如 mcp__<server>__<tool>；提取 <server>
            let n = a.name();
            if let Some(rest) = n.strip_prefix("mcp__") {
                if let Some((server, _tool)) = rest.split_once("__") {
                    *counts.entry(server.to_string()).or_insert(0) += 1;
                }
            }
        }
        self.clients
            .iter()
            .map(|c| ServerStatus {
                name: c.server_name().to_string(),
                transport: c.transport_kind(),
                tool_count: counts.get(c.server_name()).copied().unwrap_or(0),
            })
            .collect()
    }
}

/// 单个 MCP server 的状态（manager 视角）。
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    pub transport: &'static str,
    pub tool_count: usize,
}

/// resources + prompts 数量摘要（不在 ServerStatus 里因为要异步取，
/// 而 server_statuses 是 sync 快路径）。
#[derive(Debug, Clone)]
pub struct ServerInventory {
    pub name: String,
    pub resource_count: usize,
    pub prompt_count: usize,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_manager_has_no_tools() {
        let m = McpManager::empty();
        assert_eq!(m.server_count(), 0);
        assert_eq!(m.tool_count(), 0);
        assert!(m.tool_adapters().is_empty());
    }

    #[test]
    fn server_instructions_collects_nonempty_client_instructions() {
        let client = Arc::new(
            crate::client::MockMcpClient::new("github", Vec::new())
                .with_instructions("Use issues carefully."),
        );
        let m = McpManager::from_clients(vec![client]);
        let instructions = m.server_instructions();
        assert_eq!(instructions.len(), 1);
        assert_eq!(instructions[0].name, "github");
        assert_eq!(instructions[0].instructions, "Use issues carefully.");
    }

    #[tokio::test]
    async fn connect_all_attempts_streamable_http() {
        // StreamableHttp 现在 wire 起来了；连不上的 URL 会 ConnectFailed → skip。
        // 行为：服务器数仍为 0（连接失败），但*尝试过*（不再是早返 stub）。
        let mut servers = HashMap::new();
        servers.insert(
            "remote".into(),
            McpServerConfig::StreamableHttp {
                url: "http://127.0.0.1:1/nonexistent".into(),
                headers: HashMap::new(),
                oauth_provider: None,
                scope: None,
            },
        );
        let m = McpManager::connect_all(servers).await;
        assert_eq!(m.server_count(), 0); // 失败仍跳过
    }

    fn adapters_of(client: &Arc<crate::client::MockMcpClient>, tool: &str) -> Vec<Arc<dyn Tool>> {
        let meta = crate::client::McpToolMeta {
            name: tool.into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            hints: Default::default(),
        };
        vec![Arc::new(McpToolAdapter::new(client.clone(), meta)) as Arc<dyn Tool>]
    }

    #[tokio::test]
    async fn installing_a_server_under_a_taken_name_replaces_it() {
        let old = Arc::new(crate::client::MockMcpClient::new("api", vec![]));
        let other = Arc::new(crate::client::MockMcpClient::new("docs", vec![]));
        let new = Arc::new(crate::client::MockMcpClient::new("api", vec![]));
        let mut m = McpManager::empty();
        m.install_server("api", old.clone(), adapters_of(&old, "old_tool"));
        m.install_server("docs", other.clone(), adapters_of(&other, "read"));

        m.install_server("api", new.clone(), adapters_of(&new, "new_tool"));

        assert_eq!(m.server_count(), 2);
        let api: Vec<_> = m
            .clients()
            .iter()
            .filter(|c| c.server_name() == "api")
            .collect();
        assert_eq!(api.len(), 1);
        let names: Vec<&str> = m.tool_adapters().iter().map(|a| a.name()).collect();
        assert_eq!(names, vec!["mcp__docs__read", "mcp__api__new_tool"]);
    }

    /// Regression: `elicitation_callback` used to be a plain
    /// `Option<ElicitationCallback>` snapshotted into each `McpToolAdapter`
    /// at construction time. `refresh_tools()` — which every real embedder
    /// calls before it gets a chance to call `set_elicitation_callback`
    /// (see `runtime::agent::Builder::build()`) — built adapters holding
    /// `None` forever, so a callback set afterward silently never reached
    /// them. Now it's a shared cell: setting the callback *after*
    /// `refresh_tools()` already ran must still reach the adapters it built.
    #[tokio::test]
    async fn elicitation_callback_set_after_refresh_tools_still_reaches_existing_adapters() {
        let client = Arc::new(crate::client::MockMcpClient::new(
            "docs-server",
            vec![crate::client::McpToolMeta {
                name: "fetch".into(),
                description: Some("fetches a doc".into()),
                input_schema: serde_json::json!({"type": "object"}),
                hints: Default::default(),
            }],
        ));
        client.push_response(
            "fetch",
            crate::client::McpCallResult {
                content: vec![crate::client::McpContent::Text(
                    "See elicitation://docs-server.example/confirm for details".into(),
                )],
                is_error: false,
                meta: None,
            },
        );
        let mut manager = McpManager::from_clients(vec![client]);
        // Adapters get built here, *before* any callback is registered —
        // the ordering every real caller (daemon) actually uses.
        manager.refresh_tools().await;
        assert_eq!(manager.tool_count(), 1, "sanity: one adapter should exist");

        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_cb = seen.clone();
        manager.set_elicitation_callback(Arc::new(move |server, url| {
            seen_for_cb.lock().unwrap().push((server, url));
        }));

        let adapter = manager.tool_adapters()[0].clone();
        let ctx = base::tool::ToolContext::for_test(std::env::temp_dir());
        let progress = base::tool::ProgressSender::noop("test");
        adapter
            .call(serde_json::json!({}), ctx, progress)
            .await
            .expect("mock tool call succeeds");

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            &[(
                "docs-server".to_string(),
                "elicitation://docs-server.example/confirm".to_string()
            )],
            "callback registered after refresh_tools() should still see the elicitation URL \
             from an adapter built before it existed"
        );
    }

    #[tokio::test]
    async fn connect_all_returns_empty_for_empty_input() {
        let m = McpManager::connect_all(HashMap::new()).await;
        assert_eq!(m.server_count(), 0);
        assert_eq!(m.tool_count(), 0);
    }

    // ── server_instructions bounds ──

    #[test]
    fn server_instructions_are_bounded_per_server() {
        let long = "x".repeat(MAX_SERVER_INSTRUCTION_BYTES * 3);
        let client = Arc::new(
            crate::client::MockMcpClient::new("chatty", Vec::new()).with_instructions(long.clone()),
        );
        let m = McpManager::from_clients(vec![client]);
        let out = m.server_instructions();
        assert_eq!(out.len(), 1);
        assert!(
            out[0].instructions.len() < long.len(),
            "instructions were not truncated"
        );
        assert!(out[0].instructions.contains("[... truncated"));
        assert!(
            out[0].instructions.len()
                <= MAX_SERVER_INSTRUCTION_BYTES + INSTRUCTION_TRUNCATION_NOTE.len()
        );
    }

    #[test]
    fn server_instructions_are_bounded_in_total_across_servers() {
        let long = "y".repeat(MAX_SERVER_INSTRUCTION_BYTES);
        let clients: Vec<crate::client::McpClientHandle> = (0..10)
            .map(|i| {
                Arc::new(
                    crate::client::MockMcpClient::new(format!("s{i}"), Vec::new())
                        .with_instructions(long.clone()),
                ) as crate::client::McpClientHandle
            })
            .collect();
        let m = McpManager::from_clients(clients);
        let total: usize = m
            .server_instructions()
            .iter()
            .map(|i| i.instructions.len())
            .sum();
        assert!(
            total <= MAX_TOTAL_SERVER_INSTRUCTION_BYTES + 10 * INSTRUCTION_TRUNCATION_NOTE.len(),
            "total instruction bytes {total} exceeded the budget"
        );
    }

    #[test]
    fn server_instructions_never_split_a_multibyte_character() {
        // The regression `truncate_at_char_boundary` exists for: a byte-slice
        // cut here would panic, since every character is 3 bytes.
        let long = "中".repeat(MAX_SERVER_INSTRUCTION_BYTES);
        let client =
            Arc::new(crate::client::MockMcpClient::new("cn", Vec::new()).with_instructions(long));
        let m = McpManager::from_clients(vec![client]);
        let out = m.server_instructions();
        assert!(out[0].instructions.starts_with("中中"));
    }

    // ── prompts as slash commands ──

    fn prompt_meta(
        name: &str,
        args: Vec<crate::client::McpPromptArg>,
    ) -> crate::client::McpPromptMeta {
        crate::client::McpPromptMeta {
            name: name.into(),
            description: Some(format!("{name} description")),
            arguments: args,
        }
    }

    fn required(name: &str) -> crate::client::McpPromptArg {
        crate::client::McpPromptArg {
            name: name.into(),
            description: None,
            required: Some(true),
        }
    }

    async fn manager_with_prompt_server() -> McpManager {
        let client = Arc::new(
            crate::client::MockMcpClient::new("github", Vec::new())
                .with_prompts(vec![
                    prompt_meta("review_pr", vec![required("repo"), required("pr")]),
                    prompt_meta("broken", vec![]),
                ])
                .with_prompt_body("review_pr", "Review PR {pr} in {repo}, carefully."),
        );
        let mut m = McpManager::from_clients(vec![client]);
        m.refresh_prompts().await;
        m
    }

    #[tokio::test]
    async fn prompts_are_collected_as_slash_commands() {
        let m = manager_with_prompt_server().await;
        let names: Vec<String> = m.all_prompts().iter().map(|p| p.command_name()).collect();
        assert_eq!(names, vec!["mcp__github__review_pr", "mcp__github__broken"]);
        assert!(m.find_prompt_command("mcp__github__review_pr").is_some());
        assert!(m.find_prompt_command("mcp__github__nope").is_none());
        assert!(m.find_prompt_command("review_pr").is_none());
    }

    #[tokio::test]
    async fn invoking_a_prompt_command_injects_the_servers_messages() {
        let m = manager_with_prompt_server().await;
        let out = m
            .invoke_prompt_command("mcp__github__review_pr", "repo=acme/widgets pr=42")
            .await;
        assert!(!out.is_error, "{}", out.text);
        // Arguments actually reached the server (the mock substitutes them).
        assert!(
            out.text
                .contains("Review PR 42 in acme/widgets, carefully."),
            "{}",
            out.text
        );
        // ...and the injected content carries provenance.
        assert!(out.text.contains(
            "<command-name>mcp__github__review_pr repo=acme/widgets pr=42</command-name>"
        ));
    }

    #[tokio::test]
    async fn missing_required_argument_never_reaches_the_server() {
        let client = Arc::new(
            crate::client::MockMcpClient::new("github", Vec::new())
                .with_prompts(vec![prompt_meta(
                    "review_pr",
                    vec![required("repo"), required("pr")],
                )])
                .with_prompt_body("review_pr", "body"),
        );
        let mut m = McpManager::from_clients(vec![client.clone()]);
        m.refresh_prompts().await;

        let out = m
            .invoke_prompt_command("mcp__github__review_pr", "repo=acme/widgets")
            .await;
        assert!(out.is_error);
        assert!(
            out.text.contains("missing required argument: pr"),
            "{}",
            out.text
        );
        // The point of validating: no silent empty `prompts/get`.
        assert!(client.prompt_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_prompt_that_errors_degrades_to_visible_text() {
        let m = manager_with_prompt_server().await;
        // `broken` is declared but has no canned body → the server errors.
        let out = m.invoke_prompt_command("mcp__github__broken", "").await;
        assert!(out.is_error);
        assert!(out.text.contains("could not be run"), "{}", out.text);
        assert!(out.text.contains("unknown prompt 'broken'"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_down_server_degrades_to_visible_text() {
        // Prompt list was collected while up, then the server went away.
        let up = Arc::new(
            crate::client::MockMcpClient::new("github", Vec::new())
                .with_prompts(vec![prompt_meta("review_pr", vec![])])
                .with_prompt_body("review_pr", "body"),
        );
        let mut m = McpManager::from_clients(vec![up]);
        m.refresh_prompts().await;
        let entry = m.all_prompts().to_vec();

        let down = Arc::new(crate::client::MockMcpClient::new("github", Vec::new()).down())
            as crate::client::McpClientHandle;
        let mut m = McpManager::from_clients(vec![down]);
        m.prompts = entry;

        let out = m.invoke_prompt_command("mcp__github__review_pr", "").await;
        assert!(out.is_error);
        assert!(out.text.contains("is not connected"), "{}", out.text);
    }

    #[tokio::test]
    async fn an_unknown_prompt_command_degrades_to_visible_text() {
        let m = manager_with_prompt_server().await;
        let out = m.invoke_prompt_command("mcp__ghost__thing", "").await;
        assert!(out.is_error);
        assert!(out
            .text
            .contains("no connected MCP server exposes this prompt"));
    }

    // ── resources as `@` references ──

    async fn manager_with_resource_server() -> McpManager {
        let client = Arc::new(
            crate::client::MockMcpClient::new("docs", Vec::new())
                .with_resources(vec![crate::client::McpResourceMeta {
                    uri: "doc://guide/install".into(),
                    name: "install guide".into(),
                    description: None,
                    mime_type: Some("text/markdown".into()),
                }])
                .with_resource_text("doc://guide/install", "Run `cargo install agix`."),
        );
        McpManager::from_clients(vec![client])
    }

    #[tokio::test]
    async fn an_at_reference_resolves_and_inlines_content() {
        let m = manager_with_resource_server().await;
        let out = m
            .resolve_resource_refs("summarise @docs:doc://guide/install for me")
            .await
            .expect("reference should have been found");
        assert!(out.contains("<mcp-resource server=\"docs\" uri=\"doc://guide/install\">"));
        assert!(out.contains("Run `cargo install agix`."));
    }

    #[tokio::test]
    async fn non_matching_at_signs_never_trigger_a_fetch() {
        let m = manager_with_resource_server().await;
        for text in [
            "mail me at xbitshans@gmail.com",
            "@Component({ selector: 'app' })",
            "fn f<'a>(x: &'a str) -> &'a str { x }",
            "npm i @anthropic-ai/sdk",
            "@media (min-width: 600px) {}",
            "@todo: fix this",
        ] {
            assert!(
                m.resolve_resource_refs(text).await.is_none(),
                "false positive on: {text}"
            );
        }
    }

    #[tokio::test]
    async fn a_bad_uri_degrades_to_a_visible_error_block() {
        let m = manager_with_resource_server().await;
        let out = m
            .resolve_resource_refs("check @docs:doc://guide/nope please")
            .await
            .expect("reference should have been found");
        assert!(out.contains("error=\"true\""));
        assert!(out.contains("no such resource"), "{out}");
    }

    #[tokio::test]
    async fn an_unknown_server_degrades_to_a_visible_error_block() {
        let m = manager_with_resource_server().await;
        let out = m
            .resolve_resource_refs("check @ghost:doc://x/y")
            .await
            .expect("reference should have been found");
        assert!(
            out.contains("no connected MCP server named 'ghost'"),
            "{out}"
        );
        assert!(out.contains("connected: docs"), "{out}");
    }

    #[tokio::test]
    async fn a_down_server_degrades_to_a_visible_error_block() {
        let client = Arc::new(crate::client::MockMcpClient::new("docs", Vec::new()).down());
        let m = McpManager::from_clients(vec![client]);
        let out = m
            .resolve_resource_refs("check @docs:doc://guide/install")
            .await
            .expect("reference should have been found");
        assert!(out.contains("error=\"true\""));
        assert!(out.contains("is not connected"), "{out}");
    }

    #[tokio::test]
    async fn inlined_resource_content_is_bounded() {
        let huge = "z".repeat(MAX_RESOURCE_BYTES * 2);
        let client = Arc::new(
            crate::client::MockMcpClient::new("docs", Vec::new())
                .with_resource_text("doc://huge", huge.clone()),
        );
        let m = McpManager::from_clients(vec![client]);
        let out = m.resolve_resource_refs("@docs:doc://huge").await.unwrap();
        assert!(out.len() < huge.len());
        assert!(out.contains("[... truncated"));
    }

    #[tokio::test]
    async fn sessions_without_mcp_servers_ignore_at_references_entirely() {
        let m = McpManager::empty();
        assert!(m
            .resolve_resource_refs("@docs:doc://guide/install")
            .await
            .is_none());
    }
}

#[cfg(test)]
mod notification_wiring_tests {
    use super::*;

    /// Regression: `McpNotification` existed, `dispatch_notification`
    /// existed, and nothing in the process ever constructed one — a server
    /// announcing new tools was talking to a wall. This pins the queue that
    /// closes the gap.
    #[tokio::test]
    async fn a_manager_starts_with_nothing_announced() {
        let mut m = McpManager::empty();
        assert!(m.pending_notifications().is_empty());
        assert!(
            !m.refresh_tools_if_announced().await,
            "nothing announced means no work"
        );
    }

    #[tokio::test]
    async fn an_announced_tool_change_triggers_exactly_one_refresh() {
        let mut m = McpManager::empty();
        m.notifications.push(McpNotification::ToolListChanged {
            server: "github".into(),
        });

        assert!(m.refresh_tools_if_announced().await);
        assert!(
            !m.refresh_tools_if_announced().await,
            "one announcement is one refresh, not a standing order"
        );
    }

    /// A resource or prompt change does not invalidate the tool list, and
    /// re-reading every server's tools for it would be work nobody asked
    /// for.
    #[tokio::test]
    async fn other_announcements_do_not_trigger_a_tool_refresh() {
        let mut m = McpManager::empty();
        m.notifications.push(McpNotification::ResourceListChanged {
            server: "github".into(),
        });
        assert!(!m.refresh_tools_if_announced().await);
    }
}
