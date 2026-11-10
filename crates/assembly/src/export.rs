//! `session.export`: a session, its sidechains and everything they refer to,
//! written as one versioned archive to a path the host names.
//!
//! The pool takes the snapshot — every log cut at one moment, plus what only
//! the live pool knows — and hands it here as an [`ExportSnapshot`]. This file
//! turns that into the archive: per session the transcript as stored, the
//! `session.history` and `session.entries` views over it, the images decoded
//! out of it, and a `manifest.json` that says what is where. The layout and
//! the manifest are the format a future `session.import` reads, which is why
//! the manifest carries a version of its own (docs/daemon_rpc_protocol.md,
//! `session.export`).
//!
//! Everything here is blocking file I/O and runs on a blocking thread.

use crate::session_pool::api::PoolError;
use base::session::SessionId;
use base64::Engine as _;
use history::entry::{EnvelopedEntry, LogEntry};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const FORMAT: &str = "agix.session-export";
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    Dir,
}

impl ArchiveFormat {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "zip" => Some(Self::Zip),
            "dir" => Some(Self::Dir),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Dir => "dir",
        }
    }
}

/// What goes into the archive beyond the root session's own log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Include {
    pub sidechains: bool,
    pub attachments: bool,
    pub recording: bool,
}

impl Default for Include {
    fn default() -> Self {
        Self {
            sidechains: true,
            attachments: true,
            recording: false,
        }
    }
}

impl Include {
    pub fn parse(names: &[String]) -> Result<Self, String> {
        let mut include = Self {
            sidechains: false,
            attachments: false,
            recording: false,
        };
        for name in names {
            match name.as_str() {
                "sidechains" => include.sidechains = true,
                "attachments" => include.attachments = true,
                "recording" => include.recording = true,
                other => {
                    return Err(format!(
                        "unknown include `{other}`: sidechains, attachments or recording"
                    ))
                }
            }
        }
        Ok(include)
    }

    fn names(self) -> Vec<&'static str> {
        [
            (self.sidechains, "sidechains"),
            (self.attachments, "attachments"),
            (self.recording, "recording"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect()
    }
}

#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub session_id: String,
    pub path: PathBuf,
    pub format: ArchiveFormat,
    pub include: Include,
}

#[derive(Debug)]
pub enum ExportError {
    Invalid(String),
    Failed(String),
}

impl From<ExportError> for PoolError {
    fn from(e: ExportError) -> Self {
        match e {
            ExportError::Invalid(m) => PoolError::InvalidRequest(m),
            ExportError::Failed(m) => PoolError::Internal(m),
        }
    }
}

/// Refuse a target before anything is read: it must be absolute, must not
/// exist yet (a symlink there counts), its directory must exist, and it must
/// not land inside the history store, where it would read as a session.
pub fn check_target(path: &Path, history_roots: &[PathBuf]) -> Result<(), ExportError> {
    if !path.is_absolute() {
        return Err(ExportError::Invalid(format!(
            "path must be absolute: {}",
            path.display()
        )));
    }
    if path.symlink_metadata().is_ok() {
        return Err(ExportError::Invalid(format!(
            "{} already exists; export never overwrites",
            path.display()
        )));
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(ExportError::Invalid(format!(
            "path names no file: {}",
            path.display()
        )));
    };
    let parent = std::fs::canonicalize(parent)
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| {
            ExportError::Invalid(format!("{} is not an existing directory", parent.display()))
        })?;
    let target = parent.join(name);
    for root in history_roots {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if target.starts_with(&root) {
            return Err(ExportError::Invalid(format!(
                "{} is inside the session history store ({})",
                path.display(),
                root.display()
            )));
        }
    }
    Ok(())
}

/// One session as the pool saw it at the cut.
pub struct SessionPart {
    pub session_id: SessionId,
    pub entries: Vec<EnvelopedEntry>,
    /// `session.history`'s items for the whole transcript.
    pub history: Vec<Value>,
    /// `session.entries`' rows.
    pub extension_entries: Vec<Value>,
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub total_input_tokens: Option<u64>,
    pub total_output_tokens: Option<u64>,
    pub compact_count: u64,
    pub memory: Option<String>,
    pub active: bool,
    pub current_turn: Option<String>,
    /// What the live session holds that its log does not; `None` when it is
    /// not in memory.
    pub runtime: Option<Value>,
}

