//! # FileWatcher — watches filesystem paths and fires `HookEvent::FileChanged`.
//!
//! Uses the `notify` crate to monitor configured paths for file modifications,
//! creations, and deletions. Each change is debounced (300ms window) and then
//! fanned out as a `HookEvent::FileChanged` hook call on the [`HookRunner`].
//!
//! The hook payload carries the session's ambient fields, and in `tool_input`:
//! - `file_path`: the absolute path of the changed file
//! - `change_type`: one of `"created"`, `"modified"`, `"deleted"`
//!
//! The watcher thread holds the runner weakly: the runner owns the watcher,
//! so a strong reference would keep both alive after the session is gone.

use crate::config::HookEvent;
use crate::payload::{Ambient, HookInput};
use crate::runner::HookRunner;
use notify::event::{Event, EventKind};
use notify::{recommended_watcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// File system change watcher that fires `HookEvent::FileChanged` hooks.
///
/// Creates a background thread that receives notify events, debounces them
/// (300ms default), and dispatches `HookEvent::FileChanged` on the provided
/// [`HookRunner`] for each distinct change.
pub struct FileWatcher {
    watcher: notify::RecommendedWatcher,
    watched: HashSet<PathBuf>,
    /// Stops the hooks it started when it goes, with the runner that owns it.
    stopped: CancellationToken,
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        self.stopped.cancel();
    }
}

impl FileWatcher {
    /// Start watching nothing yet; [`add`](Self::add) names the files.
    ///
    /// Background thread captures the current Tokio runtime handle so that
    /// hook execution is dispatched asynchronously. If no Tokio runtime is
    /// active, it logs a warning and dispatches nothing.
    pub fn start(
        hook_runner: &Arc<HookRunner>,
        debounce_ms: u64,
        ambient: Ambient,
    ) -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        })
        .map_err(|e| format!("notify watcher creation failed: {e}"))?;
        let stopped = CancellationToken::new();

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let runner = Arc::downgrade(hook_runner);
                let stop = stopped.clone();
                std::thread::Builder::new()
                    .name("file-watcher".into())
                    .spawn(move || run_watcher_loop(rx, runner, handle, debounce_ms, ambient, stop))
                    .map_err(|e| format!("could not start the file-watcher thread: {e}"))?;
            }
            Err(_) => {
                tracing::warn!(
                    "FileWatcher: no Tokio runtime active — hook dispatch will be skipped"
                );
            }
        }
        Ok(Self {
            watcher,
            watched: HashSet::new(),
            stopped,
        })
    }

    /// Watch `paths` too. A path already watched, or one that does not exist,
    /// is skipped.
    pub fn add(&mut self, paths: &[PathBuf]) -> Result<(), String> {
        for path in paths {
            if self.watched.contains(path) {
                continue;
            }
            if !path.exists() {
                tracing::warn!(?path, "FileWatcher path does not exist, skipping");
                continue;
            }
            self.watcher
                .watch(path, RecursiveMode::NonRecursive)
                .map_err(|e| format!("failed to watch '{}': {e}", path.display()))?;
            self.watched.insert(path.clone());
        }
        Ok(())
    }
}

/// Background loop that processes notify events and dispatches hook calls.
///
/// Implements a simple debounce: events are held in a pending map keyed by
/// file path. When the channel times out (no new events for `debounce_ms`),
/// all ready events (older than `debounce_ms`) are flushed and hooks fired.
fn run_watcher_loop(
    rx: Receiver<notify::Result<Event>>,
    hook_runner: Weak<HookRunner>,
    handle: tokio::runtime::Handle,
    debounce_ms: u64,
    ambient: Ambient,
    stop: CancellationToken,
) {
    let debounce = Duration::from_millis(debounce_ms);
    // Use the debounce duration as the channel timeout — if nothing new arrives
    // for one debounce window, flush all pending entries that are old enough.
    let mut pending: HashMap<PathBuf, (Instant, String)> = HashMap::new();

    loop {
        match rx.recv_timeout(debounce) {
            Ok(Ok(event)) => {
                let (paths, change_type) = extract_event_info(&event);
                for path in paths {
                    // Update the pending entry — resets its timer.
                    pending.insert(path, (Instant::now(), change_type.clone()));
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "FileWatcher notify error");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Channel idle for the debounce window — flush all ready events.
                let now = Instant::now();
                let ready: Vec<(PathBuf, String)> = pending
                    .drain()
                    .filter(|(_, (time, _))| now.duration_since(*time) >= debounce)
                    .map(|(path, (_, change_type))| (path, change_type))
                    .collect();
                if ready.is_empty() {
                    continue;
                }
                let Some(runner) = hook_runner.upgrade() else {
                    break;
                };
                for (path, change_type) in ready {
                    let input = HookInput::from_ambient(&ambient, "FileChanged").with_tool(
                        "FileWatcher",
                        serde_json::json!({
                            "file_path": path.display().to_string(),
                            "change_type": &change_type,
                        }),
                    );
                    let runner = runner.clone();
                    let stop = stop.clone();
                    handle.spawn(async move {
                        runner.run(HookEvent::FileChanged, &input, &stop).await;
                    });
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::debug!("FileWatcher channel closed, exiting thread");
                break;
            }
        }
    }
}

/// Extract the relevant path(s) and a human-readable change type from a notify
/// [`Event`].
fn extract_event_info(event: &Event) -> (Vec<PathBuf>, String) {
    let change_type = match &event.kind {
        EventKind::Create(_) => "created",
        EventKind::Modify(_) => "modified",
        EventKind::Remove(_) => "deleted",
        _ => "other",
    };
    (event.paths.clone(), change_type.to_string())
}
