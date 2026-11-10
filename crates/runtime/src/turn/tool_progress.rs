//! 工具调用的进度：把 `ProgressSender` 的报告限流成 `AgentEvent::ToolProgress`。
//!
//! 每次调用一个 [`ProgressGate`]：一秒内的第一条立刻发，其余合并成最后一条，到点由计时器
//! 补发，调用结束时再补一次；合并掉的条数随下一条以 `dropped` 带出。限流逻辑在
//! [`ProgressThrottle`] 里，时间由调用方传入。

use base::interface::event::AgentEvent;
use base::tool::{ProgressCallback, ProgressUpdate};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

pub(super) const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
pub(super) const MAX_MESSAGE_BYTES: usize = 4 * 1024;

#[derive(Debug, PartialEq)]
pub(super) struct Emit {
    pub update: ProgressUpdate,
    pub dropped: u64,
}

#[derive(Debug, PartialEq)]
pub(super) enum Offer {
    Send(Emit),
    /// Held for later. `arm` is when to look again, given only when no
    /// earlier look is already scheduled.
    Hold {
        arm: Option<Instant>,
    },
    Closed,
}

#[derive(Default)]
pub(super) struct ProgressThrottle {
    last_sent: Option<Instant>,
    pending: Option<ProgressUpdate>,
    dropped: u64,
    armed: bool,
    closed: bool,
}

impl ProgressThrottle {
    pub(super) fn offer(&mut self, update: ProgressUpdate, now: Instant) -> Offer {
        if self.closed {
            return Offer::Closed;
        }
        let due = self.due_at();
        if due.is_none_or(|at| now >= at) && self.pending.is_none() {
            self.last_sent = Some(now);
            return Offer::Send(Emit {
                update,
                dropped: std::mem::take(&mut self.dropped),
            });
        }
        if self.pending.replace(update).is_some() {
            self.dropped += 1;
        }
        if self.armed {
            return Offer::Hold { arm: None };
        }
        self.armed = true;
        Offer::Hold {
            arm: Some(due.unwrap_or(now)),
        }
    }

    /// The held report, once its window has passed.
    pub(super) fn due(&mut self, now: Instant) -> Option<Emit> {
        self.armed = false;
        if self.closed || self.due_at().is_some_and(|at| now < at) {
            return None;
        }
        let update = self.pending.take()?;
        self.last_sent = Some(now);
        Some(Emit {
            update,
            dropped: std::mem::take(&mut self.dropped),
        })
    }

    /// When a report still held after [`Self::due`] should be looked at.
    pub(super) fn rearm(&mut self) -> Option<Instant> {
        if self.closed || self.pending.is_none() || self.armed {
            return None;
        }
        self.armed = true;
        self.due_at()
    }

    /// The call returned: what was held goes out now, nothing after it.
    pub(super) fn finish(&mut self) -> Option<Emit> {
        if self.closed {
            return None;
        }
        self.closed = true;
        let update = self.pending.take()?;
        Some(Emit {
            update,
            dropped: std::mem::take(&mut self.dropped),
        })
    }

    pub(super) fn close(&mut self) {
        self.closed = true;
        self.pending = None;
    }

    fn due_at(&self) -> Option<Instant> {
        self.last_sent.map(|at| at + PROGRESS_INTERVAL)
    }
}

/// The callback behind one tool call's `ProgressSender`.
pub(super) struct ProgressGate {
    tool_use_id: String,
    name: String,
    turn_id: String,
    events: crate::agent::EventSender,
    throttle: Mutex<ProgressThrottle>,
}

impl ProgressGate {
    pub(super) fn new(
        tool_use_id: &str,
        name: &str,
        turn_id: &str,
        events: crate::agent::EventSender,
    ) -> Arc<Self> {
        Arc::new(Self {
            tool_use_id: tool_use_id.to_string(),
            name: name.to_string(),
            turn_id: turn_id.to_string(),
            events,
            throttle: Mutex::default(),
        })
    }

    pub(super) fn finish(&self) {
        let last = self.lock().finish();
        if let Some(emit) = last {
            self.emit(emit);
        }
    }

    pub(super) fn close(&self) {
        self.lock().close();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ProgressThrottle> {
        self.throttle.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, Emit { update, dropped }: Emit) {
        let _ = self.events.send(AgentEvent::ToolProgress {
            tool_use_id: self.tool_use_id.clone(),
            name: self.name.clone(),
            message: update.message,
            progress: update.progress,
            total: update.total,
            dropped,
            turn_id: self.turn_id.clone(),
        });
    }

    fn look_again_at(self: Arc<Self>, at: Instant) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            tokio::time::sleep_until(at).await;
            let (due, again) = {
                let mut throttle = self.lock();
                let due = throttle.due(Instant::now());
                (due, throttle.rearm())
            };
            if let Some(emit) = due {
                self.emit(emit);
            }
            if let Some(at) = again {
                self.look_again_at(at);
            }
        });
    }
}

/// Lets a call's `ProgressSender` reach its gate, and only while the call is
/// running: a clone a tool keeps past its return finds the gate closed.
pub(super) struct GateHandle(pub Arc<ProgressGate>);

impl ProgressCallback for GateHandle {
    fn on_progress(&self, tool_use_id: &str, data: &str) {
        self.on_update(tool_use_id, ProgressUpdate::text(data));
    }

