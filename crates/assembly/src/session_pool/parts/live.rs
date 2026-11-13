//! 内存里的会话 `LiveSessions`：会话表、每会话的磁盘临界区锁、容量与空闲超时、
//! 刚被拿出表还在停的引擎、整池关闭的状态，以及引擎的代际号。
//!
//! 表里每个会话是一个 `LiveSession`：会话本身的 `SessionCore`（状态、事件出口、
//! steer 队列、进程组、名字、上下文用量与日志是否落后；建会话或冷恢复时建，关闭、驱逐、整池
//! 关闭时丢，重建不换）、当前这一代引擎的 `EngineSlot`，以及回合槽 `TurnSlot`
//! （`phase.rs`）。门面经这里的窄方法读写这张表，拿不到表本身。
//!
//! 锁序：`session_locks[sid]` → `sessions` → `TurnSlot.rx` →
//! { `SessionHub.inner` | `SessionCore.*` | `SessionState.*` }，叶子锁之间互不嵌套。
//! `session_locks` 的表锁和 `stopping` 只在取、放的那一下持有。

use crate::session_pool::parts::hub::SessionHub;
use crate::session_pool::parts::phase::{Generation, Generations, TurnSlot};
use crate::session_pool::{CreateError, SessionNamer, AGENT_EXIT_GRACE, PROCESS_GROUP_GRACE};
use base::context::SessionState;
use base::event::ContextUsage;
use base::interface::scene::SceneSpec;
use runtime::agent::InputSender;
use runtime::steering::SteerHandle;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::warn;

/// What a session is called, and whether it may still name itself.
#[derive(Debug, Clone, Default)]
pub(in crate::session_pool) struct Naming {
    /// Set by the first-turn namer or `session.rename`.
    pub(in crate::session_pool) name: Option<String>,
    /// The name was typed by somebody: a guess the namer makes later does not
    /// replace it.
    pub(in crate::session_pool) named_by_user: bool,
    pub(in crate::session_pool) is_first_turn: bool,
}

/// Whether the session's log has everything the session holds. While it is
/// behind, the session in memory is the only complete copy: a rebuild has its
/// engine write the rest first, and eviction passes it over.
#[derive(Default)]
pub(in crate::session_pool) struct LogHealth {
    behind: AtomicBool,
}

impl LogHealth {
    pub(in crate::session_pool) fn is_behind(&self) -> bool {
        self.behind.load(Ordering::SeqCst)
    }

    pub(in crate::session_pool) fn set_behind(&self, behind: bool) {
        self.behind.store(behind, Ordering::SeqCst);
    }
}

/// The session itself, whichever engine runs it.
pub(in crate::session_pool) struct SessionCore {
    id: Arc<str>,
    /// The scene this session was created under — the one its `Meta` records,
    /// not this pool's default.
    scene: Arc<SceneSpec>,
    /// `None` for a no-project session.
    project: Option<PathBuf>,
    created_at: Instant,
    state: Arc<SessionState>,
    /// Who watches this session, what it is still asking, and when it was
    /// last active.
    hub: Arc<SessionHub>,
    steer: SteerHandle,
    /// The process groups this session's commands started, recorded when
    /// `sandbox.reap_process_groups` is on and stopped when the session ends.
    process_groups: Arc<platform::GroupLedger>,
    naming: Mutex<Naming>,
    /// How full the context was after the last turn that called the model.
    context: Mutex<Option<ContextUsage>>,
    log: LogHealth,
    /// How its last failed turn failed.
    last_failure: Mutex<Option<base::event::FailureKind>>,
}

impl SessionCore {
    pub(in crate::session_pool) fn new(
        id: &str,
        scene: Arc<SceneSpec>,
        project: Option<PathBuf>,
        naming: Naming,
    ) -> Arc<Self> {
        Self::active_at(id, scene, project, naming, Instant::now())
    }

