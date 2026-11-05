//! # FileWatcher — watches filesystem paths and fires `HookEvent::FileChanged`.
//!
//! Uses the `notify` crate to monitor configured paths for file modifications,
//! creations, and deletions. Each change is debounced (300ms window) and then
//! fanned out as a `HookEvent::FileChanged` hook call on the [`HookRunner`].
//!
//! The hook payload (`tool_input`) contains:
//! - `file_path`: the absolute path of the changed file
//! - `change_type`: one of `"created"`, `"modified"`, `"deleted"`

use crate::config::HookEvent;
use crate::payload::HookInput;
use crate::runner::HookRunner;
use notify::event::{Event, EventKind};
use notify::{recommended_watcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing;

/// File system change watcher that fires `HookEvent::FileChanged` hooks.
///
/// Creates a background thread that receives notify events, debounces them
/// (300ms default), and dispatches `HookEvent::FileChanged` on the provided
/// [`HookRunner`] for each distinct change.
pub struct FileWatcher {
    /// Kept alive to keep file-system events flowing.
    #[allow(dead_code)]
    watcher_handle: Option<notify::RecommendedWatcher>,
}

impl FileWatcher {
    pub fn new() -> Self {
        Self {
            watcher_handle: None,
        }
    }

    /// Watch the given paths and fire `HookEvent::FileChanged` on the provided
    /// [`HookRunner`] when files are modified, created, or deleted.
    ///
    /// Each distinct file generates a hook call debounced to `debounce_ms`
    /// (300ms default) — rapid successive changes to the same file cause at
    /// most one hook invocation.
    ///
    /// Background thread captures the current Tokio runtime handle so that
    /// hook execution is dispatched asynchronously. If no Tokio runtime is
    /// active, the background thread will log a warning and skip hook dispatch.
    ///
    /// Returns an error if filesystem watching cannot be initialised.
    pub fn watch_paths(
        &mut self,
        paths: &[PathBuf],
        hook_runner: Arc<HookRunner>,
        debounce_ms: u64,
    ) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();

        let mut w = recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        })
        .map_err(|e| format!("notify watcher creation failed: {e}"))?;

        for path in paths {
            if path.exists() {
                w.watch(path, RecursiveMode::NonRecursive)
                    .map_err(|e| format!("failed to watch '{}': {e}", path.display()))?;
            } else {
                tracing::warn!(?path, "FileWatcher path does not exist, skipping");
            }
        }

        // Capture the current tokio runtime handle so we can spawn tasks from
        // the background thread.
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                tracing::warn!(
                    "FileWatcher: no Tokio runtime active — hook dispatch will be skipped"
                );
                self.watcher_handle = Some(w);
                return Ok(());
            }
        };

        std::thread::Builder::new()
            .name("file-watcher".into())
            .spawn(move || {
                run_watcher_loop(rx, &hook_runner, &handle, debounce_ms);
            })
            .map_err(|e| format!("could not start the file-watcher thread: {e}"))?;

        self.watcher_handle = Some(w);
        Ok(())
    }
}

impl Default for FileWatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Background loop that processes notify events and dispatches hook calls.
///
/// Implements a simple debounce: events are held in a pending map keyed by
/// file path. When the channel times out (no new events for `debounce_ms`),
/// all ready events (older than `debounce_ms`) are flushed and hooks fired.
fn run_watcher_loop(
    rx: Receiver<notify::Result<Event>>,
    hook_runner: &Arc<HookRunner>,
    handle: &tokio::runtime::Handle,
    debounce_ms: u64,
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

                for (path, change_type) in ready {
                    let input = HookInput {
                        hook_event_name: "FileChanged".into(),
                        session_id: String::new(),
                        cwd: std::env::current_dir()
                            .unwrap_or_default()
                            .display()
                            .to_string(),
                        permission_mode: "default".into(),
                        tool_input: Some(serde_json::json!({
                            "file_path": path.display().to_string(),
                            "change_type": &change_type,
                        })),
                        tool_name: Some("FileWatcher".into()),
                        tool_use_id: None,
                        tool_result: None,
                        is_error: None,
                        user_prompt: None,
                        ..Default::default()
                    };

                    let runner = hook_runner.clone();
                    handle.spawn(async move {
                        runner.run(HookEvent::FileChanged, &input).await;
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
