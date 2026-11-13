//! 设置与绑定 `BindingCache`：守护进程自己那一对的设置（`config.reload` 热替换的那份），
//! 每个 `(project, scene)` 对的 [`ProjectBinding`]，以及看着它们来源文件的监视。
//! 宿主覆盖层和托管设置层只在这里叠加。
//!
//! 锁序：`bindings` 在前，`last_good_routers` 在后（都是 tokio `Mutex`）；`settings`
//! 是 tokio `RwLock`，只在取值或替换的那一下持有；`epochs` 和 `pool_watch` 是 std 叶子
//! 锁。持着 `bindings` 时不读盘，也不调别的协作者。

mod binding;
mod host_overrides;

pub(in crate::session_pool) use binding::{
    tier_covers, BindingStamp, ProjectBinding, ProjectSceneKey,
};
pub(in crate::session_pool) use host_overrides::HostOverrides;

use crate::session_pool::off_the_runtime;
use crate::watch_hub::{Tier, WatchHub, WatchKey, WatchTarget, WatchToken};
use base::settings::LoadedSettings;
use model::client::AnthropicClient;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock};

/// Where this pool looked for the deployment's managed settings, and what it
/// found there: applied on top of every settings it loads.
pub(in crate::session_pool) struct ManagedLayer {
    pub(in crate::session_pool) path: PathBuf,
    pub(in crate::session_pool) settings: Option<base::managed_settings::ManagedSettings>,
}

/// What [`BindingCache::cached`] found for a pair.
pub(in crate::session_pool) enum Cached {
    Ready(Arc<ProjectBinding>),
    /// A settings file, skill or agent-type directory of one of the pair's
    /// tiers appeared since its binding was built; the binding is still
    /// cached. `own` says whether the pair is this daemon's own.
    Appeared {
        tier: Tier,
        own: bool,
    },
    Missing,
}

/// The bindings built so far, and every pair's epoch.
struct Cache {
    /// One binding per pair this pool has built a session for, built on
    /// demand and dropped by [`BindingCache::invalidate`]. The pool's own pair
    /// is in here like any other; `settings` stays the separate hot-swappable
    /// copy.
    ///
    /// Scene is part of the key because the scene tier is a real layer.
    /// Keying by project alone gave every session whichever scene happened to
    /// be built first.
    ///
    /// Holds only current bindings: `invalidate` bumps the epochs and drops
    /// the entries under this lock, and a build is inserted under it only if
    /// its epoch is still the pair's.
    bindings: AsyncMutex<HashMap<ProjectSceneKey, Arc<ProjectBinding>>>,
    /// Every pair a build has begun for, and how many times it has been
    /// invalidated since. Bumped only while `bindings` is held.
    epochs: std::sync::Mutex<HashMap<ProjectSceneKey, u64>>,
}

/// What is laid over every settings this pool loads from disk, in order.
struct Layers {
    /// What the host changed on this daemon's own pair's settings before
    /// handing them over. Every other pair, and this one reloaded from disk,
    /// would otherwise lose it.
    host: HostOverrides,
    managed: ManagedLayer,
}

pub(in crate::session_pool) struct BindingCache {
    /// This daemon's own pair's settings, hot-swapped by `config.reload` and
    /// `config.setProvider`. Sessions already running keep the snapshot they
    /// were built with; everything not tied to a session reads this one.
    settings: AsyncRwLock<Arc<LoadedSettings>>,
    layers: Layers,
    cache: Cache,
    /// The last router each pair was successfully served by, kept across
    /// invalidation so a provider config that stops resolving — or stops
    /// producing a buildable client — leaves working routing in place.
    last_good_routers: AsyncMutex<HashMap<ProjectSceneKey, Arc<base::provider::TaskRouter>>>,
    /// One `notify` watcher for every settings file every binding was merged
    /// from. `None` when the watcher could not be started: configuration
    /// changes then reach the daemon only through `config.reload`.
    watch: Option<WatchHub>,
    /// The watch on the tiers of this daemon's own pair, held for the pool's
    /// lifetime. Binding watches come and go with the bindings, and with every
    /// binding invalidated nothing would be left to notice the next change.
    pool_watch: std::sync::Mutex<Vec<WatchToken>>,
    credentials: Arc<dyn base::interface::credentials::CredentialSource>,
    /// The client a pair with no provider configured talks through.
    client: Arc<dyn AnthropicClient>,
    paths: Arc<dyn crate::config::DaemonPaths>,
    /// The project a session that names none belongs to.
    cwd: PathBuf,
    plugins: Arc<crate::plugins::PluginSubsystem>,
}

