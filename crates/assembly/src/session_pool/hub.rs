//! 每个会话自己的事件出口 `SessionHub`：谁在看它、它还在等谁回答、它最近一次
//! 有动静是什么时候。事件经它同步交给订阅者，不经过池级的 `sessions` 锁。
//!
//! 锁序里它是叶子：持着它不 `.await`、不去拿别的锁；`SessionWatcher::deliver`
//! 只做非阻塞的入队。

use super::api::{AskKind, PendingAsk, SessionFrame, SessionWatcher, WatcherId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(super) struct SessionHub {
    inner: Mutex<Inner>,
    epoch: Instant,
    active_ms: AtomicU64,
}

#[derive(Default)]
struct Inner {
    watchers: Vec<Arc<dyn SessionWatcher>>,
    pending: HashMap<String, PendingAsk>,
}

impl SessionHub {
    pub(super) fn new() -> Self {
        Self::active_at(Instant::now())
    }

    pub(super) fn active_at(at: Instant) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            epoch: at,
            active_ms: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn touch(&self) {
        let ms = Instant::now()
            .saturating_duration_since(self.epoch)
            .as_millis();
        self.active_ms
            .fetch_max(u64::try_from(ms).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub(super) fn last_active(&self) -> Instant {
        self.epoch + Duration::from_millis(self.active_ms.load(Ordering::Relaxed))
    }

    pub(super) fn publish(&self, frame: &SessionFrame) {
        self.publish_except(frame, None);
    }

    /// To every watcher but `except` — the one that hears the same thing
    /// from its own call. A watcher that cannot take the frame is dropped.
    pub(super) fn publish_except(&self, frame: &SessionFrame, except: Option<WatcherId>) {
        self.touch();
        deliver(&mut self.lock(), frame, except);
    }

    /// Recorded before it goes out, under the same lock: a watcher that
    /// subscribes meanwhile is handed it either way.
    pub(super) fn publish_ask(
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
        deliver(&mut inner, &frame, None);
    }

    /// Start watching, and what is still being asked as of that moment.
    pub(super) fn subscribe(&self, watcher: Arc<dyn SessionWatcher>) -> Vec<PendingAsk> {
        let mut inner = self.lock();
        let id = watcher.id();
        inner.watchers.retain(|w| w.id() != id);
        inner.watchers.push(watcher);
        inner.pending.values().cloned().collect()
    }

    pub(super) fn unsubscribe(&self, id: WatcherId) {
        self.lock().watchers.retain(|w| w.id() != id);
    }

    #[cfg(test)]
    pub(super) fn is_watched_by(&self, id: WatcherId) -> bool {
        self.lock().watchers.iter().any(|w| w.id() == id)
    }

    pub(super) fn ask_kind(&self, prompt_id: &str) -> Option<AskKind> {
        self.lock().pending.get(prompt_id).map(|ask| ask.kind)
    }

    pub(super) fn forget_ask(&self, prompt_id: &str) {
        self.lock().pending.remove(prompt_id);
    }

    pub(super) fn forget_asks_of_task(&self, task_id: &str) {
        self.lock()
            .pending
            .retain(|_, ask| ask.task_id.as_deref() != Some(task_id));
    }

    /// Forget the asks a turn's own engine raised; a background task's stay.
    pub(super) fn retain_task_asks(&self) {
        self.lock().pending.retain(|_, ask| ask.task_id.is_some());
    }

    #[cfg(test)]
    pub(super) fn pending_prompt_ids(&self) -> Vec<String> {
        self.lock().pending.keys().cloned().collect()
    }

    /// Take over `other`'s watchers and open asks — a session rebuilt under
    /// a hub of its own while the old one was still in place.
    pub(super) fn absorb(&self, other: &SessionHub) {
        let taken = std::mem::take(&mut *other.lock());
        let mut inner = self.lock();
        for watcher in taken.watchers {
            let id = watcher.id();
            if !inner.watchers.iter().any(|w| w.id() == id) {
                inner.watchers.push(watcher);
            }
        }
        for (prompt_id, ask) in taken.pending {
            inner.pending.entry(prompt_id).or_insert(ask);
        }
    }

    /// Back to watching nothing and asking nothing.
    pub(super) fn clear(&self) {
        *self.lock() = Inner::default();
    }
}

fn deliver(inner: &mut Inner, frame: &SessionFrame, except: Option<WatcherId>) {
    inner
        .watchers
        .retain(|w| Some(w.id()) == except || w.deliver(frame));
}
