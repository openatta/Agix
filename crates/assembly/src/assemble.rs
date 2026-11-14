//! Building a running engine out of a configuration.
//!
//! The order these pieces go together in, and what they default to, is the
//! engine's knowledge rather than its embedder's: read the settings, resolve
//! the provider routing, build the credentials and the client, place the
//! history roots, migrate an old layout, build the [`SessionPool`], then run
//! the startup steps a session silently misses without — plugin components,
//! MCP connection, the janitor. A step nobody performs is no compile error, so
//! this is the one place that knows: `main.rs` is a command line and a set of
//! listeners on top of it, and a host embedding the daemon hands over a
//! [`DaemonConfig`] and gets back an engine.
//!
//! What stays out: anything with a process to itself. Discovery locks,
//! listeners, signal handling and startup telemetry belong to whatever *runs*
//! a daemon, and a host embedding one has its own answers for all four.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base::interface::credentials::CredentialSource;
use base::interface::permission::PermissionPolicy;
use base::interface::scene::SceneSpec;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use tracing::{info, warn};

use crate::config::DaemonConfig;
pub use crate::session_pool::McpOptions;
use crate::session_pool::{SessionCustomizer, SessionPool, DEFAULT_PERMISSION_PROMPT_TIMEOUT};

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
    pub scenes: Vec<Arc<SceneSpec>>,
    /// How long an unanswered permission prompt may block a turn, over every
    /// session's `execution.permission_prompt_timeout_secs`. Left at
    /// [`DEFAULT_PERMISSION_PROMPT_TIMEOUT`] it counts as not given, and the
    /// settings decide (300 seconds when they say nothing either).
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
    /// The host's own say on every call, weighed as one more tier of rules:
    /// its deny stands in every mode, its permit is one allow rule.
    pub host_policy: Option<Arc<dyn PermissionPolicy>>,
    /// Rust plugins every session this pool builds is built with, in order.
    pub plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    /// The host's last word on every session engine — see
    /// [`SessionCustomizer`].
    pub customize_session: Option<SessionCustomizer>,
    /// MCP: where sign-ins are kept, the OAuth client, the enterprise IdP
    /// and the policy. An empty struct in a build without the `mcp` module.
    pub mcp: McpOptions,
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
            host_policy: None,
            plugins: Vec::new(),
            customize_session: None,
            mcp: McpOptions::default(),
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
    scene: Arc<SceneSpec>,
    assembly: Assembly,
) -> anyhow::Result<Arc<SessionPool>> {
    let (managed_path, managed) = managed_settings(config);
    let settings = Arc::new(config.settings.clone().with_managed(managed.clone()));
    for (section, module) in crate::modules::unassembled_settings(&settings.values) {
        tracing::warn!(
            section,
            module,
            "settings configure a module this build does not include; ignored"
        );
    }

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
    if matches!(
        settings.values.permission_mode,
        base::settings::PermissionMode::BypassPermissions
    ) {
        warn!(
            "settings.permission_mode = bypassPermissions — no permission rule is consulted for \
             any tool call in any session; this daemon is trusting its host to sandbox it"
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

    let mut pool = SessionPool::builder(
        config.session_cap,
        config.session_idle_timeout_secs,
        client,
        settings,
        scene,
        assembly.host_policy,
        cwd,
        history_store,
        config.paths.clone(),
    )
    .permission_prompt_timeout(
        (assembly.permission_prompt_timeout != DEFAULT_PERMISSION_PROMPT_TIMEOUT)
            .then_some(assembly.permission_prompt_timeout),
    )
    .credentials(credentials)
    .managed_settings(managed_path, managed)
    .host_changes(&own_settings_on_disk(config), &config.settings)
    .plugins(assembly.plugins);
    if let Some(customize) = assembly.customize_session {
        pool = pool.session_customizer(customize);
    }
    pool = pool.mcp(assembly.mcp.resolve(config.mcp_policy_path.as_deref())?);
    let pool = pool.build();

    for scene in assembly.scenes {
        pool.register_scene(scene);
    }
    for extra in &assembly.extra_scenes {
        if extra == pool.scene_id() {
            continue; // already active; not an error to repeat it
        }
        pool.activate_scene(extra)
            .await
            .map_err(|e| anyhow::anyhow!("scene `{extra}`: {e}"))?;
    }

    // Before anything can create a session: a session built while this is
    // still running would be missing every plugin tool, with nothing to
    // indicate why.
    pool.load_plugins().await;

    // Backgrounded: a slow or unreachable server can never delay startup, and
    // a failure warns and emits `mcp_connect_failed` rather than aborting.
    pool.connect_mcp_servers_in_background();

    pool.start_janitor();
    pool.start_config_watcher();
    pool.start_scheduler();

    Ok(pool)
}

/// The daemon's own pair's settings as they are on disk, read the way
/// `load_daemon_config` read them — `--max-tokens` fallback included — so
/// what differs from `config.settings` is what the host changed afterwards.
fn own_settings_on_disk(config: &DaemonConfig) -> base::settings::LoadedSettings {
    let paths = &config.paths;
    let mut on_disk = base::settings::LoadedSettings::load(
        paths.global_root(),
        paths.config_root(),
        paths.project_root().join(".agix"),
        &config.settings.paths.scope,
        &config.settings.values.model.model_name,
    );
    if on_disk.values.model.max_tokens == base::settings::DEFAULT_MAX_TOKENS {
        on_disk.values.model.max_tokens = config.settings.values.model.max_tokens;
    }
    on_disk
}

/// The managed settings file this pool reads — the one the config names,
/// which must exist, else the platform's — and what is in it. Settings that
/// already carry a managed layer (an embedder's `Tier::Managed` source) keep
/// it when there is no file.
fn managed_settings(
    config: &DaemonConfig,
) -> (PathBuf, Option<base::managed_settings::ManagedSettings>) {
    use base::managed_settings::{managed_settings_path, ManagedSettings};
    let (path, required) = match &config.managed_settings_path {
        Some(path) => (path.clone(), true),
        None => (managed_settings_path(), false),
    };
    let managed =
        ManagedSettings::read(&path, required).or_else(|| config.settings.managed.clone());
    (path, managed)
}

/// Multi-provider routing, or `None` when the deployment configured none.
///
/// Purely additive: with no `providers`, sub-agent spawns inherit the one
/// client built above. An invalid configuration fails the assembly instead of starting a
/// daemon that silently routes everything to the default.
fn validate_task_router_config(
    config: &DaemonConfig,
    credentials: &Arc<dyn CredentialSource>,
) -> anyhow::Result<()> {
    if config.settings.values.providers.is_empty() {
        return Ok(());
    }
    let (resolved, warnings) = base::provider::resolve_task_models(
        &config.settings.values.providers,
        config.settings.values.default_provider.as_deref(),
        &config.settings.values.task_models,
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
        .values
        .default_provider
        .as_deref()
        .expect("resolve_task_models already validated default_provider is set");
    crate::model_router::build_task_router(
        &config.settings.values.providers,
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
