//! MCP 连接：准入、后台首连与迟到的连接、按 `(project, scene)` 共享的 manager、会话
//! 拿到的那一份、配置变化后的对账，以及 `mcp.addServer` / `mcp.updateServer`。

use super::*;
use crate::session_pool::parts::bindings::tier_covers;
use crate::session_pool::*;

impl SessionPool {
    /// This pool's MCP authorization state.
    pub fn mcp_auth(&self) -> Arc<mcp::oauth::McpAuth> {
        self.mcp.auth.clone()
    }

    /// Drop the cached connections whose `mcp_servers` came from `tier`, so the
    /// next session or rebuild on those keys connects with the changed config
    /// (a new `Authorization`, say) instead of reusing the old connection.
    /// `keep` is a key the caller reconnects itself.
    pub(in crate::session_pool) async fn forget_mcp_built_from(
        &self,
        tier: &crate::watch_hub::Tier,
        keep: Option<&ProjectSceneKey>,
    ) {
        self.mcp
            .cache
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
    pub(in crate::session_pool) async fn mcp_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<McpManager> {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        if let Some(existing) = self.mcp.cache.current(&key) {
            return existing;
        }
        let settings = self.settings_for_project(project_root, scene_id).await;
        self.first_mcp_connect(key, &settings).await
    }

    /// What a session's engine gets of its pair's MCP servers: its own
    /// `McpManager` built from the pair's connected client handles (cheap
    /// `Arc` clones — not a reconnect), since a manager needs `&mut self` to
    /// refresh its tools, which a shared one cannot give it. `None` when
    /// nothing is connected or connecting, so the common "no MCP servers"
    /// case pays nothing. The count is the pair's connected servers.
    pub(in crate::session_pool) async fn mcp_session_module(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> (usize, Option<Arc<dyn runtime::module::Module>>) {
        let central = self.mcp_for_project(project_root, scene_id).await;
        let count = central.server_count();
        if count == 0 && central.pending().is_empty() {
            return (count, None);
        }
        let mut per_session = central.share();
        per_session.refresh_tools().await;
        (count, Some(Arc::new(mcp::McpModule::new(per_session))))
    }

    /// The MCP servers a `(project, scene)` with these settings connects:
    /// its `mcp_servers` and every active plugin's `[[mcp]]` declarations.
    pub(in crate::session_pool) fn configured_mcp_servers(
        &self,
        settings: &Settings,
    ) -> HashMap<String, serde_json::Value> {
        let mut servers = settings.mcp_servers.clone();
        servers.extend(self.plugins.mcp_servers());
        servers
    }

    /// The one gate every MCP server passes before this pool connects it.
    pub(in crate::session_pool) fn admit_mcp(
        &self,
        name: &str,
        cfg: &mcp::config::McpServerConfig,
    ) -> Result<(), String> {
        self.mcp.policy.admit(name, cfg).inspect_err(|reason| {
            tracing::warn!(server = %name, %reason, "MCP server not connected");
        })
    }

    /// A stdio server starts, and its relative command and paths are found,
    /// in the directory of whoever declared it: a plugin's package root, the
    /// project root for the project's own, the scene's or the global
    /// configuration directory otherwise. Never the daemon's working
    /// directory, which may be some other project.
    pub(in crate::session_pool) fn placed_mcp_server(
        &self,
        settings: &base::settings::LoadedSettings,
        name: &str,
        cfg: mcp::config::McpServerConfig,
    ) -> mcp::config::McpServerConfig {
        let home = self
            .plugins
            .mcp_server_dirs()
            .remove(name)
            .or_else(|| settings.mcp_server_dirs.get(name).cloned())
            .unwrap_or_else(|| self.bindings.paths().global_root());
        cfg.run_in(&home)
    }

    /// Parse the servers `settings` configure and pass each through
    /// `admit_mcp`, a project's own after `confined_to_own_env`. Every entry
    /// that will not be connected gets a `mcp_connect_failed` event here.
    pub(in crate::session_pool) fn admit_mcp_servers(
        &self,
        settings: &base::settings::LoadedSettings,
    ) -> AdmittedMcp {
        let plugin_servers = self.plugins.mcp_servers();
        let projects_own: std::collections::HashSet<String> = project_tier_mcp_servers(settings)
            .into_iter()
            .filter(|name| !plugin_servers.contains_key(name))
            .collect();
        let mut admitted = AdmittedMcp::default();
        for (name, v) in &self.configured_mcp_servers(&settings.values) {
            let parsed = serde_json::from_value::<mcp::config::McpServerConfig>(v.clone())
                .map_err(|e| {
                    tracing::warn!(server = %name, error = %e, "invalid mcp_servers config, skipping");
                    format!("invalid config: {e}")
                })
                .and_then(|cfg| {
                    if projects_own.contains(name) {
                        cfg.confined_to_own_env().inspect_err(|reason| {
                            tracing::warn!(server = %name, %reason, "MCP server not connected");
                        })
                    } else {
                        Ok(cfg)
                    }
                });
            let refusal = match parsed {
                Ok(cfg) => match self.admit_mcp(name, &cfg) {
                    Ok(()) => {
                        admitted
                            .servers
                            .insert(name.clone(), self.placed_mcp_server(settings, name, cfg));
                        continue;
                    }
                    Err(reason) => {
                        admitted.denied.push(mcp::manager::DeniedServer {
                            name: name.clone(),
                            transport: cfg.transport_kind(),
                            reason: reason.clone(),
                        });
                        reason
                    }
                },
                Err(reason) => reason,
            };
            self.emit_event(api::DaemonEvent::McpConnectFailed {
                server: name.clone(),
                error: refusal,
            });
            admitted.refused.push(name.clone());
        }
        admitted
    }

    /// Current MCP connection status for this pool's *default* project —
    /// see `mcp::manager::McpManager::server_statuses`. A pure cache read:
    /// unlike `mcp_for_project`, this never triggers a connect, so an
    /// unconnected (or not-yet-connected) project just reports no servers
    /// rather than paying for a connect attempt on a status query.
    pub async fn mcp_status(&self) -> Vec<api::McpServerStatus> {
        let startup_scene = self.bindings.startup_scene().await;
        let manager = self
            .mcp
            .cache
            .current(&(Some(self.cwd().to_path_buf()), startup_scene))
            .unwrap_or_else(|| Arc::new(McpManager::empty()));
        let mut entries = mcp_status_entries(&manager);
        entries.extend(untrusted_mcp_entries(&*self.bindings.pool_settings().await));
        entries
    }

    /// Connect the default project's MCP servers (`configured_mcp_servers`)
    /// in the background — never blocks the caller. Each outcome is a
    /// `daemon.event`: `mcp_connected`, or `mcp_connect_failed` for a config
    /// that does not parse, a server the policy denies, or one that could
    /// not be reached, with the reason. This is the default project's first
    /// connect (see `first_mcp_connect`), so a session created meanwhile
    /// waits for it rather than connecting again.
    pub fn connect_mcp_servers_in_background(self: &Arc<Self>) {
        self.watch_mcp_auth();
        let pool = self.clone();
        tokio::spawn(async move {
            let settings = pool.bindings.pool_settings().await;
            if pool.configured_mcp_servers(&settings.values).is_empty() {
                return;
            }
            let key = (Some(pool.cwd().to_path_buf()), settings.paths.scope.clone());
            let manager = pool.first_mcp_connect(key, &settings).await;
            for status in manager.server_statuses() {
                pool.emit_event(mcp_connected_event(&status));
            }
            let not_connected = manager.failed().iter().map(|f| &f.name);
            for name in not_connected.chain(manager.pending().iter().map(|p| &p.name)) {
                pool.emit_event(mcp_connect_failed_event(&manager, name));
            }
        });
    }

    /// The pair's MCP manager: the cached one, or a new connect of the
    /// servers `settings` declare, cached. The connect returns after
    /// [`MCP_SESSION_CONNECT_WAIT`]; a server still connecting then is
    /// finished by [`Self::finish_mcp_connects`]. A caller that arrives while
    /// the pair's first connect runs waits for it rather than starting every
    /// server a second time; other pairs do not wait.
    pub(in crate::session_pool) async fn first_mcp_connect(
        &self,
        key: ProjectSceneKey,
        settings: &base::settings::LoadedSettings,
    ) -> Arc<McpManager> {
        let slot = self.mcp.cache.slot(&key);
        let update = slot.update().await;
        if let Some(existing) = update.current() {
            return existing;
        }
        let AdmittedMcp {
            servers: admitted,
            denied,
            ..
        } = self.admit_mcp_servers(settings);
        let pool = self.this.upgrade();
        let wait = pool.as_ref().map(|_| MCP_SESSION_CONNECT_WAIT);
        let (mut manager, late) = McpManager::connect_all_within(
            admitted,
            settings.values.sandbox.subprocess_env.clone(),
            Some(self.mcp.auth.clone()),
            wait,
        )
        .await;
        // A server may have announced a change while this manager was being
        // built, after its list was read.
        manager.refresh_if_changed().await;
        let manager = Arc::new(manager.with_denied(denied));
        update.set(manager.clone());
        drop(update);
        if let (Some(pool), Some(late)) = (pool, late) {
            tokio::spawn(Self::finish_mcp_connects(Arc::downgrade(&pool), key, late));
        }
        manager
    }

    /// Take each server that finishes after its pair's manager was handed
    /// out into that manager, announce it, and have the pair's sessions
    /// rebuilt on it before their next turn. Stops when the pair's manager
    /// has been replaced or dropped since — a reload connects anew.
    pub(in crate::session_pool) async fn finish_mcp_connects(
        pool: std::sync::Weak<Self>,
        key: ProjectSceneKey,
        mut late: mcp::manager::LateConnects,
    ) {
        while let Some(outcome) = late.next().await {
            let Some(pool) = pool.upgrade() else {
                return;
            };
            let server = outcome.server().to_string();
            let connected = outcome.connected();
            let Some(slot) = pool.mcp.cache.existing(&key) else {
                return;
            };
            let updated = {
                let update = slot.update().await;
                let Some(current) = update.current() else {
                    return;
                };
                let mut updated = current.share();
                if !updated.absorb(outcome) {
                    continue;
                }
                let updated = Arc::new(updated);
                update.set(updated.clone());
                updated
            };
            pool.emit_event(match connected {
                Some(s) => mcp_connected_event(&s),
                None => mcp_connect_failed_event(&updated, &server),
            });
            pool.invalidate_mcp_keys(std::slice::from_ref(&key)).await;
        }
    }

    /// Reconnect MCP servers to match `mcp_servers` (from freshly-reloaded
    /// `Settings`) — the `config.reload`/`config.setProvider` counterpart to
    /// `connect_mcp_servers_in_background`, which runs once at daemon
    /// startup. `create()` sources MCP from the MCP cache, not
    /// from `Settings`, so a reloaded `mcp_servers` reaches sessions only
    /// through this.
    ///
    /// Connects the *complete* new config rather than diffing it against the
    /// old one: a config that removed every server produces an empty
    /// `McpManager`, which disconnects everything. Awaited inline, unlike the
    /// startup path, so the `config.reload` / `config.setProvider` response
    /// reports the connections as they are — but for at most
    /// [`MCP_SESSION_CONNECT_WAIT`]: a server still connecting then is
    /// reported as connecting and finished in the background like a
    /// session's (`finish_mcp_connects`).
    ///
    /// Only replaces the cache entry for `(project_root, scene_id)`;
    /// every other pair's cached connections are left untouched.
    ///
    /// Returns `(connected, failed, connecting)` server names for
    /// the reload's `RoutingStatus`.
    pub(in crate::session_pool) async fn reconcile_mcp_servers(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        settings: &base::settings::LoadedSettings,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        let slot = self.mcp.cache.slot(&key);
        let update = slot.update().await;
        let AdmittedMcp {
            servers: admitted,
            denied,
            refused,
        } = self.admit_mcp_servers(settings);
        let requested: std::collections::HashSet<String> = admitted.keys().cloned().collect();
        let pool = self.this.upgrade();
        let wait = pool.as_ref().map(|_| MCP_SESSION_CONNECT_WAIT);
        let (manager, late) = McpManager::connect_all_within(
            admitted,
            settings.values.sandbox.subprocess_env.clone(),
            Some(self.mcp.auth.clone()),
            wait,
        )
        .await;
        let manager = manager.with_denied(denied);
        let statuses = manager.server_statuses();
        let connected: Vec<String> = statuses.iter().map(|s| s.name.clone()).collect();
        let mut connecting: Vec<String> = manager
            .connecting()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        connecting.sort();
        let settled: std::collections::HashSet<&String> =
            connected.iter().chain(&connecting).collect();
        let unreachable: Vec<String> = requested
            .iter()
            .filter(|name| !settled.contains(name))
            .cloned()
            .collect();

        for status in &statuses {
            self.emit_event(mcp_connected_event(status));
        }
        for name in &unreachable {
            self.emit_event(mcp_connect_failed_event(&manager, name));
        }
        let mut failed = unreachable;
        failed.extend(refused);

        update.set(Arc::new(manager));
        drop(update);
        if let (Some(pool), Some(late)) = (pool, late) {
            tokio::spawn(Self::finish_mcp_connects(Arc::downgrade(&pool), key, late));
        }
        (connected, failed, connecting)
    }

    /// Write an MCP server's configuration into the project tier — its
    /// `headers` and `env` into the uncommitted `settings.local.json`, the
    /// rest into settings.json — then connect it live so it's usable
    /// immediately without a daemon restart.
    pub async fn add_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        config: serde_json::Value,
    ) -> Result<api::McpServerWritten, PoolError> {
        self.put_mcp_server(project_root, scene_id, name, config, false)
            .await
    }

    /// `mcp.updateServer` — replace the configuration of a server the
    /// project's settings already declare (a rotated token, a new header) and
    /// reconnect it. Sessions on the project pick the new connection up at
    /// their next turn.
    pub async fn update_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        config: serde_json::Value,
    ) -> Result<api::McpServerWritten, PoolError> {
        self.put_mcp_server(project_root, scene_id, name, config, true)
            .await
    }

