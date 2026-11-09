//! # SkillWatcher — watches skill directories for file changes.
//!
//! Uses the `notify` crate to monitor SKILL.md / *.md files in skill directories.
//! When a skill file is modified, added, or removed, the path is collected.
//! Callers poll via [`SkillWatcher::check_and_reload`] to pick up changes.

use crate::manager::SkillManager;
use notify::event::Event;
use notify::{recommended_watcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use tracing;

/// Append-only log of changed-skill-file events, tagged with the generation
/// they arrived at. Lets multiple independent readers (one per
/// [`SkillManager`] sharing this watcher — see
/// [`SkillWatcher::check_and_reload_since`]) each track their own read
/// cursor without racing to drain a single queue, the way one shared
/// `Vec`-backed queue would. In practice this stays small: skill file edits
/// are infrequent, human-driven events, not a high-volume stream.
struct ChangeLog {
    generation: u64,
    entries: Vec<(u64, PathBuf)>,
}

/// Watches skill directories for SKILL.md / *.md file changes.
///
/// Runs a background thread that collects file-system events into a shared,
/// generation-tagged log. Call [`check_and_reload`] (single-consumer) or
/// [`check_and_reload_since`] (multi-consumer, e.g. one `SkillWatcher`
/// shared by every session in a daemon's `SessionPool` instead of each
/// session starting its own `notify` watcher thread) periodically — e.g. at
/// the start of each turn — to apply pending reloads.
pub struct SkillWatcher {
    /// The notify watcher (kept alive so events keep flowing).
    #[allow(dead_code)]
    watcher_handle: Option<notify::RecommendedWatcher>,
    log: Arc<Mutex<ChangeLog>>,
}

impl SkillWatcher {
    pub fn new() -> Self {
        Self {
            watcher_handle: None,
            log: Arc::new(Mutex::new(ChangeLog {
                generation: 0,
                entries: Vec::new(),
            })),
        }
    }

    /// The log's current generation — callers that start sharing this
    /// watcher after it's already running (e.g. a new session attaching to
    /// a pool-level watcher) should initialize their read cursor to this
    /// value, not `0`: their `SkillManager` was just built from a fresh disk
    /// scan, so it doesn't need to replay history that predates it.
    pub fn current_generation(&self) -> u64 {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .generation
    }

    /// Start watching the given directories for skill-file changes.
    ///
    /// Only files named `SKILL.md` or ending in `.md` are tracked.
    /// Watches **recursively** — `skills/` directories with subdirectories
    /// like `skills/my-skill/SKILL.md` are fully covered.
    ///
    /// Returns an error if underlying notify setup fails (permissions, kernel
    /// limits, etc.).
    pub fn watch_skills(&mut self, paths: &[PathBuf]) -> Result<(), String> {
        let log = self.log.clone();
        let (tx, rx) = std::sync::mpsc::channel();

        let mut w = recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        })
        .map_err(|e| format!("notify watcher creation failed: {e}"))?;

        for path in paths {
            if path.exists() {
                w.watch(path, RecursiveMode::Recursive)
                    .map_err(|e| format!("failed to watch '{}': {e}", path.display()))?;
            } else {
                tracing::warn!(?path, "Skill watch path does not exist, skipping");
            }
        }

        // Background thread: dequeue events and append them to the shared log.
        std::thread::Builder::new()
            .name("skill-watcher".into())
            .spawn(move || {
                drain_events(rx, &log);
            })
            .expect("skill-watcher thread");

        self.watcher_handle = Some(w);
        Ok(())
    }

    /// Consume all changed skill-file paths logged so far and reload the
    /// corresponding skills in the given [`SkillManager`].
    ///
    /// Single-consumer: assumes this `SkillWatcher` isn't shared with any
    /// other reader. For a watcher shared across multiple `SkillManager`s,
    /// use [`check_and_reload_since`](Self::check_and_reload_since) instead
    /// — this method's own cursor is implicit (whatever hasn't been consumed
    /// yet), which is exactly what a second concurrent caller here would
    /// race to also consume, silently missing events.
    ///
    /// Returns the number of skills successfully reloaded.
    pub fn check_and_reload(&self, manager: &SkillManager) -> usize {
        self.check_and_reload_since(manager, &AtomicU64::new(0))
    }

    /// Reload every skill changed since `last_seen`'s generation, then
    /// advance `last_seen` to the log's current generation.
    ///
    /// Safe for multiple independent callers (e.g. one `SkillManager` per
    /// session, all sharing one pool-level `SkillWatcher`) each holding
    /// their own `last_seen` cursor — unlike a drain-based queue, reading
    /// here never consumes entries out from under another reader.
    pub fn check_and_reload_since(&self, manager: &SkillManager, last_seen: &AtomicU64) -> usize {
        let (paths, new_generation) = {
            let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
            let since = last_seen.load(Ordering::Acquire);
            if since >= log.generation {
                return 0;
            }
            let mut seen = std::collections::HashSet::new();
            let paths: Vec<PathBuf> = log
                .entries
                .iter()
                .filter(|(gen, p)| *gen > since && seen.insert(p))
                .map(|(_, p)| p.clone())
                .collect();
            (paths, log.generation)
        };
        if paths.is_empty() {
            last_seen.store(new_generation, Ordering::Release);
            return 0;
        }

        let mut count = 0;
        for path in &paths {
            match manager.reload_skill(path) {
                Ok(()) => count += 1,
                Err(e) => tracing::warn!(?path, error = %e, "Failed to reload skill"),
            }
        }
        last_seen.store(new_generation, Ordering::Release);
        count
    }
}

impl Default for SkillWatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Process notify events in a loop on a background thread.
///
/// Filters for SKILL.md / *.md file changes and appends them to the shared,
/// generation-tagged log for [`SkillWatcher::check_and_reload`] /
/// [`SkillWatcher::check_and_reload_since`] to later consume.
fn drain_events(rx: Receiver<notify::Result<Event>>, log: &Arc<Mutex<ChangeLog>>) {
    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                let skill_changes: Vec<PathBuf> = event
                    .paths
                    .into_iter()
                    .filter(|p| is_skill_file(p))
                    .collect();

                if !skill_changes.is_empty() {
                    if let Ok(mut guard) = log.lock() {
                        guard.generation += 1;
                        let gen = guard.generation;
                        for p in skill_changes {
                            guard.entries.push((gen, p));
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Skill watcher notify error");
            }
            Err(_) => {
                // Channel closed — sender dropped, stop the thread.
                tracing::debug!("Skill watcher channel closed, exiting thread");
                break;
            }
        }
    }
}

/// Returns `true` if the file is a skill markdown file.
///
/// Matches:
/// - `SKILL.md` (subdirectory format: `skills/<name>/SKILL.md`)
/// - `*.md` (flat format: `<name>.md`)
fn is_skill_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "md")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_changed_several_times_is_reloaded_once() {
        let watcher = SkillWatcher::new();
        {
            let mut log = watcher.log.lock().unwrap();
            for (generation, name) in [(1, "a.md"), (2, "b.md"), (3, "a.md"), (4, "a.md")] {
                log.generation = generation;
                log.entries.push((generation, PathBuf::from(name)));
            }
        }
        assert_eq!(watcher.check_and_reload(&SkillManager::new()), 2);
    }
}
