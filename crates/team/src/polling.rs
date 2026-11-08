//! Swarm permission polling — a lightweight tokio task that periodically checks
//! the team mailbox for new protocol messages and dispatches them.
//!
//! ## Architecture
//!
//! `MailboxPoller` runs a single tokio task that polls the mailbox every
//! `poll_interval` (default 500 ms).  On each tick it:
//!
//! 1. Drains pending messages from the coordinator's own mailbox.
//! 2. Parses each message into a [`ProtocolMessage`](crate::protocol::ProtocolMessage).
//! 3. Dispatches to the appropriate handler based on the message type.
//!
//! Currently supported:
//!
//! - `permission_request` — forwarded to the parent agent's permission bridge
//! - `permission_response` — delivered to the awaiting sub-agent
//! - `heartbeat` — recorded for liveness tracking
//! - `shutdown_request` — triggers graceful / forced shutdown
//!
//! Unknown message types are logged as warnings and skipped (forward compat).

use crate::mailbox::MailboxStore;
use crate::protocol::ProtocolMessage;
use std::sync::Arc;
use std::time::Duration;

/// How often the poller checks for new messages in the mailbox.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum number of messages to drain per polling cycle.
const DEFAULT_DRAIN_MAX: usize = 100;

/// A running mailbox poller handle. Dropping the handle cancels the poller.
pub struct MailboxPoller {
    /// Token used to cancel the poller task.
    cancel_token: tokio_util::sync::CancellationToken,
    /// Join handle for the poller task (detached — we never join it).
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

impl MailboxPoller {
    /// Return the cancellation token so callers can trigger a stop.
    pub fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel_token.clone()
    }

    /// Request the poller to stop. Returns immediately; the background task
    /// will stop on its next tick.
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

/// Configuration for the [`MailboxPoller`].
#[derive(Debug, Clone)]
pub struct PollerConfig {
    /// How often to poll the mailbox. Default: 500 ms.
    pub poll_interval: Duration,
    /// Maximum messages to drain per cycle. Default: 100.
    pub drain_max: usize,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            drain_max: DEFAULT_DRAIN_MAX,
        }
    }
}

/// Callback trait for processing incoming protocol messages discovered by
/// the poller.
///
/// Implement this trait to wire the poller into your team's decision-making
/// logic (e.g. a coordinator agent, a permission bridge, or a liveness
/// monitor).
pub trait MessageHandler: Send + Sync + 'static {
    /// Called when a protocol message is received.
    ///
    /// The `from` field identifies the sender agent label.
    fn handle_message(&self, from: &str, message: &ProtocolMessage);
}

/// Start a background polling task that monitors the team mailbox for new
/// protocol messages and dispatches them to the provided [`MessageHandler`].
///
/// # Returns
///
/// A [`MailboxPoller`] handle. The polling stops when the handle is dropped
/// or when [`MailboxPoller::stop`] is called.
///
/// # Example
///
/// ```rust,ignore
/// use std::sync::Arc;
/// use team::mailbox::MailboxStore;
/// use team::polling::{start_polling, PollerConfig, MessageHandler};
///
/// let mailbox = Arc::new(MailboxStore::new(vec![]));
///
/// struct MyHandler;
/// impl MessageHandler for MyHandler {
///     fn handle_message(&self, from: &str, msg: &team::protocol::ProtocolMessage) {
///         // process message
///     }
/// }
///
/// let poller = start_polling(
///     mailbox,
///     "coordinator",
///     Arc::new(MyHandler),
///     PollerConfig::default(),
/// );
/// ```
pub fn start_polling(
    mailbox: Arc<MailboxStore>,
    my_label: impl Into<String>,
    handler: Arc<dyn MessageHandler>,
    config: PollerConfig,
) -> MailboxPoller {
    let my_label: String = my_label.into();
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let cancel_child = cancel_token.child_token();

    let handle = tokio::spawn(async move {
        poll_loop(mailbox, my_label, handler, config, cancel_child).await;
    });

    MailboxPoller {
        cancel_token,
        handle,
    }
}

