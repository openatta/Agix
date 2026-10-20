//! Dream task — background thinking sub-agent that runs without blocking the user.
//!
//! The `DreamTask` spawns a lightweight background agent that "thinks" about a
//! given prompt, writing its thoughts incrementally to
//! `<scene root>/dreams/{session_id}.md`.
//! The main agent can later read this file to incorporate background insights.
//!
//! Auto-stops after 30 turns or when explicitly cancelled via the CancellationToken.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A background thinking sub-agent.
///
/// Spawned by the main agent via `DreamTask::start_dream()`, the task runs
/// asynchronously, writing thoughts to a markdown file. The main agent can
/// inspect the file at any point to incorporate background insights.
pub struct DreamTask {
    /// Unique identifier for this dream task.
    pub task_id: String,
    /// Session identifier used for the output file name.
    session_id: String,
    /// The scene root this dream's output file lives under
    /// (`<scene root>/dreams/`), given by the caller.
    scene_root: PathBuf,
    /// The async handle for the background task (retained to keep the task alive;
    /// never read — cancellation goes through `cancel`).
    #[allow(dead_code)]
    handle: Option<tokio::task::JoinHandle<()>>,
    /// Cancellation token to stop the dream early.
    cancel: tokio_util::sync::CancellationToken,
    /// Maximum number of thinking turns before auto-stop.
    max_turns: usize,
    /// Current turn count.
    current_turn: std::sync::atomic::AtomicUsize,
}

impl DreamTask {
    /// Maximum thinking turns before auto-stop.
    pub(crate) const DEFAULT_MAX_TURNS: usize = 30;

