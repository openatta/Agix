//! 内存里的会话 `LiveSessions`：会话表、每会话的磁盘临界区锁、容量与空闲超时、
//! 刚被拿出表还在停的引擎、整池关闭的状态，以及引擎的代际号。每个会话的
//! `LiveSession` 带着它的回合槽 `TurnSlot`（`phase.rs`）和事件出口 `SessionHub`。
//!
//! 锁序：`session_locks[sid]` → `sessions` → `TurnSlot.rx` → `SessionHub.inner`（叶子）。
//! `session_locks` 的表锁和 `stopping` 只在取、放的那一下持有。
//!
//! D3 会把 `LiveSession` 分成会话的 `SessionCore` 与引擎的 `EngineSlot`，那是这个
//! 模块内部的事；门面经 `lock()` 看到的仍是一张以会话 id 为键的表。

use crate::session_pool::parts::hub::SessionHub;
use crate::session_pool::parts::phase::{Generation, Generations, TurnSlot};
use crate::session_pool::{CreateError, SessionNamer, AGENT_EXIT_GRACE, PROCESS_GROUP_GRACE};
use base::interface::scene::SceneSpec;
use runtime::agent::InputSender;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::warn;

pub(in crate::session_pool) struct LiveSession {
    pub(in crate::session_pool) input_tx: InputSender,
    /// Which engine this entry is, what it is doing, and the receiver its
    /// turns take. A session runs one turn at a time; the running one is
    /// named here, so a refusal can name it
    /// (`SESSION_BUSY.data.current_turn_id`) and `session.get` can report
    /// `turn_state`.
    pub(in crate::session_pool) turn: TurnSlot,
    pub(in crate::session_pool) cancel: CancellationToken,
    /// Cancelled once the Agent's task has returned and dropped the Agent.
    pub(in crate::session_pool) exited: CancellationToken,
    /// How full the context was after the last turn that called the model.
    pub(in crate::session_pool) context: Option<base::event::ContextUsage>,
    /// The scene this session was created under — the one its `Meta` records,
    /// not this pool's default. Auto-naming is the one behaviour decided per
    /// turn rather than at build time, so it is the one that needs the scene
    /// kept here to ask.
    pub(in crate::session_pool) scene: Arc<SceneSpec>,
    /// The project this session is bound to, `None` for a no-project session.
    pub(in crate::session_pool) project: Option<PathBuf>,
    /// Session name（CHAT 场景首轮后由 LLM 生成；CODING 为 None）。
    pub(in crate::session_pool) name: Option<String>,
    pub(in crate::session_pool) created_at: Instant,
    /// 用于首轮命名判断。
    pub(in crate::session_pool) is_first_turn: bool,
    /// Set by `session.rename`. The first-turn auto-namer skips a session
    /// that has one: a name somebody typed outranks a name a model guessed,
    /// and the guess arrives later.
    pub(in crate::session_pool) named_by_user: bool,
    pub(in crate::session_pool) namer: SessionNamer,
    /// The `stale` flag of the binding this Agent was built from. `run_turn`
    /// rebuilds the session before its next turn once it is set — lazily,
    /// so an invalidation never has to walk the sessions.
    pub(in crate::session_pool) binding_stale: Arc<std::sync::atomic::AtomicBool>,
    /// The engine's own session state: the tasks `session.tasks` lists.
    pub(in crate::session_pool) state: Option<Arc<base::context::SessionState>>,
    /// How the engine delegates, for `session.startAgent`.
    pub(in crate::session_pool) delegator: Option<Arc<dyn base::interface::delegate::Delegator>>,
    /// What this session's bound scripts did, if it bound any.
    ///
    /// Kept because a script that quietly does nothing is the failure mode of
    /// this whole tier, and a log line on the daemon's stderr is not something
    /// the person asking "why is my script not working" has. `session.get`
    /// reads it.
    pub(in crate::session_pool) scripts: Option<Arc<base::interface::script::ScriptLedger>>,
    /// The checks this session's plugins registered, for `session.get`.
    pub(in crate::session_pool) health: Arc<base::interface::health::HealthChecks>,
    /// Who watches this session, what it is still asking, and when it was
    /// last active. Every frame the session produces goes to all of its
    /// watchers; a turn is not owned by the one that started it. Kept across
    /// a rebuild, so the watchers and the open asks go on with the session.
    pub(in crate::session_pool) hub: Arc<SessionHub>,
    /// The process groups this session's commands started, kept when
    /// `sandbox.reap_process_groups` is on and stopped when the session ends.
    pub(in crate::session_pool) process_groups: Option<Arc<platform::GroupLedger>>,
}