    pub(in crate::session_pool) fn active_at(
        id: &str,
        scene: Arc<SceneSpec>,
        project: Option<PathBuf>,
        naming: Naming,
        at: Instant,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: Arc::from(id),
            scene,
            project,
            created_at: at,
            state: Arc::new(SessionState::new()),
            hub: Arc::new(SessionHub::active_at(at)),
            steer: SteerHandle::default(),
            process_groups: platform::GroupLedger::new(),
            naming: Mutex::new(naming),
            context: Mutex::new(None),
            log: LogHealth::default(),
            last_failure: Mutex::new(None),
        })
    }

    pub(in crate::session_pool) fn id(&self) -> &str {
        &self.id
    }

    pub(in crate::session_pool) fn scene(&self) -> &Arc<SceneSpec> {
        &self.scene
    }

    pub(in crate::session_pool) fn project(&self) -> Option<&Path> {
        self.project.as_deref()
    }

    pub(in crate::session_pool) fn created_at(&self) -> Instant {
        self.created_at
    }

    pub(in crate::session_pool) fn state(&self) -> &Arc<SessionState> {
        &self.state
    }

    pub(in crate::session_pool) fn hub(&self) -> &Arc<SessionHub> {
        &self.hub
    }

    pub(in crate::session_pool) fn steer(&self) -> &SteerHandle {
        &self.steer
    }

    pub(in crate::session_pool) fn process_groups(&self) -> &Arc<platform::GroupLedger> {
        &self.process_groups
    }

    pub(in crate::session_pool) fn naming(&self) -> MutexGuard<'_, Naming> {
        self.naming.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(in crate::session_pool) fn context(&self) -> Option<ContextUsage> {
        *self.context.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(in crate::session_pool) fn set_context(&self, context: ContextUsage) {
        *self.context.lock().unwrap_or_else(|e| e.into_inner()) = Some(context);
    }

    pub(in crate::session_pool) fn last_failure(&self) -> Option<base::event::FailureKind> {
        *self.last_failure.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(in crate::session_pool) fn set_last_failure(&self, kind: base::event::FailureKind) {
        *self.last_failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(kind);
    }

    pub(in crate::session_pool) fn log(&self) -> &LogHealth {
        &self.log
    }

    pub(in crate::session_pool) fn reap_process_groups_later(&self) {
        let ledger = self.process_groups.clone();
        tokio::spawn(async move { ledger.reap(PROCESS_GROUP_GRACE).await });
    }
}

/// One engine of a session: the `generation`-th the pool brought up, and
/// what the session reaches it by.
pub(in crate::session_pool) struct EngineSlot {
    pub(in crate::session_pool) generation: Generation,
    pub(in crate::session_pool) input_tx: InputSender,
    pub(in crate::session_pool) cancel: CancellationToken,
    /// Cancelled once the Agent's task has returned and dropped the Agent.
    pub(in crate::session_pool) exited: CancellationToken,
    pub(in crate::session_pool) namer: SessionNamer,
    /// The stamp of the binding this engine was built from. `run_turn`
    /// rebuilds the session before its next turn once the cache no longer
    /// considers it current — lazily, so an invalidation never has to walk
    /// the sessions.
    pub(in crate::session_pool) binding: super::bindings::BindingStamp,
    /// How the engine delegates, for `session.startAgent`.
    pub(in crate::session_pool) delegator: Option<Arc<dyn base::interface::delegate::Delegator>>,
    /// What this engine's bound scripts did, if it bound any.
    ///
    /// Kept because a script that quietly does nothing is the failure mode of
    /// this whole tier, and a log line on the daemon's stderr is not something
    /// the person asking "why is my script not working" has. `session.get`
    /// reads it.
    pub(in crate::session_pool) scripts: Option<Arc<base::interface::script::ScriptLedger>>,
    /// The checks this engine's plugins registered, for `session.get`.
    pub(in crate::session_pool) health: Arc<base::interface::health::HealthChecks>,
}

pub(in crate::session_pool) struct LiveSession {
    pub(in crate::session_pool) core: Arc<SessionCore>,
    pub(in crate::session_pool) engine: EngineSlot,
    /// What the session is doing, and the receiver its turns take. A session
    /// runs one turn at a time; the running one is named here, so a refusal
    /// can name it (`SESSION_BUSY.data.current_turn_id`) and `session.get`
    /// can report `turn_state`.
    pub(in crate::session_pool) turn: TurnSlot,
}

impl LiveSession {
    /// Whether this is `core`'s entry, still on engine `generation`.
    pub(in crate::session_pool) fn runs(
        &self,
        core: &Arc<SessionCore>,
        generation: Generation,
    ) -> bool {
        Arc::ptr_eq(&self.core, core) && self.engine.generation == generation
    }

    /// Out of `Rebuilding` onto `engine`, whose events arrive on `rx`,
    /// handing back the engine it replaced.
    pub(in crate::session_pool) fn replace_engine(
        &mut self,
        engine: EngineSlot,
        rx: runtime::agent::EventReceiver,
    ) -> Option<EngineSlot> {
        if !self.turn.finish_rebuild(rx) {
            return None;
        }
        Some(std::mem::replace(&mut self.engine, engine))
    }

    /// Whether eviction may take the session out of memory: it is doing
    /// nothing eviction would cut off, and its log has everything it holds.
    fn evictable(&self) -> bool {
        !self.is_busy() && !self.core.log.is_behind()
    }

    /// Whether eviction would cut something off: a turn, a rebuild, or
    /// background tasks.
    pub(in crate::session_pool) fn is_busy(&self) -> bool {
        !self.turn.is_idle()
            || self
                .core
                .state
                .list_running_tasks()
                .iter()
                .any(|(_, status)| matches!(status, base::context::RunningStatus::Running))
    }
}

/// What making room for a session took out of the table.
#[derive(Default)]
pub(in crate::session_pool) struct Displaced {
    /// The least recently active idle session, for a full table.
    pub(in crate::session_pool) evicted: Option<(String, LiveSession)>,
    /// An entry already under the same id.
    pub(in crate::session_pool) replaced: Option<LiveSession>,
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
    /// Numbers every engine this pool brings up; the number is also the one
    /// the engine attaches to its session's state under.
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

    pub(in crate::session_pool) async fn with_live<T>(
        &self,
        sid: &str,
        f: impl FnOnce(&LiveSession) -> T,
    ) -> Option<T> {
        self.sessions.lock().await.get(sid).map(f)
    }

    pub(in crate::session_pool) async fn with_live_mut<T>(
        &self,
        sid: &str,
        f: impl FnOnce(&mut LiveSession) -> T,
    ) -> Option<T> {
        self.sessions.lock().await.get_mut(sid).map(f)
    }

    pub(in crate::session_pool) async fn core_of(&self, sid: &str) -> Option<Arc<SessionCore>> {
        self.with_live(sid, |live| live.core.clone()).await
    }

    /// Whether `core` is still the session in memory under its id.
    pub(in crate::session_pool) async fn holds(&self, core: &Arc<SessionCore>) -> bool {
        self.with_live(core.id(), |live| Arc::ptr_eq(&live.core, core))
            .await
            .unwrap_or(false)
    }

    pub(in crate::session_pool) async fn contains(&self, sid: &str) -> bool {
        self.sessions.lock().await.contains_key(sid)
    }

    pub(in crate::session_pool) async fn ids(&self) -> Vec<String> {
        self.sessions.lock().await.keys().cloned().collect()
    }

    pub(in crate::session_pool) async fn len(&self) -> usize {
        self.sessions.lock().await.len()
    }

    pub(in crate::session_pool) async fn map_all<T>(
        &self,
        f: impl FnMut(&LiveSession) -> T,
    ) -> Vec<T> {
        self.sessions.lock().await.values().map(f).collect()
    }

    /// Put `live` in the table under `sid`, making room for it. Refused once
    /// the pool is shutting down — checked under the lock `shutdown_all`
    /// drains the table with, so nothing inserted outlives the shutdown.
    pub(in crate::session_pool) async fn insert_evicting(
        &self,
        sid: &str,
        live: LiveSession,
    ) -> Result<Displaced, String> {
        let mut sessions = self.sessions.lock().await;
        self.refuse_while_shutting_down()?;
        let evicted = if sessions.contains_key(sid) || sessions.len() < self.cap {
            None
        } else {
            evict_lru(&mut sessions)
        };
        let replaced = sessions.insert(sid.to_string(), live);
        Ok(Displaced { evicted, replaced })
    }

    pub(in crate::session_pool) async fn remove(&self, sid: &str) -> Option<LiveSession> {
        self.sessions.lock().await.remove(sid)
    }

    /// Take `sid` out of the table if its entry is the one `pred` accepts.
    pub(in crate::session_pool) async fn remove_if(
        &self,
        sid: &str,
        pred: impl FnOnce(&LiveSession) -> bool,
    ) -> Option<LiveSession> {
        let mut sessions = self.sessions.lock().await;
        if sessions.get(sid).is_some_and(pred) {
            sessions.remove(sid)
        } else {
            None
        }
    }

    /// Take every session out, each handed to `f` under the same lock.
    pub(in crate::session_pool) async fn drain<T>(
        &self,
        mut f: impl FnMut(String, LiveSession) -> T,
    ) -> Vec<T> {
        self.sessions
            .lock()
            .await
            .drain()
            .map(|(sid, live)| f(sid, live))
            .collect()
    }

    /// Take out the sessions idle for longer than `idle_timeout` at `now`,
    /// and say how many are left.
    pub(in crate::session_pool) async fn take_idle(
        &self,
        now: Instant,
    ) -> (Vec<(String, LiveSession)>, usize) {
        let mut sessions = self.sessions.lock().await;
        let taken = idle_session_ids(&sessions, now, self.idle_timeout)
            .into_iter()
            .filter_map(|sid| sessions.remove_entry(&sid))
            .collect();
        (taken, sessions.len())
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn cap(&self) -> usize {
        self.cap
    }

    pub(in crate::session_pool) fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// How many engines taken out of the table have not stopped yet.
    pub(in crate::session_pool) fn stopping_engines(&self) -> usize {
        self.stopping
            .lock()
            .expect("stopping is never held across a panic")
            .values()
            .filter(|exited| !exited.is_cancelled())
            .count()
    }

    pub(in crate::session_pool) fn next_generation(&self) -> Generation {
        self.generations.next()
    }

    /// Stop `live`, just taken out of `sessions` under `sid`, without
    /// waiting for it: whoever brings `sid` back waits instead
    /// ([`Self::wait_until_stopped`]). Nothing it still produces reaches its
    /// watchers.
    pub(in crate::session_pool) fn stop_removed(&self, sid: &str, live: &LiveSession) {
        live.engine.cancel.cancel();
        live.core.hub.clear();
        let mut stopping = self
            .stopping
            .lock()
            .expect("stopping is never held across a panic");
        stopping.retain(|_, exited| !exited.is_cancelled());
        stopping.insert(sid.to_string(), live.engine.exited.clone());
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

/// Take the least recently active idle session out of `sessions`, or `None`
/// when every session is busy or holds what its log lacks — the pool then
/// runs over its cap rather than cut one off or lose what it holds.
pub(in crate::session_pool) fn evict_lru(
    sessions: &mut HashMap<String, LiveSession>,
) -> Option<(String, LiveSession)> {
    let sid = sessions
        .iter()
        .filter(|(_, live)| live.evictable())
        .min_by_key(|(_, live)| live.core.hub.last_active())
        .map(|(k, _)| k.clone())?;
    sessions.remove_entry(&sid)
}

/// The ids of the sessions whose `last_active` is more than `timeout` before
/// `now`, that have nothing running and whose log has everything they hold. A pure function so a test can hand it
/// a session that went quiet long ago instead of waiting for the janitor.
pub(in crate::session_pool) fn idle_session_ids(
    sessions: &HashMap<String, LiveSession>,
    now: Instant,
    timeout: Duration,
) -> Vec<String> {
    sessions
        .iter()
        .filter(|(_, live)| {
            now.saturating_duration_since(live.core.hub.last_active()) > timeout && live.evictable()
        })
        .map(|(sid, _)| sid.clone())
        .collect()
}
