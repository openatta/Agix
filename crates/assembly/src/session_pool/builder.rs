//! `SessionPool` 的构造：`SessionPool::builder(..)` 收集宿主给的部件，`build()`
//! 用 `Arc::new_cyclic` 一次建成，池从构造起就持有指向自己的 `Weak`。

use super::*;

pub(crate) struct SessionPoolBuilder {
    cap: usize,
    idle_timeout: Duration,
    client: Arc<dyn AnthropicClient>,
    settings: Arc<LoadedSettings>,
    scene: Arc<SceneSpec>,
    permission: Option<PermissionSet>,
    cwd: PathBuf,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    paths: Arc<dyn crate::config::DaemonPaths>,
    credentials: Arc<dyn base::interface::credentials::CredentialSource>,
    managed: Option<(PathBuf, Option<base::managed_settings::ManagedSettings>)>,
    permission_prompt_timeout: Duration,
    rust_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    customize: Option<SessionCustomizer>,
    #[cfg(feature = "mcp")]
    mcp: McpParts,
}

#[cfg(feature = "mcp")]
#[derive(Default)]
struct McpParts {
    credential_store: Option<Arc<dyn mcp::oauth::McpCredentialStore>>,
    oauth_client: Option<mcp::oauth::McpOAuthClient>,
    policy: Option<mcp::policy::McpPolicyGate>,
    enterprise_idp: Option<mcp::config::EnterpriseIdp>,
}

#[cfg(feature = "mcp")]
impl McpParts {
    fn into_auth(
        self,
        settings: &LoadedSettings,
    ) -> (Arc<mcp::oauth::McpAuth>, Arc<mcp::policy::McpPolicyGate>) {
        let auth = match self.credential_store {
            Some(store) => mcp::oauth::McpAuth::new(store),
            None => mcp::oauth::McpAuth::with_file_store(
                settings.paths.user_data_dir.join("mcp").join("oauth"),
            ),
        };
        let auth = match self.oauth_client {
            Some(client) => auth.with_client(client),
            None => auth,
        };
        let policy = Arc::new(self.policy.unwrap_or_else(mcp::policy::McpPolicyGate::open));
        auth.set_policy(policy.clone());
        auth.set_enterprise_idp(self.enterprise_idp);
        (Arc::new(auth), policy)
    }
}

impl SessionPool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn builder(
        cap: usize,
        idle_timeout_secs: u64,
        client: Arc<dyn AnthropicClient>,
        settings: Arc<LoadedSettings>,
        scene: Arc<SceneSpec>,
        permission: Option<PermissionSet>,
        cwd: PathBuf,
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
        paths: Arc<dyn crate::config::DaemonPaths>,
    ) -> SessionPoolBuilder {
        SessionPoolBuilder {
            cap,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            client,
            settings,
            scene,
            permission,
            cwd,
            history_store,
            paths,
            credentials: Arc::new(base::interface::credentials::ConfigCredentials),
            managed: None,
            permission_prompt_timeout: DEFAULT_PERMISSION_PROMPT_TIMEOUT,
            rust_plugins: Vec::new(),
            customize: None,
            #[cfg(feature = "mcp")]
            mcp: McpParts::default(),
        }
    }
}

impl SessionPoolBuilder {
    /// Resolve provider keys through `credentials` instead of only what each
    /// provider entry says.
    pub(crate) fn credentials(
        mut self,
        credentials: Arc<dyn base::interface::credentials::CredentialSource>,
    ) -> Self {
        self.credentials = credentials;
        self
    }

    /// The deployment's managed settings, read from `path`: put on top of the
    /// pool's own settings now and of every settings it loads later.
    pub(crate) fn managed_settings(
        mut self,
        path: PathBuf,
        managed: Option<base::managed_settings::ManagedSettings>,
    ) -> Self {
        self.managed = Some((path, managed));
        self
    }

    /// How long an unanswered permission prompt may block a turn before the
    /// daemon denies it on the client's behalf. `Duration::ZERO` waits
    /// forever, the engine's own behavior.
    pub(crate) fn permission_prompt_timeout(mut self, timeout: Duration) -> Self {
        self.permission_prompt_timeout = timeout;
        self
    }

    /// Build every session engine with these Rust plugins.
    pub(crate) fn plugins(mut self, plugins: Vec<Arc<dyn runtime::plugin::Plugin>>) -> Self {
        self.rust_plugins = plugins;
        self
    }

    /// Hand every session engine this pool builds to `customize` last.
    pub(crate) fn session_customizer(mut self, customize: SessionCustomizer) -> Self {
        self.customize = Some(customize);
        self
    }

