//! Scene 表：注册、重载、启停，以及按 id 解析 scene。

use super::*;

impl SessionPool {
    pub(super) fn scene_registry(&self) -> Arc<scene::SceneRegistry> {
        self.scene_registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// This daemon instance's *default* scene id — see the `scene` field.
    pub fn scene_id(&self) -> &str {
        self.scene.id()
    }

    /// Resolve `scene_id` against the currently-active scenes, or `None`
    /// (the pool default) against `self.scene` directly. `session.create`'s
    /// resolution rule: omitted `scene` param always means the default,
    /// regardless of what else is active.
    pub(super) async fn resolve_scene(&self, scene_id: Option<&str>) -> Option<Arc<dyn Scene>> {
        match scene_id {
            None => Some(self.scene.clone()),
            Some(id) => self.active_scenes.read().await.get(id).cloned(),
        }
    }

    /// `scene.list` — every scene this binary registers, each flagged with
    /// whether it's currently active. `sessions` counts *in-memory* live
    /// sessions recorded under that scene (an approximation — matches what
    /// `session.list`'s own active/inactive split already accepts; a
    /// disk-wide count would need scanning every session's `Meta`, not
    /// worth it just for this summary view).
    pub async fn list_scenes(&self) -> Vec<serde_json::Value> {
        let active = self.active_scenes.read().await;
        let mut counts: HashMap<String, usize> = HashMap::new();
        {
            let sessions = self.sessions.lock().await;
            for sid in sessions.keys() {
                if let Some(scene) = self.facts_of(sid).await.scene {
                    *counts.entry(scene).or_insert(0) += 1;
                }
            }
        }
        self.scene_registry()
            .list_all()
            .into_iter()
            .map(|info| {
                serde_json::json!({
                    "scene": info.id,
                    "name": info.name,
                    "description": info.description,
                    "active": active.contains_key(&info.id),
                    "sessions": counts.get(&info.id).copied().unwrap_or(0),
                    "requires_project": info.requires_project,
                    "supports_team": info.supports_team,
                })
            })
            .collect()
    }

    /// `scene.activate {scene}` — idempotent. `Err` when `scene_id` isn't
    /// one this binary registers at all (`scene.list` would never show it).
    /// Make a scene the host defines known to this pool, so `scene.list`
    /// shows it and `scene.activate` (or [`Assembly::extra_scenes`]) can turn
    /// it on. Registering does not activate. A scene with the id of a built-in
    /// one replaces it.
    ///
    /// [`Assembly::extra_scenes`]: crate::assemble::Assembly::extra_scenes
    pub fn register_scene(&self, scene: Arc<dyn Scene>) {
        {
            let mut host = self.host_scenes.write().unwrap_or_else(|e| e.into_inner());
            host.retain(|s| s.id() != scene.id());
            host.push(scene);
        }
        let (registry, _) = self.scene_registry_now(&self.plugins.scenes());
        *self
            .scene_registry
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);
    }

