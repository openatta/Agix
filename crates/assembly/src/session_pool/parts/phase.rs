//! 会话的回合槽 `TurnSlot`：正处在哪个阶段（`SessionPhase`：空闲 / 回合中 /
//! 重建中），以及当前这一代引擎事件接收端的存放处；`Generation` 给会话的每一代
//! 引擎编号。阶段说了算，接收端只是放在这里；所有转换都在池的 `sessions` 锁里
//! 同步完成，代际的比对由持有槽的 `LiveSession` 做。

use crate::session_pool::api::{Busy, Initiator, WatcherId};
use crate::session_pool::parts::live::SessionCore;
use runtime::agent::EventReceiver;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::session_pool) struct Generation(u64);

impl Generation {
    pub(in crate::session_pool) fn get(self) -> u64 {
        self.0
    }
}

#[derive(Default)]
pub(in crate::session_pool) struct Generations(AtomicU64);

impl Generations {
    pub(in crate::session_pool) fn next(&self) -> Generation {
        Generation(self.0.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

#[derive(Debug)]
pub(in crate::session_pool) enum SessionPhase {
    Idle,
    Running(RunningTurn),
    Rebuilding,
}

#[derive(Debug)]
pub(in crate::session_pool) struct RunningTurn {
    pub(in crate::session_pool) turn_id: String,
    pub(in crate::session_pool) initiator: Initiator,
    /// Hears how the turn ended from its own `run_turn` result rather than
    /// from an `error` frame.
    pub(in crate::session_pool) origin: Option<WatcherId>,
    /// The turn's `turn_complete` or `error` frame has gone out.
    pub(in crate::session_pool) terminated: bool,
}

impl RunningTurn {
    pub(in crate::session_pool) fn new(
        turn_id: String,
        initiator: Initiator,
        origin: Option<WatcherId>,
    ) -> Self {
        Self {
            turn_id,
            initiator,
            origin,
            terminated: false,
        }
    }
}

/// What a running turn holds: the session it runs in, the engine it started
/// on and the receiver it took. What it writes back goes to `core`; the slot
/// it gives back is the one of `core`'s entry while that entry still runs
/// engine `generation`.
pub(in crate::session_pool) struct TurnTicket {
    pub(in crate::session_pool) core: Arc<SessionCore>,
    pub(in crate::session_pool) generation: Generation,
    pub(in crate::session_pool) turn_id: String,
    pub(in crate::session_pool) rx: Arc<AsyncMutex<Option<EventReceiver>>>,
}

impl TurnTicket {
    pub(in crate::session_pool) fn sid(&self) -> &str {
        self.core.id()
    }
}

pub(in crate::session_pool) struct TurnSlot {
    pub(in crate::session_pool) phase: SessionPhase,
    pub(in crate::session_pool) rx: Arc<AsyncMutex<Option<EventReceiver>>>,
    /// Woken whenever the session stops running a turn — a `run_turn` with
    /// `on_busy: queue` waits on it.
    pub(in crate::session_pool) idle: Arc<tokio::sync::Notify>,
}

impl TurnSlot {
    pub(in crate::session_pool) fn new(rx: EventReceiver) -> Self {
        Self {
            phase: SessionPhase::Idle,
            rx: Arc::new(AsyncMutex::new(Some(rx))),
            idle: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(in crate::session_pool) fn is_idle(&self) -> bool {
        matches!(self.phase, SessionPhase::Idle)
    }

    pub(in crate::session_pool) fn running(&self) -> Option<&RunningTurn> {
        match &self.phase {
            SessionPhase::Running(turn) => Some(turn),
            _ => None,
        }
    }

    pub(in crate::session_pool) fn current_turn_id(&self) -> Option<String> {
        self.running().map(|turn| turn.turn_id.clone())
    }

    pub(in crate::session_pool) fn claim(
        &mut self,
        turn: RunningTurn,
    ) -> Result<(), &SessionPhase> {
        if !self.is_idle() {
            return Err(&self.phase);
        }
        self.phase = SessionPhase::Running(turn);
        Ok(())
    }

    pub(in crate::session_pool) fn mark_terminated(&mut self) -> bool {
        match &mut self.phase {
            SessionPhase::Running(turn) => {
                turn.terminated = true;
                true
            }
            _ => false,
        }
    }

    /// Back to `Idle` from the turn `turn_id`, handing back what that turn
    /// was.
    pub(in crate::session_pool) fn release(&mut self, turn_id: &str) -> Option<RunningTurn> {
        match &self.phase {
            SessionPhase::Running(turn) if turn.turn_id == turn_id => {}
            _ => return None,
        }
        let SessionPhase::Running(turn) = std::mem::replace(&mut self.phase, SessionPhase::Idle)
        else {
            unreachable!("matched as running above");
        };
        self.idle.notify_waiters();
        Some(turn)
    }

    pub(in crate::session_pool) fn begin_rebuild(&mut self) -> Result<(), &SessionPhase> {
        if !self.is_idle() {
            return Err(&self.phase);
        }
        self.phase = SessionPhase::Rebuilding;
        Ok(())
    }

    pub(in crate::session_pool) fn abort_rebuild(&mut self) -> bool {
        if !matches!(self.phase, SessionPhase::Rebuilding) {
            return false;
        }
        self.phase = SessionPhase::Idle;
        self.idle.notify_waiters();
        true
    }

    /// Out of `Rebuilding` onto the next engine, whose events arrive on `rx`.
    pub(in crate::session_pool) fn finish_rebuild(&mut self, rx: EventReceiver) -> bool {
        if !matches!(self.phase, SessionPhase::Rebuilding) {
            return false;
        }
        self.rx = Arc::new(AsyncMutex::new(Some(rx)));
        self.phase = SessionPhase::Idle;
        self.idle.notify_waiters();
        true
    }

    pub(in crate::session_pool) fn busy(&self, session_id: &str) -> Option<Busy> {
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
        TurnSlot::new(rx)
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
        slot.claim(turn("t1")).expect("idle");
        assert!(
            matches!(slot.claim(turn("t2")), Err(SessionPhase::Running(t)) if t.turn_id == "t1")
        );
        assert_eq!(slot.current_turn_id().as_deref(), Some("t1"));
    }

    #[test]
    fn a_release_lands_only_on_its_own_turn() {
        let mut slot = slot();
        slot.claim(turn("t1")).unwrap();
        assert!(slot.release("t2").is_none());
        assert_eq!(slot.release("t1").map(|t| t.turn_id).as_deref(), Some("t1"));
        assert!(slot.is_idle());
        assert!(slot.release("t1").is_none());
    }

    #[test]
    fn terminated_is_marked_on_the_running_turn() {
        let mut slot = slot();
        assert!(!slot.mark_terminated());
        slot.claim(turn("t1")).unwrap();
        assert!(slot.mark_terminated());
        assert!(slot.running().is_some_and(|t| t.terminated));
    }

    #[test]
    fn a_rebuild_holds_the_slot_until_it_is_aborted_or_finished() {
        let mut slot = slot();
        slot.begin_rebuild().expect("idle");
        assert!(matches!(
            slot.claim(turn("t1")),
            Err(SessionPhase::Rebuilding)
        ));
        assert!(slot.abort_rebuild());
        assert!(slot.is_idle());
        assert!(!slot.abort_rebuild());
        slot.claim(turn("t1")).unwrap();
        assert!(slot.begin_rebuild().is_err());
        slot.release("t1").unwrap();

        let before = slot.rx.clone();
        slot.begin_rebuild().unwrap();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(slot.finish_rebuild(rx));
        assert!(slot.is_idle());
        assert!(
            !Arc::ptr_eq(&before, &slot.rx),
            "the next engine's receiver"
        );
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(!slot.finish_rebuild(rx));
    }

    #[test]
    fn busy_says_what_the_session_is_doing() {
        use crate::session_pool::api::BusyReason;
        let mut slot = slot();
        assert!(slot.busy("S1").is_none());
        slot.claim(turn("t1")).unwrap();
        let busy = slot.busy("S1").expect("busy");
        assert_eq!(busy.reason, BusyReason::TurnRunning);
        assert_eq!(busy.current_turn_id.as_deref(), Some("t1"));
        slot.release("t1");
        slot.begin_rebuild().unwrap();
        let busy = slot.busy("S1").expect("busy");
        assert_eq!(busy.reason, BusyReason::Rebuilding);
        assert_eq!(busy.current_turn_id, None);
    }
}
