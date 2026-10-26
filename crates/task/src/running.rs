//! File-persisted running background-task state.
//!
//! Stores individual task files at `<scene root>/running/{task_id}.json`,
//! where the scene root is given by the caller.
//! Each file holds a snapshot of the task's output, events_log, and status.
//! Written asynchronously (fire-and-forget) on each state transition.
//!
//! ## Crash recovery
//! On process restart, `scan_and_mark_stale()` reads any surviving
//! `*.json` files and returns them with status set to `Failed("process restarted")`.
//! The caller (engine startup) injects these into `SessionState.running_tasks`
//! so `TaskOutput` can report "task was lost in a restart" rather than silently
//! returning "not found".

use base::context::RunningStatus;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningTaskData {
    pub task_id: String,
    pub output: String,
    pub events_log: Vec<String>,
    pub status: RunningStatus,
    pub created_at: i64,
    pub updated_at: i64,
    /// True if this task was running in the background (as opposed to foreground).
    /// Recovered tasks from crash recovery always have this set to true.
    #[serde(default)]
    pub is_backgrounded: bool,
}

impl RunningTaskData {
    /// Returns `true` if this task was/is running in the background.
    pub fn is_background_task(&self) -> bool {
        self.is_backgrounded
    }
}

fn base_dir(scene_root: &Path) -> PathBuf {
    scene_root.join("running")
}

fn sanitise_path(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// File-backed running-task store, sharing the same directory layout as
/// `TaskStore` but maintaining separate files under `~/.agix/code/running/`.
///
/// All operations are async; callers typically fire-and-forget via `tokio::spawn`.
#[derive(Debug)]
pub struct RunningTaskStore;

impl RunningTaskStore {
    /// Persist a running task's current state to disk.
    /// Creates `<scene root>/running/` if needed.
    pub async fn save(
        &self,
        task_id: &str,
        output: &str,
        events_log: &[String],
        status: &RunningStatus,
        scene_root: &Path,
    ) -> std::io::Result<()> {
        let dir = base_dir(scene_root);
        tokio::fs::create_dir_all(&dir).await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let data = RunningTaskData {
            task_id: task_id.to_string(),
            output: output.to_string(),
            events_log: events_log.to_vec(),
            status: status.clone(),
            created_at: now,
            updated_at: now,
            is_backgrounded: true,
        };
        let bytes = serde_json::to_vec_pretty(&data)?;
        let path = base_dir(scene_root).join(format!("{}.json", sanitise_path(task_id)));
        tokio::fs::write(&path, &bytes).await?;
        Ok(())
    }

    /// Remove a persisted running task file. Returns true if it existed.
    pub async fn remove(&self, task_id: &str, scene_root: &Path) -> bool {
        let path = base_dir(scene_root).join(format!("{}.json", sanitise_path(task_id)));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => false,
        }
    }

    /// Load a single running task from disk. Returns `None` silently on error.
    pub async fn load(&self, task_id: &str, scene_root: &Path) -> Option<RunningTaskData> {
        let path = base_dir(scene_root).join(format!("{}.json", sanitise_path(task_id)));
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Scan `<scene root>/running/` for all persisted task files and return them
    /// with their status overridden to `Failed("process restarted")`.
    ///
    /// Call this on engine startup to detect tasks orphaned by a crash.
    pub fn scan_and_mark_stale(&self, scene_root: &Path) -> std::io::Result<Vec<RunningTaskData>> {
        let dir = base_dir(scene_root);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut tasks = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if let Ok(mut data) = serde_json::from_str::<RunningTaskData>(&content) {
                data.status = RunningStatus::Failed("process restarted".into());
                data.is_backgrounded = true;
                tasks.push(data);
            }
        }
        Ok(tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One scene root per test.
    ///
    /// These used to be a single `#[test]` walking a global `TEST_BASE`
    /// mutex through six sections, because the directory was process-global
    /// and two tests running at once would have stepped on each other.
    /// Passing the root in is what makes them ordinary independent tests.
    fn scene_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn a_saved_task_reads_back() {
        let root = scene_root();
        RunningTaskStore
            .save(
                "task-test-1",
                "output text",
                &["→ Bash".into(), "  ✓ (result)".into()],
                &RunningStatus::Running,
                root.path(),
            )
            .await
            .unwrap();

        let loaded = RunningTaskStore
            .load("task-test-1", root.path())
            .await
            .unwrap();
        assert_eq!(loaded.task_id, "task-test-1");
        assert_eq!(loaded.output, "output text");
        assert_eq!(loaded.events_log, vec!["→ Bash", "  ✓ (result)"]);
        assert_eq!(loaded.status, RunningStatus::Running);
    }

    #[tokio::test]
    async fn loading_a_task_that_was_never_saved_is_none() {
        let root = scene_root();
        assert!(RunningTaskStore
            .load("nonexistent", root.path())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn removing_twice_reports_the_second_as_a_no_op() {
        let root = scene_root();
        RunningTaskStore
            .save(
                "task-remove-test",
                "",
                &[],
                &RunningStatus::Running,
                root.path(),
            )
            .await
            .unwrap();
        assert!(
            RunningTaskStore
                .remove("task-remove-test", root.path())
                .await
        );
        assert!(RunningTaskStore
            .load("task-remove-test", root.path())
            .await
            .is_none());
        assert!(
            !RunningTaskStore
                .remove("task-remove-test", root.path())
                .await
        );
    }

    /// What survives a restart is a task file with no process behind it, so
    /// the scan reports them as failed rather than still running.
    #[tokio::test]
    async fn tasks_left_on_disk_come_back_marked_failed() {
        let root = scene_root();
        for (id, output, event) in [
            ("task-stale-1", "partial", "→ Bash"),
            ("task-stale-2", "more", "→ Read"),
        ] {
            RunningTaskStore
                .save(
                    id,
                    output,
                    &[event.into()],
                    &RunningStatus::Running,
                    root.path(),
                )
                .await
                .unwrap();
        }

        let stale = RunningTaskStore.scan_and_mark_stale(root.path()).unwrap();
        assert_eq!(stale.len(), 2);
        for task in &stale {
            match &task.status {
                RunningStatus::Failed(msg) => assert!(msg.contains("restarted")),
                other => panic!("expected Failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn scanning_a_root_with_nothing_in_it_finds_nothing() {
        let root = scene_root();
        assert!(RunningTaskStore
            .scan_and_mark_stale(root.path())
            .unwrap()
            .is_empty());
    }

    /// A task id is not a path component until it has been sanitised.
    #[tokio::test]
    async fn a_traversal_shaped_id_round_trips_without_escaping_the_root() {
        let root = scene_root();
        RunningTaskStore
            .save(
                "../../evil",
                "data",
                &[],
                &RunningStatus::Running,
                root.path(),
            )
            .await
            .unwrap();
        assert!(RunningTaskStore
            .load("../../evil", root.path())
            .await
            .is_some());

        let escaped = root
            .path()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("evil.json");
        assert!(!escaped.exists(), "the id escaped the root");
    }
}