    fn on_update(&self, _tool_use_id: &str, mut update: ProgressUpdate) {
        if update.message.len() > MAX_MESSAGE_BYTES {
            let mut cut = MAX_MESSAGE_BYTES;
            while !update.message.is_char_boundary(cut) {
                cut -= 1;
            }
            update.message.truncate(cut);
        }
        let offer = self.0.lock().offer(update, Instant::now());
        match offer {
            Offer::Send(emit) => self.0.emit(emit),
            Offer::Hold { arm: Some(at) } => self.0.clone().look_again_at(at),
            Offer::Hold { arm: None } | Offer::Closed => {}
        }
    }
}

/// Closes the gate however the call ends, a dropped future included.
pub(super) struct CloseOnDrop(pub Arc<ProgressGate>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    fn sent(offer: Offer) -> (String, u64) {
        match offer {
            Offer::Send(Emit { update, dropped }) => (update.message, dropped),
            other => panic!("expected a send, got {other:?}"),
        }
    }

    #[test]
    fn the_first_report_goes_out_at_once_and_the_rest_of_the_second_merges_into_the_last() {
        let t0 = Instant::now();
        let mut throttle = ProgressThrottle::default();
        assert_eq!(
            sent(throttle.offer(ProgressUpdate::text("1"), t0)),
            ("1".into(), 0)
        );
        assert_eq!(
            throttle.offer(ProgressUpdate::text("2"), at(t0, 10)),
            Offer::Hold {
                arm: Some(at(t0, 1000))
            }
        );
        for n in 3..=1000 {
            assert_eq!(
                throttle.offer(ProgressUpdate::text(n.to_string()), at(t0, 10 + n)),
                Offer::Hold { arm: None }
            );
        }
        assert_eq!(throttle.due(at(t0, 999)), None, "not before its second");
        assert_eq!(throttle.rearm(), Some(at(t0, 1000)));
        let emit = throttle.due(at(t0, 1000)).expect("the held report");
        assert_eq!(
            (emit.update.message.as_str(), emit.dropped),
            ("1000", 998),
            "999 held, one of them sent: 998 merged away"
        );
        assert_eq!(throttle.rearm(), None, "nothing left to send");
    }

    #[test]
    fn after_a_quiet_second_a_report_goes_out_at_once() {
        let t0 = Instant::now();
        let mut throttle = ProgressThrottle::default();
        sent(throttle.offer(ProgressUpdate::text("a"), t0));
        assert_eq!(
            sent(throttle.offer(ProgressUpdate::text("b"), at(t0, 1500))),
            ("b".into(), 0)
        );
    }

    #[test]
    fn the_call_ending_sends_what_was_held_and_nothing_after() {
        let t0 = Instant::now();
        let mut throttle = ProgressThrottle::default();
        sent(throttle.offer(ProgressUpdate::text("step 1"), t0));
        throttle.offer(ProgressUpdate::text("step 2"), at(t0, 1));
        throttle.offer(ProgressUpdate::text("step 3"), at(t0, 2));
        let last = throttle.finish().expect("the merged last report");
        assert_eq!((last.update.message.as_str(), last.dropped), ("step 3", 1));
        assert_eq!(
            throttle.offer(ProgressUpdate::text("late"), at(t0, 5000)),
            Offer::Closed
        );
        assert_eq!(throttle.due(at(t0, 5000)), None);
        assert_eq!(throttle.finish(), None);
    }

    #[test]
    fn the_call_ending_with_nothing_held_sends_nothing() {
        let t0 = Instant::now();
        let mut throttle = ProgressThrottle::default();
        sent(throttle.offer(ProgressUpdate::text("only"), t0));
        assert_eq!(throttle.finish(), None);
    }

    #[test]
    fn a_dropped_call_sends_nothing_more() {
        let t0 = Instant::now();
        let mut throttle = ProgressThrottle::default();
        sent(throttle.offer(ProgressUpdate::text("a"), t0));
        throttle.offer(ProgressUpdate::text("b"), at(t0, 1));
        throttle.close();
        assert_eq!(throttle.due(at(t0, 2000)), None);
        assert_eq!(throttle.finish(), None);
    }

    fn gate() -> (
        Arc<ProgressGate>,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let events = crate::event_bus::EventBus::new(tx);
        (ProgressGate::new("toolu_1", "Slow", "turn-1", events), rx)
    }

    fn progress_of(event: AgentEvent) -> (String, u64) {
        match event {
            AgentEvent::ToolProgress {
                tool_use_id,
                message,
                dropped,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_1");
                (message, dropped)
            }
            other => panic!("not a tool progress event: {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_report_goes_out_when_its_second_is_up_without_another_report() {
        let (gate, mut rx) = gate();
        let sender = base::tool::ProgressSender::with_callback(
            "toolu_1",
            Arc::new(GateHandle(gate.clone())),
        );
        sender.send("one");
        sender.send("two");
        sender.send("three");
        assert_eq!(progress_of(rx.try_recv().unwrap()), ("one".into(), 0));
        assert!(rx.try_recv().is_err(), "the rest are held");
        tokio::time::sleep(PROGRESS_INTERVAL).await;
        assert_eq!(progress_of(rx.recv().await.unwrap()), ("three".into(), 1));
        gate.finish();
        assert!(rx.try_recv().is_err(), "nothing was left to finish with");
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_message_is_cut_on_a_character_boundary() {
        let (gate, mut rx) = gate();
        let sender =
            base::tool::ProgressSender::with_callback("toolu_1", Arc::new(GateHandle(gate)));
        sender.send(&"é".repeat(MAX_MESSAGE_BYTES));
        let (message, _) = progress_of(rx.try_recv().unwrap());
        assert!(message.len() <= MAX_MESSAGE_BYTES);
        assert!(message.chars().all(|c| c == 'é'));
    }
}
