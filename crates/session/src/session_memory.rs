//! SessionMemory — auto-maintenance of the `session_memory.md` sidecar file.
//!
//! Tracks extraction timestamps and staleness to prompt the model to update
//! its cross-session notes every N turns.
//!
//! The sidecar file lives at `{sessions_root}/{session_id}/session_memory.md`
//! and carries a YAML frontmatter block with metadata:
//!
//! ```markdown
//! ---
//! extraction_started: <utc-rfc3339>
//! extraction_completed: <utc-rfc3339>
//! last_update_turn: <u32>
//! ---
//! # Session Memory
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

const SESSION_MEMORY_HEADER: &str =
    "# Session Memory\n\nTrack persistent facts about the user, project, and workflow.\n";

/// Manages the session_memory.md sidecar file with timestamp and staleness tracking.
///
/// Wrapped in `Arc<Mutex<>>` when shared so that mark-extraction and staleness
/// checks are safe across concurrent access.
#[derive(Clone)]
pub struct SessionMemory {
    inner: Arc<SessionMemoryInner>,
}

struct SessionMemoryInner {
    /// Absolute path to the session_memory.md sidecar file.
    path: PathBuf,
    /// The turn number when session memory was last updated.
    last_update_turn: AtomicU32,
    /// Internal mutability for file I/O.
    io_lock: Mutex<()>,
}

impl SessionMemory {
    /// Create a new SessionMemory handle pointing at `path`.
    ///
    /// The file is **not** created here — call [`init_session_memory`] to
    /// write the initial header.
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: Arc::new(SessionMemoryInner {
                path,
                last_update_turn: AtomicU32::new(0),
                io_lock: Mutex::new(()),
            }),
        }
    }

    /// Create the `session_memory.md` file with a default header if it does
    /// not already exist. Safe to call multiple times.
    pub async fn init_session_memory(&self) -> std::io::Result<()> {
        let _guard = self.inner.io_lock.lock().await;
        let path = &self.inner.path;
        if path.try_exists().unwrap_or(false) {
            return Ok(()); // already exists
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let header = format!(
            "---\nextraction_started: ~\nextraction_completed: ~\nlast_update_turn: 0\n---\n\n{}",
            SESSION_MEMORY_HEADER
        );
        tokio::fs::write(path, &header).await?;
        Ok(())
    }

    /// Record that an extraction cycle has started by writing the current
    /// UTC timestamp into the frontmatter.
    pub async fn mark_extraction_started(&self) -> std::io::Result<()> {
        let _guard = self.inner.io_lock.lock().await;
        let now = iso_now();
        self.update_frontmatter_field("extraction_started", &now)
            .await?;
        Ok(())
    }

    /// Record that extraction completed. Updates the completion timestamp and
    /// the `last_update_turn` counter so staleness can be computed.
    pub async fn mark_extraction_completed(&self, turn_count: u32) -> std::io::Result<()> {
        let _guard = self.inner.io_lock.lock().await;
        let now = iso_now();
        self.update_frontmatter_field("extraction_completed", &now)
            .await?;
        self.inner
            .last_update_turn
            .store(turn_count, Ordering::Release);
        // Also persist turn_count into the frontmatter
        self.update_frontmatter_field("last_update_turn", &turn_count.to_string())
            .await?;
        Ok(())
    }

    /// Returns `true` if `current_turn - last_update_turn > 10`, meaning the
    /// session notes have not been refreshed recently enough.
    pub fn is_stale(&self, current_turn: u32) -> bool {
        let last = self.inner.last_update_turn.load(Ordering::Acquire);
        let threshold: u32 = 10;
        current_turn.saturating_sub(last) > threshold
    }

    /// The turn number at which session memory was last updated.
    pub fn last_update_turn(&self) -> u32 {
        self.inner.last_update_turn.load(Ordering::Acquire)
    }

    /// Path to the session_memory.md sidecar file.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Read the full content of the session_memory.md file, if it exists.
    pub async fn load_content(&self) -> std::io::Result<Option<String>> {
        let path = &self.inner.path;
        if !path.try_exists().unwrap_or(false) {
            return Ok(None);
        }
        let content = tokio::fs::read_to_string(path).await?;
        Ok(Some(content))
    }

    /// Update a single frontmatter field in-place, preserving the rest of the
    /// file content. The frontmatter is a YAML block delimited by `---`.
    async fn update_frontmatter_field(&self, key: &str, value: &str) -> std::io::Result<()> {
        let path = &self.inner.path;
        let content = tokio::fs::read_to_string(path).await.unwrap_or_default();

        let (frontmatter, body) = if let Some(rest) = content.strip_prefix("---") {
            if let Some((fm, rest)) = rest.split_once("\n---") {
                (fm.trim().to_string(), rest.trim_start().to_string())
            } else {
                (String::new(), content.clone())
            }
        } else {
            (String::new(), content.clone())
        };

        // Rebuild frontmatter: update matching line or append.
        let mut found = false;
        let mut new_fm = String::new();
        for line in frontmatter.lines() {
            if let Some(prefix) = line.split(':').next() {
                if prefix.trim() == key {
                    new_fm.push_str(&format!("{}: {}\n", key, value));
                    found = true;
                } else {
                    new_fm.push_str(line);
                    new_fm.push('\n');
                }
            } else {
                new_fm.push_str(line);
                new_fm.push('\n');
            }
        }
        if !found {
            new_fm.push_str(&format!("{}: {}\n", key, value));
        }

        let final_content = format!("---\n{}---\n\n{}", new_fm, body);
        tokio::fs::write(path, &final_content).await?;
        Ok(())
    }
}

