//! The response examples in `docs/daemon_rpc_protocol.md` are real responses.
//!
//! `protocol_doc_matches_dispatch.rs` keeps the *set* of documented methods
//! honest; nothing kept the *shapes* honest, and they had drifted badly —
//! documented fields no struct had, documented `error.data` no code path
//! attached, whole parameters (`cursor`, `before_seq`, `status`) that were
//! never read. A client is written against those examples, and none of them
//! is compiled.
//!
//! So each example marked `// ← result` is read back out of the file and
//! compared, field by field, against what a real daemon answers over a real
//! socket. The example may say *less* than the response — a new field does
//! not break the doc — but it may not say anything different.
//!
//! `daemon.doctor` is checked the same way by `daemon/src/doctor.rs`'s
//! `documented_shape_tests`, which can call `run_doctor` directly and so
//! doesn't need a socket.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use model::types::MessagesRequest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

// ── The doc side ────────────────────────────────────────────────────────

fn protocol_doc() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .join("docs")
        .join("daemon_rpc_protocol.md");
    std::fs::read_to_string(&path).expect("protocol doc is readable")
}

/// The section under `#### \`method\``, up to the next heading of any level.
fn section<'a>(doc: &'a str, method: &str) -> &'a str {
    let heading = format!("`{method}`");
    let start = doc
        .lines()
        .scan(0usize, |offset, line| {
            let here = *offset;
            *offset += line.len() + 1;
            Some((here, line))
        })
        .find(|(_, line)| line.starts_with("#### ") && line.contains(&heading))
        .unwrap_or_else(|| panic!("the doc no longer has a `#### {heading}` heading"))
        .0;
    let rest = &doc[start..];
    let body = &rest[rest.find('\n').map(|i| i + 1).unwrap_or(rest.len())..];
    match body.find("\n#") {
        Some(end) => &body[..end],
        None => body,
    }
}

/// Everything after the `// ← result` marker in this section's first fenced
/// block that carries one, with `//` annotations removed.
///
/// Quote-aware, because a `//` inside a string would otherwise take the rest
/// of the line with it.
fn documented_result(doc: &str, method: &str) -> serde_json::Value {
    let body = section(doc, method);
    let fenced = body
        .split("```jsonc\n")
        .skip(1)
        .filter_map(|rest| rest.split_once("\n```").map(|(block, _)| block))
        .find(|block| {
            block
                .lines()
                .any(|l| l.trim_start().starts_with("// ← result"))
        })
        .unwrap_or_else(|| panic!("`{method}`'s section no longer carries a `// ← result` block"));

    let after_marker = fenced
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("// ← result"))
        .skip(1);

    let mut json = String::new();
    for line in after_marker {
        json.push_str(strip_line_comment(line));
        json.push('\n');
    }
    serde_json::from_str(&json).unwrap_or_else(|e| {
        panic!("`{method}`'s documented result does not parse as JSON ({e}):\n{json}")
    })
}

fn strip_line_comment(line: &str) -> &str {
    let chars: Vec<char> = line.chars().collect();
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in chars.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '/' if !in_string && chars.get(i + 1) == Some(&'/') => {
                let byte = line
                    .char_indices()
                    .nth(i)
                    .map(|(b, _)| b)
                    .unwrap_or(line.len());
                return &line[..byte];
            }
            _ => {}
        }
    }
    line
}

/// A documented string containing `…` stands for a value that varies by
/// machine or moment — only the field's presence is claimed, not its value
/// or type. An empty documented object/array claims the shape and nothing
/// about the contents.
fn is_placeholder(v: &serde_json::Value) -> bool {
    v.as_str().is_some_and(|s| s.contains('…'))
}

/// Every field the example writes must be in `actual` at the same path with
/// the same value. Arrays match element-wise by *content*, not position:
/// each documented element must match some element of the response, since
/// several of these lists have no defined order.
fn assert_documented(path: &str, documented: &serde_json::Value, actual: &serde_json::Value) {
    if let Err(why) = check(path, documented, actual) {
        panic!("{why}\ndocumented: {documented}\nresponse:   {actual}");
    }
}

