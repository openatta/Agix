//! 每个会话自己的事件出口 `SessionHub`：谁在看它、它还在等谁回答、它最近一次
//! 有动静是什么时候。事件经它同步交给订阅者，不经过池级的 `sessions` 锁。
//!
//! 锁序里它是叶子：持着它不 `.await`、不去拿别的锁。帧在锁下定好收件人、排进
//! 发件队列，放开锁再交给 `SessionWatcher::deliver`。投递由另一把锁串行：同一时刻
//! 只有一个线程在投递，发帧的调用返回时它的帧已经交出去了，每个订阅者收到的帧仍是
//! 发出的先后。`deliver` 里再往这个 hub 发帧不会卡住：同一线程上再发的帧排在队尾，
//! 由正在投递的这一层接着交出去。回合的终止帧是在池的 `sessions` 锁下发出的，所以
//! `deliver` 不能等池；要用池的工作交给它另起的任务。

use crate::session_pool::api::{AskKind, PendingAsk, SessionFrame, SessionWatcher, WatcherId};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

pub(in crate::session_pool) struct SessionHub {
    inner: Mutex<Inner>,
    delivery: Mutex<()>,
    epoch: Instant,
    active_ms: AtomicU64,
}

#[derive(Default)]
struct Inner {
    watchers: Vec<Arc<dyn SessionWatcher>>,
    pending: HashMap<String, PendingAsk>,
    outbox: VecDeque<Outgoing>,
    delivering_on: Option<ThreadId>,
}

struct Outgoing {
    frame: SessionFrame,
    to: Vec<Arc<dyn SessionWatcher>>,
}

impl SessionHub {
    pub(in crate::session_pool) fn active_at(at: Instant) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            delivery: Mutex::new(()),
            epoch: at,
            active_ms: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(in crate::session_pool) fn touch(&self) {
        let ms = Instant::now()
            .saturating_duration_since(self.epoch)
            .as_millis();
        self.active_ms
            .fetch_max(u64::try_from(ms).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub(in crate::session_pool) fn last_active(&self) -> Instant {
        self.epoch + Duration::from_millis(self.active_ms.load(Ordering::Relaxed))
    }

    pub(in crate::session_pool) fn publish(&self, frame: &SessionFrame) {
        self.publish_except(frame, None);
    }

    /// To every watcher but `except` — the one that hears the same thing
    /// from its own call. A watcher that cannot take the frame is dropped.
    pub(in crate::session_pool) fn publish_except(
        &self,
        frame: &SessionFrame,
        except: Option<WatcherId>,
    ) {
        self.touch();
        self.send(self.lock(), frame, except);
    }

    /// Recorded before it goes out, under the same lock: a watcher that
    /// subscribes meanwhile is handed it either way.
    pub(in crate::session_pool) fn publish_ask(
        &self,
        frame: Arc<SessionFrame>,
        prompt_id: String,
        task_id: Option<String>,
        kind: AskKind,
    ) {
        self.touch();
        let mut inner = self.lock();
        inner.pending.insert(
            prompt_id.clone(),
            PendingAsk {
                prompt_id,
                task_id,
                kind,
                frame: frame.clone(),
            },
        );
        self.send(inner, &frame, None);
    }

    /// The ask `prompt_id` is no longer open: forgotten and announced under
    /// the same lock, so a watcher that subscribes meanwhile is handed either
    /// the ask or the frame that ends it, never the ask alone.
    pub(in crate::session_pool) fn publish_resolved(&self, frame: &SessionFrame, prompt_id: &str) {
        self.touch();
        let mut inner = self.lock();
        inner.pending.remove(prompt_id);
        self.send(inner, frame, None);
    }

    /// Start watching, and what is still being asked as of that moment.
    pub(in crate::session_pool) fn subscribe(
        &self,
        watcher: Arc<dyn SessionWatcher>,
    ) -> Vec<PendingAsk> {
        let mut inner = self.lock();
        let id = watcher.id();
        inner.watchers.retain(|w| w.id() != id);
        inner.watchers.push(watcher);
        inner.pending.values().cloned().collect()
    }

    pub(in crate::session_pool) fn unsubscribe(&self, id: WatcherId) {
        self.lock().watchers.retain(|w| w.id() != id);
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn is_watched_by(&self, id: WatcherId) -> bool {
        self.lock().watchers.iter().any(|w| w.id() == id)
    }

    pub(in crate::session_pool) fn ask_kind(&self, prompt_id: &str) -> Option<AskKind> {
        self.lock().pending.get(prompt_id).map(|ask| ask.kind)
    }

    pub(in crate::session_pool) fn forget_asks_of_task(&self, task_id: &str) {
        self.lock()
            .pending
            .retain(|_, ask| ask.task_id.as_deref() != Some(task_id));
    }

    /// Forget the asks a turn's own engine raised; a background task's stay.
    pub(in crate::session_pool) fn retain_task_asks(&self) {
        self.lock().pending.retain(|_, ask| ask.task_id.is_some());
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn pending_prompt_ids(&self) -> Vec<String> {
        self.lock().pending.keys().cloned().collect()
    }

    /// Back to watching nothing and asking nothing.
    pub(in crate::session_pool) fn clear(&self) {
        let mut inner = self.lock();
        inner.watchers.clear();
        inner.pending.clear();
    }

    /// Queue `frame` for whoever watches now and deliver the queue with the
    /// state lock released. Returns once `frame` has been handed over, except
    /// when called from inside a `deliver` on this hub: the delivery under way
    /// hands it over next. A watcher that cannot take a frame is dropped and
    /// gets no more.
    fn send(
        &self,
        mut inner: MutexGuard<'_, Inner>,
        frame: &SessionFrame,
        except: Option<WatcherId>,
    ) {
        let to = inner
            .watchers
            .iter()
            .filter(|w| Some(w.id()) != except)
            .cloned()
            .collect();
        inner.outbox.push_back(Outgoing {
            frame: frame.clone(),
            to,
        });
        let this_thread = std::thread::current().id();
        if inner.delivering_on == Some(this_thread) {
            return;
        }
        drop(inner);
        let _delivery = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        let _unwind = StillDelivering(self);
        // By the subscription, not its id: one that subscribes again under
        // the same id while this delivery goes on is a new one, and stays.
        let mut dropped: Vec<Arc<dyn SessionWatcher>> = Vec::new();
        let is_dropped = |dropped: &[Arc<dyn SessionWatcher>], w: &Arc<dyn SessionWatcher>| {
            dropped.iter().any(|d| Arc::ptr_eq(d, w))
        };
        loop {
            let mut inner = self.lock();
            if !dropped.is_empty() {
                inner.watchers.retain(|w| !is_dropped(&dropped, w));
            }
            let Some(Outgoing { frame, to }) = inner.outbox.pop_front() else {
                inner.delivering_on = None;
                return;
            };
            inner.delivering_on = Some(this_thread);
            drop(inner);
            for watcher in to {
                if !is_dropped(&dropped, &watcher) && !watcher.deliver(&frame) {
                    dropped.push(watcher);
                }
            }
        }
    }
}

/// A watcher that panics in `deliver` must not leave the hub believing a
/// delivery is still under way, which would hold every later frame back.
struct StillDelivering<'a>(&'a SessionHub);

impl Drop for StillDelivering<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.lock().delivering_on = None;
        }
    }
}
