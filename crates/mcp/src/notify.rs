//! Receiving a server's notifications.
//!
//! MCP is bidirectional: a server can tell the client its tool list changed
//! rather than waiting to be asked again. `McpNotification` and
//! `McpManager::dispatch_notification` were written for that and then never
//! connected to a transport — nothing in the process ever constructed one,
//! so a server announcing new tools was talking to a wall.
//!
//! This is the missing half. The client handler that rmcp drives is no
//! longer the unit type, and forwards the notifications we act on.

use crate::manager::McpNotification;
use rmcp::handler::client::ClientHandler;
use rmcp::service::{NotificationContext, RoleClient};
use std::sync::Arc;

/// Called on the transport's task when a server sends a notification.
///
/// Deliberately a plain callback rather than a channel: a receiver has to be
/// drained by someone, and a notification nobody drains is the same silence
/// this module exists to fix.
pub type NotificationSink = Arc<dyn Fn(McpNotification) + Send + Sync>;

/// The client handler rmcp drives for one connection.
///
/// Everything except the notifications we care about keeps `ClientHandler`'s
/// default behavior, which is what the unit type provided before.
#[derive(Clone)]
pub struct NotifyingHandler {
    server: String,
    sink: Option<NotificationSink>,
}

impl NotifyingHandler {
    pub fn new(server: impl Into<String>, sink: Option<NotificationSink>) -> Self {
        Self {
            server: server.into(),
            sink,
        }
    }

    /// A handler that receives notifications and discards them.
    pub fn silent(server: impl Into<String>) -> Self {
        Self::new(server, None)
    }

    fn emit(&self, notification: McpNotification) {
        if let Some(sink) = &self.sink {
            sink(notification);
        }
    }
}

impl ClientHandler for NotifyingHandler {
    /// The one that matters: a server whose tools changed is a server whose
    /// cached tool list is now wrong, and the host has to re-ask.
    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server = self.server.clone();
        async move {
            tracing::debug!(server = %server, "MCP server announced a tool list change");
            self.emit(McpNotification::ToolListChanged { server });
        }
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server = self.server.clone();
        async move {
            self.emit(McpNotification::ResourceListChanged { server });
        }
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server = self.server.clone();
        async move {
            self.emit(McpNotification::PromptListChanged { server });
        }
    }
}

/// Notifications that have arrived and not yet been acted on.
///
/// A queue rather than an immediate refresh because the reaction — re-asking
/// a server for its tools — needs `&mut McpManager`, and the notification
/// arrives on the transport's own task where no such handle exists. Parking
/// it here lets whoever owns the manager pick it up.
#[derive(Default)]
pub struct NotificationQueue {
    pending: std::sync::Mutex<Vec<McpNotification>>,
}

impl NotificationQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A sink that parks into this queue.
    pub fn sink(self: &Arc<Self>) -> NotificationSink {
        let queue = Arc::clone(self);
        Arc::new(move |n| queue.push(n))
    }

    pub fn push(&self, notification: McpNotification) {
        self.lock().push(notification);
    }

    /// Take everything queued so far, leaving the queue empty.
    pub fn drain(&self) -> Vec<McpNotification> {
        std::mem::take(&mut *self.lock())
    }

    /// Has any server announced that its tools changed? Answering this
    /// clears the queue, since the answer is acted on by re-asking every
    /// server anyway.
    pub fn take_tools_changed(&self) -> bool {
        self.drain()
            .iter()
            .any(|n| matches!(n, McpNotification::ToolListChanged { .. }))
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<McpNotification>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn recorder() -> (NotificationSink, Arc<Mutex<Vec<McpNotification>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        (Arc::new(move |n| sink.lock().unwrap().push(n)), seen)
    }

    #[test]
    fn a_handler_carries_the_server_it_belongs_to() {
        let (sink, seen) = recorder();
        let h = NotifyingHandler::new("github", Some(sink));
        h.emit(McpNotification::ToolListChanged {
            server: h.server.clone(),
        });
        let recorded = seen.lock().unwrap();
        match &recorded[0] {
            McpNotification::ToolListChanged { server } => assert_eq!(server, "github"),
            other => panic!("expected ToolListChanged, got {other:?}"),
        }
    }

    /// A connection with no sink behaves exactly as the unit handler did,
    /// which is what keeps this change invisible to callers that do not want
    /// notifications.
    #[test]
    fn a_silent_handler_drops_everything_without_complaint() {
        let h = NotifyingHandler::silent("github");
        h.emit(McpNotification::ToolListChanged {
            server: "github".into(),
        });
    }

    #[test]
    fn the_queue_parks_notifications_until_someone_can_act_on_them() {
        let q = NotificationQueue::new();
        assert!(q.is_empty());

        NotifyingHandler::new("github", Some(q.sink())).emit(McpNotification::ToolListChanged {
            server: "github".into(),
        });
        assert!(!q.is_empty());

        assert_eq!(q.drain().len(), 1);
        assert!(q.is_empty(), "draining takes them");
    }

    #[test]
    fn asking_whether_tools_changed_clears_the_queue() {
        let q = NotificationQueue::new();
        q.push(McpNotification::ResourceListChanged { server: "a".into() });
        q.push(McpNotification::ToolListChanged { server: "b".into() });

        assert!(q.take_tools_changed());
        assert!(
            q.is_empty(),
            "the answer is acted on by re-asking every server, so nothing is left to replay"
        );
        assert!(!q.take_tools_changed(), "and it does not fire twice");
    }

    #[test]
    fn other_notifications_do_not_claim_the_tools_changed() {
        let q = NotificationQueue::new();
        q.push(McpNotification::PromptListChanged { server: "a".into() });
        assert!(!q.take_tools_changed());
    }
}
