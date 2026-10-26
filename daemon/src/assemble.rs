//! Building a running engine out of a configuration.
//!
//! The order these pieces go together in, and what they default to, is the
//! engine's knowledge rather than its embedder's. Before this module every
//! host had to know it: read the settings, resolve the provider routing,
//! build the credentials and the client, place the history roots, migrate an
//! old layout, hand eleven positional arguments to [`SessionPool::new`] in
//! the right order, then remember the three startup steps a session silently
//! misses without — plugin components, MCP connection, the janitor.
//!
//! That knowledge was copied. `main.rs` had it, the test harness had it
//! ("`SessionPool::new` the way `main.rs` does", it said), and every
//! daemon-level test file had a shortened version of its own. A copy is not
//! merely duplication here: when the assembly changes, nothing tells the
//! copies. There is no compile error for a step nobody performs.
//!
//! So this is the one place that knows. `main.rs` is a command line and a set
//! of listeners on top of it, and a host embedding the daemon hands over a
//! [`DaemonConfig`] and gets back an engine.
//!
//! What stays out: anything with a process to itself. Discovery locks,
//! listeners, signal handling and startup telemetry belong to whatever *runs*
//! a daemon, and a host embedding one has its own answers for all four.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base::interface::credentials::CredentialSource;
use base::interface::permission::{Permission, PermissionOutcome};
use base::interface::scene::AgentScene;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use tracing::{info, warn};

use crate::config::DaemonConfig;
use crate::session_pool::{SessionCustomizer, SessionPool, DEFAULT_PERMISSION_PROMPT_TIMEOUT};

/// Allow-everything permission — **the opt-out**, no longer the default.
///
/// The original design comment here read "always-allow permission for daemon
/// sessions (IDE plugins manage their own sandbox)". That was a real
/// decision, not an oversight: the daemon's callers were assumed to be
/// editor plugins that had already sandboxed the workspace themselves, so a
/// second permission layer inside the engine would only have been friction,
/// and nothing on the wire could answer a prompt anyway.
///
/// **That boundary moved on purpose.** Two things changed underneath it.
/// First, the daemon grew a real answer channel — `session.respondToPrompt`
/// plus the `kind:"prompt"` stream frame — so "stop and ask" is now
/// expressible over JSON-RPC instead of being a dead end. Second, the
/// caller population stopped being "editor plugins we shipped": anything
/// that can open the socket gets a fully-tooled agent with Bash and file
/// writes, and "the host sandboxes it" became an assumption the daemon was
/// making on the host's behalf rather than one the host had stated.
///
/// So the default is now `PermissionMode::Default` (ask), and a host that
/// genuinely does sandbox the daemon states so explicitly — either
/// `"permission_mode": "bypassPermissions"` in any settings.json tier
/// (daemon-wide) or `options.permission_mode: "bypassPermissions"` on
/// `session.run_turn` (one session). Both resolve to *this* instance; see
/// `daemon::session_pool::resolve_session_permission` and
/// `docs/daemon_rpc_protocol.md`'s "权限默认值" section.
///
/// Kept as a distinct type rather than folded into `RuleSetPermission`'s
/// bypass mode because the two aren't quite the same: `PermissionGate` keeps
/// a small bypass-immune deny list (`is_path_bypass_immune`) that still
/// fires under `bypassPermissions`, whereas this really does permit
/// everything — which is what "I have my own sandbox" asks for.
///
/// The pool holds one instance and hands it out only to sessions whose
/// effective `permission_mode` is `bypassPermissions`; everything else gets a
/// real `RuleSetPermission` built from the settings.
pub struct AllowAllPermission;

