//! MCP 连接：准入、后台首连与重连、按项目共享的 manager、server 增改、OAuth 授权相关 RPC。

use super::*;

impl SessionPool {
    /// The MCP servers a `(project, scene)` with these settings connects:
    /// its `mcp_servers` and every active plugin's `[[mcp]]` declarations.
    #[cfg(feature = "mcp")]
    pub(super) fn configured_mcp_servers(
        &self,
        settings: &Settings,
    ) -> HashMap<String, serde_json::Value> {
        let mut servers = settings.mcp_servers.clone();
        servers.extend(self.plugins.mcp_servers());
        servers
    }

    /// The one gate every MCP server passes before this pool connects it.
    #[cfg(feature = "mcp")]
    pub(super) fn admit_mcp(
        &self,
        name: &str,
        cfg: &mcp::config::McpServerConfig,
    ) -> Result<(), String> {
        self.mcp_policy.admit(name, cfg).inspect_err(|reason| {
            tracing::warn!(server = %name, %reason, "MCP server not connected");
        })
    }

    /// A stdio server starts, and its relative command and paths are found,
    /// in the directory of whoever declared it: a plugin's package root, the
    /// project root for the project's own, the scene's or the global
    /// configuration directory otherwise. Never the daemon's working
    /// directory, which may be some other project.
    #[cfg(feature = "mcp")]
    pub(super) fn placed_mcp_server(
        &self,
        settings: &Settings,
        name: &str,
        cfg: mcp::config::McpServerConfig,
    ) -> mcp::config::McpServerConfig {
        let home = self
            .plugins
            .mcp_server_dirs()
            .remove(name)
            .or_else(|| settings.mcp_server_dirs.get(name).cloned())
            .unwrap_or_else(|| self.paths.global_root());
        cfg.run_in(&home)
    }

