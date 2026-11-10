//! 会话的回合槽 `TurnSlot`：这个条目是第几代引擎（`Generation`）、正处在哪个
//! 阶段（`SessionPhase`：空闲 / 回合中 / 重建中），以及引擎事件接收端的存放处。
//! 阶段说了算，接收端只是放在这里；所有转换都在池的 `sessions` 锁里同步完成。

use super::api::{Busy, Initiator, WatcherId};
use runtime::agent::EventReceiver;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Generation(u64);

#[derive(Default)]
pub(super) struct Generations(AtomicU64);

impl Generations {
    pub(super) fn next(&self) -> Generation {
        Generation(self.0.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

#[derive(Debug)]
pub(super) enum SessionPhase {
    Idle,
    Running(RunningTurn),
    Rebuilding,
}

#[derive(Debug)]
pub(super) struct RunningTurn {
    pub(super) turn_id: String,
    pub(super) initiator: Initiator,
    /// Hears how the turn ended from its own `run_turn` result rather than
    /// from an `error` frame.
    pub(super) origin: Option<WatcherId>,
    /// The turn's `turn_complete` or `error` frame has gone out.
    pub(super) terminated: bool,
}

impl RunningTurn {
    pub(super) fn new(turn_id: String, initiator: Initiator, origin: Option<WatcherId>) -> Self {
        Self {
            turn_id,
            initiator,
            origin,
            terminated: false,
        }
    }
}

/// What a running turn holds: the entry it started on and the receiver it
/// took. Everything it writes back to the session is checked against
/// `generation`, so an entry brought up later under the same id is left alone.
pub(super) struct TurnTicket {
    pub(super) sid: String,
    pub(super) generation: Generation,
    pub(super) turn_id: String,
    pub(super) rx: Arc<AsyncMutex<Option<EventReceiver>>>,
}

pub(super) struct TurnSlot {
    pub(super) generation: Generation,
    pub(super) phase: SessionPhase,
    pub(super) rx: Arc<AsyncMutex<Option<EventReceiver>>>,
    /// Woken whenever the session stops running a turn — a `run_turn` with
    /// `on_busy: queue` waits on it.
    pub(super) idle: Arc<tokio::sync::Notify>,
}

impl TurnSlot {
    pub(super) fn new(generation: Generation, rx: EventReceiver) -> Self {
        Self {
            generation,
            phase: SessionPhase::Idle,
            rx: Arc::new(AsyncMutex::new(Some(rx))),
            idle: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(super) fn is_idle(&self) -> bool {
        matches!(self.phase, SessionPhase::Idle)
    }

    pub(super) fn running(&self) -> Option<&RunningTurn> {
        match &self.phase {
            SessionPhase::Running(turn) => Some(turn),
            _ => None,
        }
    }

    pub(super) fn current_turn_id(&self) -> Option<String> {
        self.running().map(|turn| turn.turn_id.clone())
    }

    pub(super) fn claim(&mut self, turn: RunningTurn) -> Result<Generation, &SessionPhase> {
        if !self.is_idle() {
            return Err(&self.phase);
        }
        self.phase = SessionPhase::Running(turn);
        Ok(self.generation)
    }

    pub(super) fn mark_terminated(&mut self, g: Generation) -> bool {
        match &mut self.phase {
            SessionPhase::Running(turn) if g == self.generation => {
                turn.terminated = true;
                true
            }
            _ => false,
        }
    }

    /// Back to `Idle` from the turn `turn_id` of generation `g`, handing back
    /// what that turn was.
    pub(super) fn release(&mut self, g: Generation, turn_id: &str) -> Option<RunningTurn> {
        match &self.phase {
            SessionPhase::Running(turn) if g == self.generation && turn.turn_id == turn_id => {}
            _ => return None,
        }
        let SessionPhase::Running(turn) = std::mem::replace(&mut self.phase, SessionPhase::Idle)
        else {
            unreachable!("matched as running above");
        };
        self.idle.notify_waiters();
        Some(turn)
    }

    pub(super) fn begin_rebuild(&mut self) -> Result<Generation, &SessionPhase> {
        if !self.is_idle() {
            return Err(&self.phase);
        }
        self.phase = SessionPhase::Rebuilding;
        Ok(self.generation)
    }

    pub(super) fn abort_rebuild(&mut self, g: Generation) -> bool {
        if g != self.generation || !matches!(self.phase, SessionPhase::Rebuilding) {
            return false;
        }
        self.phase = SessionPhase::Idle;
        self.idle.notify_waiters();
        true
    }

    pub(super) fn busy(&self, session_id: &str) -> Option<Busy> {
        match &self.phase {
            SessionPhase::Idle => None,
            SessionPhase::Running(turn) => Some(Busy::turn_running(session_id, &turn.turn_id)),
            SessionPhase::Rebuilding => Some(Busy::rebuilding(session_id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> TurnSlot {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        TurnSlot::new(Generations::default().next(), rx)
    }

    fn turn(id: &str) -> RunningTurn {
        RunningTurn::new(id.into(), Initiator::Client, None)
    }

    #[test]
    fn generations_never_repeat() {
        let generations = Generations::default();
        assert_ne!(generations.next(), generations.next());
    }

    #[test]
    fn only_an_idle_slot_is_claimed() {
        let mut slot = slot();
        let g = slot.claim(turn("t1")).expect("idle");
        assert_eq!(g, slot.generation);
        assert!(
            matches!(slot.claim(turn("t2")), Err(SessionPhase::Running(t)) if t.turn_id == "t1")
        );
        assert_eq!(slot.current_turn_id().as_deref(), Some("t1"));
    }

    #[test]
    fn a_release_lands_only_on_its_own_turn_and_generation() {
        let mut slot = slot();
        let g = slot.claim(turn("t1")).unwrap();
        assert!(slot.release(Generation(g.0 + 1), "t1").is_none());
        assert!(slot.release(g, "t2").is_none());
        assert_eq!(
            slot.release(g, "t1").map(|t| t.turn_id).as_deref(),
            Some("t1")
        );
        assert!(slot.is_idle());
        assert!(slot.release(g, "t1").is_none());
    }

    #[test]
    fn terminated_is_marked_on_the_running_turn_of_its_generation() {
        let mut slot = slot();
        assert!(!slot.mark_terminated(slot.generation));
        let g = slot.claim(turn("t1")).unwrap();
        assert!(!slot.mark_terminated(Generation(g.0 + 1)));
        assert!(slot.mark_terminated(g));
        assert!(slot.running().is_some_and(|t| t.terminated));
    }

    #[test]
    fn a_rebuild_holds_the_slot_until_it_is_aborted() {
        let mut slot = slot();
        let g = slot.begin_rebuild().expect("idle");
        assert!(matches!(
            slot.claim(turn("t1")),
            Err(SessionPhase::Rebuilding)
        ));
        assert!(!slot.abort_rebuild(Generation(g.0 + 1)));
        assert!(slot.abort_rebuild(g));
        assert!(slot.is_idle());
        assert!(!slot.abort_rebuild(g));
        slot.claim(turn("t1")).unwrap();
        assert!(slot.begin_rebuild().is_err());
    }

    #[test]
    fn busy_says_what_the_session_is_doing() {
        use super::super::api::BusyReason;
        let mut slot = slot();
        assert!(slot.busy("S1").is_none());
        let g = slot.claim(turn("t1")).unwrap();
        let busy = slot.busy("S1").expect("busy");
        assert_eq!(busy.reason, BusyReason::TurnRunning);
        assert_eq!(busy.current_turn_id.as_deref(), Some("t1"));
        slot.release(g, "t1");
        slot.begin_rebuild().unwrap();
        let busy = slot.busy("S1").expect("busy");
        assert_eq!(busy.reason, BusyReason::Rebuilding);
        assert_eq!(busy.current_turn_id, None);
    }
}
