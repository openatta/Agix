//! 项目绑定：按项目解析设置与 `ProjectBinding`、监视范围、失效与目录刷新、按项目的 MCP。

use super::*;

impl SessionPool {
    /// Merged settings for `project_root` (global → scene → project tiers),
    /// built once and cached — see the `projects` field doc comment.
    /// `None` is the global no-project tier (docs/ARCHITECTURE.md §3.2):
    /// `local_data_dir` becomes
    /// `global_data_dir` itself rather than `<project>/.agix`, so project-
    /// layer settings are skipped entirely, same as the design's "no
    /// specific project" case.
    ///
    /// Does not validate that `project_root` exists — `Settings::load`
    /// already degrades a missing/unreadable `settings.json` to defaults
    /// (soft-degrade, same as every other tier). Callers that need to
    /// reject a nonexistent project outright (`session.create`) check that
    /// separately, since resume callers deliberately do **not**: a project
    /// directory that moved after the session was created should still
    /// resume (`docs/daemon_rpc_protocol.md` §3.4), not fail.
    pub(super) async fn settings_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<Settings> {
        self.binding_for(project_root, scene_id)
            .await
            .settings
            .clone()
    }

    /// The [`ProjectBinding`] for this pair, built on first use.
    ///
    /// The pool's own `(cwd, startup scene)` pair takes its settings from the
    /// hot-swappable `self.settings` that `config.reload` writes, rather than
    /// loading them again — but it is cached here like every other pair, and
    /// `apply_reloaded_settings` drops it so the next session picks the new
    /// ones up.
    pub(super) async fn binding_for(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<ProjectBinding> {
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        let is_default_pair =
            key.0.as_deref() == Some(self.cwd.as_path()) && scene_id == startup_scene;
        let cached = self.bindings.lock().await.get(&key).cloned();
        if let Some(existing) = cached {
            let Some(tier) = existing.appeared().cloned() else {
                return existing;
            };
            info!(tier = ?tier, "a settings file, skill or agent-type directory was created; reloading");
            if is_default_pair {
                Box::pin(self.reload_settings_from(None, scene_id, Some(&tier))).await;
            } else {
                self.invalidate_bindings(Some(&tier)).await;
            }
            if let Some(rebuilt) = self.bindings.lock().await.get(&key) {
                return rebuilt.clone();
            }
        }

        let settings = if is_default_pair {
            self.settings.read().await.clone()
        } else {
            let default_model = self.settings.read().await.model.model_name.clone();
            let local_dir = match &key.0 {
                Some(root) => root.join(".agix"),
                None => self.paths.global_root(),
            };
            let (global_root, scene_root, scene_id) = (
                self.paths.global_root(),
                self.scene_root(scene_id, &startup_scene),
                scene_id.to_string(),
            );
            let loaded = off_the_runtime(move || {
                Settings::load(
                    global_root,
                    scene_root,
                    local_dir,
                    &scene_id,
                    &default_model,
                )
            })
            .await;
            Arc::new(self.with_managed_layer(loaded))
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
        // Whoever got there first wins; a duplicate build is wasted work, not
        // a second live catalog.
        let mut bindings = self.bindings.lock().await;
        bindings.entry(key).or_insert(built).clone()
    }

    /// Watch everything this binding was built from — its settings files,
    /// and the skill and agent-type directories of each of its three tiers —
    /// so a change to any of them invalidates it (see
    /// `start_config_watcher`).
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
    pub(super) fn watch_tiers_of(
        &self,
        settings: &Settings,
        key: &ProjectSceneKey,
    ) -> (
        Vec<crate::watch_hub::WatchToken>,
        Vec<(crate::watch_hub::Tier, PathBuf)>,
    ) {
        use crate::watch_hub::{Tier, WatchKey, WatchTarget};
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
                base::interface::settings::SETTINGS_FILE,
                base::interface::settings::SETTINGS_LOCAL_FILE,
            ] {
                subscribe(&tier, WatchTarget::Settings, config_dir.join(file));
            }
            subscribe(&tier, WatchTarget::Skills, skills_dir);
            subscribe(&tier, WatchTarget::Agents, config_dir.join("agents"));
        }
        (tokens, unwatched)
    }