    /// Parse the servers `settings` configure and pass each through
    /// `admit_mcp`, a project's own after `confined_to_own_env`. Every entry
    /// that will not be connected gets a `mcp_connect_failed` event here.
    #[cfg(feature = "mcp")]
    pub(super) fn admit_mcp_servers(&self, settings: &Settings) -> AdmittedMcp {
        let plugin_servers = self.plugins.mcp_servers();
        let projects_own: std::collections::HashSet<String> = project_tier_mcp_servers(settings)
            .into_iter()
            .filter(|name| !plugin_servers.contains_key(name))
            .collect();
        let mut admitted = AdmittedMcp::default();
        for (name, v) in &self.configured_mcp_servers(settings) {
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
    #[cfg(feature = "mcp")]
    pub async fn mcp_status(&self) -> Vec<serde_json::Value> {
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let manager = self
            .mcp_by_project
            .current(&(Some(self.cwd.clone()), startup_scene))
            .unwrap_or_else(|| Arc::new(McpManager::empty()));
        let mut entries = mcp_status_entries(&manager);
        entries.extend(untrusted_mcp_entries(&*self.settings.read().await));
        entries
    }

    /// Connect the default project's MCP servers (`configured_mcp_servers`)
    /// in the background — never blocks the caller. Each outcome is a
    /// `daemon.event`: `mcp_connected`, or `mcp_connect_failed` for a config
    /// that does not parse, a server the policy denies, or one that could
    /// not be reached, with the reason. This is the default project's first
    /// connect (see `first_mcp_connect`), so a session created meanwhile
    /// waits for it rather than connecting again.
    #[cfg(feature = "mcp")]
    pub fn connect_mcp_servers_in_background(self: &Arc<Self>) {
        let _ = self.this.set(Arc::downgrade(self));
        self.watch_mcp_auth();
        let pool = self.clone();
        tokio::spawn(async move {
            let settings = pool.settings.read().await.clone();
            if pool.configured_mcp_servers(&settings).is_empty() {
                return;
            }
            let key = (Some(pool.cwd.clone()), settings.paths.scope.clone());
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
    #[cfg(feature = "mcp")]
    pub(super) async fn first_mcp_connect(
        &self,
        key: ProjectSceneKey,
        settings: &Settings,
    ) -> Arc<McpManager> {
        let slot = self.mcp_by_project.slot(&key);
        let update = slot.update().await;
        if let Some(existing) = update.current() {
            return existing;
        }
        let AdmittedMcp {
            servers: admitted,
            denied,
            ..
        } = self.admit_mcp_servers(settings);
        let pool = self.this.get().and_then(std::sync::Weak::upgrade);
        let wait = pool.as_ref().map(|_| MCP_SESSION_CONNECT_WAIT);
        let (mut manager, late) = McpManager::connect_all_within(
            admitted,
            settings.sandbox.subprocess_env.clone(),
            Some(self.mcp_auth.clone()),
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
    #[cfg(feature = "mcp")]
    pub(super) async fn finish_mcp_connects(
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
            let Some(slot) = pool.mcp_by_project.existing(&key) else {
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
    /// startup. `create()` sources MCP from the `mcp_by_project` cache, not
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
    /// Only replaces `mcp_by_project`'s entry for `(project_root, scene_id)`;
    /// every other pair's cached connections are left untouched.
    ///
    /// Returns `(connected, failed, connecting)` server names for
    /// `ReloadReport`.
    #[cfg(feature = "mcp")]
    pub(super) async fn reconcile_mcp_servers(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        settings: &Settings,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        let slot = self.mcp_by_project.slot(&key);
        let update = slot.update().await;
        let AdmittedMcp {
            servers: admitted,
            denied,
            refused,
        } = self.admit_mcp_servers(settings);
        let requested: std::collections::HashSet<String> = admitted.keys().cloned().collect();
        let pool = self.this.get().and_then(std::sync::Weak::upgrade);
        let wait = pool.as_ref().map(|_| MCP_SESSION_CONNECT_WAIT);
        let (manager, late) = McpManager::connect_all_within(
            admitted,
            settings.sandbox.subprocess_env.clone(),
            Some(self.mcp_auth.clone()),
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
    #[cfg(feature = "mcp")]
    pub async fn add_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        config: serde_json::Value,
    ) -> Result<serde_json::Value, PoolError> {
        self.put_mcp_server(project_root, scene_id, name, config, false)
            .await
    }

    /// `mcp.updateServer` — replace the configuration of a server the
    /// project's settings already declare (a rotated token, a new header) and
    /// reconnect it. Sessions on the project pick the new connection up at
    /// their next turn.
    #[cfg(feature = "mcp")]
    pub async fn update_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        config: serde_json::Value,
    ) -> Result<serde_json::Value, PoolError> {
        self.put_mcp_server(project_root, scene_id, name, config, true)
            .await
    }

    /// Refused before anything is written when the MCP policy denies it:
    /// a settings entry the pool would only ever deny is no use to anyone.
    #[cfg(feature = "mcp")]
    pub(super) async fn put_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        config: serde_json::Value,
        must_exist: bool,
    ) -> Result<serde_json::Value, PoolError> {
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

    #[cfg(feature = "mcp")]
    pub(super) async fn write_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        cfg: &mcp::config::McpServerConfig,
        config: serde_json::Value,
        must_exist: bool,
    ) -> Result<serde_json::Value, String> {
        use base::settings::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};
        let settings_path = self.settings_path_of(project_root);
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
            Some(project_root.unwrap_or(self.cwd.as_path()).to_path_buf()),
            scene_id.to_string(),
        );
        let target_settings = self
            .settings_for_project(target.0.as_deref(), scene_id)
            .await;
        let env = target_settings.sandbox.subprocess_env.clone();
        let cfg = &cfg
            .clone()
            .run_in(target.0.as_deref().unwrap_or(self.cwd.as_path()));
        let slot = self.mcp_by_project.slot(&target);
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
                .with_auth(Some(self.mcp_auth.clone()))
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

        let mut result = serde_json::json!({
            "written_to": settings_path.display().to_string(),
            "servers": entries,
        });
        if let Some(path) = secrets_written_to {
            result["secrets_written_to"] = serde_json::Value::String(path);
        }
        Ok(result)
    }

    /// Report this pool's MCP authorization transitions as `daemon.event`s,
    /// and take a server whose tokens stopped working off every connection
    /// that has it.
    #[cfg(feature = "mcp")]
    pub(super) fn watch_mcp_auth(self: &Arc<Self>) {
        use mcp::oauth::AuthEvent;
        let pool = Arc::downgrade(self);
        self.mcp_auth.set_listener(Arc::new(move |event| {
            let Some(pool) = pool.upgrade() else {
                return;
            };
            pool.emit_event(match &event {
                AuthEvent::Required { server, scopes, .. }
                | AuthEvent::StepUp { server, scopes, .. } => api::DaemonEvent::McpAuthRequired {
                    server: server.clone(),
                    scopes: scopes.clone(),
                },
                AuthEvent::Completed { server, .. } => api::DaemonEvent::McpAuthCompleted {
                    server: server.clone(),
                },
                AuthEvent::Failed { server, error, .. } => api::DaemonEvent::McpAuthFailed {
                    server: server.clone(),
                    error: error.clone(),
                },
            });
            if let AuthEvent::Required { url, .. } = event {
                tokio::spawn(async move { pool.demote_mcp_servers(&url).await });
            }
        }));
    }

    #[cfg(feature = "mcp")]
    pub(super) async fn invalidate_mcp_keys(&self, keys: &[ProjectSceneKey]) {
        use crate::watch_hub::Tier;
        for (root, scene) in keys {
            let tier = match root {
                Some(root) => Tier::Project(root.clone()),
                None => Tier::Scene(scene.clone()),
            };
            self.invalidate_bindings(Some(&tier)).await;
        }
    }

    /// Disconnect every server at `url` until someone signs in again.
    #[cfg(feature = "mcp")]
    pub(super) async fn demote_mcp_servers(&self, url: &str) {
        let mut touched = Vec::new();
        for (key, slot) in self.mcp_by_project.slots() {
            let update = slot.update().await;
            let Some(manager) = update.current() else {
                continue;
            };
            let names: Vec<String> = manager
                .servers_at(url)
                .into_iter()
                .filter(|n| manager.clients().iter().any(|c| c.server_name() == n))
                .collect();
            if names.is_empty() {
                continue;
            }
            let mut updated = manager.share();
            for name in &names {
                updated.demote(name);
            }
            updated.refresh_tools().await;
            update.set(Arc::new(updated));
            touched.push(key);
        }
        self.invalidate_mcp_keys(&touched).await;
    }

    /// Connect again every server at `url` — and only those — wherever this
    /// pool has it, connected or waiting for a sign-in. Each outcome is a
    /// `mcp_connected` / `mcp_connect_failed` event.
    #[cfg(feature = "mcp")]
    pub(super) async fn reconnect_mcp_servers(&self, url: &str) -> Vec<String> {
        self.reconnect_mcp_where(|manager| manager.servers_at(url))
            .await
    }

    /// [`Self::reconnect_mcp_servers`] for the servers `pick` names in each
    /// cached manager.
    #[cfg(feature = "mcp")]
    pub(super) async fn reconnect_mcp_where(
        &self,
        pick: impl Fn(&McpManager) -> Vec<String>,
    ) -> Vec<String> {
        let mut touched = Vec::new();
        let mut outcomes: HashMap<String, Result<mcp::manager::ServerStatus, Option<String>>> =
            HashMap::new();
        for (key, slot) in self.mcp_by_project.slots() {
            let update = slot.update().await;
            let Some(manager) = update.current() else {
                continue;
            };
            let names = pick(&manager);
            if names.is_empty() {
                continue;
            }
            let mut updated = manager.share();
            for name in &names {
                if let Some(cfg) = updated.config_of(name).cloned() {
                    updated.add_server(name, &cfg).await;
                }
            }
            updated.refresh_tools().await;
            let statuses = updated.server_statuses();
            for name in names {
                let outcome = match statuses.iter().find(|s| s.name == name) {
                    Some(status) => Ok(status.clone()),
                    None => Err(updated
                        .failed()
                        .iter()
                        .find(|f| f.name == name)
                        .map(|f| f.error.clone())),
                };
                let telling = |o: &Result<_, Option<String>>| match o {
                    Ok(_) => 2,
                    Err(Some(_)) => 1,
                    Err(None) => 0,
                };
                let seen = outcomes.entry(name).or_insert(Err(None));
                if telling(&outcome) > telling(seen) {
                    *seen = outcome;
                }
            }
            update.set(Arc::new(updated));
            touched.push(key);
        }
        self.invalidate_mcp_keys(&touched).await;
        let mut names: Vec<String> = outcomes.keys().cloned().collect();
        names.sort();
        for (name, outcome) in outcomes {
            self.emit_event(match outcome {
                Ok(s) => mcp_connected_event(&s),
                Err(error) => api::DaemonEvent::McpConnectFailed {
                    server: name,
                    error: match error {
                        Some(error) => format!("connect failed after authorizing: {error}"),
                        None => {
                            "connect failed after authorizing — see mcp.status and the daemon log"
                                .into()
                        }
                    },
                },
            });
        }
        names
    }

    /// `mcp.auth.setIdentity` — the user's enterprise identity, which every
    /// `"grant": "enterprise"` server waiting for one is then connected with.
    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_set_identity(
        &self,
        identity: mcp::oauth::EnterpriseIdentity,
    ) -> Result<serde_json::Value, PoolError> {
        if self.mcp_auth.enterprise_idp().is_none() {
            return Err(PoolError::InvalidRequest(
                "no enterprise IdP is configured: the host's Assembly::mcp_enterprise_idp or \
                 the MCP policy's enterprise_idp names one"
                    .into(),
            ));
        }
        self.mcp_auth.set_identity(Some(identity));
        let reconnected = self
            .reconnect_mcp_where(|manager| {
                manager
                    .pending()
                    .iter()
                    .filter(|p| {
                        manager
                            .config_of(&p.name)
                            .and_then(|c| c.oauth())
                            .is_some_and(|o| o.grant == mcp::config::OAuthGrant::Enterprise)
                    })
                    .map(|p| p.name.clone())
                    .collect()
            })
            .await;
        Ok(serde_json::json!({"reconnected": reconnected}))
    }

    /// The configuration of the MCP server `name` on a project, for a
    /// sign-in. A server the policy denies never gets one.
    #[cfg(feature = "mcp")]
    pub(super) async fn mcp_server_to_authorize(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
    ) -> Result<mcp::config::McpServerConfig, PoolError> {
        let root = project_root.unwrap_or(self.cwd.as_path());
        let manager = self.mcp_for_project(Some(root), scene_id).await;
        if let Some(cfg) = manager.config_of(name) {
            return Ok(cfg.clone());
        }
        if let Some(denied) = manager.denied().iter().find(|d| d.name == name) {
            return Err(PoolError::McpServerDenied(denied.reason.clone()));
        }
        Err(PoolError::InvalidRequest(format!(
            "no MCP server `{name}` is configured for this project"
        )))
    }

    /// `mcp.auth.start` — begin a sign-in to an MCP server. A loopback flow
    /// completes itself when the browser comes back; an external one waits
    /// for `mcp.auth.complete`.
    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_start(
        self: &Arc<Self>,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        redirect: mcp::oauth::Redirect,
        scopes: Option<Vec<String>>,
    ) -> Result<serde_json::Value, PoolError> {
        let cfg = self
            .mcp_server_to_authorize(project_root, scene_id, name)
            .await?;
        let mut start = self
            .mcp_auth
            .start(name, &cfg, redirect, scopes)
            .await
            .map_err(PoolError::from)?;
        if let Some(mut callbacks) = start.callbacks.take() {
            let pool = Arc::downgrade(self);
            let flow_id = start.flow_id.clone();
            tokio::spawn(async move {
                while let Some(callback) = callbacks.recv().await {
                    let Some(pool) = pool.upgrade() else {
                        return;
                    };
                    let outcome = pool.finish_mcp_auth(&flow_id, &callback.url).await;
                    let open = pool.mcp_auth.flow_server(&flow_id).await.is_some();
                    let _ = callback
                        .reply
                        .send(outcome.map(|done| done.server).map_err(|e| e.to_string()));
                    if !open {
                        return;
                    }
                }
            });
        }
        Ok(serde_json::json!({
            "flow_id": start.flow_id,
            "authorize_url": start.authorize_url,
            "redirect_uri": start.redirect_uri,
            "expires_in": start.expires_in,
        }))
    }

    #[cfg(feature = "mcp")]
    pub(super) async fn finish_mcp_auth(
        &self,
        flow_id: &str,
        callback_url: &str,
    ) -> Result<mcp::oauth::FlowDone, mcp::oauth::AuthFlowError> {
        let done = self.mcp_auth.complete(flow_id, callback_url).await?;
        self.reconnect_mcp_servers(&done.url).await;
        Ok(done)
    }

    /// `mcp.auth.complete` — finish a sign-in with the URL the browser was
    /// sent back to, then reconnect that server.
    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_complete(
        &self,
        flow_id: &str,
        callback_url: &str,
    ) -> Result<serde_json::Value, PoolError> {
        let done = self
            .finish_mcp_auth(flow_id, callback_url)
            .await
            .map_err(PoolError::from)?;
        Ok(serde_json::json!({"server": done.server, "scopes": done.scopes}))
    }

    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_cancel(&self, flow_id: &str) -> Result<serde_json::Value, PoolError> {
        let server = self
            .mcp_auth
            .cancel(flow_id)
            .await
            .map_err(PoolError::from)?;
        Ok(serde_json::json!({"server": server}))
    }

    /// `mcp.auth.logout` — revoke and forget a server's tokens, and
    /// disconnect it.
    #[cfg(feature = "mcp")]
    pub async fn mcp_auth_logout(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
    ) -> Result<serde_json::Value, PoolError> {
        let cfg = self
            .mcp_server_to_authorize(project_root, scene_id, name)
            .await?;
        self.mcp_auth
            .logout(name, &cfg)
            .await
            .map_err(PoolError::from)?;
        if let Some(url) =
            mcp::oauth::server_url(&cfg).and_then(|u| mcp::oauth::canonical_server_url(&u).ok())
        {
            self.demote_mcp_servers(&url).await;
        }
        Ok(serde_json::json!({"server": name}))
    }
}
