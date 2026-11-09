//! FileRecorder — appends telemetry events as JSONL to a file.
//!
//! Implements [`TelemetryRecorder`] so it can be injected wherever a
//! `TelemetryHandle` would be used. Events are written synchronously
//! (blocking) to guarantee ordering within a turn. A file that reaches its
//! size limit is moved to `<file>.1` (replacing the one before) and a new one
//! started, so a long-lived daemon keeps at most two files' worth.

use crate::events::TelemetryEvent;
use crate::handle::{TelemetryHandleError, TelemetryRecorder};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// What [`FileRecorder::new`] lets a file grow to before it rotates.
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A `TelemetryRecorder` that appends each event as one JSON line to a file.
///
/// ```ignore
/// let rec = FileRecorder::new("/tmp/session.jsonl")?;
/// rec.record(event)?;
/// ```
pub struct FileRecorder {
    path: PathBuf,
    max_bytes: u64,
    file: Mutex<Open>,
}

struct Open {
    file: File,
    len: u64,
}

impl FileRecorder {
    /// Creates a new recorder that writes (or appends) to `path`, rotating
    /// at [`DEFAULT_MAX_BYTES`].
    pub fn new(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::with_max_bytes(path, DEFAULT_MAX_BYTES)
    }

    /// [`Self::new`] with its own rotation size.
    pub fn with_max_bytes(path: impl Into<PathBuf>, max_bytes: u64) -> std::io::Result<Self> {
        let path: PathBuf = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = open_append(&path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path,
            max_bytes,
            file: Mutex::new(Open { file, len }),
        })
    }

    fn rotate(&self, open: &mut Open) -> std::io::Result<()> {
        let mut rotated = self.path.as_os_str().to_owned();
        rotated.push(".1");
        std::fs::rename(&self.path, PathBuf::from(rotated))?;
        open.file = open_append(&self.path)?;
        open.len = 0;
        Ok(())
    }
}

fn open_append(path: &std::path::Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl TelemetryRecorder for FileRecorder {
    fn record(&self, event: TelemetryEvent) -> Result<(), TelemetryHandleError> {
        let mut line =
            serde_json::to_string(&event).map_err(|e| TelemetryHandleError::Io(e.to_string()))?;
        line.push('\n');
        let mut open = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if open.len > 0 && open.len + line.len() as u64 > self.max_bytes {
            self.rotate(&mut open)
                .map_err(|e| TelemetryHandleError::Io(e.to_string()))?;
        }
        open.file
            .write_all(line.as_bytes())
            .and_then(|()| open.file.flush())
            .map_err(|e| TelemetryHandleError::Io(e.to_string()))?;
        open.len += line.len() as u64;
        Ok(())
    }

    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::ready(()))
    }
}

impl std::fmt::Debug for FileRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileRecorder")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{TurnStartPayload, UserPromptSubmitPayload};

    fn event() -> TelemetryEvent {
        TelemetryEvent::turn_start(
            "sess",
            1,
            None,
            TurnStartPayload {
                resumed: false,
                is_retry: false,
            },
        )
    }

    #[test]
    fn a_file_past_its_limit_moves_aside_and_a_new_one_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let line = serde_json::to_string(&event()).unwrap().len() as u64 + 1;
        let rec = FileRecorder::with_max_bytes(&path, line * 3).unwrap();
        for _ in 0..5 {
            rec.record(event()).unwrap();
        }
        let lines = |p: &std::path::Path| {
            std::fs::read_to_string(p)
                .unwrap_or_default()
                .lines()
                .count()
        };
        assert_eq!(lines(&dir.path().join("t.jsonl.1")), 3);
        assert_eq!(lines(&path), 2);
    }

    #[test]
    fn a_write_that_fails_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let rec = FileRecorder::new(&path).unwrap();
        rec.file.lock().unwrap().file = File::open(&path).unwrap();
        let err = rec
            .record(TelemetryEvent::user_prompt_submit(
                "sess",
                1,
                None,
                UserPromptSubmitPayload {
                    char_count: 1,
                    attachment_count: 0,
                },
            ))
            .unwrap_err();
        assert!(matches!(err, TelemetryHandleError::Io(_)), "{err:?}");
    }
}