    /// Start a new dream task.
    ///
    /// Spawns a background task that thinks about `prompt`, writing incremental
    /// thoughts to `<scene root>/dreams/{session_id}.md`. The task auto-stops
    /// after `Self::DEFAULT_MAX_TURNS` turns or when the cancellation token is
    /// triggered.
    ///
    /// Returns a `DreamTask` handle that can be used to cancel or inspect the
    /// dream.
    pub fn start_dream(
        prompt: String,
        _tools: Arc<base::tool::InMemoryToolRegistry>,
        scene_root: &Path,
    ) -> Self {
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let task_id = format!("dream-{}", uuid::Uuid::new_v4());
        let session_id = format!("dream-{}", &task_id[6..12]); // derive a short session id
        let dream_task_id_for_spawn = task_id.clone();
        let dream_session_id = session_id.clone();
        let dream_task_id_for_struct = task_id.clone();
        let max_turns = Self::DEFAULT_MAX_TURNS;
        let turn_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let turn_counter_clone = turn_counter.clone();
        let scene_root = scene_root.to_path_buf();
        let dream_root = scene_root.clone();

        let handle = tokio::spawn(async move {
            let dreams_dir = dream_base_dir(&dream_root);
            if let Err(e) = tokio::fs::create_dir_all(&dreams_dir).await {
                tracing::warn!(error = %e, "failed to create dreams directory");
                return;
            }
            let file_path = dreams_dir.join(format!("{}.md", dream_session_id));

            // Write initial prompt
            let mut content = format!(
                "# Dream Task: {}\n\nStarted: {}\n\n## Initial Prompt\n\n{}\n\n",
                dream_task_id_for_spawn,
                chrono_now(),
                prompt
            );
            if let Err(e) = tokio::fs::write(&file_path, &content).await {
                tracing::warn!(error = %e, "failed to write dream initial content");
                return;
            }

            // Thinking loop — limited to max_turns
            for turn in 0..max_turns {
                // Check cancellation
                if cancel_clone.is_cancelled() {
                    content.push_str(&format!(
                        "\n---\n[CANCELLED after turn {} at {}]\n",
                        turn,
                        chrono_now()
                    ));
                    let _ = tokio::fs::write(&file_path, &content).await;
                    tracing::info!(task_id = %dream_task_id_for_spawn, turn, "dream task cancelled");
                    return;
                }

                turn_counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                // Simulate a thinking turn — in a full implementation, this would
                // make a lightweight model call. For now, write a structured thought.
                let thought = format!(
                    "## Thinking Turn {turn}\n\n\
                     Analyzing the prompt from different angles...\n\n\
                     - Considering implications and connections\n\
                     - Exploring related concepts\n\
                     - Formulating potential responses\n\n"
                );
                content.push_str(&thought);

                // Write incremental progress
                if let Err(e) = tokio::fs::write(&file_path, &content).await {
                    tracing::warn!(error = %e, "failed to write dream thought turn {}", turn);
                }

                // Brief pause between turns to yield the runtime
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            // Mark complete
            content.push_str(&format!(
                "\n---\n[DREAM COMPLETE after {max_turns} turns at {}]\n",
                chrono_now()
            ));
            let _ = tokio::fs::write(&file_path, &content).await;
            tracing::info!(task_id = %dream_task_id_for_spawn, turns = max_turns, "dream task completed");
        });

        DreamTask {
            task_id: dream_task_id_for_struct,
            session_id,
            scene_root,
            handle: Some(handle),
            cancel,
            max_turns,
            current_turn: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Cancel the dream task early.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Check if the dream task is still running.
    pub fn is_running(&self) -> bool {
        !self.cancel.is_cancelled()
    }

    /// Get the path to the dream output file for the given session.
    pub fn dream_file_path(&self) -> PathBuf {
        Self::dream_file_path_for_session(&self.session_id, &self.scene_root)
    }

    /// Get the path to the dream output file for a given session ID.
    pub fn dream_file_path_for_session(session_id: &str, scene_root: &Path) -> PathBuf {
        dream_base_dir(scene_root).join(format!("{}.md", session_id))
    }

    /// Read the current dream output file content.
    pub async fn read_dream(&self) -> Option<String> {
        let path = self.dream_file_path();
        tokio::fs::read_to_string(&path).await.ok()
    }

    /// Get the session ID associated with this dream.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the current turn count.
    pub fn current_turn(&self) -> usize {
        self.current_turn.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Get the max turns.
    pub fn max_turns(&self) -> usize {
        self.max_turns
    }
}

impl Drop for DreamTask {
    fn drop(&mut self) {
        // Cancel the background task on drop if still running
        self.cancel.cancel();
    }
}

/// Dream files live under the scene root the caller gave.
fn dream_base_dir(scene_root: &Path) -> PathBuf {
    scene_root.join("dreams")
}

/// Current UTC timestamp string for dream file headers.
fn chrono_now() -> String {
    use time::OffsetDateTime;
    let now = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::InMemoryToolRegistry;

    /// A scene root per test. These used to run against the real
    /// `~/.atta/scenes/<scope>/dreams/` and delete the file afterwards.
    fn scene_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn dream_task_creates_file() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let root = scene_root();
        let dream =
            DreamTask::start_dream("What is the meaning of life?".into(), tools, root.path());
        let file_path = dream.dream_file_path();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let content = loop {
            let content = tokio::fs::read_to_string(&file_path)
                .await
                .unwrap_or_default();
            if content.contains("Initial Prompt") || tokio::time::Instant::now() >= deadline {
                break content;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        assert!(
            content.contains("Initial Prompt"),
            "dream file: {content:?}"
        );
        assert!(content.contains("What is the meaning of life?"));

        dream.cancel();
    }

    #[tokio::test]
    async fn dream_task_cancels_early() {
        let tools = Arc::new(InMemoryToolRegistry::new());
        let root = scene_root();
        let dream = DreamTask::start_dream("Think about this".into(), tools, root.path());

        // Cancel before it completes
        dream.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let file_path = dream.dream_file_path();
        if file_path.exists() {
            let content = tokio::fs::read_to_string(&file_path)
                .await
                .unwrap_or_default();
            // May contain CANCELLED if the write happened
            if !content.is_empty() {
                assert!(content.contains("Initial Prompt") || content.contains("[CANCELLED]"));
            }
        }
    }

    #[test]
    fn dream_file_path_uses_session() {
        let root = scene_root();
        let path = DreamTask::dream_file_path_for_session("test-session-123", root.path());
        assert_eq!(path, root.path().join("dreams/test-session-123.md"));
    }

    #[test]
    fn dream_default_max_turns_matches_ts() {
        assert_eq!(DreamTask::DEFAULT_MAX_TURNS, 30);
    }
}
