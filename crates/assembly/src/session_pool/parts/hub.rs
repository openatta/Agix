//! 每个会话自己的事件出口 `SessionHub`：谁在看它、它还在等谁回答、它最近一次
//! 有动静是什么时候。事件经它同步交给订阅者，不经过池级的 `sessions` 锁。
//!
//! 锁序里它是叶子：持着它不 `.await`、不去拿别的锁；`SessionWatcher::deliver`
//! 只做非阻塞的入队。

use crate::session_pool::api::{AskKind, PendingAsk, SessionFrame, SessionWatcher, WatcherId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(in crate::session_pool) struct SessionHub {
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
    pub(in crate::session_pool) fn active_at(at: Instant) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
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
        deliver(&mut self.lock(), frame, except);
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
        deliver(&mut inner, &frame, None);
    }

    /// The ask `prompt_id` is no longer open: forgotten and announced under
    /// the same lock, so a watcher that subscribes meanwhile is handed either
    /// the ask or the frame that ends it, never the ask alone.
    pub(in crate::session_pool) fn publish_resolved(&self, frame: &SessionFrame, prompt_id: &str) {
        self.touch();
        let mut inner = self.lock();
        inner.pending.remove(prompt_id);
        deliver(&mut inner, frame, None);
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
        *self.lock() = Inner::default();
    }
}

fn deliver(inner: &mut Inner, frame: &SessionFrame, except: Option<WatcherId>) {
    inner
        .watchers
        .retain(|w| Some(w.id()) == except || w.deliver(frame));
}