pub(in crate::session_pool) struct LiveSessions {
    sessions: AsyncMutex<HashMap<String, LiveSession>>,
    /// Per-`session_id` mutexes serializing a session's own disk-mutating
    /// critical sections against each other — see
    /// [`Self::with_session_lock`]. Entries are dropped again once nothing
    /// references them, so this stays proportional to concurrently-contended
    /// ids, not every id this pool has ever touched.
    session_locks: AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    cap: usize,
    idle_timeout: Duration,
    /// The engines taken out of `sessions` that may still be stopping, by
    /// session id. A session brought back under one of these ids waits for
    /// that engine first, so the two never run side by side.
    stopping: std::sync::Mutex<HashMap<String, CancellationToken>>,
    /// Set once the pool starts shutting down; no session is created after.
    shutting_down: AtomicBool,
    /// Cancelled when the shutdown has finished.
    shut_down: CancellationToken,
    /// Numbers every engine this pool brings up, so a turn can tell the entry
    /// it started on from one brought up later under the same id.
    generations: Generations,
}

impl LiveSessions {
    pub(in crate::session_pool) fn new(cap: usize, idle_timeout: Duration) -> Self {
        Self {
            sessions: AsyncMutex::new(HashMap::new()),
            session_locks: AsyncMutex::new(HashMap::new()),
            cap,
            idle_timeout,
            stopping: std::sync::Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            shut_down: CancellationToken::new(),
            generations: Generations::default(),
        }
    }

    /// The table of sessions in memory, keyed by id.
    pub(in crate::session_pool) async fn lock(
        &self,
    ) -> MutexGuard<'_, HashMap<String, LiveSession>> {
        self.sessions.lock().await
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn cap(&self) -> usize {
        self.cap
    }

    pub(in crate::session_pool) fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    pub(in crate::session_pool) fn next_generation(&self) -> Generation {
        self.generations.next()
    }

    /// Make room for one more session in `sessions`: take out the least
    /// recently active idle one when the table is full and `sid` is not
    /// already in it (an in-place rebuild replaces, it does not add).
    pub(in crate::session_pool) fn make_room(
        &self,
        sessions: &mut HashMap<String, LiveSession>,
        sid: &str,
    ) -> Option<(String, LiveSession)> {
        if sessions.contains_key(sid) || sessions.len() < self.cap {
            return None;
        }
        evict_lru(sessions)
    }

    /// Stop `live`, just taken out of `sessions` under `sid`, without
    /// waiting for it: whoever brings `sid` back waits instead
    /// ([`Self::wait_until_stopped`]). Nothing it still produces reaches its
    /// watchers.
    pub(in crate::session_pool) fn stop_removed(&self, sid: &str, live: &LiveSession) {
        live.cancel.cancel();
        live.hub.clear();
        let mut stopping = self
            .stopping
            .lock()
            .expect("stopping is never held across a panic");
        stopping.retain(|_, exited| !exited.is_cancelled());
        stopping.insert(sid.to_string(), live.exited.clone());
    }

    /// Wait, within [`AGENT_EXIT_GRACE`], for the engine `sid` last ran on
    /// to stop, if it has not.
    pub(in crate::session_pool) async fn wait_until_stopped(
        &self,
        sid: &str,
    ) -> Result<(), CreateError> {
        let exited = self
            .stopping
            .lock()
            .expect("stopping is never held across a panic")
            .get(sid)
            .cloned();
        let Some(exited) = exited else {
            return Ok(());
        };
        if tokio::time::timeout(AGENT_EXIT_GRACE, exited.cancelled())
            .await
            .is_err()
        {
            warn!(%sid, "the session's previous engine did not stop within the grace period");
            return Err(CreateError::previous_engine_running(sid));
        }
        Ok(())
    }

