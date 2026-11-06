//! Reads a recording back into calls with their responses attached.
//!
//! The read path is deliberately forgiving where the history store is strict.
//! A session transcript is what `resume` rebuilds a conversation from, so
//! silently skipping a line there would reconstruct a wrong session and
//! refusing the file is the safer failure. A recording is diagnostic data:
//! surfacing the parts that parse beats refusing the whole file, so an
//! unreadable line is counted, warned about with its line number, and stepped
//! over. The count is carried on [`Recording`] so a caller can say how much of
//! a recording it is actually looking at.

use base::interface::model::ModelEvent;
use std::path::Path;

use super::format::{CallRecord, EndRecord, Header, Record};
use super::pack::{expand_text_run, expand_tool_args_run};

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("recording not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("recording has no header line")]
    MissingHeader,
    #[error("unsupported recording format version {found} (this build reads {supported})")]
    UnsupportedVersion { found: u32, supported: u32 },
}

/// One recorded model call: what went out, what came back, and how it ended.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub request: CallRecord,
    pub response: Vec<ModelEvent>,
    /// Absent when the recording stops mid-call — the process died while the
    /// stream was open. Replay reproduces that as a stream that simply ends.
    pub end: Option<EndRecord>,
}

#[derive(Debug, Clone)]
pub struct Recording {
    pub header: Header,
    pub calls: Vec<RecordedCall>,
    /// Lines that could not be interpreted. Non-zero means this recording is
    /// being read partially.
    pub damaged: usize,
}

pub fn load(dir: &Path) -> Result<Recording, ReadError> {
    let path = dir.join("calls.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ReadError::NotFound(path.display().to_string()))
        }
        Err(e) => return Err(ReadError::Io(e)),
    };

    let mut header: Option<Header> = None;
    let mut calls: Vec<RecordedCall> = Vec::new();
    let mut damaged = 0usize;

    for (i, line) in content.lines().enumerate() {
        let line_no = i + 1;
        if line.trim().is_empty() {
            continue;
        }
        let record = match serde_json::from_str::<Record>(line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(line = line_no, error = %e, "recording: unreadable line, skipped");
                damaged += 1;
                continue;
            }
        };

        match record {
            Record::Recording(h) => {
                if h.version > super::format::FORMAT_VERSION {
                    return Err(ReadError::UnsupportedVersion {
                        found: h.version,
                        supported: super::format::FORMAT_VERSION,
                    });
                }
                header = Some(h);
            }
            Record::Call(request) => calls.push(RecordedCall {
                request: *request,
                response: Vec::new(),
                end: None,
            }),
            Record::Chunk(c) => push_events(
                &mut calls,
                c.call,
                std::iter::once(c.chunk),
                line_no,
                &mut damaged,
            ),
            Record::TextChunks(run) => match expand_text_run(&run, false) {
                Ok(chunks) => push_events(
                    &mut calls,
                    run.call,
                    chunks.into_iter().map(|c| c.chunk),
                    line_no,
                    &mut damaged,
                ),
                Err(e) => {
                    tracing::warn!(line = line_no, error = %e, "recording: malformed packed run, skipped");
                    damaged += 1;
                }
            },
            Record::ThinkingChunks(run) => match expand_text_run(&run, true) {
                Ok(chunks) => push_events(
                    &mut calls,
                    run.call,
                    chunks.into_iter().map(|c| c.chunk),
                    line_no,
                    &mut damaged,
                ),
                Err(e) => {
                    tracing::warn!(line = line_no, error = %e, "recording: malformed packed run, skipped");
                    damaged += 1;
                }
            },
            Record::ToolArgsChunks(run) => match expand_tool_args_run(&run) {
                Ok(chunks) => push_events(
                    &mut calls,
                    run.call,
                    chunks.into_iter().map(|c| c.chunk),
                    line_no,
                    &mut damaged,
                ),
                Err(e) => {
                    tracing::warn!(line = line_no, error = %e, "recording: malformed packed run, skipped");
                    damaged += 1;
                }
            },
            Record::End(end) => match calls.iter_mut().find(|c| c.request.seq == end.call) {
                Some(call) => call.end = Some(end),
                None => {
                    tracing::warn!(
                        line = line_no,
                        call = end.call,
                        "recording: end record with no matching call, skipped"
                    );
                    damaged += 1;
                }
            },
            Record::Unknown => {
                tracing::warn!(line = line_no, "recording: unknown record type, skipped");
                damaged += 1;
            }
        }
    }

    Ok(Recording {
        header: header.ok_or(ReadError::MissingHeader)?,
        calls,
        damaged,
    })
}