fn check(
    path: &str,
    documented: &serde_json::Value,
    actual: &serde_json::Value,
) -> Result<(), String> {
    if is_placeholder(documented) {
        return Ok(());
    }
    match documented {
        serde_json::Value::Object(fields) => {
            let actual = actual
                .as_object()
                .ok_or_else(|| format!("{path}: documented as an object, response is not"))?;
            for (key, value) in fields {
                let found = actual
                    .get(key)
                    .ok_or_else(|| format!("{path}.{key}: documented, absent from response"))?;
                check(&format!("{path}.{key}"), value, found)?;
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            let actual = actual
                .as_array()
                .ok_or_else(|| format!("{path}: documented as an array, response is not"))?;
            for (i, item) in items.iter().enumerate() {
                if !actual.iter().any(|a| check(path, item, a).is_ok()) {
                    return Err(format!(
                        "{path}[{i}]: no element of the response matches this documented one"
                    ));
                }
            }
            Ok(())
        }
        scalar if scalar == actual => Ok(()),
        scalar => Err(format!(
            "{path}: documented {scalar}, response has {actual}"
        )),
    }
}

// ── The daemon side ─────────────────────────────────────────────────────

/// One canned assistant turn: says "ok" and stops. `session.run_turn` has to
/// reach `turn_complete` for its response shape to exist at all.
struct OneTurnClient;

impl AnthropicClient for OneTurnClient {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::TextDelta { text: "ok".into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: Some(Usage::default()),
            },
        ];
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

struct Daemon {
    sock: std::path::PathBuf,
    project: std::path::PathBuf,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

async fn start_daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("doc.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "coding",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = 8;
    // Post-turn memory extraction would fire a second model call and slow the
    // one turn this test runs; nothing here reads memory.
    config.settings.memory_enabled = false;

    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &cwd,
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .unwrap(),
    );

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::scene::coding::CodingScene),
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            model_client: Some(Arc::new(OneTurnClient)),
            transcripts: daemon::Transcripts::In(store),
            permission: Some(Arc::new(PermitAll)),
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
        project: cwd,
        _dir: dir,
        _handle: handle,
    }
}

struct PermitAll;