fn iso_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sm(path: PathBuf) -> SessionMemory {
        SessionMemory::new(path)
    }

    #[tokio::test]
    async fn init_creates_file_with_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path.clone());
        sm.init_session_memory().await.unwrap();

        assert!(path.exists());
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("Session Memory"));
        assert!(content.contains("extraction_started"));
    }

    #[tokio::test]
    async fn init_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path.clone());
        sm.init_session_memory().await.unwrap();
        // Second call should not error
        sm.init_session_memory().await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn mark_extraction_updates_frontmatter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path.clone());
        sm.init_session_memory().await.unwrap();

        sm.mark_extraction_started().await.unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!content.contains("extraction_started: ~"));
        assert!(content.contains("extraction_started: "));

        // Skip the ~ check above — we just verify the field was updated
        sm.mark_extraction_completed(5).await.unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!content.contains("extraction_completed: ~"));
        assert!(content.contains("extraction_completed: "));
        assert!(content.contains("last_update_turn: 5"));
    }

    #[test]
    fn staleness_tracks_turn_delta() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path);
        assert!(sm.last_update_turn() == 0);
        // At turn 0 with last_update=0 → 0-0=0 ≤ 10 → not stale
        assert!(!sm.is_stale(0));
        // At turn 20 with last_update=0 → 20 > 10 → stale
        assert!(sm.is_stale(20));
    }

    #[tokio::test]
    async fn mark_completed_resets_staleness() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path);
        sm.init_session_memory().await.unwrap();

        sm.mark_extraction_completed(5).await.unwrap();
        assert!(!sm.is_stale(10)); // delta ≤ 10
        assert!(!sm.is_stale(15)); // delta = 10, not > 10
        assert!(sm.is_stale(16)); // delta = 11, > 10
    }

    #[tokio::test]
    async fn load_content_returns_none_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path);
        let content = sm.load_content().await.unwrap();
        assert!(content.is_none());
    }

    #[tokio::test]
    async fn load_content_returns_file_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path.clone());
        sm.init_session_memory().await.unwrap();

        let content = sm.load_content().await.unwrap();
        assert!(content.is_some());
        assert!(content.unwrap().contains("Session Memory"));
    }
}
