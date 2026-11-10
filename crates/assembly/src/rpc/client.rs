//! 一条客户端连接：连接 id、发往它的帧队列，以及队列满时退订并告知的机制。

use std::sync::Arc;

/// Somewhere to put one outgoing JSON-RPC frame.
///
/// The transports disagree about framing — a byte stream needs a delimiter
/// and an explicit flush, a WebSocket connection is already message-framed —
/// and nothing above this line should have to know which one it is talking
/// to.
///
/// Responses and streamed events both go through this one implementation, so
/// both flush after every frame; a streamed token never waits in a buffer.
#[async_trait::async_trait]
pub trait FrameSink: Send + Sync {
    /// Write one complete frame. `false` means the peer is gone and the
    /// caller should stop writing to this connection.
    async fn send_json(&self, json: String) -> bool;
}

/// A sink shared by the dispatcher and by any streaming task it spawns.
pub type Sink = Arc<dyn FrameSink>;

/// Identifies one client connection for its lifetime.
///
/// A connection is the unit of subscription: a browser tab holds one, and
/// when it goes away its subscriptions go with it — and nothing else does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u64);

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn-{}", self.0)
    }
}

/// How many frames may sit queued for one connection before it is treated as
/// not reading.
///
/// Generous: a turn's worth of token deltas is well under this, so the limit
/// is only reached by a client that has stopped draining. Past it, the
/// session broadcast drops that connection's *subscription* rather than
/// waiting, and the connection is told so — see [`Client::try_send_for`].
const OUTBOUND_QUEUE_DEPTH: usize = 1024;

/// One client connection: an id, and a queue in front of its sink.
///
/// Everything written to a connection goes through the one queue, responses
/// and pushed frames alike, so a client sees them in the order the daemon
/// produced them. Two write paths into one socket would let a response
/// overtake the events that led to it.
///
/// The queue also decouples the agent loop from the client: a session's
/// broadcast enqueues and moves on, so one tab that has stopped reading
/// cannot hold up the turn — or the other tabs watching it.
pub struct Client {
    id: ConnectionId,
    tx: tokio::sync::mpsc::Sender<Outgoing>,
    enqueued: std::sync::Arc<std::sync::atomic::AtomicU64>,
    dropped: std::sync::Arc<std::sync::Mutex<Vec<DroppedSubscription>>>,
    daemon_events: std::sync::atomic::AtomicBool,
}

/// A session this connection was unsubscribed from because its queue was
/// full, and how many frames had been queued when that happened: the notice
/// goes out once those are written, so it arrives after everything the old
/// subscription delivered and before anything a new one will.
struct DroppedSubscription {
    session_id: String,
    after: u64,
}