/// A recording one of the sessions writes to, measured at the cut.
pub struct RecordingPart {
    pub name: String,
    pub dir: PathBuf,
    pub calls_len: u64,
    pub sessions: Vec<String>,
}

pub struct ExportSnapshot {
    pub root: SessionId,
    pub cut_at: time::OffsetDateTime,
    pub sessions: Vec<SessionPart>,
    pub forks: Vec<SessionId>,
    pub recordings: Vec<RecordingPart>,
    pub warnings: Vec<String>,
}

/// Write the archive and answer with what `session.export` returns.
pub fn write(request: &ExportRequest, snapshot: ExportSnapshot) -> Result<Value, ExportError> {
    let partial = partial_path(&request.path);
    let result = write_to(request, snapshot, &partial).and_then(|summary| {
        finish(&partial, &request.path, request.format)?;
        Ok(summary)
    });
    if result.is_err() {
        let _ = match request.format {
            ArchiveFormat::Zip => std::fs::remove_file(&partial),
            ArchiveFormat::Dir => std::fs::remove_dir_all(&partial),
        };
    }
    let mut summary = result?;
    let bytes = archive_bytes(&request.path, request.format);
    summary["bytes"] = json!(bytes);
    Ok(summary)
}

fn partial_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.partial-{}", SessionId::new()))
}

fn write_to(
    request: &ExportRequest,
    snapshot: ExportSnapshot,
    partial: &Path,
) -> Result<Value, ExportError> {
    let failed =
        |e: std::io::Error| ExportError::Failed(format!("could not write the archive: {e}"));
    let mut sink: Box<dyn Sink> = match request.format {
        ArchiveFormat::Zip => Box::new(ZipSink::create(partial).map_err(failed)?),
        ArchiveFormat::Dir => Box::new(DirSink::create(partial).map_err(failed)?),
    };
    let summary = pack(request, snapshot, sink.as_mut()).map_err(failed)?;
    sink.finish().map_err(failed)?;
    Ok(summary)
}

/// Move the finished archive into place without ever replacing something
/// that appeared there meanwhile.
fn finish(partial: &Path, target: &Path, format: ArchiveFormat) -> Result<(), ExportError> {
    let exists = || {
        ExportError::Invalid(format!(
            "{} appeared while the archive was being written; export never overwrites",
            target.display()
        ))
    };
    let failed = |e: std::io::Error| {
        ExportError::Failed(format!("could not move the archive into place: {e}"))
    };
    if format == ArchiveFormat::Zip {
        // A hard link fails when the target exists, which a rename would not.
        match std::fs::hard_link(partial, target) {
            Ok(()) => {
                let _ = std::fs::remove_file(partial);
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(exists()),
            Err(_) => {}
        }
    }
    if target.symlink_metadata().is_ok() {
        return Err(exists());
    }
    std::fs::rename(partial, target).map_err(failed)
}

fn archive_bytes(path: &Path, format: ArchiveFormat) -> u64 {
    match format {
        ArchiveFormat::Zip => path.metadata().map(|m| m.len()).unwrap_or(0),
        ArchiveFormat::Dir => dir_bytes(path),
    }
}

fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

trait Sink {
    fn put(&mut self, name: &str, bytes: &[u8], compress: bool) -> std::io::Result<()>;
    fn finish(self: Box<Self>) -> std::io::Result<()>;
}

struct DirSink {
    root: PathBuf,
}

impl DirSink {
    fn create(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir(root)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }
}

impl Sink for DirSink {
    fn put(&mut self, name: &str, bytes: &[u8], _compress: bool) -> std::io::Result<()> {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)
    }

    fn finish(self: Box<Self>) -> std::io::Result<()> {
        Ok(())
    }
}

struct ZipSink {
    zip: zip::ZipWriter<std::fs::File>,
}

impl ZipSink {
    fn create(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self {
            zip: zip::ZipWriter::new(file),
        })
    }
}

impl Sink for ZipSink {
    fn put(&mut self, name: &str, bytes: &[u8], compress: bool) -> std::io::Result<()> {
        let method = if compress {
            zip::CompressionMethod::Deflated
        } else {
            zip::CompressionMethod::Stored
        };
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(method)
            .large_file(true);
        self.zip.start_file(name, options).map_err(zip_error)?;
        self.zip.write_all(bytes)
    }

