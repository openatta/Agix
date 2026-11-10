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
}
