//! `BackgroundHooks`：只通知事件的 hook 在后台跑的那些任务，以及宿主关停时等它们、
//! 等不到就叫停它们的令牌。宿主建一份交给它的每个 `HookRunner`，关停时 `drain`。

use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// How long the hooks stopped by [`BackgroundHooks::drain`] get to go: a
/// command hook's group has `SIGTERM`, then `SIGKILL` half a second later.
const STOP_GRACE: Duration = Duration::from_secs(1);

/// The hooks of events whose answer is ignored, run apart from the call that
/// fired them, which may stop waiting for them. Clones share the tasks.
#[derive(Clone, Default)]
pub struct BackgroundHooks {
    tasks: TaskTracker,
    shutdown: CancellationToken,
}

impl BackgroundHooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub(super) fn spawn<F>(&self, task: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tasks.spawn(task)
    }

    /// What stops these hooks: cancelled only by [`Self::drain`].
    pub(super) fn shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    /// Wait for the hooks running now, and any started meanwhile, until
    /// `deadline`; then stop those still running — a command hook's process
    /// group gets `SIGTERM`, then `SIGKILL` — and give them a moment to go.
    /// Each one stopped is logged.
    pub async fn drain(&self, deadline: tokio::time::Instant) {
        self.tasks.close();
        if tokio::time::timeout_at(deadline, self.tasks.wait())
            .await
            .is_ok()
        {
            return;
        }
        self.shutdown.cancel();
        let _ = tokio::time::timeout(STOP_GRACE, self.tasks.wait()).await;
    }
}