    /// Keep MCP sign-ins in `store` instead of files under the user data
    /// root, and present `client` to authorization servers.
    #[cfg(feature = "mcp")]
    pub(crate) fn mcp_auth(
        mut self,
        store: Option<Arc<dyn mcp::oauth::McpCredentialStore>>,
        client: Option<mcp::oauth::McpOAuthClient>,
    ) -> Self {
        self.mcp.credential_store = store;
        self.mcp.oauth_client = client;
        self
    }

    #[cfg(feature = "mcp")]
    pub(crate) fn mcp_policy(mut self, policy: mcp::policy::McpPolicyGate) -> Self {
        self.mcp.policy = Some(policy);
        self
    }

    /// The enterprise IdP `"grant": "enterprise"` MCP servers exchange the
    /// user's identity at.
    #[cfg(feature = "mcp")]
    pub(crate) fn mcp_enterprise_idp(mut self, idp: Option<mcp::config::EnterpriseIdp>) -> Self {
        self.mcp.enterprise_idp = idp;
        self
    }

    pub(crate) fn build(self) -> Arc<SessionPool> {
        let (managed_settings_path, managed_settings, settings) = match self.managed {
            Some((path, managed)) => {
                let settings = Arc::new((*self.settings).clone().with_managed(managed.clone()));
                (path, managed, settings)
            }
            None => (
                base::managed_settings::managed_settings_path(),
                None,
                self.settings,
            ),
        };
        let (events_tx, _) = tokio::sync::broadcast::channel(1024);
        #[cfg(feature = "mcp")]
        let (mcp_auth, mcp_policy) = self.mcp.into_auth(&settings);
        let _ = std::fs::create_dir_all(&settings.paths.user_data_dir);
        let scheduler = Scheduler::load(settings.paths.user_data_dir.join("scheduled_tasks.json"));

        let paths = self.paths;
        let scene = self.scene;
        let plugins = crate::plugins::PluginSubsystem::new(paths.clone(), settings.plugins.clone());
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools::register_builtin_tools(&tools);

        let host_scenes = vec![scene.clone()];
        let scene_registry = std::sync::RwLock::new(Arc::new(
            registry_of(&host_scenes, &[], &paths.global_root().join("scenes"), &[]).0,
        ));
        let mut active_scenes = HashMap::new();
        active_scenes.insert(scene.id.clone(), scene.clone());

        let pool_telemetry = build_pool_telemetry(&settings);
        let specs = SpecStore::new(self.history_store.clone(), self.cap);

        Arc::new_cyclic(|this| SessionPool {
            sessions: AsyncMutex::new(HashMap::new()),
            session_locks: AsyncMutex::new(HashMap::new()),
            cap: self.cap,
            idle_timeout: self.idle_timeout,
            client: self.client,
            settings: AsyncRwLock::new(settings),
            scene,
            active_scenes: AsyncRwLock::new(active_scenes),
            scene_registry,
            host_scenes: std::sync::RwLock::new(host_scenes),
            registered_scenes: std::sync::RwLock::new(Vec::new()),
            permission: self.permission,
            tools,
            cwd: self.cwd,
            history_store: self.history_store,
            paths,
            #[cfg(feature = "mcp")]
            mcp_by_project: mcp_cache::McpCache::default(),
            #[cfg(feature = "mcp")]
            mcp_auth,
            #[cfg(feature = "mcp")]
            mcp_policy,
            managed_settings_path,
            managed_settings,
            scheduler,
            this: this.clone(),
            events_tx,
            health: std::sync::RwLock::new(base::interface::health::HealthChecks::new()),
            plugins,
            last_good_routers: AsyncMutex::new(HashMap::new()),
            bindings: AsyncMutex::new(HashMap::new()),
            watch: match crate::watch_hub::WatchHub::new() {
                Ok(hub) => Some(hub),
                Err(e) => {
                    warn!(error = %e, "config file watching unavailable; changes need config.reload");
                    None
                }
            },
            pool_watch: std::sync::Mutex::new(Vec::new()),
            specs,
            stopping: std::sync::Mutex::new(HashMap::new()),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            generations: Generations::default(),
            shut_down: CancellationToken::new(),
            served: std::sync::Mutex::new(Vec::new()),
            permission_prompt_timeout: self.permission_prompt_timeout,
            credentials: self.credentials,
            rust_plugins: self.rust_plugins,
            customize: self.customize,
            pool_telemetry,
        })
    }
}
