//! Project-local session state plus global session sidecars.

use crate::error::HistoryError;
use crate::path::{
    project_session_state_file, session_file, session_memory_file, session_metadata_file,
    session_prompt_state_file, session_repl_input_history_file, session_sidecar_dir,
    session_tui_input_history_file,
};
use base::session::SessionId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

const PROJECT_SESSION_SCHEMA_VERSION: u32 = 1;
const SESSION_METADATA_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectSessionState {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub canonical_cwd: String,
    pub history_file: String,
    pub updated_at: String,
}

impl ProjectSessionState {
    pub fn new(canonical_cwd: &Path, project_history_dir: &Path, session: SessionId) -> Self {
        Self {
            schema_version: PROJECT_SESSION_SCHEMA_VERSION,
            session_id: session,
            canonical_cwd: canonical_cwd.display().to_string(),
            history_file: session_file(project_history_dir, &session)
                .display()
                .to_string(),
            updated_at: now_rfc3339(),
        }
    }

    pub async fn load(canonical_cwd: &Path) -> Result<Option<Self>, HistoryError> {
        let path = project_session_state_file(canonical_cwd);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(HistoryError::Io(e)),
        };
        let state = serde_json::from_str(&content)?;
        Ok(Some(state))
    }

    pub async fn save(&self, canonical_cwd: &Path) -> Result<(), HistoryError> {
        let path = project_session_state_file(canonical_cwd);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        write_json_atomic(&path, self).await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMetadata {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub canonical_cwd: String,
    pub project_state_file: String,
    pub history_file: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_user_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_output_tokens: Option<u64>,
    #[serde(default)]
    pub compact_count: u64,
    /// User-assigned tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// PR link associated with this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_link: Option<String>,
    /// Agent mode that was active (e.g. "plan", "coding").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<String>,
}

impl SessionMetadata {
    pub fn new(
        canonical_cwd: &Path,
        project_history_dir: &Path,
        session: SessionId,
    ) -> SessionMetadata {
        let now = now_rfc3339();
        Self {
            schema_version: SESSION_METADATA_SCHEMA_VERSION,
            session_id: session,
            canonical_cwd: canonical_cwd.display().to_string(),
            project_state_file: project_session_state_file(canonical_cwd)
                .display()
                .to_string(),
            history_file: session_file(project_history_dir, &session)
                .display()
                .to_string(),
            created_at: now.clone(),
            updated_at: now,
            title: None,
            first_user_prompt: None,
            latest_summary: None,
            total_input_tokens: None,
            total_output_tokens: None,
            compact_count: 0,
            tags: Vec::new(),
            pr_link: None,
            agent_mode: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSidecarPaths {
    pub dir: PathBuf,
    pub tui_input_history: PathBuf,
    pub repl_input_history: PathBuf,
    pub session_memory: PathBuf,
    pub prompt_state: PathBuf,
    pub metadata: PathBuf,
}

pub fn session_sidecar_paths_in(sessions_root: &Path, session: &SessionId) -> SessionSidecarPaths {
    SessionSidecarPaths {
        dir: session_sidecar_dir(sessions_root, session),
        tui_input_history: session_tui_input_history_file(sessions_root, session),
        repl_input_history: session_repl_input_history_file(sessions_root, session),
        session_memory: session_memory_file(sessions_root, session),
        prompt_state: session_prompt_state_file(sessions_root, session),
        metadata: session_metadata_file(sessions_root, session),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SessionPromptState {
    pub schema_version: u32,
    pub model: String,
    pub permission_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summarized_message_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summarized_entry_id: Option<base::id::Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_at_last_memory_update: Option<usize>,
    #[serde(default)]
    pub session_memory_initialized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_extraction_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_extraction_completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_extraction_status: Option<MemoryExtractionStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryExtractionStatus {
    Idle,
    Running,
    Completed,
    Failed,
}

impl SessionPromptState {
    pub fn new(model: impl Into<String>, permission_mode: impl Into<String>) -> Self {
        Self {
            schema_version: 1,
            model: model.into(),
            permission_mode: permission_mode.into(),
            last_compaction_at: None,
            last_turn_at: None,
            last_summarized_message_count: None,
            last_summarized_entry_id: None,
            tokens_at_last_memory_update: None,
            session_memory_initialized: false,
            memory_extraction_started_at: None,
            memory_extraction_completed_at: None,
            memory_extraction_status: Some(MemoryExtractionStatus::Idle),
        }
    }

    pub async fn load(
        sessions_root: &Path,
        session: SessionId,
    ) -> Result<Option<Self>, HistoryError> {
        let path = session_prompt_state_file(sessions_root, &session);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(HistoryError::Io(e)),
        };
        Ok(Some(serde_json::from_str(&content)?))
    }

    pub async fn save(&self, sessions_root: &Path, session: SessionId) -> Result<(), HistoryError> {
        let path = session_prompt_state_file(sessions_root, &session);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, serde_json::to_vec_pretty(self)?).await?;
        Ok(())
    }
}

pub async fn ensure_session_sidecar_in(
    sessions_root: &Path,
    canonical_cwd: &Path,
    project_history_dir: &Path,
    session: SessionId,
) -> Result<SessionSidecarPaths, HistoryError> {
    let paths = session_sidecar_paths_in(sessions_root, &session);
    tokio::fs::create_dir_all(&paths.dir).await?;

    let mut metadata = match tokio::fs::read_to_string(&paths.metadata).await {
        Ok(content) => serde_json::from_str::<SessionMetadata>(&content)
            .unwrap_or_else(|_| SessionMetadata::new(canonical_cwd, project_history_dir, session)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            SessionMetadata::new(canonical_cwd, project_history_dir, session)
        }
        Err(e) => return Err(HistoryError::Io(e)),
    };
    metadata.updated_at = now_rfc3339();
    metadata.canonical_cwd = canonical_cwd.display().to_string();
    metadata.history_file = session_file(project_history_dir, &session)
        .display()
        .to_string();
    metadata.project_state_file = project_session_state_file(canonical_cwd)
        .display()
        .to_string();

    write_json_atomic(&paths.metadata, &metadata).await?;
    Ok(paths)
}

/// What one appended entry adds to a session's running counts.
///
/// Only compactions today. The tokens a turn spent are reported by the
/// provider and recorded in telemetry, but `LogEntry::Assistant` carries
/// `usage: None` on every path that writes one — so counting them here would
/// be counting nothing, and a total that is always zero reads like a session
/// that spent nothing rather than like a number nobody keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CounterDelta {
    pub compactions: u64,
}

impl CounterDelta {
    pub fn of(entry: &crate::entry::LogEntry) -> Self {
        match entry {
            crate::entry::LogEntry::Compact { .. } => Self { compactions: 1 },
            _ => Self::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Add one entry's numbers to what the sidecar already holds.
///
/// A by-product of a write that already happened, which is why it is counted
/// here rather than recomputed by whoever wants it: the alternative is
/// reading a whole transcript per session, per listing, to answer how many
/// times this conversation has been compacted.
pub async fn add_counters_in(
    sessions_root: &Path,
    canonical_cwd: &Path,
    project_history_dir: &Path,
    session: SessionId,
    delta: CounterDelta,
) -> Result<(), HistoryError> {
    if delta.is_empty() {
        return Ok(());
    }
    let paths = session_sidecar_paths_in(sessions_root, &session);
    let mut metadata = match tokio::fs::read_to_string(&paths.metadata).await {
        Ok(content) => serde_json::from_str::<SessionMetadata>(&content)
            .unwrap_or_else(|_| SessionMetadata::new(canonical_cwd, project_history_dir, session)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            SessionMetadata::new(canonical_cwd, project_history_dir, session)
        }
        Err(e) => return Err(HistoryError::Io(e)),
    };
    metadata.compact_count += delta.compactions;
    metadata.updated_at = now_rfc3339();
    tokio::fs::create_dir_all(&paths.dir).await?;
    write_json_atomic(&paths.metadata, &metadata).await
}

/// Set (or clear) the name a person gave this session, creating the sidecar
/// if this is the first thing anyone has recorded about it.
///
/// Read-modify-write rather than a blind overwrite: the file also carries the
/// counters and the paths, and a rename is not a reason to forget them.
pub async fn set_session_title_in(
    sessions_root: &Path,
    canonical_cwd: &Path,
    project_history_dir: &Path,
    session: SessionId,
    title: Option<String>,
) -> Result<(), HistoryError> {
    let paths =
        ensure_session_sidecar_in(sessions_root, canonical_cwd, project_history_dir, session)
            .await?;
    let mut metadata = match tokio::fs::read_to_string(&paths.metadata).await {
        Ok(content) => serde_json::from_str::<SessionMetadata>(&content)
            .unwrap_or_else(|_| SessionMetadata::new(canonical_cwd, project_history_dir, session)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            SessionMetadata::new(canonical_cwd, project_history_dir, session)
        }
        Err(e) => return Err(HistoryError::Io(e)),
    };
    metadata.title = title;
    metadata.updated_at = now_rfc3339();
    write_json_atomic(&paths.metadata, &metadata).await
}

/// What the sidecar has recorded about this session: the name somebody gave
/// it, and the totals counted as its log was written.
///
/// A session nobody has written anything about answers with the empty record
/// rather than an error — not having been counted is a normal state.
pub async fn session_stats_in(
    sessions_root: &Path,
    session: SessionId,
) -> Result<crate::store::SessionStats, HistoryError> {
    let path = session_metadata_file(sessions_root, &session);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(crate::store::SessionStats::default())
        }
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let Ok(metadata) = serde_json::from_str::<SessionMetadata>(&content) else {
        return Ok(crate::store::SessionStats::default());
    };
    Ok(crate::store::SessionStats {
        title: metadata.title,
        compact_count: metadata.compact_count,
    })
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

async fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), HistoryError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("json")
    ));
    tokio::fs::write(&tmp, serde_json::to_vec_pretty(value)?).await?;
    tokio::fs::rename(tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn project_state_roundtrips() {
        let cwd = TempDir::new().unwrap();
        let history = TempDir::new().unwrap();
        let session = SessionId::new();
        let state = ProjectSessionState::new(cwd.path(), history.path(), session);

        state.save(cwd.path()).await.unwrap();
        let loaded = ProjectSessionState::load(cwd.path())
            .await
            .unwrap()
            .expect("state should exist");

        assert_eq!(loaded.session_id, session);
        assert_eq!(loaded.schema_version, PROJECT_SESSION_SCHEMA_VERSION);
        assert!(loaded.history_file.ends_with(&format!("{session}.jsonl")));
        assert!(!cwd.path().join(".agix/code/session.json.tmp").exists());
    }

    #[tokio::test]
    async fn missing_project_state_returns_none() {
        let cwd = TempDir::new().unwrap();
        let loaded = ProjectSessionState::load(cwd.path()).await.unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn metadata_points_at_project_and_history() {
        let cwd = PathBuf::from("/tmp/project");
        let history = PathBuf::from("/tmp/history");
        let session = SessionId::new();
        let metadata = SessionMetadata::new(&cwd, &history, session);

        assert_eq!(metadata.session_id, session);
        assert!(metadata.project_state_file.ends_with(".agix/session.json"));
        assert!(metadata.history_file.ends_with(&format!("{session}.jsonl")));
    }

    #[tokio::test]
    async fn sidecar_paths_use_separate_tui_and_repl_history_files() {
        let cwd = TempDir::new().unwrap();
        let history = TempDir::new().unwrap();
        let sessions = TempDir::new().unwrap();
        let session = SessionId::new();

        let paths = ensure_session_sidecar_in(sessions.path(), cwd.path(), history.path(), session)
            .await
            .unwrap();

        assert!(paths.dir.exists());
        assert!(paths.tui_input_history.ends_with("tui_input_history.jsonl"));
        assert!(paths.repl_input_history.ends_with("repl_input_history.txt"));
        assert_ne!(paths.tui_input_history, paths.repl_input_history);
        assert!(paths.metadata.exists());
        assert!(!paths.metadata.with_extension("json.tmp").exists());
    }
}