    pub(super) fn scene_registry_now(
        &self,
        plugin_scenes: &[Arc<dyn Scene>],
    ) -> (scene::SceneRegistry, Vec<String>) {
        registry_of(
            &self.host_scenes.read().unwrap_or_else(|e| e.into_inner()),
            plugin_scenes,
            &self.paths.global_root().join("scenes"),
            &self
                .registered_scenes
                .read()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Rebuild the scene table from everything that contributes to it, and
    /// point active scenes that are still there at their current definition —
    /// a declarative scene edited on disk is served as edited to the sessions
    /// created from now on. Sessions already running keep the definition they
    /// were built with.
    pub(super) async fn refresh_scene_table(&self) -> SceneTableChange {
        let before: std::collections::BTreeSet<String> =
            self.scene_registry().ids().into_iter().collect();
        let (registry, refused) = self.scene_registry_now(&self.plugins.scenes());
        let after: std::collections::BTreeSet<String> = registry.ids().into_iter().collect();
        {
            let mut active = self.active_scenes.write().await;
            for (id, scene) in active.iter_mut() {
                if let Some(current) = registry.resolve(id) {
                    *scene = current;
                }
            }
        }
        *self
            .scene_registry
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);
        SceneTableChange {
            added: after.difference(&before).cloned().collect(),
            removed: before.difference(&after).cloned().collect(),
            refused,
        }
    }

    /// `scene.reload`: read the declarative scenes on disk again.
    pub async fn reload_scenes(&self) -> serde_json::Value {
        let change = self.refresh_scene_table().await;
        let mut scenes = self.scene_registry().ids();
        scenes.sort();
        serde_json::json!({
            "scenes": scenes,
            "added": change.added,
            "removed": change.removed,
            "errors": change.refused,
        })
    }

    /// `scene.register`: add a declarative scene from its `scene.json` and
    /// `prompt.md` text, in memory only. It is not activated.
    pub async fn register_scene_from_sources(
        &self,
        scene_json: &str,
        prompt_md: &str,
    ) -> Result<String, RpcError> {
        let scene = scene::DeclarativeScene::from_sources(
            std::path::Path::new("scene.register"),
            scene_json,
            prompt_md,
        )
        .map_err(|e| (codes::INVALID_PARAMS, e.to_string()))?;
        let id = scene.id().to_string();
        if self.scene_registry().resolve(&id).is_some() {
            return Err((
                codes::INVALID_PARAMS,
                format!("scene `{id}` is already registered"),
            ));
        }
        self.registered_scenes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(scene));
        self.refresh_scene_table().await;
        Ok(id)
    }

    /// `scene.unregister`: withdraw a scene `scene.register` added. Refused
    /// while sessions in memory run in it, as `scene.deactivate` is.
    pub async fn unregister_scene(&self, scene_id: &str) -> Result<(), RpcError> {
        let known = self
            .registered_scenes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|s| s.id() == scene_id);
        if !known {
            return Err((
                codes::SCENE_NOT_FOUND,
                format!("scene `{scene_id}` was not added by scene.register"),
            ));
        }
        self.deactivate_scene(scene_id).await?;
        self.registered_scenes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|s| s.id() != scene_id);
        self.refresh_scene_table().await;
        Ok(())
    }

    pub async fn activate_scene(&self, scene_id: &str) -> Result<(), RpcError> {
        if self.active_scenes.read().await.contains_key(scene_id) {
            return Ok(());
        }
        let Some(resolved) = self.scene_registry().resolve(scene_id) else {
            return Err((codes::SCENE_NOT_FOUND, format!("unknown scene: {scene_id}")));
        };
        self.active_scenes
            .write()
            .await
            .insert(scene_id.to_string(), resolved);
        Ok(())
    }

    /// `scene.deactivate {scene}` — refuses while any *in-memory* session is
    /// recorded under this scene (`SCENE_HAS_ACTIVE_SESSIONS`, with the
    /// blocking session ids so the caller can close them and retry).
    /// Deactivating only removes it from `active_scenes` — no session
    /// history is touched, and a deactivated scene's sessions can still be
    /// listed (`session.list`) and resumed once it's reactivated.
    pub async fn deactivate_scene(&self, scene_id: &str) -> Result<(), RpcError> {
        let blocking: Vec<String> = {
            let sessions = self.sessions.lock().await;
            let mut out = Vec::new();
            for sid in sessions.keys() {
                if let Some(scene) = self.facts_of(sid).await.scene {
                    if scene == scene_id {
                        out.push(sid.clone());
                    }
                }
            }
            out
        };
        if !blocking.is_empty() {
            return Err((
                codes::SCENE_HAS_ACTIVE_SESSIONS,
                format!(
                    "scene {scene_id} has active sessions: {}",
                    blocking.join(", ")
                ),
            ));
        }
        self.active_scenes.write().await.remove(scene_id);
        Ok(())
    }
}