#[async_trait::async_trait]
impl Permission for AllowAllPermission {
    async fn check(
        &self,
        _tool: &str,
        _input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> PermissionOutcome {
        PermissionOutcome::Permit
    }
}

/// The parts of an assembly a caller may legitimately differ on.
///
/// Deliberately small. Every field here is either something the process knows
/// and the configuration cannot (the working directory), or a seam a test or
/// a host genuinely substitutes (the model, the store, the permission). The
/// rest of the assembly is not configurable, because a host choosing its own
/// order for it is the problem this module exists to remove.
pub struct Assembly {
    /// The project new sessions belong to when they name none. `None` is the
    /// process's own working directory.
    pub cwd: Option<PathBuf>,
    /// Scenes to activate beyond the one the pool is built with. An unknown
    /// name fails the assembly rather than quietly starting with fewer
    /// scenes than asked for.
    pub extra_scenes: Vec<String>,
    /// Scenes the host defines beyond the built-in ones. Registered before
    /// `extra_scenes` is read, so they can be named there; otherwise they are
    /// available to `scene.activate`.
    pub scenes: Vec<Arc<dyn AgentScene>>,
    /// How long an unanswered permission prompt may block a turn.
    pub permission_prompt_timeout: Duration,
    /// Where model calls go. `None` builds an Anthropic client keyed from
    /// `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`, else from `credentials`
    /// asked for provider `"default"`.
    pub model_client: Option<Arc<dyn AnthropicClient>>,
    /// Keys for whatever the configuration leaves unset: a provider entry
    /// with neither `api_key` nor `api_key_env` is answered from here, as is
    /// the default client when the environment has no key. For a host that
    /// keeps credentials in a vault or keychain rather than on disk.
    pub credentials: Option<Arc<dyn base::interface::credentials::CredentialSource>>,
    /// Where transcripts live.
    pub transcripts: Transcripts,
    /// What a `bypassPermissions` session runs under. `None` is
    /// [`AllowAllPermission`].
    pub permission: Option<Arc<dyn Permission>>,
    /// The host's last word on every session engine — see
    /// [`SessionCustomizer`].
    pub customize_session: Option<SessionCustomizer>,
    /// Resolves OAuth bearer tokens for MCP servers with an `oauth_provider`
    /// when this pool's token store has none. `None`: stored tokens only.
    pub mcp_oauth_resolver: Option<Arc<dyn mcp::oauth::McpOAuthResolver>>,
}

impl Default for Assembly {
    fn default() -> Self {
        Self {
            cwd: None,
            extra_scenes: Vec::new(),
            scenes: Vec::new(),
            permission_prompt_timeout: DEFAULT_PERMISSION_PROMPT_TIMEOUT,
            model_client: None,
            credentials: None,
            transcripts: Transcripts::default(),
            permission: None,
            customize_session: None,
            mcp_oauth_resolver: None,
        }
    }
}

/// Where a pool keeps transcripts.
///
/// `Nowhere` is a real mode, not a test shortcut: it is what a daemon falls
/// into when the store cannot be opened at startup, and a session read or
/// branch has no possible answer there. Expressible here so that mode is
/// reachable through this entry point rather than only by assembling a pool
/// by hand — which is the thing this module exists to stop.
#[derive(Default)]
pub enum Transcripts {
    /// Under the config's own global root, migrating a pre-0.1.5 layout
    /// first.
    #[default]
    UnderGlobalRoot,
    /// This store, wherever the caller put it.
    In(Arc<dyn history::store::HistoryStore>),
    /// No persistence at all.
    Nowhere,
}

/// A pool that is built, wired and serving.
///
/// Several pools can share one process, each on its own configuration. The
/// one exception is the OpenTelemetry exporter (`otel` feature): it installs
/// process-wide providers, so the first endpoint any pool starts it on is the
/// one every pool reports to, and a different endpoint is warned about once.
///
/// "Serving" is the part a caller cannot see and would not think to do: the
/// returned pool has already loaded its plugin components, kicked off MCP
/// connection in the background, and started its janitor. A pool missing any
/// of those looks identical and behaves subtly worse — sessions with no
/// plugin tools, servers that never connect, sessions that never expire.
pub async fn pool(
    config: &DaemonConfig,
    scene: Arc<dyn AgentScene>,
    assembly: Assembly,
) -> anyhow::Result<Arc<SessionPool>> {
    let settings = Arc::new(config.settings.clone());

    // Validation, not a value: every session's router is built by its own
    // `(project, scene)` binding from that pair's settings. What this still
    // does is fail startup loudly on a daemon whose own project names a
    // provider config that cannot resolve, rather than letting every session
    // quietly fall back to the plain client.
    let credentials: Arc<dyn CredentialSource> = match &assembly.credentials {
        Some(host) => Arc::new(base::interface::credentials::ConfigThen(host.clone())),
        None => Arc::new(base::interface::credentials::ConfigCredentials),
    };
    validate_task_router_config(config, &credentials)?;
    let client = match assembly.model_client {
        Some(client) => client,
        None => default_anthropic_client(assembly.credentials.as_deref())?,
    };
    let permission = assembly
        .permission
        .unwrap_or_else(|| Arc::new(AllowAllPermission));
    if matches!(
        config.settings.permission_mode,
        base::interface::settings::PermissionMode::BypassPermissions
    ) {
        warn!(
            "settings.permission_mode = bypassPermissions — every tool call in every session \
             runs unchecked; this daemon is trusting its host to sandbox it"
        );
    }

    let global_dir = config.settings.paths.global_data_dir.clone();

    let cwd = assembly
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let history_store = match assembly.transcripts {
        Transcripts::UnderGlobalRoot => default_history_store(&global_dir, &cwd).await,
        Transcripts::In(store) => Some(store),
        Transcripts::Nowhere => None,
    };

    let mut pool = SessionPool::new(
        config.session_cap,
        config.session_idle_timeout_secs,
        client,
        settings,
        scene,
        permission,
        cwd,
        history_store,
        config.paths.clone(),
    )
    .with_permission_prompt_timeout(assembly.permission_prompt_timeout)
    .with_credentials(credentials);
    if let Some(customize) = assembly.customize_session {
        pool = pool.with_session_customizer(customize);
    }
    if let Some(resolver) = assembly.mcp_oauth_resolver {
        pool = pool.with_mcp_oauth_resolver(resolver);
    }
    let pool = Arc::new(pool);

    for scene in assembly.scenes {
        pool.register_scene(scene);
    }
    for extra in &assembly.extra_scenes {
        if extra == pool.scene_id() {
            continue; // already active; not an error to repeat it
        }
        pool.activate_scene(extra)
            .await
            .map_err(|(_, message)| anyhow::anyhow!("scene `{extra}`: {message}"))?;
    }

    // Before anything can create a session: a session built while this is
    // still running would be missing every plugin tool, with nothing to
    // indicate why.
    pool.load_plugin_components().await;

    // Plugin-declared MCP servers (`plugin.toml`'s `[mcp] servers`) merge in
    // here so they take the same centrally-connected, shared-across-sessions
    // path user-configured ones do. Connecting is deliberately backgrounded:
    // a slow or unreachable server can never delay startup, and a failure
    // warns and emits `mcp_connect_failed` rather than aborting.
    let mut mcp_servers = config.settings.mcp_servers.clone();
    mcp_servers.extend(pool.plugin_mcp_servers().await);
    pool.connect_mcp_servers_in_background(mcp_servers);

    pool.start_janitor();
    pool.start_config_watcher();

    Ok(pool)
}

/// Multi-provider routing, or `None` when the deployment configured none.
///
/// Purely additive: with no `providers`, every session behaves exactly as it
/// did before routing existed — sub-agent spawns inherit the one client built
/// above. An invalid configuration fails the assembly instead of starting a
/// daemon that silently routes everything to the default.
fn validate_task_router_config(
    config: &DaemonConfig,
    credentials: &Arc<dyn CredentialSource>,
) -> anyhow::Result<()> {
    if config.settings.providers.is_empty() {
        return Ok(());
    }
    let (resolved, warnings) = base::provider::resolve_task_models(
        &config.settings.providers,
        config.settings.default_provider.as_deref(),
        &config.settings.task_models,
    )
    .map_err(|e| anyhow::anyhow!("invalid multi-provider LLM config: {e}"))?;
    for w in &warnings {
        warn!("model routing: {w}");
    }
    for (task, r) in &resolved {
        info!(task = %task, provider = %r.provider_id, model = %r.model, "model routing resolved");
    }
    let default_provider = config
        .settings
        .default_provider
        .as_deref()
        .expect("resolve_task_models already validated default_provider is set");
    crate::model_router::build_task_router(
        &config.settings.providers,
        default_provider,
        resolved,
        credentials,
    )
    .map_err(|e| anyhow::anyhow!("failed to build multi-provider model router: {e}"))?;
    Ok(())
}

/// The client used when no provider routing applies: keyed from the
/// environment, else from the host's source, asked for provider `"default"`.
fn default_anthropic_client(
    host: Option<&dyn CredentialSource>,
) -> anyhow::Result<Arc<dyn AnthropicClient>> {
    let none = base::provider::ProviderConfig::default();
    let api_key = base::interface::credentials::EnvCredentials::anthropic()
        .api_key("default", &none)
        .or_else(|env| match host {
            Some(host) => host.api_key("default", &none),
            None => Err(env),
        })
        .map_err(|_| {
            anyhow::anyhow!(
                "set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY, or have the host supply credentials"
            )
        })?;
    let auth = AuthMode::ApiKey(api_key.expose().to_string());
    Ok(match std::env::var("ANTHROPIC_BASE_URL").ok() {
        Some(mut url) => {
            // Ensure trailing slash so Url::join appends instead of replacing
            if !url.ends_with('/') {
                url.push('/');
            }
            let base = reqwest::Url::parse(&url)
                .map_err(|e| anyhow::anyhow!("invalid ANTHROPIC_BASE_URL: {e}"))?;
            Arc::new(HttpAnthropicClient::with_base(auth, base)?)
        }
        None => Arc::new(HttpAnthropicClient::new(auth)?),
    })
}

/// Transcripts under `global_dir`, with a pre-0.1.5 layout relocated first.
///
/// `None` — logged, not fatal — when the store cannot be opened: a daemon
/// that runs without persistence is degraded, and one that refuses to start
/// is unavailable.
async fn default_history_store(
    global_dir: &std::path::Path,
    cwd: &std::path::Path,
) -> Option<Arc<dyn history::store::HistoryStore>> {
    // Before the store is built against the new layout, not after — otherwise
    // a returning user's existing sessions are simply not where anything
    // looks for them. Idempotent and move-only; a clean tree is a no-op.
    let migration = history::migrate::migrate_layout(global_dir);
    if !migration.did_nothing() {
        info!(
            transcripts = migration.transcripts_moved,
            sidecars = migration.sidecars_moved,
            skipped = migration.skipped_existing,
            failed = migration.failed,
            "migrated session state to the projects/ + sessions/ layout"
        );
    }

    let roots = history::path::HistoryRoots::under(global_dir);
    match history::store::JsonlHistoryStore::with_roots(cwd, roots).await {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            warn!(error = %e, "failed to initialize session history store; sessions will be in-memory only for this run");
            None
        }
    }
}