fn push_events(
    calls: &mut [RecordedCall],
    call_seq: u64,
    events: impl Iterator<Item = ModelEvent>,
    line_no: usize,
    damaged: &mut usize,
) {
    match calls.iter_mut().find(|c| c.request.seq == call_seq) {
        Some(call) => call.response.extend(events),
        None => {
            tracing::warn!(
                line = line_no,
                call = call_seq,
                "recording: chunk with no matching call, skipped"
            );
            *damaged += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::blob::BlobId;
    use crate::recorder::format::{now_ms, ChunkRecord, Outcome, RecordedParams, FORMAT_VERSION};
    use crate::recorder::writer::RecordingWriter;
    use base::interface::model::Usage;
    use base::interface::settings::ThinkingMode;
    use base::provider::ApiType;

    fn header() -> Header {
        Header {
            version: FORMAT_VERSION,
            name: "run".into(),
            session_id: "S1".into(),
            parent: None,
            agent_type: None,
            created_at: now_ms(),
            engine_version: "test".into(),
        }
    }

    fn call_record(seq: u64) -> CallRecord {
        CallRecord {
            seq,
            ts: 1000,
            session_id: Some("S1".into()),
            parent_session_id: None,
            agent_type: None,
            turn: 0,
            step: 0,
            purpose: None,
            provider: "anthropic".into(),
            api_type: ApiType::Anthropic,
            params: RecordedParams {
                search_max_uses: None,
                model: "m".into(),
                max_tokens: 100,
                thinking_mode: ThinkingMode::Off,
                fallback_model: None,
                cache_edits: vec![],
            },
            system: vec![BlobId("a".repeat(16))],
            tools: BlobId("b".repeat(16)),
            messages: vec![],
            input_map: None,
        }
    }

    /// Writes a call, three text deltas (which pack into one row), and an end.
    fn write_one_call(root: &Path) {
        let writer = RecordingWriter::create(root, header());
        writer
            .append(&Record::Call(Box::new(call_record(0))))
            .unwrap();
        let chunks: Vec<_> = (0..3)
            .map(|i| ChunkRecord {
                seq: i + 1,
                ts: 1000 + i * 10,
                call: 0,
                chunk: ModelEvent::TextDelta {
                    text: format!("t{i}"),
                },
            })
            .collect();
        for record in crate::recorder::pack::pack(chunks) {
            writer.append(&record).unwrap();
        }
        writer
            .append(&Record::End(EndRecord {
                seq: 4,
                ts: 1100,
                call: 0,
                outcome: Outcome::Ok,
                stop_reason: Some("end_turn".into()),
                usage: Some(Usage {
                    input_tokens: 5,
                    output_tokens: 6,
                    ..Default::default()
                }),
                duration_ms: 100,
            }))
            .unwrap();
        writer.flush().unwrap();
    }

    #[test]
    fn a_written_recording_reads_back_with_its_response_attached() {
        let root = tempfile::tempdir().unwrap();
        write_one_call(root.path());

        let recording = load(&root.path().join("run")).unwrap();
        assert_eq!(recording.damaged, 0);
        assert_eq!(recording.calls.len(), 1);
        let call = &recording.calls[0];
        assert_eq!(call.response.len(), 3);
        assert!(matches!(
            &call.response[0],
            ModelEvent::TextDelta { text } if text == "t0"
        ));
        assert!(matches!(call.end.as_ref().unwrap().outcome, Outcome::Ok));
    }

    #[test]
    fn an_unknown_record_type_is_skipped_and_counted() {
        let root = tempfile::tempdir().unwrap();
        write_one_call(root.path());
        let path = root.path().join("run").join("calls.jsonl");
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.push_str("{\"type\":\"from_the_future\",\"x\":1}\n");
        std::fs::write(&path, content).unwrap();

        let recording = load(&root.path().join("run")).unwrap();
        assert_eq!(recording.damaged, 1);
        assert_eq!(recording.calls.len(), 1);
        assert_eq!(recording.calls[0].response.len(), 3);
    }

    #[test]
    fn an_unparseable_line_is_skipped_and_counted() {
        let root = tempfile::tempdir().unwrap();
        write_one_call(root.path());
        let path = root.path().join("run").join("calls.jsonl");
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.push_str("{ this is not json\n");
        std::fs::write(&path, content).unwrap();

        let recording = load(&root.path().join("run")).unwrap();
        assert_eq!(recording.damaged, 1);
        assert_eq!(recording.calls.len(), 1);
    }

    #[test]
    fn a_call_cut_off_mid_stream_reads_back_without_an_end() {
        let root = tempfile::tempdir().unwrap();
        let writer = RecordingWriter::create(root.path(), header());
        writer
            .append(&Record::Call(Box::new(call_record(0))))
            .unwrap();
        writer
            .append(&Record::Chunk(ChunkRecord {
                seq: 1,
                ts: 1010,
                call: 0,
                chunk: ModelEvent::TextDelta {
                    text: "partial".into(),
                },
            }))
            .unwrap();
        writer.flush().unwrap();

        let recording = load(&root.path().join("run")).unwrap();
        assert_eq!(recording.calls.len(), 1);
        assert_eq!(recording.calls[0].response.len(), 1);
        assert!(recording.calls[0].end.is_none());
    }

    #[test]
    fn a_newer_format_version_is_refused_rather_than_misread() {
        let root = tempfile::tempdir().unwrap();
        let mut h = header();
        h.version = FORMAT_VERSION + 1;
        let writer = RecordingWriter::create(root.path(), h);
        writer
            .append(&Record::Call(Box::new(call_record(0))))
            .unwrap();
        writer.flush().unwrap();

        assert!(matches!(
            load(&root.path().join("run")),
            Err(ReadError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn a_missing_recording_is_reported_as_not_found() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(&root.path().join("absent")),
            Err(ReadError::NotFound(_))
        ));
    }
}
