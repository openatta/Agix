//! What one session did with the plugin, counted where it happens and
//! reported through `health_check` — which a daemon shows in `session.get`
//! under `health` — and through the `demo-rust.activity` prompt block.
//! `on_turn_start` is what counts the turns.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base::interface::health::{CheckResult, HealthCheck};
use runtime::plugin::TurnStart;

pub const CHECK: &str = "demo-rust.activity";

#[derive(Debug, Default)]
pub struct Activity {
    pub turns: AtomicU64,
    pub stamps: AtomicU64,
    pub refused: AtomicU64,
    pub redacted: AtomicU64,
    pub replies: AtomicU64,
    notice_error: Mutex<Option<String>>,
}

impl Activity {
    pub fn turn_counter(self: &Arc<Self>) -> TurnStart {
        let activity = self.clone();
        Arc::new(move || {
            activity.turns.fetch_add(1, Ordering::Relaxed);
        })
    }

    pub fn count(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_notice_error(&self, error: Option<String>) {
        *self.notice_error.lock().unwrap_or_else(|e| e.into_inner()) = error;
    }

    fn notice_error(&self) -> Option<String> {
        self.notice_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "turns": self.turns.load(Ordering::Relaxed),
            "stamps": self.stamps.load(Ordering::Relaxed),
            "refused": self.refused.load(Ordering::Relaxed),
            "redacted": self.redacted.load(Ordering::Relaxed),
            "replies": self.replies.load(Ordering::Relaxed),
        })
    }
}

pub struct ActivityCheck {
    activity: Arc<Activity>,
}

impl ActivityCheck {
    pub fn new(activity: Arc<Activity>) -> Self {
        Self { activity }
    }
}

impl HealthCheck for ActivityCheck {
    fn name(&self) -> &str {
        CHECK
    }

    fn check(&self) -> CheckResult {
        let details = self.activity.snapshot();
        match self.activity.notice_error() {
            Some(error) => {
                CheckResult::degraded(format!(".demo/notice.md could not be read: {error}"))
            }
            None => CheckResult::ok("working"),
        }
        .with_details(details)
    }
}