    fn finish(self: Box<Self>) -> std::io::Result<()> {
        let mut file = self.zip.finish().map_err(zip_error)?;
        file.flush()?;
        file.sync_all()
    }
}

fn zip_error(e: zip::result::ZipError) -> std::io::Error {
    std::io::Error::other(e)
}

/// The images found while packing, one file per distinct content.
#[derive(Default)]
struct Attachments {
    by_sha: BTreeMap<String, Attachment>,
    /// Files first seen since the last flush, not yet in the archive.
    pending: Vec<(String, Vec<u8>)>,
}

struct Attachment {
    file: String,
    media_type: String,
    bytes: usize,
    refs: BTreeMap<(String, String), Option<usize>>,
}

impl Attachments {
    /// Record that `entry_id` of `session_id` shows `image`. A transcript line
    /// has no `seq`; the `session.history` item showing it does, and fills it
    /// in when it carries the same id.
    fn note(&mut self, session_id: &str, entry_id: &str, seq: Option<usize>, image: &Decoded) {
        let attachment = self.by_sha.entry(image.sha256.clone()).or_insert_with(|| {
            self.pending.push((image.file.clone(), image.bytes.clone()));
            Attachment {
                file: image.file.clone(),
                media_type: image.media_type.clone(),
                bytes: image.bytes.len(),
                refs: BTreeMap::new(),
            }
        });
        let slot = attachment
            .refs
            .entry((session_id.to_string(), entry_id.to_string()))
            .or_insert(None);
        if seq.is_some() {
            *slot = seq;
        }
    }
}

/// One session's transcript as JSONL, each line as the store holds it — with
/// image data swapped for a placeholder when attachments are left out.
fn pack_transcript(
    session_id: &str,
    entries: &[EnvelopedEntry],
    include: Include,
    attachments: &mut Attachments,
    findings: &mut Findings,
) -> std::io::Result<String> {
    let mut out = String::new();
    for env in entries {
        let mut line = serde_json::to_value(env)?;
        let entry_id = line
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match &env.entry {
            LogEntry::Blob { blob } => findings.unresolved_blobs.push(json!({
                "session_id": session_id,
                "entry_id": entry_id,
                "blob": blob,
            })),
            // A host's payload is its own vocabulary; nothing in it is ours to
            // reinterpret as an image.
            LogEntry::Extension { .. } => {}
            _ => for_each_image(&mut line, &mut |block| {
                if let Some(url) = block.pointer("/source/url").and_then(Value::as_str) {
                    findings.url_images.push(json!({
                        "session_id": session_id,
                        "entry_id": entry_id,
                        "url": url,
                    }));
                    return;
                }
                match decode_image(block) {
                    Some(image) if include.attachments => {
                        attachments.note(session_id, &entry_id, None, &image);
                    }
                    Some(image) => *block = placeholder(&image),
                    None => findings.warnings.push(format!(
                        "an image in entry {entry_id} of session {session_id} does not decode; left inline"
                    )),
                }
            }),
        }
        out.push_str(&serde_json::to_string(&line)?);
        out.push('\n');
    }
    Ok(out)
}

/// Copy one recording: `calls.jsonl` up to the length measured at the cut,
/// whole lines only, and its content-addressed blobs. Returns how many calls
/// the copied part holds.
fn pack_recording(recording: &RecordingPart, sink: &mut dyn Sink) -> std::io::Result<usize> {
    use std::io::Read;
    let base = format!("recordings/{}", recording.name);
    let mut calls = Vec::new();
    std::fs::File::open(recording.dir.join("calls.jsonl"))?
        .take(recording.calls_len)
        .read_to_end(&mut calls)?;
    calls.truncate(calls.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1));
    let count = calls
        .split(|&b| b == b'\n')
        .filter(|line| {
            serde_json::from_slice::<Value>(line)
                .ok()
                .is_some_and(|v| v["type"] == "call")
        })
        .count();
    sink.put(&format!("{base}/calls.jsonl"), &calls, true)?;
    if let Ok(blobs) = std::fs::read_dir(recording.dir.join("blobs")) {
        for blob in blobs.flatten() {
            if !blob.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let name = blob.file_name().to_string_lossy().into_owned();
            sink.put(
                &format!("{base}/blobs/{name}"),
                &std::fs::read(blob.path())?,
                true,
            )?;
        }
    }
    Ok(count)
}

