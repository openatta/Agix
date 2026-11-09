//! Append-only writer for one recording's `calls.jsonl`.
//!
//! Holds the file open and buffers, rather than reopening per line: a recording
//! writes one record per streamed token, and a syscall triple per token would
//! make the recorder's cost visible in the agent's own latency.
//!
//! Buffering is bounded by an explicit flush discipline rather than by size.
//! The caller flushes right after the request record so that a process killed
//! mid-stream still leaves behind what was in flight, and again when the call
//! ends. Nothing here retries or panics — a recording is diagnostic data, and
//! failing to write one must never take the turn down with it.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use super::blob::BlobStore;
use super::format::{Header, Record};

pub struct RecordingWriter {
    dir: PathBuf,
    blobs: BlobStore,
    header: Header,
    seq: AtomicU64,
    file: Mutex<Option<BufWriter<std::fs::File>>>,
}

impl RecordingWriter {
    /// Performs no I/O. A recording that never sees a call leaves nothing on
    /// disk, so enabling the recorder on a session that makes no model call
    /// does not litter the root with empty directories.
    pub fn create(root: &Path, header: Header) -> Self {
        let dir = root.join(&header.name);
        Self {
            blobs: BlobStore::new(&dir),
            dir,
            header,
            seq: AtomicU64::new(0),
            file: Mutex::new(None),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    /// Reserve `n` consecutive line numbers, returning the first.
    ///
    /// A packed row covers several chunks and promises `seq0 + k` names the
    /// k-th of them, so its numbers have to be handed out together — one at a
    /// time would let a concurrently recording session interleave into the
    /// middle of the block and make that promise false.
    pub fn next_seq_block(&self, n: usize) -> u64 {
        self.seq.fetch_add(n as u64, Ordering::SeqCst)
    }

    pub fn append(&self, record: &Record) -> std::io::Result<()> {
        let line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let file = match guard.as_mut() {
            Some(file) => file,
            None => guard.insert(self.open()?),
        };
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")
    }

    pub fn flush(&self) -> std::io::Result<()> {
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }

    /// One writer is one recording, so an existing file under the same name is
    /// truncated rather than appended to. Appending would restart `seq` at zero
    /// partway through a file, and a reader resolving chunk and `end` ownership
    /// by `seq` would silently attach the new run's response to the old run's
    /// calls. Leftover blobs are harmless — they are content-addressed, so an
    /// unreferenced one costs disk and nothing else, and a re-record of similar
    /// content reuses them.
    fn open(&self) -> std::io::Result<BufWriter<std::fs::File>> {
        super::private_dir(&self.dir)?;
        let file = super::private_file(&self.dir.join("calls.jsonl"))?;
        let mut writer = BufWriter::new(file);
        let header = serde_json::to_string(&Record::Recording(self.header.clone()))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writer.write_all(header.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(writer)
    }
}

impl Drop for RecordingWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::format::{now_ms, ChunkRecord, FORMAT_VERSION};
    use base::interface::model::ModelEvent;

    fn header(name: &str) -> Header {
        Header {
            version: FORMAT_VERSION,
            name: name.into(),
            session_id: "S1".into(),
            parent: None,
            agent_type: None,
            created_at: now_ms(),
            engine_version: "test".into(),
        }
    }

    fn chunk(seq: u64) -> Record {
        Record::Chunk(ChunkRecord {
            seq,
            ts: 1000 + seq,
            call: 0,
            chunk: ModelEvent::TextDelta {
                text: format!("t{seq}"),
            },
        })
    }

    fn lines(dir: &Path, name: &str) -> Vec<String> {
        std::fs::read_to_string(dir.join(name).join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn creating_a_writer_touches_no_disk() {
        let root = tempfile::tempdir().unwrap();
        let _writer = RecordingWriter::create(root.path(), header("run"));
        assert!(!root.path().join("run").exists());
    }

    #[test]
    fn the_header_is_written_once_before_the_first_record() {
        let root = tempfile::tempdir().unwrap();
        let writer = RecordingWriter::create(root.path(), header("run"));
        writer.append(&chunk(0)).unwrap();
        writer.append(&chunk(1)).unwrap();
        writer.flush().unwrap();

        let lines = lines(root.path(), "run");
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(r#""type":"recording""#));
        assert!(lines[1].contains(r#""type":"chunk""#));
    }

    #[test]
    fn seq_is_monotonic_and_starts_at_zero() {
        let root = tempfile::tempdir().unwrap();
        let writer = RecordingWriter::create(root.path(), header("run"));
        assert_eq!(writer.next_seq(), 0);
        assert_eq!(writer.next_seq(), 1);
        assert_eq!(writer.next_seq(), 2);
    }

    #[test]
    fn a_flushed_record_survives_dropping_the_writer_mid_recording() {
        let root = tempfile::tempdir().unwrap();
        {
            let writer = RecordingWriter::create(root.path(), header("run"));
            writer.append(&chunk(0)).unwrap();
            writer.flush().unwrap();
            writer.append(&chunk(1)).unwrap();
        }
        assert_eq!(lines(root.path(), "run").len(), 3);
    }

    /// Re-recording under a name that already exists replaces it.
    ///
    /// Appending instead would put a second run's records after the first with
    /// `seq` restarting at zero, and a reader resolving `end.call`/chunk
    /// ownership by seq would attach the second run's response to the first
    /// run's calls — silently, and only in a file nobody reads until something
    /// has already gone wrong. Re-recording into the same round is a normal
    /// thing to do (`tests/run_api.sh` twice in one day), so this has to be
    /// the safe operation.
    #[test]
    fn re_recording_replaces_the_previous_run() {
        let root = tempfile::tempdir().unwrap();
        {
            let writer = RecordingWriter::create(root.path(), header("run"));
            writer.append(&chunk(0)).unwrap();
            writer.append(&chunk(1)).unwrap();
        }
        {
            let writer = RecordingWriter::create(root.path(), header("run"));
            writer.append(&chunk(0)).unwrap();
        }
        let lines = lines(root.path(), "run");
        assert_eq!(
            lines.len(),
            2,
            "expected header + one record from the second run, got:\n{}",
            lines.join("\n")
        );
        assert!(lines[0].contains(r#""type":"recording""#));
    }

    #[test]
    fn blobs_live_under_the_recording_directory() {
        let root = tempfile::tempdir().unwrap();
        let writer = RecordingWriter::create(root.path(), header("run"));
        writer.blobs().put(&"content").unwrap();
        assert!(root.path().join("run").join("blobs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_recording_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let writer = RecordingWriter::create(root.path(), header("run"));
        writer.append(&chunk(0)).unwrap();
        let id = writer.blobs().put(&"content").unwrap();
        let mode =
            |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let run = root.path().join("run");
        assert_eq!(mode(run.clone()), 0o700);
        assert_eq!(mode(run.join("calls.jsonl")), 0o600);
        assert_eq!(mode(run.join("blobs")), 0o700);
        assert_eq!(mode(run.join("blobs").join(&id.0)), 0o600);
    }
}
