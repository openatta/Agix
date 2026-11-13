//! 场景表 `SceneCatalog`：池的默认场景、此刻在服务的场景，以及能服务的全部场景
//! （内置、宿主的、磁盘上声明的、`scene.register` 加的、插件的）。插件的场景由
//! 门面作为参数传进来，这里不认识插件子系统；会话用哪个场景、场景里有没有会话，
//! 也都由门面判断。
//!
//! 锁序：四把 std `RwLock` 都是叶子，持锁时不 `.await`；重建注册表时同时持
//! `host` 和 `registered` 的读锁，此外不嵌套。

use crate::session_pool::api::{PoolError, SceneSummary};
use base::interface::scene::SceneSpec;
use scene::SceneRegistry;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tracing::warn;

pub(in crate::session_pool) struct SceneTableChange {
    pub(in crate::session_pool) added: Vec<String>,
    pub(in crate::session_pool) removed: Vec<String>,
    pub(in crate::session_pool) refused: Vec<String>,
}

pub(in crate::session_pool) struct SceneCatalog {
    /// This daemon instance's default scene, resolved from `--scene` at
    /// startup. Every session created without an explicit `scene` uses it.
    default: Arc<SceneSpec>,
    /// Every scene this daemon instance currently serves, keyed by id —
    /// always at least the default, plus whatever `--scenes` added at startup
    /// or `scene.activate` added at runtime. The default and the servable are
    /// related but distinct: the default is always servable, not everything
    /// servable is the default.
    active: RwLock<HashMap<String, Arc<SceneSpec>>>,
    /// Every scene this pool can serve, active or not — what `scene.list`
    /// enumerates and `scene.activate` resolves against. Swapped wholesale;
    /// a session already running holds its own `Arc<SceneSpec>` and is
    /// unaffected.
    registry: RwLock<Arc<SceneRegistry>>,
    /// Scenes the host defined rather than the binary or a plugin: the
    /// default and every one passed to `register_host`. Kept so each rebuild
    /// of the registry has them; without that, a host scene once deactivated
    /// could never be activated again.
    host: RwLock<Vec<Arc<SceneSpec>>>,
    /// Scenes `scene.register` added, held in memory only.
    registered: RwLock<Vec<Arc<SceneSpec>>>,
    /// Where the declarative scenes live: `<state root>/scenes/<id>/`.
    user_scenes_dir: PathBuf,
}

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

impl SceneCatalog {
    pub(in crate::session_pool) fn new(default: Arc<SceneSpec>, user_scenes_dir: PathBuf) -> Self {
        let host = vec![default.clone()];
        let registry = registry_of(&host, &[], &user_scenes_dir, &[]).0;
        let active = HashMap::from([(default.id.clone(), default.clone())]);
        Self {
            default,
            active: RwLock::new(active),
            registry: RwLock::new(Arc::new(registry)),
            host: RwLock::new(host),
            registered: RwLock::new(Vec::new()),
            user_scenes_dir,
        }
    }

    pub(in crate::session_pool) fn default(&self) -> &Arc<SceneSpec> {
        &self.default
    }

    pub(in crate::session_pool) fn default_id(&self) -> &str {
        self.default.id.as_str()
    }

    /// `scene_id` among the active scenes; `None` is the default, whatever
    /// else is active.
    pub(in crate::session_pool) fn resolve(
        &self,
        scene_id: Option<&str>,
    ) -> Option<Arc<SceneSpec>> {
        match scene_id {
            None => Some(self.default.clone()),
            Some(id) => read(&self.active).get(id).cloned(),
        }
    }

    pub(in crate::session_pool) fn registry(&self) -> Arc<SceneRegistry> {
        read(&self.registry).clone()
    }

    pub(in crate::session_pool) fn is_active(&self, scene_id: &str) -> bool {
        read(&self.active).contains_key(scene_id)
    }

    /// The default scene and every active one; the default may be among the
    /// active ones too.
    pub(in crate::session_pool) fn served(&self) -> Vec<Arc<SceneSpec>> {
        std::iter::once(self.default.clone())
            .chain(read(&self.active).values().cloned())
            .collect()
    }

    /// Every registered scene, each flagged with whether it is active and
    /// how many of `sessions` (counted per scene by the caller) are in it.
    pub(in crate::session_pool) fn list(
        &self,
        sessions: &HashMap<String, usize>,
    ) -> Vec<SceneSummary> {
        let active = read(&self.active);
        self.registry()
            .list_all()
            .into_iter()
            .map(|info| SceneSummary {
                active: active.contains_key(&info.id),
                sessions: sessions.get(&info.id).copied().unwrap_or(0),
                scene: info.id,
                name: info.name,
                description: info.description,
                requires_project: info.requires_project,
                supports_team: info.supports_team,
            })
            .collect()
    }

    /// Make a scene the host defines known. A scene with the id of a
    /// built-in one replaces it. Registering does not activate.
    pub(in crate::session_pool) fn register_host(
        &self,
        scene: Arc<SceneSpec>,
        plugin_scenes: &[Arc<SceneSpec>],
    ) {
        {
            let mut host = write(&self.host);
            host.retain(|s| s.id != scene.id);
            host.push(scene);
        }
        let (registry, _) = self.registry_now(plugin_scenes);
        *write(&self.registry) = Arc::new(registry);
    }