/// An inline image decoded: its content, what it is, and the archive file it
/// goes to.
struct Decoded {
    bytes: Vec<u8>,
    media_type: String,
    sha256: String,
    file: String,
}

fn decode_image(block: &Value) -> Option<Decoded> {
    let source = block.get("source")?;
    if source.get("type")?.as_str()? != "base64" {
        return None;
    }
    let media_type = source.get("media_type")?.as_str()?.to_string();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(source.get("data")?.as_str()?)
        .ok()?;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let file = format!("attachments/{sha256}.{}", extension_of(&media_type));
    Some(Decoded {
        bytes,
        media_type,
        sha256,
        file,
    })
}

fn extension_of(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

fn placeholder(image: &Decoded) -> Value {
    json!({
        "type": "text",
        "text": format!(
            "[image omitted from export: {}, {} bytes, sha256 {}]",
            image.media_type,
            image.bytes.len(),
            image.sha256
        ),
    })
}

/// Call `visit` on every image block under `value`: message content, tool
/// results nested in it, a compaction's replacement history.
fn for_each_image(value: &mut Value, visit: &mut dyn FnMut(&mut Value)) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image")
                && map.get("source").is_some_and(Value::is_object)
            {
                visit(value);
                return;
            }
            for child in map.values_mut() {
                for_each_image(child, visit);
            }
        }
        Value::Array(items) => {
            for item in items {
                for_each_image(item, visit);
            }
        }
        _ => {}
    }
}

/// What packing one session's transcript found that the manifest reports.
#[derive(Default)]
struct Findings {
    url_images: Vec<Value>,
    unresolved_blobs: Vec<Value>,
    warnings: Vec<String>,
}

