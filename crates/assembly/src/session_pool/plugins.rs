//! 插件：加载、安装卸载、启停，以及变更后的刷新。

use super::*;

impl SessionPool {
    /// Take up the scenes installed packages own.
    ///
    /// **Every embedder that constructs a `SessionPool` must call this
    /// before serving**: `new` is synchronous and cannot, and a session
    /// created before it runs cannot enter a package's scene.
    pub async fn load_plugins(&self) {
        self.adopt_plugin_scenes().await;
    }

    /// Rebuild the scene registry from the built-ins plus whatever the
    /// current plugin set owns, and make those scenes servable.
    ///
    /// Plugin scenes are activated rather than merely registered: installing
    /// and enabling a plugin that owns a scene *is* the user's consent to
    /// serve it, and requiring a second `scene.activate` would be a step
    /// with no safety it buys — entering the scene is still an explicit
    /// `session.create`.
    ///
    /// Withdrawing one removes it from both maps, which stops new sessions
    /// from entering it. Sessions already inside keep the `Arc` they were
    /// built with and run to completion — the alternative would be pulling a
    /// session's system prompt out from under it mid-turn.
    pub(super) async fn adopt_plugin_scenes(&self) {
        let plugin_scenes = self.plugins.scenes();

        let (registry, _) = self.scene_registry_now(&plugin_scenes);
        let plugin_ids: std::collections::HashSet<String> =
            plugin_scenes.iter().map(|s| s.id().to_string()).collect();
        *self
            .scene_registry
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);

        let mut active = self.active_scenes.write().await;
        active.retain(|id, _| !id.starts_with("plugin:") || plugin_ids.contains(id));
        for s in plugin_scenes {
            active.insert(s.id().to_string(), s);
        }
    }

    /// Whether this binary has the plugin subsystem, and whether it is on —
    /// see `crate::plugins::PluginStatus`. `daemon.doctor` reports it, and
    /// the `plugin.*` RPCs refuse with `PLUGINS_DISABLED` when it is not
    /// `Enabled`.
    pub fn plugin_status(&self) -> crate::plugins::PluginStatus {
        self.plugins.status()
    }

    /// Every installed plugin with its current enable state — for the
    /// `plugin.list` RPC. Includes disabled ones, since a management UI needs
    /// to show and re-enable them.
    pub async fn list_plugins(&self) -> Vec<serde_json::Value> {
        self.plugins.list()
    }

    /// Install a plugin from an explicit source. `scope` picks which tier's
    /// cache it lands in ("global", the default a caller should use unless it
    /// specifically wants a scene-only install, or "scene"). Refreshes the
    /// active set on success so sessions created after this call see it —
    /// already-running sessions are unaffected, same as
    /// `config.setProvider`/`mcp.addServer`.
    /// Re-read the plugin directories and reload what is there.
    ///
    /// Install/uninstall/enable/disable each refresh on their own, so this is
    /// for the case none of them covers: the files changed without going
    /// through an RPC — a plugin dropped into the directory by hand, or a
    /// rebuilt component swapped in during development. Without it the only
    /// way to pick that up is restarting the daemon.
    ///
    /// Returns the installed set afterwards, so a caller learns what the
    /// rescan actually found without a second call.
    pub async fn reload_plugins(&self) -> serde_json::Value {
        self.plugins.refresh().await;
        self.refresh_after_plugin_change().await;
        serde_json::json!({"plugins": self.list_plugins().await})
    }

    pub async fn install_plugin(
        &self,
        name: &str,
        version: &str,
        download_url: &str,
        checksum: Option<&str>,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let out = self
            .plugins
            .install(name, version, download_url, checksum, scope)
            .await?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Uninstall a plugin (all versions, or a specific one) from `scope`'s
    /// tier — see `install_plugin` for the refresh semantics.
    pub async fn uninstall_plugin(
        &self,
        name: &str,
        version: Option<&str>,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let out = self.plugins.uninstall(name, version, scope).await?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Enable or disable a plugin by name in `scope`'s tier — see
    /// `install_plugin` for the refresh semantics.
    pub async fn set_plugin_enabled(
        &self,
        name: &str,
        enabled: bool,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        let out = self.plugins.set_enabled(name, enabled, scope).await?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Re-merge whatever the plugin set now contributes into the pool-wide
    /// catalogs. The subsystem has already re-read itself by this point; what
    /// is left is the state this pool derives from it. Skill directories are
    /// deliberately not re-scanned: `self.skill_catalog` is the one
    /// persistent, live-reloaded manager every session shares.
    pub(super) async fn refresh_after_plugin_change(&self) {
        self.adopt_plugin_scenes().await;
        let plugin_agent_types = self.plugins.agent_types();
        for binding in self.bindings.lock().await.values() {
            binding.refresh_plugins(&plugin_agent_types);
        }
    }
}
