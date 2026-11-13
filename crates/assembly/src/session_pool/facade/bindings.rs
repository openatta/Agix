//! 按 `(project, scene)` 取设置与绑定。绑定本身归 `BindingCache`；这里补上它不该自己
//! 做的一件事：某一层冒出新文件时，自家那一对走完整的重载（含 MCP），别的对只失效。

use crate::session_pool::*;

impl SessionPool {
    /// Merged settings for `project_root` (global → scene → project tiers).
    /// `None` is the global no-project tier (docs/ARCHITECTURE.md §3.2):
    /// `local_data_dir` becomes `global_data_dir` itself rather than
    /// `<project>/.agix`, so project-layer settings are skipped entirely, and
    /// the workspace is a `Workspace::Scratch` that `create` narrows to the
    /// session's own directory.
    ///
    /// Does not validate that `project_root` exists — `LoadedSettings::load`
    /// already degrades a missing/unreadable `settings.json` to defaults
    /// (soft-degrade, same as every other tier). Callers that need to
    /// reject a nonexistent project outright (`session.create`) check that
    /// separately, since resume callers deliberately do **not**: a project
    /// directory that moved after the session was created should still
    /// resume (`docs/daemon_rpc_protocol.md` §3.4), not fail.
    pub(in crate::session_pool) async fn settings_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<LoadedSettings> {
        self.binding_for(project_root, scene_id)
            .await
            .settings
            .clone()
    }

    /// The [`ProjectBinding`] for this pair, built on first use.
    ///
    /// The pool's own `(cwd, startup scene)` pair takes its settings from the
    /// hot-swappable copy `config.reload` writes, rather than loading them
    /// again — but it is cached like every other pair, and
    /// `apply_reloaded_settings` drops it so the next session picks the new
    /// ones up.
    pub(in crate::session_pool) async fn binding_for(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<ProjectBinding> {
        match self.bindings.cached(project_root, scene_id).await {
            Cached::Ready(binding) => return binding,
            Cached::Appeared { tier, own } => {
                info!(tier = ?tier, "a settings file, skill or agent-type directory was created; reloading");
                if own {
                    Box::pin(self.reload_settings_from(None, scene_id, Some(&tier))).await;
                } else {
                    self.invalidate_bindings(Some(&tier)).await;
                }
            }
            Cached::Missing => {}
        }
        self.bindings.get_or_build(project_root, scene_id).await
    }

    /// Drop the bindings merged from `changed` (every one when `None`) and
    /// mark them stale — see [`BindingCache::invalidate`].
    pub(in crate::session_pool) async fn invalidate_bindings(
        &self,
        changed: Option<&crate::watch_hub::Tier>,
    ) {
        self.bindings.invalidate(changed).await;
    }

    /// The binding this daemon's own project and startup scene resolve to —
    /// what a pool-level question with no session to ask about is answered
    /// from (`commands.list`, `daemon.doctor`'s skill count).
    pub(in crate::session_pool) async fn default_binding(&self) -> Arc<ProjectBinding> {
        let scene = self.bindings.startup_scene().await;
        self.binding_for(Some(self.cwd()), &scene).await
    }

    /// Drop the cached connections whose `mcp_servers` came from `tier`, so the
    /// next session or rebuild on those keys connects with the changed config
    /// (a new `Authorization`, say) instead of reusing the old connection.
    /// `keep` is a key the caller reconnects itself.
    #[cfg(feature = "mcp")]
    pub(in crate::session_pool) async fn forget_mcp_built_from(
        &self,
        tier: &crate::watch_hub::Tier,
        keep: Option<&ProjectSceneKey>,
    ) {
        self.mcp_by_project
            .retain(|key| !tier_covers(tier, key) || keep == Some(key));
    }

    /// This project's centrally-connected `McpManager` — the MCP
    /// counterpart to `settings_for_project`, same `None`/`Some(root)` key
    /// convention. Connects lazily and caches the result on first use
    /// rather than eagerly for every project this pool has ever seen a
    /// session for: most daemons only ever serve their own default project,
    /// and eagerly connecting every project mentioned in a `session.create`
    /// call would pay for MCP servers nobody's session ever actually needs.
    /// The default project is the one exception — it's still connected
    /// eagerly at startup by `connect_mcp_servers_in_background`, so by the
    /// time this is first called for it the cache is typically already warm.
    ///
    /// Not deduplicated across projects with an identical `mcp_servers`
    /// config — two projects that happen to declare the same server each
    /// get their own connection.
    #[cfg(feature = "mcp")]
    pub(in crate::session_pool) async fn mcp_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<McpManager> {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        if let Some(existing) = self.mcp_by_project.current(&key) {
            return existing;
        }
        let settings = self.settings_for_project(project_root, scene_id).await;
        self.first_mcp_connect(key, &settings).await
    }
}
