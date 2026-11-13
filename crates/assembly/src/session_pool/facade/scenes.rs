//! 场景的对外方法：列出、注册、重载、启停。场景表本身在 `SceneCatalog`，这里补上
//! 它不知道的两件事：插件贡献的场景，和哪些会话正在某个场景里。

use crate::session_pool::*;

impl SessionPool {
    /// This daemon instance's *default* scene id.
    pub fn scene_id(&self) -> &str {
        self.scenes.default_id()
    }

    /// `scene.list` — every scene this binary registers, each flagged with
    /// whether it's currently active. `sessions` counts *in-memory* live
    /// sessions recorded under that scene (an approximation — matches what
    /// `session.list`'s own active/inactive split already accepts; a
    /// disk-wide count would need scanning every session's `Meta`, not
    /// worth it just for this summary view).
    pub async fn list_scenes(&self) -> Vec<api::SceneSummary> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for sid in self.live_ids().await {
            if let Some(scene) = self.facts_of(&sid).await.scene {
                *counts.entry(scene).or_insert(0) += 1;
            }
        }
        self.scenes.list(&counts)
    }

    /// Make a scene the host defines known to this pool, so `scene.list`
    /// shows it and `scene.activate` (or [`Assembly::extra_scenes`]) can turn
    /// it on. Registering does not activate. A scene with the id of a built-in
    /// one replaces it.
    ///
    /// [`Assembly::extra_scenes`]: crate::assemble::Assembly::extra_scenes
    pub fn register_scene(&self, scene: Arc<SceneSpec>) {
        self.scenes.register_host(scene, &self.plugins.scenes());
    }

    /// `scene.reload`: read the declarative scenes on disk again.
    pub async fn reload_scenes(&self) -> api::SceneReload {
        let change = self.scenes.refresh(&self.plugins.scenes());
        let mut scenes = self.scenes.registry().ids();
        scenes.sort();
        api::SceneReload {
            scenes,
            added: change.added,
            removed: change.removed,
            errors: change.refused,
        }
    }

    /// `scene.register`: add a declarative scene from its `scene.json` and
    /// `prompt.md` text, in memory only. It is not activated.
    pub async fn register_scene_from_sources(
        &self,
        scene_json: &str,
        prompt_md: &str,
    ) -> Result<String, PoolError> {
        self.scenes
            .register_from_sources(scene_json, prompt_md, &self.plugins.scenes())
    }

    /// `scene.unregister`: withdraw a scene `scene.register` added. Refused
    /// while sessions in memory run in it, as `scene.deactivate` is.
    pub async fn unregister_scene(&self, scene_id: &str) -> Result<(), PoolError> {
        if !self.scenes.is_registered(scene_id) {
            return Err(PoolError::SceneNotRegistered {
                scene: scene_id.to_string(),
            });
        }
        self.deactivate_scene(scene_id).await?;
        self.scenes.unregister(scene_id, &self.plugins.scenes());
        Ok(())
    }

    /// `scene.activate {scene}` — idempotent. `Err` when `scene_id` isn't
    /// one this binary registers at all (`scene.list` would never show it).
    pub async fn activate_scene(&self, scene_id: &str) -> Result<(), PoolError> {
        self.scenes.activate(scene_id)
    }

    /// `scene.deactivate {scene}` — refuses while any *in-memory* session is
    /// recorded under this scene (`SCENE_HAS_ACTIVE_SESSIONS`, with the
    /// blocking session ids so the caller can close them and retry).
    /// Deactivating only removes it from the active scenes — no session
    /// history is touched, and a deactivated scene's sessions can still be
    /// listed (`session.list`) and resumed once it's reactivated.
    pub async fn deactivate_scene(&self, scene_id: &str) -> Result<(), PoolError> {
        let mut blocking = Vec::new();
        for sid in self.live_ids().await {
            if self.facts_of(&sid).await.scene.as_deref() == Some(scene_id) {
                blocking.push(sid);
            }
        }
        if !blocking.is_empty() {
            return Err(PoolError::SceneHasActiveSessions {
                scene: scene_id.to_string(),
                sessions: blocking,
            });
        }
        self.scenes.deactivate_unchecked(scene_id);
        Ok(())
    }

    async fn live_ids(&self) -> Vec<String> {
        self.sessions.lock().await.keys().cloned().collect()
    }
}
