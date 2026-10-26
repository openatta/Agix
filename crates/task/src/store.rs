//! File-persisted task list.
//!
//! Stores individual task files at `<scene root>/tasks/{list_id}/{id}.json`,
//! where the scene root is given by the caller.
//! Single-process tokio Mutex guards concurrent access within the same
//! process; the file layout is compatible with future cross-process locking
//! (flock / proper-lockfile).
//!
//! Tasks are stored as serde_json::Value to avoid circular crate dependencies
//! (the typed TaskEntry/TaskStatus live in agix-tools). Callers serialize
//! at the boundary.

use std::path::{Path, PathBuf};

/// Base directory for all task lists, under the scene root this instance was
/// given. Nothing here decides where that root is.
fn base_dir(scene_root: &Path) -> PathBuf {
    scene_root.join("tasks")
}

/// Sanitise a string for safe filesystem use.
pub fn sanitise_path(s: &str) -> String {
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

/// Result of claiming a task.
#[derive(Debug)]
pub enum ClaimResult {
    /// Successfully claimed by the agent.
    Ok(serde_json::Value),
    /// Task not found.
    NotFound,
    /// Task already claimed by another agent.
    AlreadyClaimed(String),
    /// Task is already completed / cancelled / deleted.
    AlreadyDone,
    /// Task has unresolved blockers.
    Blocked(Vec<String>),
}

/// A file-backed task store, guarded by an in-process Mutex.
///
/// ## Thread safety
/// All operations acquire the inner Mutex, so concurrent readers/writers
/// within the same process are serialised. The file layout (`{list}/.lock`)
/// is reserved for future cross-process locking.
#[derive(Debug)]
pub struct TaskStore {
    dir: PathBuf,
    lock: tokio::sync::Mutex<()>,
}

impl TaskStore {
    /// Create or open a task list. Creates the directory if needed.
    pub async fn new(list_id: &str, scene_root: &Path) -> std::io::Result<Self> {
        let dir = base_dir(scene_root).join(sanitise_path(list_id));
        tokio::fs::create_dir_all(&dir).await?;
        Ok(Self {
            dir,
            lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Resolve the task list ID from env var, falling back to `fallback`.
    pub fn list_id_from_env(fallback: &str) -> String {
        std::env::var("AGIX_TASK_LIST_ID").unwrap_or_else(|_| fallback.to_string())
    }

    fn task_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}.json", sanitise_path(id)))
    }

    /// Create a new task (serialised as serde_json::Value), return its id.
    pub async fn create(&self, task: serde_json::Value) -> std::io::Result<String> {
        let _guard = self.lock.lock().await;
        let id = task
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("<no-id>")
            .to_string();
        let path = self.task_path(&id);
        let bytes = serde_json::to_vec_pretty(&task)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(&path, bytes).await?;
        Ok(id)
    }

    /// Retrieve a task by id. Returns None if not found.
    pub async fn get(&self, id: &str) -> std::io::Result<Option<serde_json::Value>> {
        let path = self.task_path(id);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let v: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                Ok(Some(v))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Update a task by applying a mutation closure to its deserialised form.
    /// Returns the updated task or None if not found.
    pub async fn update<F>(&self, id: &str, mutate: F) -> std::io::Result<Option<serde_json::Value>>
    where
        F: FnOnce(&mut serde_json::Value),
    {
        let _guard = self.lock.lock().await;
        let path = self.task_path(id);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut task: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        mutate(&mut task);
        let bytes = serde_json::to_vec_pretty(&task)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(&path, bytes).await?;
        Ok(Some(task))
    }

    /// Delete a task file. Returns true if deleted, false if not found.
    pub async fn delete(&self, id: &str) -> std::io::Result<bool> {
        let _guard = self.lock.lock().await;
        let path = self.task_path(id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// List all tasks in the list. Skips files that fail to parse.
    pub async fn list(&self) -> std::io::Result<Vec<serde_json::Value>> {
        let _guard = self.lock.lock().await;
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        let mut tasks = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // skip dotfiles (.lock, .highwatermark)
            if path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with('.'))
            {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                        tasks.push(v);
                    }
                }
                Err(_) => continue,
            }
        }
        Ok(tasks)
    }

    /// Claim a task for an agent. Checks owner conflict, resolved status, and
    /// unresolved blocked_by dependencies (tasks not in 'completed' status block).
    /// Returns the claimed task on success.
    pub async fn claim_task(&self, id: &str, claimant: &str) -> std::io::Result<ClaimResult> {
        let _guard = self.lock.lock().await;
        let path = self.task_path(id);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ClaimResult::NotFound),
            Err(e) => return Err(e),
        };
        let mut task: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // Check already claimed
        let current_owner = task.get("owner").and_then(|v| v.as_str());
        if let Some(o) = current_owner {
            if o != claimant {
                return Ok(ClaimResult::AlreadyClaimed(o.to_string()));
            }
        }

        // Check already resolved
        let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status == "completed" || status == "cancelled" || status == "deleted" {
            return Ok(ClaimResult::AlreadyDone);
        }

        // Check blocked_by: any non-completed blocking tasks?
        let blocked_by: Vec<String> = task
            .get("blocked_by")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        if !blocked_by.is_empty() {
            // Read all tasks to find which blockers are still open
            let all_tasks = self.list_internal().await?;
            let unresolved: Vec<String> = blocked_by
                .into_iter()
                .filter(|bid| {
                    all_tasks.iter().any(|t| {
                        t.get("id").and_then(|i| i.as_str()) == Some(bid)
                            && t.get("status").and_then(|s| s.as_str()) != Some("completed")
                    })
                })
                .collect();
            if !unresolved.is_empty() {
                return Ok(ClaimResult::Blocked(unresolved));
            }
        }

        // Claim it
        task["owner"] = serde_json::Value::String(claimant.to_string());
        let bytes = serde_json::to_vec_pretty(&task)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(&path, bytes).await?;
        Ok(ClaimResult::Ok(task))
    }

    /// Release a task (clear owner, reset status to pending).
    pub async fn release_task(&self, id: &str) -> std::io::Result<bool> {
        let _guard = self.lock.lock().await;
        let path = self.task_path(id);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mut task: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        task["owner"] = serde_json::Value::Null;
        task["status"] = serde_json::Value::String("pending".into());
        let bytes = serde_json::to_vec_pretty(&task)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(&path, bytes).await?;
        Ok(true)
    }

    /// List tasks without acquiring the guard (caller already holds the lock).
    async fn list_internal(&self) -> std::io::Result<Vec<serde_json::Value>> {
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        let mut tasks = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with('.'))
            {
                continue;
            }
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                    tasks.push(v);
                }
            }
        }
        Ok(tasks)
    }

    /// Reserve the next numeric ID using a high-water-mark file
    /// for cross-process safety.
    pub async fn next_id(&self) -> std::io::Result<String> {
        let _guard = self.lock.lock().await;
        let hwm_path = self.dir.join(".highwatermark");
        let current: u64 = match tokio::fs::read_to_string(&hwm_path).await {
            Ok(s) => s.trim().parse().unwrap_or(0),
            Err(_) => 0,
        };
        let next = current + 1;
        tokio::fs::write(&hwm_path, next.to_string()).await?;
        Ok(next.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_CTR: AtomicU64 = AtomicU64::new(0);
    async fn test_store() -> TaskStore {
        let n = TEST_CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("agix-task-test-{n}"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        TaskStore {
            dir,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    #[tokio::test]
    async fn create_and_get() {
        let store = test_store().await;
        let task =
            json!({"id": "1", "subject": "test", "description": "desc", "status": "pending"});
        let id = store.create(task.clone()).await.unwrap();
        assert_eq!(id, "1");
        let fetched = store.get("1").await.unwrap().unwrap();
        assert_eq!(fetched["subject"], "test");
    }

    #[tokio::test]
    async fn get_missing() {
        let store = test_store().await;
        assert!(store.get("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_works() {
        let store = test_store().await;
        let task = json!({"id": "1", "subject": "old", "status": "pending"});
        store.create(task).await.unwrap();
        let updated = store
            .update("1", |t| {
                t["status"] = json!("completed");
                t["subject"] = json!("new");
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated["status"], "completed");
        assert_eq!(updated["subject"], "new");
    }

    #[tokio::test]
    async fn update_missing_returns_none() {
        let store = test_store().await;
        let r = store.update("missing", |_| {}).await.unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn delete_works() {
        let store = test_store().await;
        store
            .create(json!({"id": "1", "subject": "x"}))
            .await
            .unwrap();
        assert!(store.delete("1").await.unwrap());
        assert!(store.get("1").await.unwrap().is_none());
        // second delete returns false (already gone)
        assert!(!store.delete("1").await.unwrap());
    }

    #[tokio::test]
    async fn list_returns_all() {
        let store = test_store().await;
        for i in 0..3 {
            store
                .create(json!({"id": format!("{i}"), "subject": format!("task {i}")}))
                .await
                .unwrap();
        }
        let tasks = store.list().await.unwrap();
        assert_eq!(tasks.len(), 3);
    }

    #[tokio::test]
    async fn next_id_increments() {
        let store = test_store().await;
        let id1 = store.next_id().await.unwrap();
        let id2 = store.next_id().await.unwrap();
        assert_eq!(id1, "1");
        assert_eq!(id2, "2");
    }

    #[tokio::test]
    async fn list_id_from_env() {
        std::env::set_var("AGIX_TASK_LIST_ID", "my-list");
        assert_eq!(TaskStore::list_id_from_env("fallback"), "my-list");
        std::env::remove_var("AGIX_TASK_LIST_ID");
        assert_eq!(TaskStore::list_id_from_env("fallback"), "fallback");
    }
}