    /// Run `fut` while holding this pool's per-`session_id` lock — see the
    /// `session_locks` field. Closes the race between a
    /// sidechain's cascade delete (`delete_sidechain_list`, `delete_session`)
    /// and a concurrent `session.resume(that_same_id, create_if_missing:
    /// true)`: whichever side gets here first runs to completion before
    /// the other starts, so a resume can't observe the id mid-cascade (files
    /// half gone) and a completed cascade can't delete out from under a
    /// resume that just finished recreating the id. It does *not* stop the
    /// id from being reused afterward — recreating a just-deleted sidechain
    /// as a fresh orphan session is still possible; the two operations
    /// serialize instead of interleaving.
    pub(in crate::session_pool) async fn with_session_lock<T>(
        &self,
        session_id: &str,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        let lock = {
            let mut locks = self.session_locks.lock().await;
            locks
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let result = {
            let _guard = lock.lock().await;
            fut.await
        };
        drop(lock);
        let mut locks = self.session_locks.lock().await;
        if locks
            .get(session_id)
            .is_some_and(|entry| Arc::strong_count(entry) == 1)
        {
            locks.remove(session_id);
        }
        result
    }

    pub(in crate::session_pool) fn refuse_while_shutting_down(&self) -> Result<(), String> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("the daemon is shutting down".to_string());
        }
        Ok(())
    }

    /// Mark the pool as shutting down. `None` when it already was; the first
    /// caller holds the guard that marks the shutdown finished when dropped.
    pub(in crate::session_pool) fn begin_shutdown(&self) -> Option<DropGuard> {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some(self.shut_down.clone().drop_guard())
    }

    /// Wait until the shutdown in progress has finished.
    pub(in crate::session_pool) async fn shut_down(&self) {
        self.shut_down.cancelled().await;
    }

    #[cfg(test)]
    pub(in crate::session_pool) async fn session_lock_holders(&self, sid: &str) -> usize {
        self.session_locks
            .lock()
            .await
            .get(sid)
            .map_or(0, Arc::strong_count)
    }

    #[cfg(test)]
    pub(in crate::session_pool) async fn no_session_locks(&self) -> bool {
        self.session_locks.lock().await.is_empty()
    }
}

/// Whether `live` is doing anything eviction would cut off: running a turn,
/// being rebuilt, or running background tasks.
pub(in crate::session_pool) fn is_busy(live: &LiveSession) -> bool {
    !live.turn.is_idle()
        || live.state.as_ref().is_some_and(|state| {
            state
                .list_running_tasks()
                .iter()
                .any(|(_, status)| matches!(status, base::context::RunningStatus::Running))
        })
}

/// Take the least recently active idle session out of `sessions`, or `None`
/// when every session is busy — the pool then runs over its cap rather than
/// cut one off.
pub(in crate::session_pool) fn evict_lru(
    sessions: &mut HashMap<String, LiveSession>,
) -> Option<(String, LiveSession)> {
    let sid = sessions
        .iter()
        .filter(|(_, live)| !is_busy(live))
        .min_by_key(|(_, live)| live.hub.last_active())
        .map(|(k, _)| k.clone())?;
    sessions.remove_entry(&sid)
}

pub(in crate::session_pool) fn reap_process_groups_later(live: &LiveSession) {
    if let Some(ledger) = live.process_groups.clone() {
        tokio::spawn(async move { ledger.reap(PROCESS_GROUP_GRACE).await });
    }
}

/// Session id 列表：`last_active` 距 `now` 超过 `timeout`、且手上没有活的那些。拆成纯函数
/// （不做任何驱逐动作，只挑出 id）——原逻辑内嵌在 `start_janitor` 真实跑
/// 300 秒一次的循环体里，没法在测试里等 5 分钟去验证；抽出来之后可以直接用
/// 人为构造的" last_active 已经很旧"的 session 测，不用真的睡觉。
pub(in crate::session_pool) fn idle_session_ids(
    sessions: &HashMap<String, LiveSession>,
    now: Instant,
    timeout: Duration,
) -> Vec<String> {
    sessions
        .iter()
        .filter(|(_, live)| {
            now.saturating_duration_since(live.hub.last_active()) > timeout && !is_busy(live)
        })
        .map(|(sid, _)| sid.clone())
        .collect()
}