#[async_trait::async_trait]
impl base::interface::permission::Permission for PermitAll {
    async fn check(
        &self,
        _tool: &str,
        _input: &serde_json::Value,
        _cwd: &Path,
        _session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

impl Daemon {
    /// One call, draining any stream frames, returning the whole response.
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
}

/// Check one method's documented result against a live response.
async fn verify(
    doc: &str,
    daemon: &Daemon,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let actual = daemon.call(method, params).await;
    assert_documented(method, &documented_result(doc, method), &actual);
    actual
}

// ── The walk ────────────────────────────────────────────────────────────

/// Everything a client can reach without a session, in the order §11 walks
/// them. `daemon.status`'s documented `"sessions": 0` is why this runs
/// before anything is created.
#[tokio::test]
async fn documented_daemon_scene_and_config_results_are_real() {
    let doc = protocol_doc();
    let daemon = start_daemon().await;

    verify(&doc, &daemon, "daemon.ping", serde_json::json!({})).await;
    verify(&doc, &daemon, "daemon.status", serde_json::json!({})).await;
    verify(
        &doc,
        &daemon,
        "daemon.subscribeEvents",
        serde_json::json!({}),
    )
    .await;

    verify(&doc, &daemon, "scene.list", serde_json::json!({})).await;
    verify(
        &doc,
        &daemon,
        "scene.describe",
        serde_json::json!({"scene": "coding", "project_root": null}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "scene.activate",
        serde_json::json!({"scene": "chat"}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "scene.deactivate",
        serde_json::json!({"scene": "chat"}),
    )
    .await;

    verify(
        &doc,
        &daemon,
        "config.get",
        serde_json::json!({"scene": "coding", "tier": "effective"}),
    )
    .await;
    verify(&doc, &daemon, "config.getProvider", serde_json::json!({})).await;
    verify(&doc, &daemon, "config.reload", serde_json::json!({})).await;

    verify(&doc, &daemon, "mcp.status", serde_json::json!({})).await;
    verify(&doc, &daemon, "commands.list", serde_json::json!({})).await;
    verify(&doc, &daemon, "import.list", serde_json::json!({})).await;

    // `plugin.list` answers PLUGINS_DISABLED in a build without the carrier,
    // which is most builds — the feature is off by default. Its documented
    // shape is only claimed for a build that has it.
    let plugins = daemon.raw("plugin.list", serde_json::json!({})).await;
    if let Some(result) = plugins.get("result") {
        assert_documented(
            "plugin.list",
            &documented_result(&doc, "plugin.list"),
            result,
        );
    } else {
        assert_eq!(
            plugins["error"]["code"],
            daemon::rpc::codes::PLUGINS_DISABLED,
            "plugin.list neither answered nor refused for the documented reason: {plugins}"
        );
    }
}

/// The session surface, in an order that leaves each method something to
/// answer: read state before the turn that would change it, branch before
/// deleting the thing branched from.
#[tokio::test]
async fn documented_session_results_are_real() {
    let doc = protocol_doc();
    let daemon = start_daemon().await;
    let project = daemon.project.display().to_string();

    let created = verify(
        &doc,
        &daemon,
        "session.create",
        serde_json::json!({"scene": "coding", "project_root": project}),
    )
    .await;
    let sid = created["session_id"].as_str().unwrap().to_string();

    // Before the turn: `session.get` documents `name: null` and
    // `turn_state: "idle"`, both of which a turn can change.
    verify(
        &doc,
        &daemon,
        "session.get",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    verify(&doc, &daemon, "session.list", serde_json::json!({})).await;
    verify(
        &doc,
        &daemon,
        "session.subscribe",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "session.unsubscribe",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "session.interrupt",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    // A `prompt_id` nobody is waiting on is a silent success, which is the
    // documented behaviour and what makes this callable with no live prompt.
    verify(
        &doc,
        &daemon,
        "session.respondToPrompt",
        serde_json::json!({"session_id": sid, "prompt_id": "p-none",
                           "decision": {"type": "permit"}}),
    )
    .await;
    // Still in memory, so `status` is the documented `already_active`.
    verify(
        &doc,
        &daemon,
        "session.resume",
        serde_json::json!({"session_id": sid}),
    )
    .await;

    verify(
        &doc,
        &daemon,
        "session.run_turn",
        serde_json::json!({"session_id": sid, "turn_id": "t-doc", "message": "hi"}),
    )
    .await;

    verify(
        &doc,
        &daemon,
        "session.history",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "session.appendEntry",
        serde_json::json!({"session_id": sid, "ns": "com.example.cards",
                           "event": "job_done", "payload": {}}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "session.entries",
        serde_json::json!({"session_id": sid, "namespaces": ["com.example.cards"]}),
    )
    .await;
    let fork = verify(
        &doc,
        &daemon,
        "session.fork",
        serde_json::json!({"session_id": sid}),
    )
    .await;

    // Both examples say `sidechains_deleted: 0`, so both run against a
    // session nothing is recorded as a child of. `sid` is not one: the fork
    // above records it as its parent, and the cascade does not distinguish a
    // fork from a sidechain — see the note under `session.fork` in the doc.
    let spare = daemon
        .call(
            "session.create",
            serde_json::json!({"scene": "coding", "project_root": project}),
        )
        .await;
    verify(
        &doc,
        &daemon,
        "session.close",
        serde_json::json!({"session_id": spare["session_id"]}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "session.delete",
        serde_json::json!({"session_id": fork["session_id"], "dry_run": false}),
    )
    .await;
}

/// Its own daemon: this one ends the process's listener.
#[tokio::test]
async fn the_documented_shutdown_result_is_real() {
    let doc = protocol_doc();
    let daemon = start_daemon().await;
    verify(&doc, &daemon, "daemon.shutdown", serde_json::json!({})).await;
}