    fn registry_now(&self, plugin_scenes: &[Arc<SceneSpec>]) -> (SceneRegistry, Vec<String>) {
        registry_of(
            &read(&self.host),
            plugin_scenes,
            &self.user_scenes_dir,
            &read(&self.registered),
        )
    }

    /// Rebuild the scene table from everything that contributes to it, and
    /// point active scenes that are still there at their current definition —
    /// a declarative scene edited on disk is served as edited to the sessions
    /// created from now on.
    pub(in crate::session_pool) fn refresh(
        &self,
        plugin_scenes: &[Arc<SceneSpec>],
    ) -> SceneTableChange {
        let before: BTreeSet<String> = self.registry().ids().into_iter().collect();
        let (registry, refused) = self.registry_now(plugin_scenes);
        let after: BTreeSet<String> = registry.ids().into_iter().collect();
        for (id, scene) in write(&self.active).iter_mut() {
            if let Some(current) = registry.resolve(id) {
                *scene = current;
            }
        }
        *write(&self.registry) = Arc::new(registry);
        SceneTableChange {
            added: after.difference(&before).cloned().collect(),
            removed: before.difference(&after).cloned().collect(),
            refused,
        }
    }

    /// Take the plugin set's scenes in: registered and active at once, and
    /// the scenes of plugins no longer there withdrawn from both.
    pub(in crate::session_pool) fn adopt_plugin_scenes(&self, plugin_scenes: Vec<Arc<SceneSpec>>) {
        let (registry, _) = self.registry_now(&plugin_scenes);
        *write(&self.registry) = Arc::new(registry);
        let plugin_ids: std::collections::HashSet<String> =
            plugin_scenes.iter().map(|s| s.id.clone()).collect();
        let mut active = write(&self.active);
        active.retain(|id, _| !id.starts_with("plugin:") || plugin_ids.contains(id));
        for s in plugin_scenes {
            active.insert(s.id.clone(), s);
        }
    }

    /// Add a declarative scene from its `scene.json` and `prompt.md` text, in
    /// memory only. It is not activated.
    pub(in crate::session_pool) fn register_from_sources(
        &self,
        scene_json: &str,
        prompt_md: &str,
        plugin_scenes: &[Arc<SceneSpec>],
    ) -> Result<String, PoolError> {
        let scene =
            scene::DeclaredScene::from_sources(Path::new("scene.register"), scene_json, prompt_md)
                .map_err(|e| PoolError::InvalidRequest(e.to_string()))?;
        let id = scene.spec.id.clone();
        if self.registry().resolve(&id).is_some() {
            return Err(PoolError::InvalidRequest(format!(
                "scene `{id}` is already registered"
            )));
        }
        write(&self.registered).push(Arc::new(scene.spec));
        self.refresh(plugin_scenes);
        Ok(id)
    }

    pub(in crate::session_pool) fn is_registered(&self, scene_id: &str) -> bool {
        read(&self.registered).iter().any(|s| s.id == scene_id)
    }

    /// Withdraw a scene `scene.register` added. Whether sessions still run
    /// in it is the caller's to check first.
    pub(in crate::session_pool) fn unregister(
        &self,
        scene_id: &str,
        plugin_scenes: &[Arc<SceneSpec>],
    ) {
        write(&self.registered).retain(|s| s.id != scene_id);
        self.refresh(plugin_scenes);
    }

    /// Idempotent. `Err` when `scene_id` is not registered at all.
    pub(in crate::session_pool) fn activate(&self, scene_id: &str) -> Result<(), PoolError> {
        if self.is_active(scene_id) {
            return Ok(());
        }
        let Some(resolved) = self.registry().resolve(scene_id) else {
            return Err(PoolError::SceneNotFound {
                scene: scene_id.to_string(),
            });
        };
        write(&self.active).insert(scene_id.to_string(), resolved);
        Ok(())
    }

    /// Stop serving `scene_id`. Whether sessions still run in it is the
    /// caller's to check first.
    pub(in crate::session_pool) fn deactivate_unchecked(&self, scene_id: &str) {
        write(&self.active).remove(scene_id);
    }
}

/// The scenes a pool can serve: built-ins, then the host's, then the
/// declarative scenes under `user_scenes` (`<state root>/scenes/<id>/`), then
/// those `scene.register` added, then the plugins'. A declarative or
/// registered scene cannot take an id already there; one that does not load
/// is left out, named in the returned list, and the rest load.
fn registry_of(
    host_scenes: &[Arc<SceneSpec>],
    plugin_scenes: &[Arc<SceneSpec>],
    user_scenes: &Path,
    registered: &[Arc<SceneSpec>],
) -> (SceneRegistry, Vec<String>) {
    let mut registry = SceneRegistry::new();
    let mut refused = Vec::new();
    registry.register_builtin();
    for s in host_scenes {
        registry.register(s.clone());
    }
    for e in registry.register_declarative_dir(user_scenes) {
        warn!(error = %e, "a declarative scene was not loaded");
        refused.push(e.to_string());
    }
    for s in registered {
        if registry.resolve(&s.id).is_some() {
            let e = format!(
                "scene `{}` (scene.register): the id is already registered",
                s.id.as_str()
            );
            warn!(error = %e, "a registered scene was not loaded");
            refused.push(e);
        } else {
            registry.register(s.clone());
        }
    }
    for s in plugin_scenes {
        registry.register(s.clone());
    }
    (registry, refused)
}