fn pack(
    request: &ExportRequest,
    snapshot: ExportSnapshot,
    sink: &mut dyn Sink,
) -> std::io::Result<Value> {
    let include = request.include;
    let mut attachments = Attachments::default();
    let mut findings = Findings {
        warnings: snapshot.warnings,
        ..Findings::default()
    };
    let mut rows = Vec::with_capacity(snapshot.sessions.len());
    let mut id_map = serde_json::Map::new();
    let mut depths: BTreeMap<String, usize> = BTreeMap::new();

    for part in snapshot.sessions {
        let sid = part.session_id.to_string();
        let dir = format!("sessions/{sid}");
        id_map.insert(sid.clone(), json!(dir));

        let meta = part.entries.iter().find_map(|env| match &env.entry {
            LogEntry::Meta { .. } => serde_json::to_value(&env.entry).ok(),
            _ => None,
        });
        let facts = part
            .entries
            .iter()
            .find_map(|env| history::store::SessionFacts::of(&env.entry))
            .unwrap_or_default();
        let depth = facts
            .parent_session_id
            .as_ref()
            .and_then(|p| depths.get(p))
            .map_or(0, |d| d + 1);
        depths.insert(sid.clone(), depth);
        let terminal = part.entries.iter().rev().find_map(|env| match &env.entry {
            LogEntry::SessionEnd { state } => serde_json::to_value(state).ok(),
            _ => None,
        });

        let transcript = pack_transcript(
            &sid,
            &part.entries,
            include,
            &mut attachments,
            &mut findings,
        )?;
        let transcript_sha256 = hex::encode(Sha256::digest(transcript.as_bytes()));
        sink.put(
            &format!("{dir}/transcript.jsonl"),
            transcript.as_bytes(),
            true,
        )?;

        let mut history = part.history;
        for item in &mut history {
            let entry_id = item.get("id").and_then(Value::as_str).map(str::to_string);
            let seq = item.get("seq").and_then(Value::as_u64).map(|n| n as usize);
            for_each_image(item, &mut |block| {
                let Some(image) = decode_image(block) else {
                    return;
                };
                if !include.attachments {
                    *block = placeholder(&image);
                    return;
                }
                if let Some(entry_id) = &entry_id {
                    attachments.note(&sid, entry_id, seq, &image);
                }
                block["source"] = json!({
                    "attachment": image.file,
                    "media_type": image.media_type,
                });
            });
        }
        sink.put(
            &format!("{dir}/history.json"),
            &serde_json::to_vec_pretty(&history)?,
            true,
        )?;
        sink.put(
            &format!("{dir}/entries.json"),
            &serde_json::to_vec_pretty(&part.extension_entries)?,
            true,
        )?;
        sink.put(
            &format!("{dir}/meta.json"),
            &serde_json::to_vec_pretty(&json!({
                "session_id": sid,
                "meta": meta,
                "title": part.title,
                "tags": part.tags,
                "total_input_tokens": part.total_input_tokens,
                "total_output_tokens": part.total_output_tokens,
                "compact_count": part.compact_count,
            }))?,
            true,
        )?;
        if let Some(memory) = &part.memory {
            sink.put(&format!("{dir}/session_memory.md"), memory.as_bytes(), true)?;
        }
        if let Some(runtime) = &part.runtime {
            sink.put(
                &format!("{dir}/runtime.json"),
                &serde_json::to_vec_pretty(runtime)?,
                true,
            )?;
        }

        rows.push(json!({
            "session_id": sid,
            "kind": facts.session_kind,
            "parent_session_id": facts.parent_session_id,
            "depth": depth,
            "scene": facts.scene,
            "project_root": facts.project_root,
            "title": part.title,
            "dir": dir,
            "entry_count": part.entries.len(),
            "message_count": history.len(),
            "active": part.active,
            "turn_state": if part.current_turn.is_some() { "running" } else { "idle" },
            "terminal": terminal,
            "transcript_sha256": transcript_sha256,
        }));

        for (file, bytes) in attachments.pending.drain(..) {
            sink.put(&file, &bytes, false)?;
        }
    }

    let mut recordings = Vec::with_capacity(snapshot.recordings.len());
    for recording in &snapshot.recordings {
        match pack_recording(recording, sink) {
            Ok(calls) => recordings.push(json!({
                "dir": format!("recordings/{}", recording.name),
                "sessions": recording.sessions,
                "calls": calls,
                "covers": "latest_run",
            })),
            Err(e) => findings.warnings.push(format!(
                "recording `{}` could not be read: {e}",
                recording.name
            )),
        }
    }

    let attachment_rows: Vec<Value> = attachments
        .by_sha
        .iter()
        .map(|(sha, a)| {
            json!({
                "file": a.file,
                "sha256": sha,
                "media_type": a.media_type,
                "bytes": a.bytes,
                "refs": a.refs.iter().map(|((session_id, entry_id), seq)| json!({
                    "session_id": session_id,
                    "entry_id": entry_id,
                    "seq": seq,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let cut_at = snapshot
        .cut_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let importable = include.attachments && findings.unresolved_blobs.is_empty();
    let manifest = json!({
        "format": FORMAT,
        "format_version": FORMAT_VERSION,
        "engine_version": env!("CARGO_PKG_VERSION"),
        "protocol_version": crate::discovery::INSTANCE_PROTOCOL_VERSION,
        "cut_at": cut_at,
        "root_session_id": snapshot.root.to_string(),
        "include": include.names(),
        "importable": importable,
        "sessions": rows,
        "id_map": id_map,
        "attachments": attachment_rows,
        "url_images": findings.url_images,
        "unresolved_blobs": findings.unresolved_blobs,
        "forks": snapshot.forks.iter().map(SessionId::to_string).collect::<Vec<_>>(),
        "recordings": recordings,
        "warnings": findings.warnings,
    });
    sink.put(
        "manifest.json",
        &serde_json::to_vec_pretty(&manifest)?,
        true,
    )?;

    Ok(json!({
        "session_id": snapshot.root.to_string(),
        "path": request.path.display().to_string(),
        "format": request.format.name(),
        "format_version": FORMAT_VERSION,
        "cut_at": cut_at,
        "sessions": rows.iter().map(|row| json!({
            "session_id": row["session_id"],
            "kind": row["kind"],
            "parent_session_id": row["parent_session_id"],
            "entry_count": row["entry_count"],
        })).collect::<Vec<_>>(),
        "attachments": attachments.by_sha.len(),
        "recording": !recordings.is_empty(),
        "warnings": manifest["warnings"],
    }))
}