    /// Refused before anything is written when the MCP policy denies it:
    /// a settings entry the pool would only ever deny is no use to anyone.
    pub(in crate::session_pool) async fn put_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        config: serde_json::Value,
        must_exist: bool,
    ) -> Result<api::McpServerWritten, PoolError> {
        let invalid = |message: String| PoolError::InvalidRequest(message);
        if name.trim().is_empty() {
            return Err(invalid("name must not be empty".into()));
        }
        if !config.is_object() {
            return Err(invalid("config must be a JSON object".into()));
        }
        let mut cfg: mcp::config::McpServerConfig = serde_json::from_value(config.clone())
            .map_err(|e| invalid(format!("invalid mcp server config: {e}")))?;
        if self
            .trusted_project_tier(
                project_root,
                scene_id,
                "a server declared in its settings would not start",
                "declare the server in the global tier",
            )
            .await?
        {
            cfg = cfg.confined_to_own_env().map_err(invalid)?;
        }
        self.admit_mcp(name, &cfg)
            .map_err(PoolError::McpServerDenied)?;
        self.write_mcp_server(project_root, scene_id, name, &cfg, config, must_exist)
            .await
            .map_err(invalid)
    }

    pub(in crate::session_pool) async fn write_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        cfg: &mcp::config::McpServerConfig,
        config: serde_json::Value,
        must_exist: bool,
    ) -> Result<api::McpServerWritten, String> {
        use base::settings::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};
        let settings_path = self.bindings.settings_path_of(project_root);
        let local_path = settings_path.with_file_name(SETTINGS_LOCAL_FILE);
        let shown = settings_path.display().to_string();
        let server = name.to_string();
        let local_changed = edit_settings_dir(&settings_path, move |tier| {
            let mut project_json = tier.read(SETTINGS_FILE).map_err(|e| e.to_string())?;
            let mut local_json = tier.read(SETTINGS_LOCAL_FILE).map_err(|e| e.to_string())?;
            let local_before = local_json.clone();
            let declared = [&project_json, &local_json].iter().any(|tier| {
                tier.get("mcp_servers")
                    .and_then(|servers| servers.get(&server))
                    .is_some()
            });
            if must_exist && !declared {
                return Err(format!("no mcp server `{server}` in {shown}"));
            }

            let mut config = config;
            let secrets = take_mcp_server_secrets(&mut config);
            mcp_servers_of(&mut project_json).insert(server.clone(), config);
            // The server is replaced whole: what the local file held for it
            // would otherwise merge over the new configuration.
            let local_servers = mcp_servers_of(&mut local_json);
            if secrets.is_empty() {
                local_servers.remove(&server);
            } else {
                local_servers.insert(
                    server.clone(),
                    serde_json::Value::Object(secrets.into_iter().collect()),
                );
            }

            let local_changed = local_json != local_before;
            if local_changed {
                tier.write(SETTINGS_LOCAL_FILE, &local_json)
                    .map_err(|e| e.to_string())?;
            }
            tier.write(SETTINGS_FILE, &project_json)
                .map_err(|e| e.to_string())?;
            Ok(local_changed)
        })
        .await?;
        let secrets_written_to = local_changed.then(|| local_path.display().to_string());

        // The written file and the cache entry are the same pair: a server
        // added to one project's settings connects for that project.
        let target = (
            Some(project_root.unwrap_or(self.cwd()).to_path_buf()),
            scene_id.to_string(),
        );
        let target_settings = self
            .settings_for_project(target.0.as_deref(), scene_id)
            .await;
        let env = target_settings.values.sandbox.subprocess_env.clone();
        let cfg = &cfg
            .clone()
            .run_in(target.0.as_deref().unwrap_or(self.cwd()));
        let slot = self.mcp.cache.slot(&target);
        let (statuses, entries, failure) = {
            let update = slot.update().await;
            let current = update
                .current()
                .unwrap_or_else(|| Arc::new(McpManager::empty()));
            let still_denied = current
                .denied()
                .iter()
                .filter(|d| d.name != name)
                .cloned()
                .collect();
            let mut updated = current
                .share()
                .with_subprocess_env(env.clone())
                .with_auth(Some(self.mcp.auth.clone()))
                .with_denied(still_denied);
            updated.refresh_tools().await;
            updated.add_server(name, cfg).await;
            let statuses = updated.server_statuses();
            let entries = mcp_status_entries(&updated);
            let failure = mcp_connect_failed_event(&updated, name);
            update.set(Arc::new(updated));
            (statuses, entries, failure)
        };
        // A running session holds its own manager, cloned from the cache when
        // its engine was built. Marking its binding stale rebuilds it on the
        // new connection before its next turn; nothing in flight is touched.
        if let Some(root) = target.0 {
            self.invalidate_bindings(Some(&crate::watch_hub::Tier::Project(root)))
                .await;
        }

        // Same notification contract as the startup background connect —
        // see `connect_mcp_servers_in_background`.
        match statuses.iter().find(|s| s.name == name) {
            Some(s) => self.emit_event(mcp_connected_event(s)),
            None => self.emit_event(failure),
        }

        Ok(api::McpServerWritten {
            written_to: settings_path.display().to_string(),
            secrets_written_to,
            servers: entries,
        })
    }
}
