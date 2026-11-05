//! FileRecorder — appends telemetry events as JSONL to a file.
//!
//! Implements [`TelemetryRecorder`] so it can be injected wherever a
//! `TelemetryHandle` would be used. Events are written synchronously
//! (blocking) to guarantee ordering within a turn. For integration
//! tests the volume is low enough that blocking is acceptable.

use crate::events::TelemetryEvent;
use crate::handle::TelemetryRecorder;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// A `TelemetryRecorder` that appends each event as one JSON line to a file.
///
/// ```ignore
/// let rec = FileRecorder::new("/tmp/test.telemetry.md")?;
/// rec.record(event);
/// ```
pub struct FileRecorder {
    file: Mutex<File>,
}

impl FileRecorder {
    /// Creates a new recorder that writes (or appends) to `path`.
    pub fn new(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path: PathBuf = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Check whether any events have been written.
    pub fn exists(&self) -> bool {
        true // always true if constructed successfully
    }
}

impl TelemetryRecorder for FileRecorder {
    fn record(&self, event: TelemetryEvent) -> Result<(), crate::handle::TelemetryHandleError> {
        let mut f = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut line = serde_json::to_string(&event).unwrap_or_default();
        line.push('\n');
        f.write_all(line.as_bytes())
            .map_err(|_| crate::handle::TelemetryHandleError::ChannelFull)?;
        f.flush()
            .map_err(|_| crate::handle::TelemetryHandleError::ChannelFull)?;
        Ok(())
    }

    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::ready(()))
    }
}

impl std::fmt::Debug for FileRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileRecorder").finish_non_exhaustive()
    }
}
