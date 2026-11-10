//! Handlers for daemon-, config-, project-, scene-, plugin-, MCP- and
//! import-scoped methods.

use super::*;

impl DaemonServer {
    /// `daemon.subscribeEvents` — no params. Returns an immediate ack
    /// (`{"subscribed": true}`) and, from then on, pushes every future
    /// `daemon.event` notification (see `SessionPool::emit_event`) to this
    /// same connection as a `DaemonEventFrame` — indefinitely, until the
    /// connection closes. Does not replay past events; subscribe before the
    /// event you care about can happen (e.g. right after startup, before
    /// relying on `mcp_connected`/`import_detected`). Subscribing again on
    /// the same connection changes nothing.
    pub(super) async fn daemon_subscribe_events(
        &self,
        client: Arc<Client>,
    ) -> crate::rpc::methods::daemon::EventsSubscribed {
        let subscribed = crate::rpc::methods::daemon::EventsSubscribed { subscribed: true };
        if !client.start_daemon_events() {
            return subscribed;
        }
        let mut rx = self.pool.subscribe_events();
        let forward = client;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let frame = crate::rpc::DaemonEventFrame::pool(&event);
                        if !send_frame(&forward, &frame).await {
                            break; // connection closed
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        let frame = crate::rpc::DaemonEventFrame::server(
                            crate::rpc::ServerDaemonEvent::EventsLagged { missed },
                        );
                        if !send_frame(&forward, &frame).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        subscribed
    }

    /// The `(project_root, scene)` of a [`ConfigTarget`], this daemon's own
    /// where it names none.
    pub(super) fn target_of(
        &self,
        target: crate::rpc::methods::ConfigTarget,
    ) -> (Option<std::path::PathBuf>, String) {
        let scene = target
            .scene
            .unwrap_or_else(|| self.pool.scene_id().to_string());
        (target.project_root, scene)
    }

    /// The `(project_root, scene)` pair a configuration method acts on.
    ///
    /// Omitted means this daemon's own — the behaviour every one of these
    /// methods had when it could only ever act on that one pair. A named
    /// `project_root` reaches another project this daemon serves; `null` is
    /// refused, because these methods write (or reload) the *project* tier
    /// and a session with no project has no project tier — the same answer
    /// `config.get {tier: "project"}` already gives.
    pub(super) fn config_target(
        &self,
        params: &serde_json::Value,
    ) -> Result<(Option<std::path::PathBuf>, String), &'static str> {
        let scene = params
            .get("scene")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| self.pool.scene_id().to_string());
        match params.get("project_root") {
            None => Ok((None, scene)),
            Some(serde_json::Value::String(p)) => Ok((Some(std::path::PathBuf::from(p)), scene)),
            Some(_) => {
                Err("project_root must be a path: a session with no project has no project tier")
            }
        }
    }

    /// Shared by `scene.describe` and `config.get`: the `(scene,
    /// project_root, include_secrets)` triple they both key on.
    ///
    /// `project_root` absent and `project_root: null` mean the same thing
    /// here — describe the scene with no project — because unlike
    /// `session.create` there is no session to bind and so no difference
    /// between "unspecified" and "explicitly none".
    pub(super) fn describe_target(
        &self,
        params: &serde_json::Value,
    ) -> (Option<String>, Option<std::path::PathBuf>, bool) {
        let scene = params
            .get("scene")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let project_root = params
            .get("project_root")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);
        let include_secrets = params
            .get("include_secrets")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        (scene, project_root, include_secrets)
    }

    /// `scene.describe` params:
    /// `{"scene": "code", "project_root": "...", "include_secrets": false}`
    pub(super) async fn method_scene_describe(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let (scene, project_root, include_secrets) = self.describe_target(&params);
        let Some(scene) = scene else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing scene");
        };
        match self
            .pool
            .describe_scene(&scene, project_root.as_deref(), include_secrets)
            .await
        {
            Ok(v) => RpcResponse::ok(id, v),
            Err(e) => failure(id, e),
        }
    }

    /// `mcp.addServer` params: `{"name": "...", "config": {"type": "stdio", ...}}`
    /// (`config` is an `mcp::config::McpServerConfig`, tagged by `type`).
    /// `plugin.install` params:
    /// ```json
    /// {
    ///   "name": "code-review-helper",
    ///   "version": "1.0.0",
    ///   "download_url": "file:///path/to/plugin.zip",   // or https://...
    ///   "checksum": "<sha256 hex>",                       // required for http(s), optional for file://
    ///   "scope": "global"                                 // optional, default "global"; or "scene"
    /// }
    /// ```
    /// No marketplace lookup — installs directly from the given source (see
    /// `plugin::install::install`). Verifies the
    /// checksum before extracting; a network (`http(s)://`) source without
    /// a `checksum` is rejected outright.
    /// Is the plugin subsystem present and switched on? When it isn't, the
    /// `plugin.*` methods answer `PLUGINS_DISABLED` rather than pretending
    /// (an empty `plugin.list` would read as "nothing installed", which is a
    /// different fact from "this build has no plugin support").
    pub(super) fn plugins_enabled(&self) -> bool {
        self.pool.plugin_status() == crate::plugins::PluginStatus::Enabled
    }

    pub(super) fn plugins_disabled(&self, id: serde_json::Value) -> RpcResponse {
        failure(
            id,
            crate::session_pool::api::PoolError::PluginsDisabled {
                status: self.pool.plugin_status().as_str(),
            },
        )
    }

    pub(super) async fn method_plugin_install(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let version = match params.get("version").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing version"),
        };
        let download_url = match params.get("download_url").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing download_url"),
        };
        let checksum = params
            .get("checksum")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        match self
            .pool
            .install_plugin(&name, &version, &download_url, checksum.as_deref(), &scope)
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// `plugin.uninstall` params: `{"name": "...", "version": "..." (optional, omit = all), "scope": "global"|"scene" (optional, default "global")}`
    pub(super) async fn method_plugin_uninstall(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let version = params
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        match self
            .pool
            .uninstall_plugin(&name, version.as_deref(), &scope)
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// Shared handler for `plugin.enable`/`plugin.disable`. Params:
    /// `{"name": "...", "scope": "global"|"scene" (optional, default "global")}`
    pub(super) async fn method_plugin_set_enabled(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        enabled: bool,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        match self.pool.set_plugin_enabled(&name, enabled, &scope).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    #[cfg(feature = "mcp")]
    pub(super) async fn method_mcp_put_server(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        update: bool,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let config = match params.get("config").cloned() {
            Some(c) => c,
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing config"),
        };
        let (project_root, scene) = match self.config_target(&params) {
            Ok(target) => target,
            Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
        };
        let put = if update {
            self.pool
                .update_mcp_server(project_root.as_deref(), &scene, &name, config)
                .await
        } else {
            self.pool
                .add_mcp_server(project_root.as_deref(), &scene, &name, config)
                .await
        };
        match put {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        }
    }

    /// `mcp.auth.start` params: `{"server", "redirect"?: "loopback" |
    /// "external", "redirect_uri"? (external only), "scopes"?}` plus the
    /// `config_target` locators.
    #[cfg(feature = "mcp")]
    pub(super) async fn method_mcp_auth_start(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(server) = params.get("server").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing server");
        };
        let (project_root, scene) = match self.config_target(&params) {
            Ok(target) => target,
            Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
        };
        let redirect_uri = params.get("redirect_uri").and_then(|v| v.as_str());
        let redirect = match params.get("redirect").and_then(|v| v.as_str()) {
            None | Some("loopback") if redirect_uri.is_none() => mcp::oauth::Redirect::Loopback,
            None | Some("loopback") => {
                return RpcResponse::err(
                    id,
                    codes::INVALID_PARAMS,
                    "redirect_uri is only for \"redirect\": \"external\"",
                )
            }
            Some("external") => match redirect_uri {
                Some(uri) => mcp::oauth::Redirect::External(uri.to_string()),
                None => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "\"redirect\": \"external\" needs redirect_uri",
                    )
                }
            },
            Some(other) => {
                return RpcResponse::err(
                    id,
                    codes::INVALID_PARAMS,
                    format!("redirect must be \"loopback\" or \"external\", not {other:?}"),
                )
            }
        };
        let scopes = match params.get("scopes") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => match serde_json::from_value::<Vec<String>>(v.clone()) {
                Ok(scopes) => Some(scopes),
                Err(_) => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "scopes must be an array of strings",
                    )
                }
            },
        };
        match self
            .pool
            .mcp_auth_start(project_root.as_deref(), &scene, server, redirect, scopes)
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        }
    }

    /// `mcp.auth.complete` params: `{"flow_id", "callback_url"}`.
    #[cfg(feature = "mcp")]
    pub(super) async fn method_mcp_auth_complete(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let (Some(flow_id), Some(callback_url)) = (
            params.get("flow_id").and_then(|v| v.as_str()),
            params.get("callback_url").and_then(|v| v.as_str()),
        ) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "needs flow_id and callback_url");
        };
        match self.pool.mcp_auth_complete(flow_id, callback_url).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        }
    }

    /// `mcp.auth.cancel` params: `{"flow_id"}`.
    #[cfg(feature = "mcp")]
    pub(super) async fn method_mcp_auth_cancel(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(flow_id) = params.get("flow_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing flow_id");
        };
        match self.pool.mcp_auth_cancel(flow_id).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        }
    }

    /// `mcp.auth.logout` params: `{"server"}` plus the `config_target`
    /// locators.
    #[cfg(feature = "mcp")]
    pub(super) async fn method_mcp_auth_logout(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(server) = params.get("server").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing server");
        };
        let (project_root, scene) = match self.config_target(&params) {
            Ok(target) => target,
            Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
        };
        match self
            .pool
            .mcp_auth_logout(project_root.as_deref(), &scene, server)
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        }
    }

    /// `mcp.auth.setIdentity` params: exactly one of `{"id_token"}` /
    /// `{"refresh_token"}`.
    #[cfg(feature = "mcp")]
    pub(super) async fn method_mcp_auth_set_identity(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        use mcp::oauth::EnterpriseIdentity;
        let text = |key: &str| {
            params
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let identity = match (text("id_token"), text("refresh_token")) {
            (Some(token), None) => EnterpriseIdentity::IdToken(token),
            (None, Some(token)) => EnterpriseIdentity::RefreshToken(token),
            _ => {
                return RpcResponse::err(
                    id,
                    codes::INVALID_PARAMS,
                    "give exactly one of id_token / refresh_token",
                )
            }
        };
        match self.pool.mcp_auth_set_identity(identity).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        }
    }
}
