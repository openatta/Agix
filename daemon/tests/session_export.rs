//! `session.export` over a real socket: what an archive holds, what it leaves
//! out, that it is cut at the moment the export began while a turn keeps
//! running, and where it refuses to write.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::rpc::codes;
use daemon::DaemonServer;
use futures::StreamExt;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use model::types::MessagesRequest;
use sha2::Digest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

/// The eight bytes of a PNG signature, and the base64 a host would send them as.
const PNG_BYTES: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const PNG_BASE64: &str = "iVBORw0KGgo=";

/// Answers every call with a short reply; the call numbered `held` (from 0)
/// waits for a permit on `gate` first.
struct HeldClient {
    calls: Arc<AtomicUsize>,
    held: Option<usize>,
    gate: Arc<tokio::sync::Semaphore>,
}

fn reply(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta {
                text: text.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::EndTurn),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

impl AnthropicClient for HeldClient {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = reply(&format!("reply {n}"));
        if Some(n) != self.held {
            return Box::pin(futures::stream::iter(events.into_iter().map(Ok)));
        }
        let gate = self.gate.clone();
        Box::pin(
            futures::stream::once(async move {
                let _permit = gate.acquire().await;
                events
            })
            .flat_map(|events| futures::stream::iter(events.into_iter().map(Ok))),
        )
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

struct PermitAll;

#[async_trait::async_trait]
impl base::interface::permission::PermissionPolicy for PermitAll {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

struct Daemon {
    sock: PathBuf,
    root: PathBuf,
    out: tempfile::TempDir,
    store: Arc<dyn history::store::HistoryStore>,
    calls: Arc<AtomicUsize>,
    gate: Arc<tokio::sync::Semaphore>,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

async fn start_daemon(held: Option<usize>) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("export.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = 8;
    config.settings.memory_enabled = false;

    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &cwd,
            history::path::HistoryRoots::under(&config.settings.paths.global_data_dir),
        )
        .await
        .unwrap(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let client = Arc::new(HeldClient {
        calls: calls.clone(),
        held,
        gate: gate.clone(),
    });

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::CodeScene::default()),
        daemon::Assembly {
            cwd: Some(cwd),
            model_client: Some(client),
            transcripts: daemon::Transcripts::In(store.clone()),
            permission: Some(base::interface::permission::PermissionSet::without_grants(
                Arc::new(PermitAll),
            )),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let serving = server.clone();
    let bind = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = serving.serve_unix(&bind).await;
    });
    for _ in 0..80 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");

    Daemon {
        sock,
        root: config.settings.paths.global_data_dir.clone(),
        out: tempfile::tempdir().unwrap(),
        store,
        calls,
        gate,
        _dir: dir,
        _handle: handle,
    }
}

impl Daemon {
    async fn raw(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = serde_json::json!({
            "jsonrpc": "2.0", "method": method, "params": params, "id": 1
        });
        let mut client = UnixStream::connect(&self.sock).await.unwrap();
        client
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = client.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "{method}: connection closed before a response");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                return frame;
            }
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let response = self.raw(method, params).await;
        assert!(
            response.get("result").is_some(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }

    async fn create(&self, options: serde_json::Value) -> String {
        let created = self
            .call("session.create", serde_json::json!({"options": options}))
            .await;
        created["session_id"].as_str().unwrap().to_string()
    }

    async fn turn_with_image(&self, sid: &str) {
        self.call(
            "session.run_turn",
            serde_json::json!({
                "session_id": sid,
                "message": "what is in this picture?",
                "attachments": [{"kind": "image", "media_type": "image/png", "data": PNG_BASE64}],
            }),
        )
        .await;
    }

    fn target(&self, name: &str) -> PathBuf {
        self.out.path().join(name)
    }

    async fn export(&self, params: serde_json::Value) -> serde_json::Value {
        self.raw("session.export", params).await
    }

    /// A sub-agent's session under `parent`, written the way the agent tool
    /// writes one, with one message of its own.
    async fn sidechain_of(&self, parent: &str) -> String {
        let child = base::session::SessionId::new();
        self.store
            .append(
                child,
                history::entry::LogEntry::Meta {
                    cwd: "/tmp".into(),
                    started_at: time::OffsetDateTime::now_utc(),
                    model: "claude-sonnet-4-6".into(),
                    permission_mode: "default".into(),
                    engine_version: "0.0.1".into(),
                    agix_version: "0.0.1".into(),
                    parent_session_id: Some(parent.to_string()),
                    scene: Some("code".into()),
                    project_root: None,
                    session_kind: history::entry::SessionKind::Sidechain,
                    schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
                },
            )
            .await
            .unwrap();
        self.store
            .append(
                child,
                history::entry::LogEntry::User {
                    content: vec![base::message::ContentBlock::Text {
                        text: "look around".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        child.to_string()
    }
}

/// Every file in an archive, by the name the layout gives it.
fn archive_files(path: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    if path.is_dir() {
        collect_dir(path, path, &mut out);
        return out;
    }
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        out.insert(file.name().to_string(), bytes);
    }
    out
}

fn collect_dir(base: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_dir(base, &path, out);
        } else {
            let name = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(name, std::fs::read(&path).unwrap());
        }
    }
}

fn json_of(files: &std::collections::BTreeMap<String, Vec<u8>>, name: &str) -> serde_json::Value {
    let bytes = files
        .get(name)
        .unwrap_or_else(|| panic!("{name} is not in the archive: {:?}", files.keys()));
    serde_json::from_slice(bytes).unwrap()
}

fn transcript_lines(
    files: &std::collections::BTreeMap<String, Vec<u8>>,
    sid: &str,
) -> Vec<serde_json::Value> {
    let name = format!("sessions/{sid}/transcript.jsonl");
    String::from_utf8(files[&name].clone())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn an_export_holds_the_session_its_sidechain_and_the_image_itself() {
    let daemon = start_daemon(None).await;
    let parent = daemon.create(serde_json::json!({})).await;
    daemon.turn_with_image(&parent).await;
    let child = daemon.sidechain_of(&parent).await;

    let target = daemon.target("export.agix.zip");
    let response = daemon
        .export(serde_json::json!({"session_id": parent, "path": target}))
        .await;
    let result = &response["result"];
    assert_eq!(result["format"], "zip", "{response}");
    assert_eq!(result["attachments"], 1);
    assert_eq!(result["recording"], false);

    let files = archive_files(&target);
    let manifest = json_of(&files, "manifest.json");
    assert_eq!(manifest["format"], "agix.session-export");
    assert_eq!(manifest["format_version"], 1);
    assert_eq!(manifest["root_session_id"], parent.as_str());
    assert_eq!(manifest["importable"], true);

    let sessions = manifest["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{manifest}");
    assert_eq!(sessions[0]["session_id"], parent.as_str());
    assert_eq!(sessions[0]["depth"], 0);
    assert_eq!(sessions[1]["session_id"], child.as_str());
    assert_eq!(sessions[1]["kind"], "sidechain");
    assert_eq!(sessions[1]["parent_session_id"], parent.as_str());
    assert_eq!(sessions[1]["depth"], 1);
    for sid in [&parent, &child] {
        let dir = manifest["id_map"][sid.as_str()].as_str().unwrap();
        assert_eq!(dir, format!("sessions/{sid}"));
        for file in [
            "transcript.jsonl",
            "history.json",
            "entries.json",
            "meta.json",
        ] {
            assert!(files.contains_key(&format!("{dir}/{file}")), "{dir}/{file}");
        }
        let meta = json_of(&files, &format!("{dir}/meta.json"));
        assert_eq!(meta["meta"]["kind"], "meta");
    }
    assert!(transcript_lines(&files, &child)
        .iter()
        .any(|line| line.to_string().contains("look around")));

    let sha = hex::encode(sha2::Sha256::digest(PNG_BYTES));
    let file = format!("attachments/{sha}.png");
    assert_eq!(files.get(&file).map(Vec::as_slice), Some(&PNG_BYTES[..]));
    let attachment = &manifest["attachments"][0];
    assert_eq!(attachment["file"], file.as_str());
    assert_eq!(attachment["sha256"], sha.as_str());
    assert_eq!(attachment["refs"][0]["session_id"], parent.as_str());

    let history = json_of(&files, &format!("sessions/{parent}/history.json"));
    let showing = history
        .as_array()
        .unwrap()
        .iter()
        .find(|item| {
            item["content"].as_array().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b["source"]["attachment"] == file.as_str())
            })
        })
        .unwrap_or_else(|| panic!("no history item points at the attachment: {history}"));
    assert_eq!(attachment["refs"][0]["seq"], showing["seq"]);
    assert_eq!(attachment["refs"][0]["entry_id"], showing["id"]);
    assert!(
        transcript_lines(&files, &parent)
            .iter()
            .any(|line| line.to_string().contains(PNG_BASE64)),
        "the transcript is kept as stored"
    );

    assert!(
        !files.keys().any(|name| name.starts_with("recordings/")),
        "a recording nobody asked for: {:?}",
        files.keys()
    );
}

#[tokio::test]
async fn without_attachments_an_image_becomes_a_note_saying_what_it_was() {
    let daemon = start_daemon(None).await;
    let sid = daemon.create(serde_json::json!({})).await;
    daemon.turn_with_image(&sid).await;

    let target = daemon.target("bare.zip");
    let response = daemon
        .export(serde_json::json!({"session_id": sid, "path": target, "include": []}))
        .await;
    assert_eq!(response["result"]["attachments"], 0, "{response}");

    let files = archive_files(&target);
    assert!(!files.keys().any(|name| name.starts_with("attachments/")));
    let manifest = json_of(&files, "manifest.json");
    assert_eq!(manifest["importable"], false);
    let transcript =
        String::from_utf8(files[&format!("sessions/{sid}/transcript.jsonl")].clone()).unwrap();
    assert!(!transcript.contains(PNG_BASE64));
    assert!(transcript.contains("image omitted from export: image/png, 8 bytes"));
}

#[tokio::test]
async fn a_recording_is_in_the_archive_only_when_it_is_asked_for() {
    let daemon = start_daemon(None).await;
    let recordings = daemon.out.path().join("recordings-root");
    let sid = daemon
        .create(serde_json::json!({"recorder": {"mode": "record", "dir": recordings}}))
        .await;
    daemon
        .call(
            "session.run_turn",
            serde_json::json!({"session_id": sid, "message": "hi"}),
        )
        .await;
    assert!(recordings.join(&sid).join("calls.jsonl").exists());

    let without = daemon.target("without.zip");
    daemon
        .export(serde_json::json!({"session_id": sid, "path": without}))
        .await;
    assert!(!archive_files(&without)
        .keys()
        .any(|name| name.starts_with("recordings/")));

    let with = daemon.target("with.zip");
    let response = daemon
        .export(serde_json::json!({
            "session_id": sid, "path": with,
            "include": ["sidechains", "attachments", "recording"],
        }))
        .await;
    assert_eq!(response["result"]["recording"], true, "{response}");
    let files = archive_files(&with);
    let calls = String::from_utf8(files[&format!("recordings/{sid}/calls.jsonl")].clone()).unwrap();
    assert!(calls.contains("\"type\":\"call\""), "{calls}");
    let manifest = json_of(&files, "manifest.json");
    assert_eq!(
        manifest["recordings"][0]["dir"],
        format!("recordings/{sid}")
    );
    assert!(manifest["recordings"][0]["calls"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn an_export_during_a_turn_holds_what_was_written_before_it_began() {
    let daemon = Arc::new(start_daemon(Some(0)).await);
    let sid = daemon.create(serde_json::json!({})).await;

    let running = {
        let daemon = daemon.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            daemon
                .call(
                    "session.run_turn",
                    serde_json::json!({"session_id": sid, "message": "take your time"}),
                )
                .await
        })
    };
    for _ in 0..200 {
        if daemon.calls.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        daemon.calls.load(Ordering::SeqCst),
        1,
        "the turn never reached the model"
    );

    let target = daemon.target("mid-turn.zip");
    let response = daemon
        .export(serde_json::json!({"session_id": sid, "path": target}))
        .await;
    assert!(response["result"].is_object(), "{response}");

    daemon.gate.add_permits(1);
    running.await.unwrap();

    let files = archive_files(&target);
    let manifest = json_of(&files, "manifest.json");
    let row = &manifest["sessions"][0];
    assert_eq!(row["active"], true);
    assert_eq!(row["turn_state"], "running");
    let transcript = transcript_lines(&files, &sid);
    assert!(transcript
        .iter()
        .any(|line| line.to_string().contains("take your time")));
    assert!(!transcript
        .iter()
        .any(|line| line.to_string().contains("reply 0")));

    let after = daemon
        .store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    assert!(
        row["entry_count"].as_u64().unwrap() < after.len() as u64,
        "the turn kept writing after the cut"
    );
    assert!(after
        .iter()
        .any(|env| serde_json::to_string(env).unwrap().contains("reply 0")));
}

#[tokio::test]
async fn a_directory_export_has_the_same_layout_as_a_zip() {
    let daemon = start_daemon(None).await;
    let sid = daemon.create(serde_json::json!({})).await;
    daemon.turn_with_image(&sid).await;

    let zip = daemon.target("same.zip");
    let dir = daemon.target("same");
    daemon
        .export(serde_json::json!({"session_id": sid, "path": zip}))
        .await;
    let response = daemon
        .export(serde_json::json!({"session_id": sid, "path": dir, "format": "dir"}))
        .await;
    assert_eq!(response["result"]["format"], "dir", "{response}");
    assert!(dir.join("manifest.json").is_file());

    let names = |files: std::collections::BTreeMap<String, Vec<u8>>| -> BTreeSet<String> {
        files.into_keys().collect()
    };
    assert_eq!(names(archive_files(&zip)), names(archive_files(&dir)));
}

#[tokio::test]
async fn an_export_refuses_a_target_it_would_overwrite_or_misplace() {
    let daemon = start_daemon(None).await;
    let sid = daemon.create(serde_json::json!({})).await;
    daemon
        .call(
            "session.run_turn",
            serde_json::json!({"session_id": sid, "message": "hi"}),
        )
        .await;

    let existing = daemon.target("taken.zip");
    std::fs::write(&existing, b"somebody's file").unwrap();
    let refusals = [
        serde_json::json!({"session_id": sid, "path": existing}),
        serde_json::json!({"session_id": sid, "path": "relative/export.zip"}),
        serde_json::json!({"session_id": sid, "path": daemon.target("no/such/dir/x.zip")}),
        serde_json::json!({"session_id": sid, "path": daemon.root.join("projects").join("x.zip")}),
        serde_json::json!({"session_id": sid, "path": daemon.target("a.zip"), "include": ["everything"]}),
        serde_json::json!({"session_id": sid, "path": daemon.target("a.zip"), "format": "tar"}),
        serde_json::json!({"session_id": sid}),
    ];
    for params in refusals {
        let response = daemon.export(params.clone()).await;
        assert_eq!(
            response["error"]["code"],
            codes::INVALID_PARAMS,
            "{params} -> {response}"
        );
    }
    assert_eq!(std::fs::read(&existing).unwrap(), b"somebody's file");

    let unknown = daemon
        .export(serde_json::json!({
            "session_id": base::session::SessionId::new().to_string(),
            "path": daemon.target("unknown.zip"),
        }))
        .await;
    assert_eq!(unknown["error"]["code"], codes::SESSION_NOT_FOUND);

    let left: Vec<String> = std::fs::read_dir(daemon.out.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        left,
        ["taken.zip"],
        "a refused export left something behind"
    );
}