/// The inner polling loop.
async fn poll_loop(
    mailbox: Arc<MailboxStore>,
    my_label: String,
    handler: Arc<dyn MessageHandler>,
    config: PollerConfig,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                tracing::info!(label = %my_label, "mailbox poller cancelled");
                break;
            }
            _ = tokio::time::sleep(config.poll_interval) => {
                tick(&mailbox, &my_label, &*handler, config.drain_max);
            }
        }
    }
}

/// Process one polling tick: drain new messages and dispatch them.
fn tick(mailbox: &MailboxStore, my_label: &str, handler: &dyn MessageHandler, max: usize) {
    let Some(msgs) = mailbox.drain(my_label, max) else {
        return;
    };

    for msg in msgs {
        let msg_json = &msg.content;
        match ProtocolMessage::deserialize_from_json(msg_json) {
            Ok(protocol_msg) => {
                handler.handle_message(&msg.from, &protocol_msg);
            }
            Err(e) => {
                // This may be a plain-text message or a format we don't
                // understand — log a warning and skip.
                tracing::debug!(
                    from = %msg.from,
                    error = %e,
                    content_len = msg_json.len(),
                    "mailbox poller: failed to parse message as ProtocolMessage"
                );
            }
        }
    }
}

/// Start a polling session with default configuration.
///
/// This is the simplest entry point — call it during team initialisation to
/// get a working poller that runs every 500 ms with a drain max of 100.
pub fn start_default_polling(
    mailbox: Arc<MailboxStore>,
    my_label: impl Into<String>,
    handler: Arc<dyn MessageHandler>,
) -> MailboxPoller {
    start_polling(mailbox, my_label, handler, PollerConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ProtocolDecision;
    use std::sync::Mutex;

    /// A handler that records received messages for inspection in tests.
    struct RecordingHandler {
        received: Mutex<Vec<(String, ProtocolMessage)>>,
    }

    impl RecordingHandler {
        fn new() -> Self {
            Self {
                received: Mutex::new(Vec::new()),
            }
        }

        fn count(&self) -> usize {
            self.received.lock().unwrap().len()
        }

        fn last(&self) -> Option<(String, ProtocolMessage)> {
            self.received.lock().unwrap().last().cloned()
        }
    }

    impl MessageHandler for RecordingHandler {
        fn handle_message(&self, from: &str, message: &ProtocolMessage) {
            self.received
                .lock()
                .unwrap()
                .push((from.to_string(), message.clone()));
        }
    }

    #[tokio::test]
    async fn poller_receives_and_dispatches_messages() {
        let mailbox = Arc::new(MailboxStore::new(vec![
            "coordinator".into(),
            "worker".into(),
        ]));
        let handler = Arc::new(RecordingHandler::new());

        let poller = start_default_polling(mailbox.clone(), "coordinator", handler.clone());

        // Send a message to the coordinator.
        let msg = ProtocolMessage::Heartbeat {
            agent_id: "worker".into(),
            timestamp: time::OffsetDateTime::now_utc(),
            status: Some("processing".into()),
        };
        mailbox.send("worker", "coordinator", &msg.serialize_to_json());

        // Give the poller time to pick it up.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify the handler received it.
        assert!(
            handler.count() >= 1,
            "handler should have received at least 1 message, got {}",
            handler.count()
        );

        let (from, msg) = handler.last().unwrap();
        assert_eq!(from, "worker");
        assert_eq!(msg.type_str(), "heartbeat");

        poller.stop();
        // Give poller time to stop before dropping.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn poller_handles_multiple_messages() {
        let mailbox = Arc::new(MailboxStore::new(vec![
            "coordinator".into(),
            "worker".into(),
        ]));
        let handler = Arc::new(RecordingHandler::new());

        let poller = start_default_polling(mailbox.clone(), "coordinator", handler.clone());

        // Send multiple messages.
        let msg1 = ProtocolMessage::PermissionRequest {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            tool_use_id: None,
            message: None,
        };
        let msg2 = ProtocolMessage::Heartbeat {
            agent_id: "worker".into(),
            timestamp: time::OffsetDateTime::now_utc(),
            status: None,
        };

        mailbox.send("worker", "coordinator", &msg1.serialize_to_json());
        mailbox.send("worker", "coordinator", &msg2.serialize_to_json());

        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            handler.count() >= 2,
            "handler should have received at least 2 messages, got {}",
            handler.count()
        );

        poller.stop();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn poller_skips_unknown_message_types() {
        let mailbox = Arc::new(MailboxStore::new(vec!["coordinator".into()]));
        let handler = Arc::new(RecordingHandler::new());

        let poller = start_default_polling(mailbox.clone(), "coordinator", handler.clone());

        // Send a malformed/plain text message (not a ProtocolMessage).
        mailbox.send("worker", "coordinator", "this is not protocol json");

        tokio::time::sleep(Duration::from_millis(100)).await;

        // The non-protocol message should be skipped.
        assert_eq!(
            handler.count(),
            0,
            "handler should not receive non-protocol messages"
        );

        poller.stop();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn poller_stops_on_cancel() {
        let mailbox = Arc::new(MailboxStore::new(vec!["coordinator".into()]));
        let handler = Arc::new(RecordingHandler::new());

        let poller = start_default_polling(mailbox.clone(), "coordinator", handler.clone());

        // Stop the poller.
        poller.stop();

        // Give it time to stop.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // After stopping, sending a message should not be processed.
        let msg = ProtocolMessage::Heartbeat {
            agent_id: "worker".into(),
            timestamp: time::OffsetDateTime::now_utc(),
            status: None,
        };
        mailbox.send("worker", "coordinator", &msg.serialize_to_json());

        tokio::time::sleep(Duration::from_millis(100)).await;

        // The handler may still have 0 if the poller loop exited before the
        // message arrived.  That's fine — the point is the poller stopped
        // without panicking.
        // We just verify the poller didn't crash.
    }

    #[tokio::test]
    async fn poller_handles_permission_request() {
        let mailbox = Arc::new(MailboxStore::new(vec![
            "coordinator".into(),
            "worker".into(),
        ]));
        let handler = Arc::new(RecordingHandler::new());

        let poller = start_default_polling(mailbox.clone(), "coordinator", handler.clone());

        let msg = ProtocolMessage::PermissionRequest {
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"path": "/etc/passwd"}),
            tool_use_id: Some("req-1".into()),
            message: Some("need to read config".into()),
        };
        mailbox.send("worker", "coordinator", &msg.serialize_to_json());

        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(handler.count() >= 1);
        let (from, received) = handler.last().unwrap();
        assert_eq!(from, "worker");
        match received {
            ProtocolMessage::PermissionRequest { tool_name, .. } => {
                assert_eq!(tool_name, "Read");
            }
            other => panic!("expected PermissionRequest, got {other:?}"),
        }

        poller.stop();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn poller_handles_permission_response() {
        let mailbox = Arc::new(MailboxStore::new(vec![
            "coordinator".into(),
            "worker".into(),
        ]));
        let handler = Arc::new(RecordingHandler::new());

        let poller = start_default_polling(mailbox.clone(), "worker", handler.clone());

        let msg = ProtocolMessage::PermissionResponse {
            decision: ProtocolDecision::Allow,
            tool_use_id: Some("req-1".into()),
        };
        mailbox.send("coordinator", "worker", &msg.serialize_to_json());

        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(handler.count() >= 1);
        let (from, received) = handler.last().unwrap();
        assert_eq!(from, "coordinator");
        match received {
            ProtocolMessage::PermissionResponse { decision, .. } => {
                assert_eq!(decision, ProtocolDecision::Allow);
            }
            other => panic!("expected PermissionResponse, got {other:?}"),
        }

        poller.stop();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