    /// Drop the cached bindings merged from `changed` — every binding when
    /// `None` — and mark them stale, so the next session on those pairs is
    /// built from what is on disk now and the running ones rebuild before
    /// their next turn. A project tier sits under only its own pairs; the
    /// scene and global tiers under more.
    pub(super) async fn invalidate_bindings(&self, changed: Option<&crate::watch_hub::Tier>) {
        self.bindings.lock().await.retain(|key, binding| {
            let hit = changed.map_or(true, |tier| tier_covers(tier, key));
            if hit {
                binding.mark_stale();
            }
            !hit
        });
    }

    /// Re-read the skill or agent-type directories into the catalogs of the
    /// bindings merged from `tier`, in place. Sessions share those catalogs,
    /// so they see the change at once and nothing is rebuilt.
    pub(super) async fn refresh_catalogs(
        &self,
        tier: &crate::watch_hub::Tier,
        target: crate::watch_hub::WatchTarget,
    ) {
        use crate::watch_hub::WatchTarget;
        let plugin_agent_types = self.plugins.agent_types();
        for (key, binding) in self.bindings.lock().await.iter() {
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

    /// The binding this daemon's own project and startup scene resolve to —
    /// what a pool-level question with no session to ask about is answered
    /// from (`commands.list`, `daemon.doctor`'s skill count).
    pub(super) async fn default_binding(&self) -> Arc<ProjectBinding> {
        let scene = self.settings.read().await.paths.scope.clone();
        let cwd = self.cwd.clone();
        self.binding_for(Some(&cwd), &scene).await
    }

    /// The scene tier directory for `scene_id`.
    ///
    /// The daemon's *own* scene keeps whatever `DaemonPaths::config_root()`
    /// resolves to — tests inject a `StaticDaemonPaths` whose config root is a
    /// tempdir bearing no relation to `global_root()/scenes/<id>`, and
    /// recomputing it here would move that scene's settings out from under
    /// them. Any other scene has no such injected path, so it is derived the
    /// way `DefaultDaemonPaths::from_env` derives the first one.
    pub(super) fn scene_root(&self, scene_id: &str, startup_scene: &str) -> PathBuf {
        if scene_id == startup_scene {
            self.paths.config_root()
        } else {
            self.paths.global_root().join("scenes").join(scene_id)
        }
    }

    /// Drop the cached connections whose `mcp_servers` came from `tier`, so the
    /// next session or rebuild on those keys connects with the changed config
    /// (a new `Authorization`, say) instead of reusing the old connection.
    /// `keep` is a key the caller reconnects itself.
    #[cfg(feature = "mcp")]
    pub(super) async fn forget_mcp_built_from(
        &self,
        tier: &crate::watch_hub::Tier,
        keep: Option<&ProjectSceneKey>,
    ) {
        self.mcp_by_project
            .lock()
            .await
            .retain(|key, _| !tier_covers(tier, key) || keep == Some(key));
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
    /// get their own connection. Sharing a handle across projects is future
    /// work; this only needed to stop the wrong thing from happening (a
    /// non-default project silently inheriting the default project's
    /// connections, or never connecting its own at all).
    #[cfg(feature = "mcp")]
    pub(super) async fn mcp_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<McpManager> {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        if let Some(existing) = self.mcp_by_project.lock().await.get(&key) {
            return existing.clone();
        }
        let settings = self.settings_for_project(project_root, scene_id).await;
        self.first_mcp_connect(key, &settings).await
    }

    /// A session's own recorded `(schema_version, project_root)`, read off
    /// its `Meta` line — used by `resume_or_create` so resuming a session
    /// rebuilds it against *its own* project, not this pool's default one.
    /// `None` covers both "no `Meta` at all" (a session that predates this
    /// daemon writing one) and "pre-v2 `Meta`" (no `project_root` field
    /// existed yet) — both mean "nothing recorded, infer the pool default",
    /// same spirit as `SceneCheck::Inferred`. Distinguishing those from a
    /// v2 `Meta` that explicitly recorded `project_root: null` (a genuine
    /// no-project session) is exactly why this returns the raw
    /// `schema_version` instead of collapsing straight to `Option<PathBuf>`.
    /// The recorded `scene` rides along because it comes from the same
    /// `Meta` and reattaching needs both: a session's settings are resolved
    /// against its own project *and* its own scene tier.
    pub(super) async fn recorded_project(
        &self,
        session_id: &str,
    ) -> Option<(u32, Option<String>, Option<String>)> {
        let entries = self.load_entries(session_id).await.ok()?;
        let history::entry::LogEntry::Meta {
            schema_version,
            project_root,
            scene,
            ..
        } = find_meta(&entries)?
        else {
            return None;
        };
        Some((*schema_version, project_root.clone(), scene.clone()))
    }
}
