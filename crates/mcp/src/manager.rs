//! `McpManager` —— 串联 settings / connect / adapter 给 engine 用的入口。
//!
//! 用法：CLI 启动时把 settings.mcp_servers 喂给 `connect_all`；返回的 manager 把
//! 全部 server 的 tools 摊平成 `Vec<Arc<dyn Tool>>` 加进 ToolRegistry。
//! 不想等慢 server 的调用方用 `connect_all_within`：到点先拿已连上的，其余的结果
//! 从 [`LateConnects`] 陆续到，交给 [`McpManager::absorb`]。

use crate::adapter::McpToolAdapter;
use crate::client::{McpClient, McpClientHandle};
use crate::config::McpServerConfig;
use crate::connect::StdioMcpClient;
use crate::error::McpError;
use crate::output_cache::McpOutputCache;
use crate::prompts::{
    parse_prompt_args, render_prompt_failure, render_prompt_success, PromptInvocation,
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
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::{info, warn};

const MAX_CONCURRENT_CONNECTS: usize = 3;

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

pub struct McpManager {
    clients: Vec<McpClientHandle>,
    adapters: Vec<Arc<dyn Tool>>,
    prompts: Vec<McpPromptEntry>,
    /// What the servers have announced; every connection this manager and
    /// those shared from it make reports here.
    announcements: Arc<crate::notify::Announcements>,
    /// The announcements this manager's tools and prompts reflect.
    seen: HashMap<String, crate::notify::ListEpochs>,
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
    subprocess_env: platform::SubprocessEnv,
    /// The owning pool's authorization state, handed to every connection.
    auth: Option<Arc<crate::oauth::McpAuth>>,
    /// Every server this manager was asked to connect, connected or not, so
    /// one can be reconnected on its own.
    configs: HashMap<String, McpServerConfig>,
    /// Configured servers the MCP policy kept from connecting.
    denied: Vec<DeniedServer>,
    /// Servers waiting for a person to sign in; they contribute no tools.
    pending: Vec<PendingServer>,
    /// Servers still connecting when this manager was handed out.
    connecting: Vec<PendingServer>,
    /// Servers that could not be connected, with why.
    failed: Vec<FailedServer>,
    /// Which `connect_all_within` the `connecting` servers belong to, so a
    /// late result reaches only managers made from that connect.
    batch: Arc<()>,
}

impl McpManager {
    /// Empty/default instance with no state.
    pub fn empty() -> Self {
        Self {
            clients: Vec::new(),
            adapters: Vec::new(),
            prompts: Vec::new(),
            announcements: crate::notify::Announcements::new(),
            seen: HashMap::new(),
            output_cache: Arc::new(Mutex::new(McpOutputCache::new())),
            elicitation_callback: Arc::new(Mutex::new(None)),
            subprocess_env: platform::SubprocessEnv::default(),
            auth: None,
            configs: HashMap::new(),
            denied: Vec::new(),
            pending: Vec::new(),
            connecting: Vec::new(),
            failed: Vec::new(),
            batch: Arc::new(()),
        }
    }

    /// A manager over clients already connected, with their tools and
    /// prompts read.
    pub async fn from_clients(clients: Vec<McpClientHandle>) -> Self {
        let mut manager = Self {
            clients,
            ..Self::empty()
        };
        manager.refresh_tools().await;
        manager.refresh_prompts().await;
        manager
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

    /// 连所有 server，list_tools 完成后构造 adapters。
    /// 使用并发连接——最多 3 个 server 同时连。
    /// 单个 server 失败不整体失败 —— warn + skip。
    pub async fn connect_all(servers: HashMap<String, McpServerConfig>) -> Self {
        Self::connect_all_with(servers, platform::SubprocessEnv::default(), None).await
    }

    /// `connect_all`, with stdio servers started under `env` (the
    /// `sandbox.subprocess_env` of the settings they were declared in) and
    /// remote servers authorized through `auth` — the owning pool's, so pools
    /// in one process never share tokens. A server that asks for a sign-in
    /// is kept as [`PendingServer`] rather than connected.
    ///
    /// Waits for every server; each is bounded by its startup limit.
    pub async fn connect_all_with(
        servers: HashMap<String, McpServerConfig>,
        env: platform::SubprocessEnv,
        auth: Option<Arc<crate::oauth::McpAuth>>,
    ) -> Self {
        let (mut manager, late) = Self::connect_all_within(servers, env, auth, None).await;
        if let Some(mut late) = late {
            while let Some(outcome) = late.next().await {
                manager.absorb(outcome);
            }
        }
        manager
    }

    /// Start every stdio server this manager adds later under `env`.
    pub fn with_subprocess_env(mut self, env: platform::SubprocessEnv) -> Self {
        self.subprocess_env = env;
        self
    }

    /// Authorize every remote server this manager adds later through `auth`.
    pub fn with_auth(mut self, auth: Option<Arc<crate::oauth::McpAuth>>) -> Self {
        self.auth = auth;
        self
    }

    /// [`Self::connect_all_with`] that returns once `wait` has passed, with
    /// the servers that are connected by then. The rest keep connecting;
    /// their results come out of the returned [`LateConnects`], one by one,
    /// for [`Self::absorb`]. `None` waits for all of them.
    pub async fn connect_all_within(
        mut servers: HashMap<String, McpServerConfig>,
        subprocess_env: platform::SubprocessEnv,
        auth: Option<Arc<crate::oauth::McpAuth>>,
        wait: Option<Duration>,
    ) -> (Self, Option<LateConnects>) {
        let sharing = sharing_a_segment(&servers);
        servers.retain(|name, _| !sharing.iter().any(|f| &f.name == name));
        let elicitation_cell: Arc<Mutex<Option<ElicitationCallback>>> = Arc::new(Mutex::new(None));
        let cache = Arc::new(Mutex::new(McpOutputCache::new()));
        let announcements = crate::notify::Announcements::new();
        let mut connecting: Vec<PendingServer> = servers
            .iter()
            .map(|(name, cfg)| PendingServer {
                name: name.clone(),
                transport: cfg.transport_kind(),
            })
            .collect();
        connecting.sort_by(|a, b| a.name.cmp(&b.name));
        let mut manager = Self {
            clients: Vec::new(),
            adapters: Vec::new(),
            prompts: Vec::new(),
            announcements: announcements.clone(),
            seen: HashMap::new(),
            output_cache: cache.clone(),
            elicitation_callback: elicitation_cell.clone(),
            subprocess_env: subprocess_env.clone(),
            auth: auth.clone(),
            configs: servers.clone(),
            denied: Vec::new(),
            pending: Vec::new(),
            connecting,
            failed: sharing,
            batch: Arc::new(()),
        };
        if servers.is_empty() {
            return (manager, None);
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut set = JoinSet::new();
            let mut queued = servers.into_iter();
            loop {
                while set.len() < MAX_CONCURRENT_CONNECTS {
                    let Some((name, cfg)) = queued.next() else {
                        break;
                    };
                    let link = ServerLink {
                        cache: cache.clone(),
                        elicitation_cell: elicitation_cell.clone(),
                        announcements: announcements.clone(),
                        subprocess_env: subprocess_env.clone(),
                        auth: auth.clone(),
                    };
                    set.spawn(async move {
                        let result = Self::connect_with_retry(&name, &cfg, &link).await;
                        Outcome::of(name, result)
                    });
                }
                let joined = tokio::select! {
                    joined = set.join_next() => joined,
                    _ = tx.closed() => return,
                };
                let Some(joined) = joined else {
                    return;
                };
                match joined {
                    Ok(outcome) => {
                        if tx.send(outcome).is_err() {
                            return;
                        }
                    }
                    Err(join_err) => warn!(?join_err, "MCP connect task panicked; skipping"),
                }
            }
        });

        let mut late = LateConnects {
            rx,
            batch: manager.batch.clone(),
        };
        let deadline = wait.map(|w| tokio::time::Instant::now() + w);
        loop {
            let next = match deadline {
                Some(at) => match tokio::time::timeout_at(at, late.next()).await {
                    Ok(next) => next,
                    Err(_) => break,
                },
                None => late.next().await,
            };
            match next {
                Some(outcome) => {
                    manager.absorb(outcome);
                }
                None => break,
            }
        }
        if manager.connecting.is_empty() {
            (manager, None)
        } else {
            (manager, Some(late))
        }
    }

    /// Take in one late result of the connect `outcome` came from. Returns
    /// whether it was this manager's to take: a manager made from another
    /// connect, or one that has dropped the server since, leaves it alone.
    pub fn absorb(&mut self, outcome: ConnectOutcome) -> bool {
        if !Arc::ptr_eq(&self.batch, &outcome.batch) {
            return false;
        }
        let name = outcome.result.name().to_string();
        let Some(at) = self.connecting.iter().position(|c| c.name == name) else {
            return false;
        };
        let transport = self.connecting.remove(at).transport;
        match outcome.result {
            Outcome::Connected {
                client,
                adapters,
                prompts,
                ..
            } => {
                info!(server = %name, n_tools = adapters.len(), "MCP server connected");
                self.install_server(&name, client, adapters);
                self.prompts.extend(prompts);
            }
            Outcome::NeedsSignIn { .. } => {
                info!(server = %name, "MCP server needs a sign-in before it connects");
                self.forget(&name);
                self.pending.push(PendingServer { name, transport });
                self.pending.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Outcome::Failed { error, .. } => {
                warn!(server = %name, %error, "MCP server connect failed; skipping");
                self.forget(&name);
                self.failed.push(FailedServer {
                    name,
                    transport,
                    error,
                });
            }
        }
        true
    }

    /// Re-run `prompts/list` on every connected server and rebuild the slash
    /// command catalog. A server that fails to answer keeps the prompts it
    /// had.
    pub async fn refresh_prompts(&mut self) {
        self.relist_prompts(None).await;
    }

    async fn relist_prompts(&mut self, only: Option<&[String]>) {
        let mut prompts = Vec::new();
        for client in &self.clients {
            let name = client.server_name();
            let listed = match only {
                Some(only) if !only.iter().any(|n| n == name) => None,
                _ => try_prompts_of(client).await,
            };
            match listed {
                Some(listed) => prompts.extend(listed),
                None => prompts.extend(self.prompts.iter().filter(|p| p.server == name).cloned()),
            }
        }
        self.prompts = prompts;
    }

    async fn connect_one(
        name: &str,
        cfg: &McpServerConfig,
        link: &ServerLink,
    ) -> Result<Connected, McpError> {
        let client = StdioMcpClient::connect_with_env(
            name,
            cfg,
            Some(link.announcements.sink()),
            link.subprocess_env.clone(),
            link.auth.clone(),
        )
        .await?;
        let tools = client.list_tools().await?;
        let handle: McpClientHandle = client;
        let adapters = server_adapters(&handle, tools, &link.cache, &link.elicitation_cell);
        let prompts = prompts_of(&handle).await;
        Ok(Connected {
            client: handle,
            adapters,
            prompts,
        })
    }

    /// Connect to an MCP server with exponential backoff retry:
    /// MAX_RETRIES=5, INITIAL_DELAY=1s, MAX_DELAY=30s, ±25% jitter. A server
    /// that asks for authorization is not retried: only a sign-in changes
    /// its answer. Nor is one that ran out of time: it would only use up
    /// its whole limit again. Nor one that cannot be started at all.
    async fn connect_with_retry(
        name: &str,
        cfg: &McpServerConfig,
        link: &ServerLink,
    ) -> Result<Connected, McpError> {
        const MAX_RETRIES: u32 = 5;
        const INITIAL_DELAY_MS: u64 = 1_000;
        const MAX_DELAY_MS: u64 = 30_000;

        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            match Self::connect_one(name, cfg, link).await {
                Ok(result) => return Ok(result),
                Err(e)
                    if e.is_auth() || e.is_permanent() || matches!(e, McpError::Timeout { .. }) =>
                {
                    return Err(e)
                }
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
        self.forget(name);
        self.clients.push(client);
        self.adapters.extend(server_adapters);
    }

    /// Take a server's connection, tools, prompts, pending sign-in and
    /// failure away.
    fn forget(&mut self, name: &str) {
        let source = format!("mcp:{name}");
        self.clients.retain(|c| c.server_name() != name);
        self.adapters.retain(|a| a.source() != source);
        self.prompts.retain(|p| p.server != name);
        self.seen.remove(name);
        self.pending.retain(|p| p.name != name);
        self.failed.retain(|f| f.name != name);
    }

    /// Connect an MCP server after construction, replacing any server already
    /// connected under `name` (its tools included). A server that asks for a
    /// sign-in replaces it as pending; on any other failure the existing
    /// connection, if any, stays and a warning is logged.
    pub async fn add_server(&mut self, name: &str, cfg: &McpServerConfig) {
        let segment = crate::naming::server_segment(name);
        let taken: Vec<&str> = self
            .configs
            .keys()
            .map(String::as_str)
            .chain(self.clients.iter().map(|c| c.server_name()))
            .filter(|other| *other != name && crate::naming::server_segment(other) == segment)
            .collect();
        if !taken.is_empty() {
            let error = segment_taken(&segment, &taken);
            warn!(server = %name, %error, "MCP server not connected");
            self.failed.retain(|f| f.name != name);
            self.failed.push(FailedServer {
                name: name.to_string(),
                transport: cfg.transport_kind(),
                error,
            });
            return;
        }
        self.configs.insert(name.to_string(), cfg.clone());
        self.connecting.retain(|c| c.name != name);
        let link = ServerLink {
            cache: self.output_cache.clone(),
            elicitation_cell: self.elicitation_callback.clone(),
            announcements: self.announcements.clone(),
            subprocess_env: self.subprocess_env.clone(),
            auth: self.auth.clone(),
        };
        match Self::connect_one(name, cfg, &link).await {
            Ok(connected) => {
                info!(
                    server = %name,
                    n_tools = connected.adapters.len(),
                    "MCP server connected"
                );
                self.install_server(name, connected.client, connected.adapters);
                self.prompts.extend(connected.prompts);
            }
            Err(e) if e.is_auth() => {
                info!(server = %name, "MCP server needs a sign-in before it connects");
                self.forget(name);
                self.pending.push(PendingServer {
                    name: name.to_string(),
                    transport: cfg.transport_kind(),
                });
            }
            Err(e) => {
                warn!(server = %name, error = %e, "MCP server connect failed; skipping");
                if !self.clients.iter().any(|c| c.server_name() == name) {
                    self.failed.retain(|f| f.name != name);
                    self.failed.push(FailedServer {
                        name: name.to_string(),
                        transport: cfg.transport_kind(),
                        error: e.to_string(),
                    });
                }
            }
        }
    }

    /// Disconnect `name` until someone signs in to it again.
    pub fn demote(&mut self, name: &str) {
        let transport = self
            .configs
            .get(name)
            .map(McpServerConfig::transport_kind)
            .or_else(|| {
                self.clients
                    .iter()
                    .find(|c| c.server_name() == name)
                    .map(|c| c.transport_kind())
            });
        self.forget(name);
        if let Some(transport) = transport {
            self.pending.push(PendingServer {
                name: name.to_string(),
                transport,
            });
        }
    }

    /// A manager over the same connections — client handles are shared, no
    /// server is reconnected — that a session can own. Its tool adapters
    /// are built by [`Self::refresh_tools`]; it reads the same
    /// announcements.
    pub fn share(&self) -> Self {
        Self {
            clients: self.clients.clone(),
            adapters: Vec::new(),
            prompts: self.prompts.clone(),
            announcements: self.announcements.clone(),
            seen: self.seen.clone(),
            output_cache: Arc::new(Mutex::new(McpOutputCache::new())),
            elicitation_callback: Arc::new(Mutex::new(None)),
            subprocess_env: self.subprocess_env.clone(),
            auth: self.auth.clone(),
            configs: self.configs.clone(),
            denied: self.denied.clone(),
            pending: self.pending.clone(),
            connecting: self.connecting.clone(),
            failed: self.failed.clone(),
            batch: self.batch.clone(),
        }
    }

    /// 给 ToolRegistry 用的 adapter 列表。
    pub fn tool_adapters(&self) -> &[Arc<dyn Tool>] {
        &self.adapters
    }

    pub fn clients(&self) -> &[McpClientHandle] {
        &self.clients
    }

    pub fn auth(&self) -> Option<&Arc<crate::oauth::McpAuth>> {
        self.auth.as_ref()
    }

    pub fn config_of(&self, name: &str) -> Option<&McpServerConfig> {
        self.configs.get(name)
    }

    /// The servers, connected or pending, whose URL canonicalizes to `url`.
    pub fn servers_at(&self, url: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .configs
            .iter()
            .filter(|(name, cfg)| {
                (self.pending.iter().any(|p| &p.name == *name)
                    || self
                        .clients
                        .iter()
                        .any(|c| c.server_name() == name.as_str()))
                    && crate::oauth::server_url(cfg)
                        .and_then(|u| crate::oauth::canonical_server_url(&u).ok())
                        .is_some_and(|u| u == url)
            })
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    pub fn with_denied(mut self, denied: Vec<DeniedServer>) -> Self {
        self.denied = denied;
        self
    }

    pub fn denied(&self) -> &[DeniedServer] {
        &self.denied
    }

    pub fn pending(&self) -> &[PendingServer] {
        &self.pending
    }

    pub fn connecting(&self) -> &[PendingServer] {
        &self.connecting
    }

    pub fn failed(&self) -> &[FailedServer] {
        &self.failed
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

    /// Seed the prompt catalog without a `prompts/list` round trip, for the
    /// "command list was collected while the server was up, then the server
    /// went away" state.
    #[doc(hidden)]
    pub fn set_prompts_for_test(&mut self, prompts: Vec<McpPromptEntry>) {
        self.prompts = prompts;
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

    /// Look up a collected prompt by its slash command name
    /// (`mcp__<server>__<prompt>`).
    pub fn find_prompt_command(&self, command: &str) -> Option<&McpPromptEntry> {
        self.prompts.iter().find(|p| p.command_name() == command)
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
    /// to live here: otherwise a single chatty server could push the actual
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
        out.extend(self.pending.iter().map(|p| McpServerInstruction {
            name: p.name.clone(),
            instructions: SIGN_IN_NOTICE.to_string(),
        }));
        out
    }

    /// List again the tools, and the prompts, of the servers that announced
    /// a change since this manager last looked. Returns whether any did.
    pub async fn refresh_if_changed(&mut self) -> bool {
        let mut tools = Vec::new();
        let mut prompts = Vec::new();
        for client in &self.clients {
            let name = client.server_name();
            let now = self.announcements.of(name);
            let seen = self.seen.insert(name.to_string(), now).unwrap_or_default();
            if now.tools != seen.tools {
                tools.push(name.to_string());
            }
            if now.prompts != seen.prompts {
                prompts.push(name.to_string());
            }
        }
        if !tools.is_empty() {
            info!(servers = ?tools, "MCP servers announced a tool change; listing their tools again");
            self.relist_tools(Some(&tools)).await;
        }
        if !prompts.is_empty() {
            self.relist_prompts(Some(&prompts)).await;
        }
        !tools.is_empty() || !prompts.is_empty()
    }

    /// List every connected server's tools again. A server whose list fails
    /// keeps the tools it had; one that lists none has none.
    pub async fn refresh_tools(&mut self) {
        for client in &self.clients {
            let name = client.server_name();
            let now = self.announcements.of(name);
            self.seen.entry(name.to_string()).or_default().tools = now.tools;
        }
        self.relist_tools(None).await;
    }

    async fn relist_tools(&mut self, only: Option<&[String]>) {
        let mut adapters: Vec<Arc<dyn Tool>> = Vec::new();
        // The shared cell, not what it holds now: a callback set after this
        // runs has to reach these adapters too.
        let cell = self.elicitation_callback.clone();
        for client in &self.clients {
            let name = client.server_name();
            let listed = match only {
                Some(only) if !only.iter().any(|n| n == name) => None,
                _ => match client.list_tools().await {
                    Ok(tools) => Some(tools),
                    Err(e) => {
                        warn!(server = %name, error = %e, "MCP tool refresh failed; keeping its previous tools");
                        None
                    }
                },
            };
            match listed {
                Some(tools) => {
                    adapters.extend(server_adapters(client, tools, &self.output_cache, &cell))
                }
                None => {
                    let source = format!("mcp:{name}");
                    adapters.extend(
                        self.adapters
                            .iter()
                            .filter(|a| a.source() == source)
                            .cloned(),
                    );
                }
            }
        }
        self.adapters = adapters;
    }

    /// `/doctor` `/mcp` 用的状态摘要：每个 server 的 name / transport / 工具数。
    /// transport 字段：来自 client 的 transport_kind()——stdio / streamable_http / sse /
    /// web_socket（见 connect.rs）。
    pub fn server_statuses(&self) -> Vec<ServerStatus> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for a in &self.adapters {
            if let Some(server) = a.source().strip_prefix("mcp:") {
                *counts.entry(server.to_string()).or_insert(0) += 1;
            }
        }
        self.clients
            .iter()
            .map(|c| ServerStatus {
                name: c.server_name().to_string(),
                transport: c.transport_kind(),
                tool_count: counts.get(c.server_name()).copied().unwrap_or(0),
                connected: c.is_connected(),
            })
            .collect()
    }
}

/// The servers of `servers` whose tools would go by the same
/// `mcp__<segment>__` as another's, each refused: which of them the name
/// belongs to cannot be told, and a permission rule written for one would
/// reach the other's tools.
fn sharing_a_segment(servers: &HashMap<String, McpServerConfig>) -> Vec<FailedServer> {
    let mut by_segment: HashMap<String, Vec<&str>> = HashMap::new();
    for name in servers.keys() {
        by_segment
            .entry(crate::naming::server_segment(name))
            .or_default()
            .push(name);
    }
    let mut refused: Vec<FailedServer> = by_segment
        .iter()
        .filter(|(_, names)| names.len() > 1)
        .flat_map(|(segment, names)| {
            names.iter().map(move |name| {
                let others: Vec<&str> = names.iter().copied().filter(|n| n != name).collect();
                FailedServer {
                    name: name.to_string(),
                    transport: servers[*name].transport_kind(),
                    error: segment_taken(segment, &others),
                }
            })
        })
        .collect();
    for f in &refused {
        warn!(server = %f.name, error = %f.error, "MCP server not connected");
    }
    refused.sort_by(|a, b| a.name.cmp(&b.name));
    refused
}

fn segment_taken(segment: &str, others: &[&str]) -> String {
    let mut others: Vec<String> = others.iter().map(|o| format!("`{o}`")).collect();
    others.sort();
    format!(
        "not connected: its tools would be named mcp__{segment}__…, as those of {} would; \
         rename one of them",
        others.join(", ")
    )
}

/// What a server's tools are offered as: one adapter per tool, named among
/// the server's others by [`crate::naming::tool_names`].
fn server_adapters(
    client: &McpClientHandle,
    tools: Vec<crate::client::McpToolMeta>,
    cache: &Arc<Mutex<McpOutputCache>>,
    elicitation_cell: &Arc<Mutex<Option<ElicitationCallback>>>,
) -> Vec<Arc<dyn Tool>> {
    let names =
        crate::naming::tool_names(client.server_name(), tools.iter().map(|t| t.name.as_str()));
    tools
        .into_iter()
        .zip(names)
        .map(|(meta, name)| {
            let adapter = McpToolAdapter::with_cache(client.clone(), meta, cache.clone())
                .named(name)
                .with_elicitation_callback(elicitation_cell.clone());
            Arc::new(adapter) as Arc<dyn Tool>
        })
        .collect()
}

async fn prompts_of(client: &McpClientHandle) -> Vec<McpPromptEntry> {
    try_prompts_of(client).await.unwrap_or_default()
}

async fn try_prompts_of(client: &McpClientHandle) -> Option<Vec<McpPromptEntry>> {
    match client.list_prompts().await {
        Ok(server_prompts) => {
            info!(server = %client.server_name(), n_prompts = server_prompts.len(), "MCP prompts collected");
            Some(
                server_prompts
                    .into_iter()
                    .map(|p| McpPromptEntry {
                        server: client.server_name().to_string(),
                        name: p.name,
                        description: p.description.unwrap_or_default(),
                        arguments: p.arguments,
                    })
                    .collect(),
            )
        }
        Err(e) => {
            warn!(server = %client.server_name(), error = %e, "MCP prompt collection failed; skipping");
            None
        }
    }
}

/// What every connection a manager makes shares with it.
struct ServerLink {
    cache: Arc<Mutex<McpOutputCache>>,
    elicitation_cell: Arc<Mutex<Option<ElicitationCallback>>>,
    announcements: Arc<crate::notify::Announcements>,
    subprocess_env: platform::SubprocessEnv,
    auth: Option<Arc<crate::oauth::McpAuth>>,
}

struct Connected {
    client: McpClientHandle,
    adapters: Vec<Arc<dyn Tool>>,
    prompts: Vec<McpPromptEntry>,
}

/// How one server's connect ended, for [`McpManager::absorb`].
pub struct ConnectOutcome {
    result: Outcome,
    batch: Arc<()>,
}

enum Outcome {
    Connected {
        name: String,
        client: McpClientHandle,
        adapters: Vec<Arc<dyn Tool>>,
        prompts: Vec<McpPromptEntry>,
    },
    NeedsSignIn {
        name: String,
    },
    Failed {
        name: String,
        error: String,
    },
}

impl Outcome {
    fn name(&self) -> &str {
        match self {
            Outcome::Connected { name, .. }
            | Outcome::NeedsSignIn { name }
            | Outcome::Failed { name, .. } => name,
        }
    }

    fn of(name: String, result: Result<Connected, McpError>) -> Self {
        match result {
            Ok(c) => Outcome::Connected {
                name,
                client: c.client,
                adapters: c.adapters,
                prompts: c.prompts,
            },
            Err(e) if e.is_auth() => Outcome::NeedsSignIn { name },
            Err(e) => Outcome::Failed {
                name,
                error: e.to_string(),
            },
        }
    }
}

impl ConnectOutcome {
    pub fn server(&self) -> &str {
        self.result.name()
    }

    /// The server's status when it connected, `None` when it did not.
    pub fn connected(&self) -> Option<ServerStatus> {
        match &self.result {
            Outcome::Connected {
                client, adapters, ..
            } => Some(ServerStatus {
                name: client.server_name().to_string(),
                transport: client.transport_kind(),
                tool_count: adapters.len(),
                connected: true,
            }),
            _ => None,
        }
    }

    /// Why it did not connect, unless it is waiting for a sign-in.
    pub fn error(&self) -> Option<&str> {
        match &self.result {
            Outcome::Failed { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// The servers of a [`McpManager::connect_all_within`] that had not finished
/// when it returned. Dropping it stops them.
pub struct LateConnects {
    rx: tokio::sync::mpsc::UnboundedReceiver<Outcome>,
    batch: Arc<()>,
}

impl LateConnects {
    /// The next server to finish, `None` once all have.
    pub async fn next(&mut self) -> Option<ConnectOutcome> {
        let result = self.rx.recv().await?;
        Some(ConnectOutcome {
            result,
            batch: self.batch.clone(),
        })
    }
}

/// 单个 MCP server 的状态（manager 视角）。
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    pub transport: &'static str,
    pub tool_count: usize,
    /// The connection was up when last seen; see [`McpClient::is_connected`].
    pub connected: bool,
}

#[derive(Debug, Clone)]
pub struct FailedServer {
    pub name: String,
    pub transport: &'static str,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct DeniedServer {
    pub name: String,
    pub transport: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingServer {
    pub name: String,
    pub transport: &'static str,
}

/// What the model is told about a server it cannot use until the user signs
/// in — enough to explain a missing capability, not to act on it.
const SIGN_IN_NOTICE: &str = "Not connected: this server requires the user to sign in \
     (OAuth) before its tools are available. If the user asks for something it provides, tell \
     them to sign in to it from their client.";

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

    #[tokio::test]
    async fn server_instructions_collects_nonempty_client_instructions() {
        let client = Arc::new(
            crate::client::MockMcpClient::new("github", Vec::new())
                .with_instructions("Use issues carefully."),
        );
        let m = McpManager::from_clients(vec![client]).await;
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
                oauth: None,
                scope: None,
                timeouts: Default::default(),
            },
        );
        let m = McpManager::connect_all(servers).await;
        assert_eq!(m.server_count(), 0); // 失败仍跳过
    }

    fn missing_command() -> McpServerConfig {
        McpServerConfig::Stdio {
            command: "/nonexistent/agix-mcp-server".into(),
            args: Vec::new(),
            env: HashMap::new(),
            scope: None,
            timeouts: Default::default(),
            working_dir: None,
        }
    }

    #[tokio::test]
    async fn servers_whose_tools_would_go_by_one_prefix_are_not_connected() {
        let hashed = crate::naming::server_segment("my.srv");
        let servers = HashMap::from([
            ("my.srv".to_string(), missing_command()),
            (hashed.clone(), missing_command()),
            ("other".to_string(), missing_command()),
        ]);
        let mut m = McpManager::connect_all(servers).await;
        let error_of = |m: &McpManager, name: &str| {
            m.failed()
                .iter()
                .find(|f| f.name == name)
                .map(|f| f.error.clone())
                .unwrap_or_default()
        };
        assert!(error_of(&m, "my.srv").contains(&hashed), "{:?}", m.failed());
        assert!(error_of(&m, &hashed).contains("my.srv"), "{:?}", m.failed());
        assert!(!error_of(&m, "other").contains("mcp__"), "{:?}", m.failed());

        let late = format!("{}x", "y".repeat(45));
        let late_segment = crate::naming::server_segment(&late);
        m.add_server(&late_segment, &missing_command()).await;
        m.add_server(&late, &missing_command()).await;
        assert!(
            error_of(&m, &late).contains(&late_segment),
            "{:?}",
            m.failed()
        );
        assert!(
            !error_of(&m, &late_segment).contains("mcp__"),
            "the server already there keeps its place: {:?}",
            m.failed()
        );
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

    /// Every real embedder calls `refresh_tools()` before it gets a chance to
    /// call `set_elicitation_callback` (see `runtime::agent::Builder::build()`),
    /// so setting the callback *after* `refresh_tools()` already ran must
    /// still reach the adapters it built.
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
        let mut manager = McpManager::from_clients(vec![client]).await;
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
        let ctx = platform::tool_context_for_test(std::env::temp_dir());
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_still_connecting_when_the_wait_ends_is_finished_later() {
        let mute = McpServerConfig::Stdio {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "while IFS= read -r line; do :; done".into()],
            env: HashMap::new(),
            scope: None,
            timeouts: crate::config::McpTimeouts {
                startup_timeout_ms: Some(600),
                ..Default::default()
            },
            working_dir: None,
        };
        let (mut m, late) = McpManager::connect_all_within(
            HashMap::from([("mute".to_string(), mute)]),
            Default::default(),
            None,
            Some(Duration::from_millis(100)),
        )
        .await;
        let names: Vec<&str> = m.connecting().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["mute"]);
        let mut late = late.expect("the mute server is still connecting");

        let outcome = late.next().await.expect("an outcome");
        assert!(outcome.connected().is_none());
        assert!(outcome.error().is_some_and(|e| e.contains("initialize")));
        let mut other = McpManager::empty();
        let mut copy = m.share();
        assert!(copy.absorb(outcome));
        assert!(copy.connecting().is_empty());
        assert_eq!(copy.failed()[0].name, "mute");
        assert!(late.next().await.is_none());

        assert!(!other.absorb(ConnectOutcome {
            result: Outcome::Failed {
                name: "mute".into(),
                error: "x".into()
            },
            batch: m.batch.clone(),
        }));
        assert!(m.absorb(ConnectOutcome {
            result: Outcome::Failed {
                name: "mute".into(),
                error: "x".into()
            },
            batch: m.batch.clone(),
        }));
    }

    #[tokio::test]
    async fn connect_all_returns_empty_for_empty_input() {
        let m = McpManager::connect_all(HashMap::new()).await;
        assert_eq!(m.server_count(), 0);
        assert_eq!(m.tool_count(), 0);
    }

    #[tokio::test]
    async fn server_instructions_are_bounded_per_server() {
        let long = "x".repeat(MAX_SERVER_INSTRUCTION_BYTES * 3);
        let client = Arc::new(
            crate::client::MockMcpClient::new("chatty", Vec::new()).with_instructions(long.clone()),
        );
        let m = McpManager::from_clients(vec![client]).await;
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

    #[tokio::test]
    async fn server_instructions_are_bounded_in_total_across_servers() {
        let long = "y".repeat(MAX_SERVER_INSTRUCTION_BYTES);
        let clients: Vec<crate::client::McpClientHandle> = (0..10)
            .map(|i| {
                Arc::new(
                    crate::client::MockMcpClient::new(format!("s{i}"), Vec::new())
                        .with_instructions(long.clone()),
                ) as crate::client::McpClientHandle
            })
            .collect();
        let m = McpManager::from_clients(clients).await;
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

    #[tokio::test]
    async fn server_instructions_never_split_a_multibyte_character() {
        // A byte-slice cut here would panic, since every character is 3 bytes.
        let long = "中".repeat(MAX_SERVER_INSTRUCTION_BYTES);
        let client =
            Arc::new(crate::client::MockMcpClient::new("cn", Vec::new()).with_instructions(long));
        let m = McpManager::from_clients(vec![client]).await;
        let out = m.server_instructions();
        assert!(out[0].instructions.starts_with("中中"));
    }

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
        let mut m = McpManager::from_clients(vec![client]).await;
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
        let mut m = McpManager::from_clients(vec![client.clone()]).await;
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
        let mut m = McpManager::from_clients(vec![up]).await;
        m.refresh_prompts().await;
        let entry = m.all_prompts().to_vec();

        let down = Arc::new(crate::client::MockMcpClient::new("github", Vec::new()).down())
            as crate::client::McpClientHandle;
        let mut m = McpManager::from_clients(vec![down]).await;
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
        McpManager::from_clients(vec![client]).await
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
        let m = McpManager::from_clients(vec![client]).await;
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
        let m = McpManager::from_clients(vec![client]).await;
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
mod refresh_tests {
    use super::*;
    use crate::client::{McpCallResult, McpPromptMeta, McpToolMeta};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A server whose answers to `tools/list` and `prompts/list` the test
    /// changes, and which counts how often it is asked.
    struct Scripted {
        name: String,
        tools: Mutex<Result<Vec<&'static str>, ()>>,
        prompts: Mutex<Result<Vec<&'static str>, ()>>,
        tool_lists: AtomicUsize,
        prompt_lists: AtomicUsize,
    }

    impl Scripted {
        fn new(name: &str, tools: &[&'static str]) -> Arc<Self> {
            Arc::new(Self {
                name: name.into(),
                tools: Mutex::new(Ok(tools.to_vec())),
                prompts: Mutex::new(Ok(Vec::new())),
                tool_lists: AtomicUsize::new(0),
                prompt_lists: AtomicUsize::new(0),
            })
        }

        fn lists(&self, tools: Result<&[&'static str], ()>) {
            *self.tools.lock().unwrap() = tools.map(<[_]>::to_vec);
        }

        fn lists_prompts(&self, prompts: &[&'static str]) {
            *self.prompts.lock().unwrap() = Ok(prompts.to_vec());
        }

        fn fail(&self) -> McpError {
            McpError::NotConnected {
                name: self.name.clone(),
            }
        }
    }

    #[async_trait::async_trait]
    impl McpClient for Scripted {
        fn server_name(&self) -> &str {
            &self.name
        }

        async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError> {
            self.tool_lists.fetch_add(1, Ordering::SeqCst);
            let tools = self
                .tools
                .lock()
                .unwrap()
                .clone()
                .map_err(|_| self.fail())?;
            Ok(tools
                .into_iter()
                .map(|name| McpToolMeta {
                    name: name.into(),
                    description: None,
                    input_schema: serde_json::json!({"type": "object"}),
                    hints: Default::default(),
                })
                .collect())
        }

        async fn call_tool(
            &self,
            _: &str,
            _: serde_json::Map<String, serde_json::Value>,
        ) -> Result<McpCallResult, McpError> {
            Err(self.fail())
        }

        async fn list_prompts(&self) -> Result<Vec<McpPromptMeta>, McpError> {
            self.prompt_lists.fetch_add(1, Ordering::SeqCst);
            let prompts = self
                .prompts
                .lock()
                .unwrap()
                .clone()
                .map_err(|_| self.fail())?;
            Ok(prompts
                .into_iter()
                .map(|name| McpPromptMeta {
                    name: name.into(),
                    description: None,
                    arguments: Vec::new(),
                })
                .collect())
        }
    }

    async fn manager_of(servers: &[&Arc<Scripted>]) -> McpManager {
        let clients = servers
            .iter()
            .map(|s| (*s).clone() as McpClientHandle)
            .collect();
        McpManager::from_clients(clients).await
    }

    fn tool_names(m: &McpManager) -> Vec<String> {
        let mut names: Vec<String> = m
            .tool_adapters()
            .iter()
            .map(|a| a.name().to_string())
            .collect();
        names.sort();
        names
    }

    fn announce(m: &McpManager, n: McpNotification) {
        m.announcements.record(&n);
    }

    #[tokio::test]
    async fn a_server_whose_list_fails_keeps_its_tools_and_one_that_lists_none_loses_them() {
        let flaky = Scripted::new("flaky", &["read"]);
        let shrinking = Scripted::new("shrinking", &["gone"]);
        let steady = Scripted::new("steady", &["ping"]);
        let mut m = manager_of(&[&flaky, &shrinking, &steady]).await;
        assert_eq!(
            tool_names(&m),
            [
                "mcp__flaky__read",
                "mcp__shrinking__gone",
                "mcp__steady__ping"
            ]
        );

        flaky.lists(Err(()));
        shrinking.lists(Ok(&[]));
        m.refresh_tools().await;
        assert_eq!(tool_names(&m), ["mcp__flaky__read", "mcp__steady__ping"]);

        let mut only_failing = manager_of(&[&flaky]).await;
        flaky.lists(Ok(&["read"]));
        only_failing.refresh_tools().await;
        flaky.lists(Err(()));
        only_failing.refresh_tools().await;
        assert_eq!(tool_names(&only_failing), ["mcp__flaky__read"]);

        steady.lists(Ok(&[]));
        let mut all_empty = manager_of(&[&steady]).await;
        steady.lists(Ok(&["ping"]));
        all_empty.refresh_tools().await;
        steady.lists(Ok(&[]));
        all_empty.refresh_tools().await;
        assert!(
            tool_names(&all_empty).is_empty(),
            "{:?}",
            tool_names(&all_empty)
        );
    }

    #[tokio::test]
    async fn nothing_is_listed_again_without_an_announcement() {
        let s = Scripted::new("s", &["a"]);
        let mut m = manager_of(&[&s]).await;
        let asked = s.tool_lists.load(Ordering::SeqCst);
        for _ in 0..3 {
            assert!(!m.refresh_if_changed().await);
        }
        assert_eq!(s.tool_lists.load(Ordering::SeqCst), asked);
    }

    #[tokio::test]
    async fn an_announced_tool_change_lists_that_server_once() {
        let changed = Scripted::new("changed", &["a"]);
        let quiet = Scripted::new("quiet", &["q"]);
        let mut m = manager_of(&[&changed, &quiet]).await;
        let quiet_asked = quiet.tool_lists.load(Ordering::SeqCst);

        changed.lists(Ok(&["a", "b"]));
        announce(
            &m,
            McpNotification::ToolListChanged {
                server: "changed".into(),
            },
        );
        assert!(m.refresh_if_changed().await);
        assert_eq!(
            tool_names(&m),
            ["mcp__changed__a", "mcp__changed__b", "mcp__quiet__q"]
        );
        assert_eq!(quiet.tool_lists.load(Ordering::SeqCst), quiet_asked);
        assert!(
            !m.refresh_if_changed().await,
            "one announcement is one listing"
        );
    }

    #[tokio::test]
    async fn every_session_shared_from_a_manager_sees_the_announcement() {
        let s = Scripted::new("s", &["a"]);
        let central = manager_of(&[&s]).await;
        let mut first = central.share();
        first.refresh_tools().await;
        let mut second = central.share();
        second.refresh_tools().await;

        s.lists(Ok(&["a", "b"]));
        let sink = central.announcements.sink();
        sink(McpNotification::ToolListChanged { server: "s".into() });

        assert!(first.refresh_if_changed().await);
        assert!(second.refresh_if_changed().await);
        assert_eq!(tool_names(&first), ["mcp__s__a", "mcp__s__b"]);
        assert_eq!(tool_names(&second), ["mcp__s__a", "mcp__s__b"]);
    }

    #[tokio::test]
    async fn an_announced_prompt_change_lists_the_prompts_again_and_a_resource_change_nothing() {
        let s = Scripted::new("s", &["a"]);
        let mut m = manager_of(&[&s]).await;
        let (tools_asked, prompts_asked) = (
            s.tool_lists.load(Ordering::SeqCst),
            s.prompt_lists.load(Ordering::SeqCst),
        );

        announce(
            &m,
            McpNotification::ResourceListChanged { server: "s".into() },
        );
        assert!(!m.refresh_if_changed().await);

        s.lists_prompts(&["review"]);
        announce(
            &m,
            McpNotification::PromptListChanged { server: "s".into() },
        );
        assert!(m.refresh_if_changed().await);
        assert_eq!(s.tool_lists.load(Ordering::SeqCst), tools_asked);
        assert_eq!(s.prompt_lists.load(Ordering::SeqCst), prompts_asked + 1);
        assert!(m.find_prompt_command("mcp__s__review").is_some());
    }
}
