//! `SessionPool` 的构造：`SessionPool::builder(..)` 收集宿主给的部件，`build()`
//! 用 `Arc::new_cyclic` 一次建成，池从构造起就持有指向自己的 `Weak`。

use super::*;

pub(crate) struct SessionPoolBuilder {
    cap: usize,
    idle_timeout: Duration,
    client: Arc<dyn AnthropicClient>,
    settings: Arc<LoadedSettings>,
    scene: Arc<SceneSpec>,
    host_policy: Option<Arc<dyn PermissionPolicy>>,
    cwd: PathBuf,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    paths: Arc<dyn crate::config::DaemonPaths>,
    credentials: Arc<dyn base::interface::credentials::CredentialSource>,
    managed: Option<(PathBuf, Option<base::managed_settings::ManagedSettings>)>,
    host_overrides: HostOverrides,
    permission_prompt_timeout: Option<Duration>,
    rust_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    customize: Option<SessionCustomizer>,
    mcp: Option<McpSetup>,
}

impl SessionPool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn builder(
        cap: usize,
        idle_timeout_secs: u64,
        client: Arc<dyn AnthropicClient>,
        settings: Arc<LoadedSettings>,
        scene: Arc<SceneSpec>,
        host_policy: Option<Arc<dyn PermissionPolicy>>,
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
            host_policy,
            cwd,
            history_store,
            paths,
            credentials: Arc::new(base::interface::credentials::ConfigCredentials),
            managed: None,
            host_overrides: HostOverrides::default(),
            permission_prompt_timeout: None,
            rust_plugins: Vec::new(),
            customize: None,
            mcp: None,
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

    /// What `host` — the settings the host handed over, before the managed
    /// layer — sets differently from `on_disk`, the same pair read from disk:
    /// laid over every settings this pool loads from disk, so a value the
    /// host set in memory reaches every project and survives a reload. The
    /// rules the host added stay on this daemon's own pair.
    pub(crate) fn host_changes(mut self, on_disk: &LoadedSettings, host: &LoadedSettings) -> Self {
        self.host_overrides = HostOverrides::between(on_disk, host);
        self
    }

    /// How long an unanswered permission prompt may block a turn before the
    /// daemon denies it on the client's behalf, over every session's
    /// `execution.permission_prompt_timeout_secs`; `None` leaves that setting
    /// to decide. `Duration::ZERO` waits forever, the engine's own behavior.
    pub(crate) fn permission_prompt_timeout(mut self, timeout: Option<Duration>) -> Self {
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

    /// What the host decided about MCP; without it sign-ins are kept under
    /// the user data directory and every server is admitted.
    pub(crate) fn mcp(mut self, setup: McpSetup) -> Self {
        self.mcp = Some(setup);
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
        let settings = parts::bindings::own_settings_in(
            &self.cwd,
            settings,
            self.paths.as_ref(),
            &self.host_overrides,
            managed_settings.clone(),
        );
        let mcp = McpHub::new(self.mcp, &settings.paths.user_data_dir);
        let _ = std::fs::create_dir_all(&settings.paths.user_data_dir);
        let scheduler = Scheduler::load(settings.paths.user_data_dir.join("scheduled_tasks.json"));

        let paths = self.paths.clone();
        let plugins = Arc::new(crate::plugins::PluginSubsystem::new(
            paths.clone(),
            settings.values.plugins.clone(),
        ));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools::register_builtin_tools(&tools);

        let scenes = SceneCatalog::new(self.scene, paths.global_root().join("scenes"));

        let events = DaemonEvents::new(&settings);
        let watch = match crate::watch_hub::WatchHub::new() {
            Ok(hub) => Some(hub),
            Err(e) => {
                warn!(error = %e, "config file watching unavailable; changes need config.reload");
                None
            }
        };
        let specs = SpecStore::new(self.history_store.clone(), self.cap);
        let factory = EngineFactory::new(
            tools,
            self.host_policy,
            self.rust_plugins,
            self.customize,
            self.permission_prompt_timeout,
            self.history_store.clone(),
        );
        let bindings = BindingCache::new(
            settings,
            self.host_overrides,
            ManagedLayer {
                path: managed_settings_path,
                settings: managed_settings,
            },
            watch,
            self.credentials,
            self.client,
            paths,
            self.cwd,
            plugins.clone(),
        );

        Arc::new_cyclic(|this| SessionPool {
            live: LiveSessions::new(self.cap, self.idle_timeout),
            scenes,
            directory: SessionDirectory::new(self.history_store),
            mcp,
            scheduler,
            this: this.clone(),
            events,
            health: std::sync::RwLock::new(base::interface::health::HealthChecks::new()),
            plugins,
            bindings,
            specs,
            factory,
        })
    }
}
