//! Messages a user sends while a turn is running.
//!
//! `Agent::run`'s input demultiplexer offers them here; the turn loop takes
//! them at its step boundaries and adds them to the conversation. Whether a
//! turn is still accepting and what is queued share one lock, so "the turn is
//! past its last boundary" and "a message was queued" cannot interleave: a
//! message is either read by the model or handed back, never stranded.

use base::event::QueuedUserMessage;

/// What became of a message offered to the running turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerAck {
    /// Queued; the model reads it at the turn's next step boundary.
    Accepted,
    /// No turn is accepting messages. The sender still owns the message.
    NotRunning,
}

#[derive(Debug, Default)]
pub(crate) struct SteerQueue {
    accepting: bool,
    pending: Vec<QueuedUserMessage>,
}

pub(crate) type SharedSteerQueue = std::sync::Arc<std::sync::Mutex<SteerQueue>>;

pub(crate) fn lock(queue: &SharedSteerQueue) -> std::sync::MutexGuard<'_, SteerQueue> {
    queue.lock().unwrap_or_else(|e| e.into_inner())
}

impl SteerQueue {
    pub(crate) fn offer(&mut self, message: QueuedUserMessage) -> Result<(), QueuedUserMessage> {
        if self.accepting {
            self.pending.push(message);
            Ok(())
        } else {
            Err(message)
        }
    }

    pub(crate) fn open(&mut self) {
        self.accepting = true;
    }

    pub(crate) fn take(&mut self) -> Vec<QueuedUserMessage> {
        std::mem::take(&mut self.pending)
    }

    /// The turn's last boundary: what is queued gets delivered, and if
    /// nothing is, the turn stops accepting in the same breath.
    pub(crate) fn take_or_close(&mut self) -> Vec<QueuedUserMessage> {
        if self.pending.is_empty() {
            self.accepting = false;
        }
        self.take()
    }

    pub(crate) fn close(&mut self) -> Vec<QueuedUserMessage> {
        self.accepting = false;
        self.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str) -> QueuedUserMessage {
        QueuedUserMessage {
            message_id: id.into(),
            content: format!("text of {id}"),
        }
    }

    #[test]
    fn refuses_until_a_turn_opens_it() {
        let mut q = SteerQueue::default();
        assert!(q.offer(msg("a")).is_err());
        q.open();
        assert!(q.offer(msg("b")).is_ok());
        assert_eq!(q.take(), vec![msg("b")]);
    }

    #[test]
    fn last_boundary_closes_only_when_nothing_is_waiting() {
        let mut q = SteerQueue::default();
        q.open();
        q.offer(msg("a")).unwrap();
        assert_eq!(q.take_or_close(), vec![msg("a")]);
        assert!(q.offer(msg("b")).is_ok(), "still open after delivering");
        q.take();
        assert!(q.take_or_close().is_empty());
        assert!(
            q.offer(msg("c")).is_err(),
            "closed once nothing was waiting"
        );
    }

    #[test]
    fn close_hands_back_what_was_never_delivered() {
        let mut q = SteerQueue::default();
        q.open();
        q.offer(msg("a")).unwrap();
        assert_eq!(q.close(), vec![msg("a")]);
        assert!(q.offer(msg("b")).is_err());
    }
}
