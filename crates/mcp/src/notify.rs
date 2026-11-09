//! Receiving a server's notifications.
//!
//! A server tells its client when its tool, prompt or resource list changed.
//! [`NotifyingHandler`] is the client handler rmcp drives for a connection;
//! it forwards those notifications to a [`NotificationSink`], and
//! [`Announcements`] is the sink every connection of one manager reports
//! into. A manager — the pool's, and each session's shared from it — reads
//! the counts there against the ones it last acted on and lists again only
//! the servers that announced a change (`McpManager::refresh_if_changed`).
//!
//! Tool and prompt changes are acted on. A resource change is counted and
//! otherwise ignored: the resource tools ask the server on every call.

use crate::manager::McpNotification;
use rmcp::handler::client::ClientHandler;
use rmcp::service::{NotificationContext, RoleClient};
use std::collections::HashMap;
use std::sync::Arc;

/// Called on the transport's task when a server sends a notification.
pub type NotificationSink = Arc<dyn Fn(McpNotification) + Send + Sync>;

/// The client handler rmcp drives for one connection. Everything but the
/// list-changed notifications keeps `ClientHandler`'s default behavior.
#[derive(Clone)]
pub struct NotifyingHandler {
    server: String,
    sink: Option<NotificationSink>,
    keep: Option<Arc<dyn Send + Sync>>,
}

impl NotifyingHandler {
    pub fn new(server: impl Into<String>, sink: Option<NotificationSink>) -> Self {
        Self {
            server: server.into(),
            sink,
            keep: None,
        }
    }

    /// A handler that receives notifications and discards them.
    pub fn silent(server: impl Into<String>) -> Self {
        Self::new(server, None)
    }

    /// Hold `value` for as long as the connection runs: rmcp drops the
    /// handler when the connection's task ends.
    pub fn keeping(mut self, value: impl Send + Sync + 'static) -> Self {
        self.keep = Some(Arc::new(value));
        self
    }

    fn emit(&self, notification: McpNotification) {
        if let Some(sink) = &self.sink {
            sink(notification);
        }
    }
}

impl ClientHandler for NotifyingHandler {
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

/// How many times a server has announced each of its lists changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListEpochs {
    pub tools: u64,
    pub prompts: u64,
    pub resources: u64,
}

/// What servers have announced, as counts. Counts rather than a queue: every
/// manager sharing these connections reads the same announcements, each
/// against what it last acted on, and none of them takes them from the rest.
#[derive(Default)]
pub struct Announcements {
    epochs: std::sync::Mutex<HashMap<String, ListEpochs>>,
}

impl Announcements {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A sink that counts into this.
    pub fn sink(self: &Arc<Self>) -> NotificationSink {
        let announcements = Arc::clone(self);
        Arc::new(move |n| announcements.record(&n))
    }

    pub fn record(&self, notification: &McpNotification) {
        let (server, count): (&str, fn(&mut ListEpochs) -> &mut u64) = match notification {
            McpNotification::ToolListChanged { server } => (server, |e| &mut e.tools),
            McpNotification::PromptListChanged { server } => (server, |e| &mut e.prompts),
            McpNotification::ResourceListChanged { server } => (server, |e| &mut e.resources),
            McpNotification::Custom { .. } => return,
        };
        *count(self.lock().entry(server.to_string()).or_default()) += 1;
    }

    pub fn of(&self, server: &str) -> ListEpochs {
        self.lock().get(server).copied().unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ListEpochs>> {
        self.epochs.lock().unwrap_or_else(|e| e.into_inner())
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

    #[test]
    fn a_silent_handler_drops_everything_without_complaint() {
        let h = NotifyingHandler::silent("github");
        h.emit(McpNotification::ToolListChanged {
            server: "github".into(),
        });
    }

    #[test]
    fn announcements_are_counted_per_server_and_kind_and_stay_for_every_reader() {
        let a = Announcements::new();
        let sink = a.sink();
        assert_eq!(a.of("github"), ListEpochs::default());

        sink(McpNotification::ToolListChanged {
            server: "github".into(),
        });
        sink(McpNotification::ToolListChanged {
            server: "github".into(),
        });
        sink(McpNotification::PromptListChanged {
            server: "github".into(),
        });
        sink(McpNotification::ResourceListChanged {
            server: "db".into(),
        });

        let github = a.of("github");
        assert_eq!((github.tools, github.prompts, github.resources), (2, 1, 0));
        assert_eq!(a.of("db").resources, 1);
        assert_eq!(a.of("github"), github, "reading takes nothing away");
    }

    #[test]
    fn a_kept_value_lives_as_long_as_the_handler_and_its_clones() {
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = NotifyingHandler::silent("s").keeping(Flag(dropped.clone()));
        let clone = h.clone();
        drop(h);
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));
        drop(clone);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }
}