impl BindingCache {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::session_pool) fn new(
        settings: Arc<LoadedSettings>,
        host_overrides: HostOverrides,
        managed: ManagedLayer,
        watch: Option<WatchHub>,
        credentials: Arc<dyn base::interface::credentials::CredentialSource>,
        client: Arc<dyn AnthropicClient>,
        paths: Arc<dyn crate::config::DaemonPaths>,
        cwd: PathBuf,
        plugins: Arc<crate::plugins::PluginSubsystem>,
    ) -> Self {
        Self {
            settings: AsyncRwLock::new(settings),
            layers: Layers {
                host: host_overrides,
                managed,
            },
            cache: Cache {
                bindings: AsyncMutex::new(HashMap::new()),
                epochs: std::sync::Mutex::new(HashMap::new()),
            },
            last_good_routers: AsyncMutex::new(HashMap::new()),
            watch,
            pool_watch: std::sync::Mutex::new(Vec::new()),
            credentials,
            client,
            paths,
            cwd,
            plugins,
        }
    }

    pub(in crate::session_pool) async fn pool_settings(&self) -> Arc<LoadedSettings> {
        self.settings.read().await.clone()
    }

    /// The scene this daemon was started with, as its own settings know it.
    pub(in crate::session_pool) async fn startup_scene(&self) -> String {
        self.settings.read().await.paths.scope.clone()
    }

    pub(in crate::session_pool) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(in crate::session_pool) fn paths(&self) -> &Arc<dyn crate::config::DaemonPaths> {
        &self.paths
    }

    pub(in crate::session_pool) fn managed_settings_path(&self) -> &Path {
        &self.layers.managed.path
    }

    /// `settings` read from disk, with what the host changed in memory and
    /// then this pool's managed layer on top.
    pub(in crate::session_pool) fn with_host_layers(
        &self,
        settings: LoadedSettings,
    ) -> LoadedSettings {
        self.layers
            .host
            .apply(settings)
            .with_managed(self.layers.managed.settings.clone())
    }

    /// Whether `(project_root, scene_id)` is this daemon's own pair.
    pub(in crate::session_pool) async fn is_own_pair(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> bool {
        project_root == Some(self.cwd.as_path()) && scene_id == self.startup_scene().await
    }

    /// The cached binding of the pair, unless something it was not watching
    /// has appeared since it was built.
    pub(in crate::session_pool) async fn cached(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Cached {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        let Some(existing) = self.cache.bindings.lock().await.get(&key).cloned() else {
            return Cached::Missing;
        };
        match existing.appeared().cloned() {
            None => Cached::Ready(existing),
            Some(tier) => Cached::Appeared {
                tier,
                own: self.is_own_pair(project_root, scene_id).await,
            },
        }
    }

    /// The pair's binding: the cached one, else one built now from what is on
    /// disk (this daemon's own pair from `settings` instead).
    pub(in crate::session_pool) async fn get_or_build(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<ProjectBinding> {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        if let Some(existing) = self.cache.bindings.lock().await.get(&key) {
            return existing.clone();
        }
        let stamp = self.begin_build(&key);
        let startup_scene = self.startup_scene().await;
        let settings = if self.is_own_pair(project_root, scene_id).await {
            self.pool_settings().await
        } else {
            let default_model = self.settings.read().await.model.model_name.clone();
            let (global_root, scene_root, scene_id, project) = (
                self.paths.global_root(),
                self.scene_root(scene_id, &startup_scene),
                scene_id.to_string(),
                key.0.clone(),
            );
            let loaded = off_the_runtime(move || match project {
                Some(root) => LoadedSettings::load(
                    global_root,
                    scene_root,
                    root.join(".agix"),
                    &scene_id,
                    &default_model,
                ),
                None => LoadedSettings::load_without_project(
                    global_root,
                    scene_root,
                    &scene_id,
                    &default_model,
                ),
            })
            .await;
            Arc::new(self.with_host_layers(loaded))
        };

        let (watch, unwatched_settings) = self.watch_tiers_of(&settings, &key);
        let last_good = self.last_good_routers.lock().await.get(&key).cloned();
        let (contributions, client, credentials) = (
            self.plugins.contributions(),
            self.client.clone(),
            self.credentials.clone(),
        );
        let built = Arc::new(
            off_the_runtime(move || ProjectBinding {
                unwatched_settings,
                ..ProjectBinding::build(
                    stamp,
                    settings,
                    &contributions,
                    watch,
                    &client,
                    last_good,
                    &credentials,
                )
            })
            .await,
        );
        if let Some(router) = built.task_router.clone() {
            self.last_good_routers
                .lock()
                .await
                .insert(key.clone(), router);
        }
        // Built outside the lock — scanning skill and agent directories is
        // disk work, and holding the map across it would serialize the first
        // session of every project behind the first session of any of them.
        self.finish_build(built).await
    }

    /// The stamp a build of `key` that starts now will carry. Taken before
    /// anything is read, so an invalidation landing anywhere during the
    /// build leaves it stale.
    pub(in crate::session_pool) fn begin_build(&self, key: &ProjectSceneKey) -> BindingStamp {
        let epoch = *self
            .cache
            .epochs
            .lock()
            .expect("epochs is never held across a panic")
            .entry(key.clone())
            .or_insert(0);
        BindingStamp {
            key: key.clone(),
            epoch,
        }
    }

    /// Cache `built` unless its pair was invalidated since its build began.
    /// Whoever got there first wins; a duplicate build is wasted work, not a
    /// second live catalog. A stale build is still handed back — the session
    /// asking for it is served on it and rebuilds before its next turn — but
    /// it is not cached, so the next session reads what is on disk now.
    pub(in crate::session_pool) async fn finish_build(
        &self,
        built: Arc<ProjectBinding>,
    ) -> Arc<ProjectBinding> {
        let mut bindings = self.cache.bindings.lock().await;
        if let Some(existing) = bindings.get(&built.stamp.key) {
            return existing.clone();
        }
        if self.is_current(&built.stamp) {
            bindings.insert(built.stamp.key.clone(), built.clone());
        }
        built
    }

    /// Whether no tier the binding stamped `stamp` was merged from has been
    /// invalidated since its build began.
    pub(in crate::session_pool) fn is_current(&self, stamp: &BindingStamp) -> bool {
        self.cache
            .epochs
            .lock()
            .expect("epochs is never held across a panic")
            .get(&stamp.key)
            .copied()
            .unwrap_or(0)
            == stamp.epoch
    }

    /// Watch everything a binding was built from — its settings files, and
    /// the skill and agent-type directories of each of its three tiers — so a
    /// change to any of them invalidates it.
    ///
    /// One hub, so one `notify` stream for the whole process no matter how
    /// many projects it serves, and paths several projects share are
    /// registered once.
    ///
    /// Only paths that exist are subscribed. `WatchHub::subscribe` falls back
    /// to the nearest existing ancestor of a path that isn't there, and the
    /// ancestor of `~/.agix/settings.json` is `~/.agix` — which also holds
    /// every transcript this daemon writes. Watching that would turn each
    /// turn's own log writes into a settings-change notification. The
    /// paths left out are returned alongside the subscriptions: one created
    /// later is noticed the next time the binding is asked for
    /// (`ProjectBinding::appeared`).
    #[allow(clippy::type_complexity)]
    fn watch_tiers_of(
        &self,
        settings: &LoadedSettings,
        key: &ProjectSceneKey,
    ) -> (Vec<WatchToken>, Vec<(Tier, PathBuf)>) {
        let Some(hub) = self.watch.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        // The project tier's skills sit beside the project (`.agents/skills`)
        // rather than under `.agix`, which is why this is two paths and the
        // others are one — see `skills::module::build_default_skill_manager`.
        let project_root = key.0.clone();
        let mut tiers = vec![(
            Tier::Global,
            settings.paths.global_data_dir.clone(),
            settings.paths.global_data_dir.join("skills"),
        )];
        tiers.push((
            Tier::Scene(key.1.clone()),
            settings.paths.user_data_dir.clone(),
            settings.paths.user_data_dir.join("skills"),
        ));
        if let Some(root) = project_root {
            tiers.push((
                Tier::Project(root.clone()),
                settings.paths.local_data_dir.clone(),
                root.join(".agents").join("skills"),
            ));
        }

        let mut tokens = Vec::new();
        let mut unwatched = Vec::new();
        let mut subscribe = |tier: &Tier, target: WatchTarget, path: PathBuf| {
            if path.exists() {
                tokens.push(hub.subscribe(
                    WatchKey {
                        tier: tier.clone(),
                        target,
                    },
                    &path,
                ));
            } else {
                unwatched.push((tier.clone(), path));
            }
        };
        for (tier, config_dir, skills_dir) in tiers {
            for file in [
                base::settings::SETTINGS_FILE,
                base::settings::SETTINGS_LOCAL_FILE,
            ] {
                subscribe(&tier, WatchTarget::Settings, config_dir.join(file));
            }
            subscribe(&tier, WatchTarget::Skills, skills_dir);
            subscribe(&tier, WatchTarget::Agents, config_dir.join("agents"));
        }
        (tokens, unwatched)
    }

    /// The change notifications of every watched path, `None` when nothing
    /// could be watched. Also starts watching this daemon's own tiers, for as
    /// long as the pool lives.
    pub(in crate::session_pool) async fn watch_own_tiers(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<WatchKey>> {
        let changes = self.watch.as_ref()?.changes();
        let settings = self.pool_settings().await;
        let own = (Some(self.cwd.clone()), settings.paths.scope.clone());
        let (tokens, _) = self.watch_tiers_of(&settings, &own);
        *self
            .pool_watch
            .lock()
            .expect("pool_watch is never held across a panic") = tokens;
        Some(changes)
    }

    /// Bump the epoch of every pair merged from `changed` — every pair when
    /// `None` — and drop their cached bindings, so the next session on those
    /// pairs is built from what is on disk now, the running ones rebuild
    /// before their next turn, and a build still in flight is not cached. A
    /// project tier sits under only its own pairs; the scene and global
    /// tiers under more. Returns the cached pairs dropped.
    pub(in crate::session_pool) async fn invalidate(
        &self,
        changed: Option<&Tier>,
    ) -> Vec<ProjectSceneKey> {
        let hit = |key: &ProjectSceneKey| changed.is_none_or(|tier| tier_covers(tier, key));
        let mut bindings = self.cache.bindings.lock().await;
        self.cache
            .epochs
            .lock()
            .expect("epochs is never held across a panic")
            .iter_mut()
            .filter(|(key, _)| hit(key))
            .for_each(|(_, epoch)| *epoch += 1);
        let mut dropped = Vec::new();
        bindings.retain(|key, _| {
            let hit = hit(key);
            if hit {
                dropped.push(key.clone());
            }
            !hit
        });
        dropped
    }

    /// Re-read the skill or agent-type directories into the catalogs of the
    /// bindings merged from `tier`, in place. Sessions share those catalogs,
    /// so they see the change at once and nothing is rebuilt.
    pub(in crate::session_pool) async fn refresh_catalogs(&self, tier: &Tier, target: WatchTarget) {
        let plugin_agent_types = self.plugins.agent_types();
        for (key, binding) in self.cache.bindings.lock().await.iter() {
            if !tier_covers(tier, key) {
                continue;
            }
            match target {
                WatchTarget::Skills => binding.reload_skills(),
                WatchTarget::Agents => binding.refresh_agent_types(&plugin_agent_types),
                _ => {}
            }
        }
    }

    /// Re-merge what the plugin set now contributes into every binding's
    /// catalogs, in place.
    pub(in crate::session_pool) async fn refresh_plugins(&self) {
        let contributions = self.plugins.contributions();
        for binding in self.cache.bindings.lock().await.values() {
            binding.refresh_plugins(&contributions);
        }
    }

    /// Re-read this daemon's own pair's settings from disk and swap them in.
    pub(in crate::session_pool) async fn reload_own(&self, scene: &str) {
        let default_model = self.settings.read().await.model.model_name.clone();
        let (global_root, config_root, local_dir, scene) = (
            self.paths.global_root(),
            self.paths.config_root(),
            self.paths.project_root().join(".agix"),
            scene.to_string(),
        );
        let loaded = off_the_runtime(move || {
            LoadedSettings::load(global_root, config_root, local_dir, &scene, &default_model)
        })
        .await;
        let reloaded = Arc::new(self.with_host_layers(loaded));
        *self.settings.write().await = reloaded;
    }

    /// The scene tier directory for `scene_id`.
    ///
    /// The daemon's *own* scene keeps whatever `DaemonPaths::config_root()`
    /// resolves to — tests inject a `StaticDaemonPaths` whose config root is a
    /// tempdir bearing no relation to `global_root()/scenes/<id>`, and
    /// recomputing it here would move that scene's settings out from under
    /// them. Any other scene has no such injected path, so it is derived the
    /// way `DefaultDaemonPaths::from_env` derives the first one.
    pub(in crate::session_pool) fn scene_root(
        &self,
        scene_id: &str,
        startup_scene: &str,
    ) -> PathBuf {
        if scene_id == startup_scene {
            self.paths.config_root()
        } else {
            self.paths.global_root().join("scenes").join(scene_id)
        }
    }

    /// The project-tier `settings.json` of `project_root`, or of this
    /// daemon's own project when none is named.
    pub(in crate::session_pool) fn settings_path_of(&self, project_root: Option<&Path>) -> PathBuf {
        project_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.paths.project_root())
            .join(".agix")
            .join(base::settings::SETTINGS_FILE)
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn binding_from(&self, settings: LoadedSettings) -> ProjectBinding {
        ProjectBinding::build(
            BindingStamp {
                key: (None, String::new()),
                epoch: 0,
            },
            Arc::new(settings),
            &Default::default(),
            vec![],
            &self.client,
            None,
            &self.credentials,
        )
    }

    #[cfg(test)]
    pub(in crate::session_pool) async fn insert(
        &self,
        key: ProjectSceneKey,
        mut binding: ProjectBinding,
    ) {
        binding.stamp = self.begin_build(&key);
        self.cache
            .bindings
            .lock()
            .await
            .insert(key, Arc::new(binding));
    }

    /// Hold the bindings' lock, so whoever asks for a binding waits.
    #[cfg(test)]
    pub(in crate::session_pool) async fn hold(
        &self,
    ) -> tokio::sync::MutexGuard<'_, HashMap<ProjectSceneKey, Arc<ProjectBinding>>> {
        self.cache.bindings.lock().await
    }

    #[cfg(test)]
    pub(in crate::session_pool) async fn is_empty(&self) -> bool {
        self.cache.bindings.lock().await.is_empty()
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn watched_path_count(&self) -> Option<usize> {
        self.watch.as_ref().map(WatchHub::watched_path_count)
    }
}
