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
}