/// A queued frame, optionally carrying an acknowledgement the pump fires once
/// the frame has actually reached the sink.
struct Outgoing {
    json: String,
    written: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Client {
    /// Wrap `sink` and start the pump that drains the queue into it.
    ///
    /// The pump ends when the connection is dropped or the sink reports the
    /// peer gone; nothing else has to notice.
    pub fn new(id: ConnectionId, sink: Sink) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Outgoing>(OUTBOUND_QUEUE_DEPTH);
        let dropped: std::sync::Arc<std::sync::Mutex<Vec<DroppedSubscription>>> =
            std::sync::Arc::default();
        let pending = dropped.clone();
        tokio::spawn(async move {
            let mut written: u64 = 0;
            while let Some(item) = rx.recv().await {
                let ok = sink.send_json(item.json).await;
                if let Some(ack) = item.written {
                    let _ = ack.send(());
                }
                if !ok {
                    break;
                }
                written += 1;
                let due: Vec<String> = {
                    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
                    let (due, later): (Vec<_>, Vec<_>) =
                        pending.drain(..).partition(|d| d.after <= written);
                    *pending = later;
                    due.into_iter().map(|d| d.session_id).collect()
                };
                for session_id in due {
                    let notice =
                        super::events::SessionEventNotification::subscription_dropped(&session_id);
                    let Ok(json) = serde_json::to_string(&notice) else {
                        continue;
                    };
                    if !sink.send_json(json).await {
                        return;
                    }
                }
            }
        });
        Self {
            id,
            tx,
            enqueued: std::sync::Arc::default(),
            dropped,
            daemon_events: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// Mark this connection as receiving `daemon.event`; `false` if it
    /// already was.
    pub fn start_daemon_events(&self) -> bool {
        !self
            .daemon_events
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Enqueue and wait until the frame has reached the sink.
    ///
    /// For replies, which must not be dropped — a caller is blocked on this
    /// one — and which callers may follow by closing the connection. Waiting
    /// for room alone would not be enough there: the queue would still hold
    /// the frame when the socket closed under it.
    pub async fn send_json(&self, json: String) -> bool {
        let (ack, written) = tokio::sync::oneshot::channel();
        let queued = self
            .tx
            .send(Outgoing {
                json,
                written: Some(ack),
            })
            .await
            .is_ok();
        if queued {
            self.enqueued
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        queued && written.await.is_ok()
    }

    /// Enqueue without waiting. `false` means the connection is gone or has
    /// fallen [`OUTBOUND_QUEUE_DEPTH`] frames behind, and the caller should
    /// stop pushing to it. Used for broadcast, where waiting on one slow
    /// client would stall every other reader of the same session.
    pub fn try_send_json(&self, json: String) -> bool {
        let queued = self
            .tx
            .try_send(Outgoing {
                json,
                written: None,
            })
            .is_ok();
        if queued {
            self.enqueued
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        queued
    }

    /// [`Self::try_send_json`] for a frame of `session_id`'s broadcast. When
    /// the queue is full the caller drops this connection's subscription, and
    /// this arranges for the connection to be told — a `subscription_dropped`
    /// event, written once everything already queued has gone out — so it can
    /// subscribe again and catch up rather than miss frames without knowing.
    pub fn try_send_for(&self, session_id: &str, json: String) -> bool {
        match self.tx.try_send(Outgoing {
            json,
            written: None,
        }) {
            Ok(()) => {
                self.enqueued
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let after = self.enqueued.load(std::sync::atomic::Ordering::SeqCst);
                self.dropped
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(DroppedSubscription {
                        session_id: session_id.to_string(),
                        after,
                    });
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

/// Serialize `frame` and hand it to `sink`.
///
/// A frame that cannot be serialized is dropped with a warning rather than
/// killing the connection: it is a bug in whatever built it, and taking the
/// session down would hide that behind a disconnect.
pub async fn send_frame<T: serde::Serialize>(client: &Client, frame: &T) -> bool {
    match serde_json::to_string(frame) {
        Ok(json) => client.send_json(json).await,
        Err(e) => {
            tracing::warn!(error = %e, "could not serialize an outgoing frame; dropping it");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// A sink that writes nothing until the test opens the gate, so the
    /// queue in front of it can be filled.
    struct GatedSink {
        open: tokio::sync::Semaphore,
        written: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl FrameSink for GatedSink {
        async fn send_json(&self, json: String) -> bool {
            self.open.acquire().await.unwrap().forget();
            self.written.lock().unwrap().push(json);
            true
        }
    }

    #[tokio::test]
    async fn a_dropped_subscription_is_told_after_what_was_already_queued() {
        let sink = Arc::new(GatedSink {
            open: tokio::sync::Semaphore::new(0),
            written: std::sync::Mutex::new(Vec::new()),
        });
        let client = Client::new(ConnectionId(1), sink.clone());

        let mut queued = 0;
        while client.try_send_for("s1", format!("{{\"n\":{queued}}}")) {
            queued += 1;
        }
        assert!(queued >= OUTBOUND_QUEUE_DEPTH, "the queue filled first");

        sink.open.add_permits(queued + 1);
        for _ in 0..200 {
            if sink.written.lock().unwrap().len() == queued + 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let written = sink.written.lock().unwrap().clone();
        assert_eq!(
            written.len(),
            queued + 1,
            "every queued frame, then the notice"
        );
        let notice: Value = serde_json::from_str(written.last().unwrap()).unwrap();
        assert_eq!(notice["method"], "session.event");
        assert_eq!(notice["params"]["session_id"], "s1");
        assert_eq!(notice["params"]["event"]["kind"], "subscription_dropped");
    }
}
