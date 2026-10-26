//! Session lifecycle RPCs (`session.history` / `.fork` / `.resume`) and the
//! `ask`-by-default permission loop, end to end over the real socket.
//!
//! Separate from `daemon_e2e.rs` because everything here needs a
//! **deterministic turn**: a real `session.run_turn` that drives the engine,
//! the tool loop and the permission gate without touching the network.
//! `SessionPool` builds its model from the `Arc<dyn AnthropicClient>` it is
//! handed, so a scripted client is the entire seam — no production code
//! exists for testing's sake, and the canned events still travel through the
//! same `AnthropicModel` adapter a real call would.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use base::interface::settings::Settings;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::rpc::codes;
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{
    BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage as WireUsage,
};
use model::types::MessagesRequest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

// ── Scripted model ──────────────────────────────────────────────────────

/// An `AnthropicClient` that replays one canned SSE-event batch per call, in
/// order, then repeats the last one forever — so an unexpected extra round
/// trip ends the turn instead of hanging the test.
struct ScriptedClient {
    rounds: StdMutex<std::collections::VecDeque<Vec<StreamEvent>>>,
    last: StdMutex<Vec<StreamEvent>>,
    /// Every request the engine made, for tests that assert on what the
    /// model was actually shown (e.g. "did the fork carry the transcript?").
    seen: Arc<StdMutex<Vec<MessagesRequest>>>,
}

type SeenRequests = Arc<StdMutex<Vec<MessagesRequest>>>;

impl ScriptedClient {
    fn new(rounds: Vec<Vec<StreamEvent>>) -> (Arc<Self>, SeenRequests) {
        let seen: SeenRequests = Arc::new(StdMutex::new(Vec::new()));
        let last = rounds.last().cloned().unwrap_or_else(|| text_round("done"));
        (
            Arc::new(Self {
                rounds: StdMutex::new(rounds.into()),
                last: StdMutex::new(last),
                seen: seen.clone(),
            }),
            seen,
        )
    }
}

impl AnthropicClient for ScriptedClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        self.seen.lock().unwrap().push(req);
        let events = {
            let mut q = self.rounds.lock().unwrap();
            q.pop_front()
                .unwrap_or_else(|| self.last.lock().unwrap().clone())
        };
        if events.is_empty() {
            return Box::pin(futures::stream::pending());
        }
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

/// One assistant turn that says `text` and stops.
fn text_round(text: &str) -> Vec<StreamEvent> {
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
            usage: Some(WireUsage::default()),
        },
    ]
}

/// The same, reporting what the call cost.
///
/// A turn is several calls and a provider reports usage per call, so a round
/// that always reports zero cannot tell a sum from a last value — which is the
/// whole question for anyone budgeting on the number.
fn text_round_costing(text: &str, input_tokens: u64, output_tokens: u64) -> Vec<StreamEvent> {
    let mut round = text_round(text);
    if let Some(StreamEvent::MessageDelta { usage, .. }) = round.last_mut() {
        *usage = Some(WireUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        });
    }
    round
}

/// A tool round that reports what it cost, for the same reason.
fn tool_round_costing(
    tool: &str,
    id: &str,
    input: serde_json::Value,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<StreamEvent> {
    let mut round = tool_round(tool, id, input);
    if let Some(StreamEvent::MessageDelta { usage, .. }) = round.last_mut() {
        *usage = Some(WireUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        });
    }
    round
}

/// One assistant turn that calls `tool` with `input` and stops on `tool_use`.
fn tool_round(tool: &str, id: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: tool.to_string(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::ToolUse),
                stop_sequence: None,
            },
            usage: Some(WireUsage::default()),
        },
    ]
}

/// A turn where the model calls `Bash` with a command the classifier can't
/// vouch for — `touch` is in neither the read-only nor the destructive list,
/// so `BashTool::check_permissions` returns `Ask` and the gate falls through
/// to mode dispatch, which is exactly the path the new default changes.
fn asking_bash_rounds(marker: &str) -> Vec<Vec<StreamEvent>> {
    vec![
        tool_round(
            "Bash",
            "tool-1",
            serde_json::json!({"command": format!("touch {marker}")}),
        ),
        text_round("all done"),
    ]
}

// ── Harness ─────────────────────────────────────────────────────────────

struct ScriptedServer {
    server: Arc<DaemonServer>,
    sock: PathBuf,
    _dir: tempfile::TempDir,
    handle: tokio::task::JoinHandle<()>,
    store: Arc<dyn history::store::HistoryStore>,
}

impl ScriptedServer {
    async fn stop(self) {
        self.server.shutdown_token().cancel();
        let _ = self.handle.await;
    }
}

/// A daemon whose model is scripted, with a real `HistoryStore` (handed back
/// so tests can inspect transcripts directly) and caller-supplied `Settings`
/// so the permission posture under test is explicit rather than ambient.
async fn start_scripted_server(
    rounds: Vec<Vec<StreamEvent>>,
    settings: Settings,
    prompt_timeout: Duration,
) -> (ScriptedServer, SeenRequests) {
    start_scripted_server_in("coding", rounds, settings, prompt_timeout).await
}

/// The same daemon in a named scene — `chat` names its own sessions after the
/// first turn, which `coding` does not, and that difference is a behaviour
/// worth a test.
async fn start_scripted_server_in(
    scene_id: &str,
    rounds: Vec<Vec<StreamEvent>>,
    settings: Settings,
    prompt_timeout: Duration,
) -> (ScriptedServer, SeenRequests) {
    // Silent unless `RUST_LOG` says otherwise. These tests drive the whole
    // engine, so being able to turn logging on without editing the file is
    // worth the four lines — it is how the `respondToPrompt` deadlock this
    // module's `permission_prompt` route works around was diagnosed.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let (client, seen) = ScriptedClient::new(rounds);
    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        scene_id,
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = 8;
    // The caller's settings, kept inside this test's directory: `Settings::
    // defaults_for` carries a literal `~/.agix`, and the assembly places the
    // memory store and the history roots from exactly these paths.
    let settings_paths = config.settings.paths.clone();
    config.settings = settings;
    config.settings.paths = settings_paths;

    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &cwd,
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .unwrap(),
    );

    // The entry point `main.rs` uses: these tests are about the behaviour of
    // an assembled daemon, and an assembly of their own would be one more
    // copy to keep in step with it.
    let mut registry = scene::scene::SceneRegistry::new();
    registry.register_builtin();
    let agent_scene = registry.resolve(scene_id).expect("a builtin scene");

    let pool = daemon::assemble::pool(
        &config,
        agent_scene,
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            permission_prompt_timeout: prompt_timeout,
            model_client: Some(client),
            transcripts: daemon::Transcripts::In(store.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });
    for _ in 0..60 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");

    (
        ScriptedServer {
            server,
            sock,
            _dir: dir,
            handle,
            store,
        },
        seen,
    )
}

/// The same daemon shape but with `history_store: None` — the degraded mode
/// `main.rs` falls into when `JsonlHistoryStore::with_root` fails at
/// startup, where the session read/branch surface has no possible answer.
async fn start_server_without_history() -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock");
    let (client, _seen) = ScriptedClient::new(vec![text_round("x")]);
    let config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "coding",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::scene::coding::CodingScene),
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::Nowhere,
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });
    for _ in 0..60 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");
    (server, sock, dir, handle)
}

/// One request, one response line (for methods that don't stream).
async fn rpc(sock: &std::path::Path, msg: &str) -> serde_json::Value {
    let mut client = UnixStream::connect(sock).await.unwrap();
    client.write_all(msg.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    let mut buf = String::new();
    br.read_line(&mut buf).await.unwrap();
    serde_json::from_str(&buf).unwrap()
}

/// Send one request and drain the connection until the final response (the
/// line carrying an `id`), returning `(stream_frames, response)`.
async fn rpc_streaming(
    sock: &std::path::Path,
    msg: &str,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let mut client = UnixStream::connect(sock).await.unwrap();
    client.write_all(msg.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _w) = client.split();
    let mut br = BufReader::new(r);
    let mut frames = Vec::new();
    loop {
        let mut line = String::new();
        let n = br.read_line(&mut line).await.unwrap();
        assert!(n > 0, "connection closed before a final response arrived");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("id").is_some() {
            return (frames, v);
        }
        frames.push(v);
    }
}

fn events_of_kind<'a>(frames: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    frames
        .iter()
        .filter(|f| f["params"]["event"]["kind"] == kind)
        .map(|f| &f["params"]["event"])
        .collect()
}

/// Run one turn to completion, returning the session id.
async fn run_turn(sock: &std::path::Path, session_id: Option<&str>, message: &str) -> String {
    let params = match session_id {
        Some(sid) => format!(r#"{{"session_id":"{sid}","message":"{message}"}}"#),
        None => format!(r#"{{"message":"{message}"}}"#),
    };
    let (_frames, resp) = rpc_streaming(
        sock,
        &format!(r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{params},"id":1}}"#),
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");
    resp["result"]["session_id"].as_str().unwrap().to_string()
}

async fn history(sock: &std::path::Path, sid: &str) -> serde_json::Value {
    rpc(
        sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","limit":500}},"id":9}}"#
        ),
    )
    .await["result"]
        .clone()
}

/// `Settings` at defaults — notably `permission_mode` unset, which is now
/// `Default` (= ask), the posture under test.
fn ask_settings() -> Settings {
    Settings::defaults_for("claude-sonnet-4-6")
}

/// `ask_settings()` with post-turn memory extraction switched off — for
/// tests that inspect the *last* captured `MessagesRequest` and would
/// otherwise pick up the extraction call's fixed cheap model instead of the
/// main turn's.
fn ask_settings_no_memory() -> Settings {
    let mut s = ask_settings();
    s.memory_enabled = false;
    s
}

// ── session.list / session.get identity ─────────────────────────────────

/// A list a host can group by. Scene and project come from the session's own
/// `Meta` line, so an entry answers "which project, which scene" without the
/// caller asking `session.get` once per row — which was the only way to find
/// out, and impossible for a session that is only on disk.
#[tokio::test]
async fn a_listed_session_says_which_scene_and_project_it_belongs_to() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("answer")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hello").await;

    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":1}"#,
    )
    .await;
    let entry = listed["result"]["sessions"]
        .as_array()
        .expect("a sessions array")
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .unwrap_or_else(|| panic!("the session that just ran is not listed: {listed}"))
        .clone();

    assert_eq!(entry["scene"], "coding");
    let project_root = entry["project_root"]
        .as_str()
        .unwrap_or_else(|| panic!("the entry names no project: {entry}"));
    assert!(
        project_root.ends_with("work"),
        "the entry names the project the session was created in, got {project_root}"
    );

    // The same two fields, for the same session, out of the same place.
    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","id":2,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(detail["result"]["scene"], entry["scene"]);
    assert_eq!(detail["result"]["project_root"], entry["project_root"]);

    srv.stop().await;
}

// ── session.list counters ───────────────────────────────────────────────

/// How many times a conversation has been compacted is a by-product of a
/// write that already happened, and it reached nobody: the marker went into
/// the transcript and the count into a field with no writer. Counted as the
/// log is written, so a session that has left memory answers too.
#[tokio::test]
async fn a_listed_session_reports_how_often_it_was_compacted() {
    let (srv, _seen) = start_scripted_server_in(
        "coding",
        vec![text_round("answer")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "hello").await;

    // The engine writes this marker when it compacts; here it stands in for
    // one, since making a real turn cross the threshold needs a scene built
    // for it (see `session_event_compact.rs`).
    srv.store
        .append(
            base::session::SessionId::parse(&sid).unwrap(),
            history::entry::LogEntry::Compact {
                before_tokens: 100,
                after_tokens: 40,
                summary_block_id: None,
                replacement_history: None,
                summary: None,
                snip_removed_uuids: None,
            },
        )
        .await
        .expect("the marker lands");

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":1,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;

    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":2}"#,
    )
    .await;
    let entry = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("session missing from the list: {listed}"));

    assert_eq!(
        entry["status"], "inactive",
        "the point is the session that is only on disk: {entry}"
    );
    assert_eq!(
        entry["compact_count"], 1,
        "the compaction was counted where a listing can read it: {entry}"
    );

    srv.stop().await;
}

// ── session.rename ──────────────────────────────────────────────────────

/// A session's name belongs to the person looking at it, and it has to
/// survive that person closing the window. Kept beside the log rather than in
/// a host's own overlay, so two hosts sharing `~/.agix` agree about what a
/// session is called.
#[tokio::test]
async fn a_renamed_session_keeps_its_name_after_it_leaves_memory() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("answer")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hello").await;
    let renamed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.rename","id":1,"params":{{"session_id":"{sid}","name":"重构 daemon 装配"}}}}"#
        ),
    )
    .await;
    assert_eq!(renamed["result"]["name"], "重构 daemon 装配");

    let listed_live = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":2}"#,
    )
    .await;
    let live_entry = listed_live["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("session missing from the list: {listed_live}"));
    assert_eq!(live_entry["name"], "重构 daemon 装配");

    // Out of memory entirely — the case a host's own overlay gets wrong,
    // since it never had a row for a session it did not open.
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":3,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    let listed_cold = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":4}"#,
    )
    .await;
    let cold_entry = listed_cold["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("session missing from the list: {listed_cold}"));
    assert_eq!(
        cold_entry["status"], "inactive",
        "the session really did leave memory: {cold_entry}"
    );
    assert_eq!(
        cold_entry["name"], "重构 daemon 装配",
        "a name outlives the running session: {cold_entry}"
    );

    // Clearing puts it back to whatever names it automatically.
    let cleared = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.rename","id":5,"params":{{"session_id":"{sid}","name":null}}}}"#
        ),
    )
    .await;
    assert_eq!(cleared["result"]["name"], serde_json::Value::Null);

    srv.stop().await;
}

#[tokio::test]
async fn a_name_that_would_break_a_list_is_refused() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("answer")], ask_settings(), Duration::ZERO).await;
    let sid = run_turn(&srv.sock, None, "hello").await;

    for bad in [r#""  ""#, r#""a\nb""#] {
        let response = rpc(
            &srv.sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"session.rename","id":1,"params":{{"session_id":"{sid}","name":{bad}}}}}"#
            ),
        )
        .await;
        assert_eq!(
            response["error"]["code"],
            codes::INVALID_PARAMS as i64,
            "`{bad}` is not a name: {response}"
        );
    }

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.rename","id":2,"params":{"session_id":"WnZ55VTERDNsFKVQdkQ7hi","name":"x"}}"#,
    )
    .await;
    assert_eq!(
        missing["error"]["code"], codes::SESSION_NOT_FOUND as i64,
        "naming a session that does not exist would put an unopenable row in every listing: {missing}"
    );

    srv.stop().await;
}

/// A scene that names its own sessions does so after the first turn — by
/// which time the user may already have typed a name. The guess must not win:
/// it arrives second and it is a guess.
#[tokio::test]
async fn the_automatic_namer_does_not_overwrite_a_name_somebody_typed() {
    let (srv, _seen) = start_scripted_server_in(
        "chat",
        vec![text_round("answer"), text_round("模型猜的标题")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{}}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no session: {created}"))
        .to_string();

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.rename","id":2,"params":{{"session_id":"{sid}","name":"我起的名字"}}}}"#
        ),
    )
    .await;

    // The turn that would otherwise trigger automatic naming.
    run_turn(&srv.sock, Some(&sid), "hello").await;

    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","id":3,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(
        detail["result"]["name"], "我起的名字",
        "the name somebody typed survived the turn: {detail}"
    );

    srv.stop().await;
}

// ── session.subscribe ───────────────────────────────────────────────────

/// Opening a session that is only on disk is the most ordinary thing a client
/// does, and it used to be three calls with a failure in the middle: subscribe,
/// recognize `SESSION_NOT_FOUND`, resume, subscribe again. Every client wrote
/// that sequence, and the details it has to get right (which scene, which
/// project) are the ones it is worst placed to know.
#[tokio::test]
async fn subscribing_with_resume_opens_a_session_that_left_memory() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("answer")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hello").await;
    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":1,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], sid.as_str());

    // The default is unchanged: subscribing does not resume, and says so
    // rather than handing back a subscription that will never see a frame.
    let refused = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","id":2,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        codes::SESSION_NOT_FOUND as i64,
        "subscribe still refuses a cold session by default: {refused}"
    );

    let opened = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","id":3,"params":{{"session_id":"{sid}","resume":true}}}}"#
        ),
    )
    .await;
    assert!(
        opened.get("result").is_some(),
        "subscribe with resume opens it: {opened}"
    );
    assert_eq!(opened["result"]["session_id"], sid.as_str());
    assert_eq!(
        opened["result"]["resumed"], true,
        "the caller is told a resume happened on its behalf: {opened}"
    );
    assert_eq!(
        opened["result"]["scene_inferred"], false,
        "this transcript records its scene, so nothing was inferred: {opened}"
    );

    // Already live now: the same call subscribes without resuming again.
    let again = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","id":4,"params":{{"session_id":"{sid}","resume":true}}}}"#
        ),
    )
    .await;
    assert_eq!(
        again["result"]["resumed"], false,
        "a live session is subscribed to, not resumed: {again}"
    );

    srv.stop().await;
}

// ── session.history ─────────────────────────────────────────────────────

#[tokio::test]
async fn session_history_returns_the_real_transcript() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("first answer")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "hello there").await;
    let result = history(&srv.sock, &sid).await;

    assert_eq!(result["session_id"], sid);
    assert_eq!(
        result["active"], true,
        "the session is still live: {result}"
    );

    let messages = result["messages"].as_array().expect("messages array");
    assert!(
        messages.len() >= 2,
        "expected at least the user turn and the assistant reply: {result}"
    );
    let rendered = serde_json::to_string(messages).unwrap();
    assert!(
        rendered.contains("hello there"),
        "user turn missing: {rendered}"
    );
    assert!(
        rendered.contains("first answer"),
        "assistant turn missing: {rendered}"
    );
    assert_eq!(result["total"], messages.len());
    assert_eq!(result["has_more"], false);

    srv.stop().await;
}

#[tokio::test]
async fn session_history_respects_its_bound_and_pages() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("reply")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "turn one").await;
    run_turn(&srv.sock, Some(&sid), "turn two").await;
    run_turn(&srv.sock, Some(&sid), "turn three").await;

    // `limit: 0` is the cheap "just tell me how big this is" probe the
    // pagination contract documents.
    let probe = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","limit":0}},"id":2}}"#
        ),
    )
    .await;
    let total = probe["result"]["total"].as_u64().unwrap() as usize;
    assert!(
        total >= 6,
        "three turns should project at least six messages: {probe}"
    );
    assert!(probe["result"]["messages"].as_array().unwrap().is_empty());
    assert_eq!(probe["result"]["has_more"], true);

    // A bounded page really is bounded, and says there's more.
    let page = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","offset":0,"limit":2}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(page["result"]["messages"].as_array().unwrap().len(), 2);
    assert_eq!(page["result"]["total"], total as u64);
    assert_eq!(page["result"]["has_more"], true);

    // The tail, addressed the way the docs say to address it.
    let tail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","offset":{},"limit":2}},"id":4}}"#,
            total - 2
        ),
    )
    .await;
    assert_eq!(tail["result"]["messages"].as_array().unwrap().len(), 2);
    assert_eq!(tail["result"]["has_more"], false);
    assert_ne!(
        tail["result"]["messages"], page["result"]["messages"],
        "the tail page must not be the head page"
    );

    // An over-large limit clamps rather than erroring, and reports what was
    // actually applied.
    let clamped = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","limit":100000}},"id":5}}"#
        ),
    )
    .await;
    assert_eq!(clamped["result"]["limit"], 500);

    srv.stop().await;
}

#[tokio::test]
async fn session_history_errors_cleanly_on_unknown_malformed_and_missing_ids() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    // Well-formed id, never used.
    let unknown = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(
        unknown["error"]["code"],
        codes::SESSION_NOT_FOUND,
        "{unknown}"
    );

    // Not a BASE58 id at all — a caller mistake, not a missing session.
    let malformed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{"session_id":"not-an-id!!"},"id":2}"#,
    )
    .await;
    assert_eq!(
        malformed["error"]["code"],
        codes::INVALID_PARAMS,
        "{malformed}"
    );

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{},"id":3}"#,
    )
    .await;
    assert_eq!(missing["error"]["code"], codes::INVALID_PARAMS, "{missing}");

    srv.stop().await;
}

#[tokio::test]
async fn session_history_is_unavailable_without_a_history_store() {
    let (server, sock, _dir, handle) = start_server_without_history().await;
    let v = rpc(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(v["error"]["code"], codes::INTERNAL_ERROR, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("history_store_wired"),
        "the error should point at the doctor field that explains it: {v}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

// ── session.fork ────────────────────────────────────────────────────────

#[tokio::test]
async fn session_fork_produces_an_independent_session() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("original answer")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let original = run_turn(&srv.sock, None, "original question").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();
    assert_ne!(fork_id, original, "a fork must get its own id");
    assert_eq!(fork["result"]["parent_session_id"], original);
    assert_eq!(
        fork["result"]["active"], false,
        "a fork is disk-only until resumed"
    );

    let before_original = history(&srv.sock, &original).await;
    let before_fork = history(&srv.sock, &fork_id).await;
    // The fork's lines are new lines — their own ids and times — carrying
    // the same conversation.
    let copied = |h: &serde_json::Value| -> Vec<serde_json::Value> {
        h["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": m["role"], "content": m["content"], "origin": m["origin"],
                    "turn_id": m["turn_id"], "usage": m["usage"], "seq": m["seq"]
                })
            })
            .collect()
    };
    assert_eq!(
        copied(&before_original),
        copied(&before_fork),
        "the fork must start as an exact copy"
    );

    // Now write to the fork — a real turn, through the normal resume path.
    run_turn(&srv.sock, Some(&fork_id), "only on the fork").await;

    let after_original = history(&srv.sock, &original).await;
    let after_fork = history(&srv.sock, &fork_id).await;
    assert_eq!(
        after_original["messages"], before_original["messages"],
        "writing to the fork must not touch the original"
    );
    assert!(
        after_fork["total"].as_u64().unwrap() > before_fork["total"].as_u64().unwrap(),
        "the fork should have grown: {after_fork}"
    );
    assert!(serde_json::to_string(&after_fork["messages"])
        .unwrap()
        .contains("only on the fork"));
    assert!(
        !serde_json::to_string(&after_original["messages"])
            .unwrap()
            .contains("only on the fork"),
        "the fork's new turn leaked into the original"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_fork_at_a_message_truncates_and_carries_context_forward() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("answer one")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let original = run_turn(&srv.sock, None, "MARKERONE").await;
    run_turn(&srv.sock, Some(&original), "MARKERTWO").await;

    // Keep only the first exchange.
    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}","at_message":2}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(fork["result"]["forked_at_message"], 2, "{fork}");
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    let forked = history(&srv.sock, &fork_id).await;
    let rendered = serde_json::to_string(&forked["messages"]).unwrap();
    assert_eq!(forked["total"], 2, "{forked}");
    assert!(rendered.contains("MARKERONE"));
    assert!(
        !rendered.contains("MARKERTWO"),
        "the branch point was ignored: {rendered}"
    );

    // The truncated history is what the *model* sees on the fork's next
    // turn — that's the point of forking, not just copying a file.
    seen.lock().unwrap().clear();
    run_turn(&srv.sock, Some(&fork_id), "continue").await;
    let sent = {
        let requests = seen.lock().unwrap();
        let last = requests
            .last()
            .expect("the fork's turn must have called the model");
        format!("{:?}", last.messages)
    };
    assert!(
        sent.contains("MARKERONE"),
        "the fork lost its inherited context"
    );
    assert!(
        !sent.contains("MARKERTWO"),
        "the fork inherited turns from past its branch point"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_fork_rejects_unknown_sessions_and_bad_branch_points() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let unknown = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.fork","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(
        unknown["error"]["code"],
        codes::SESSION_NOT_FOUND,
        "{unknown}"
    );

    let sid = run_turn(&srv.sock, None, "hi").await;
    let bad = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{sid}","at_message":"lots"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(bad["error"]["code"], codes::INVALID_PARAMS, "{bad}");

    // Past the end clamps to "the whole thing" rather than erroring — the
    // caller asked for at least this much and there isn't any more.
    let clamped = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{sid}","at_message":9999}},"id":3}}"#
        ),
    )
    .await;
    assert!(clamped["result"].is_object(), "{clamped}");
    assert_eq!(
        clamped["result"]["forked_at_message"], clamped["result"]["source_message_count"],
        "{clamped}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn forked_session_records_its_parent_on_disk() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let original = run_turn(&srv.sock, None, "root").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    // Lineage lives in the fork's `Meta` line, which is what makes
    // `HistoryStore::child_sessions` able to find it.
    let children = srv.store.child_sessions(&original).await.unwrap();
    assert_eq!(
        children.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        vec![fork_id],
        "the fork should be discoverable as a child of its source"
    );

    srv.stop().await;
}

#[tokio::test]
async fn closing_a_session_does_not_delete_what_was_forked_from_it() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let original = run_turn(&srv.sock, None, "root").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    // A fork records its source in `parent_session_id` so the lineage stays
    // queryable, but it is nobody's sidechain: closing the source must leave
    // it alone, which is the whole point of forking.
    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{original}"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(
        closed["result"]["sidechains_deleted"], 0,
        "a fork is not a sidechain: {closed}"
    );

    let sid = base::session::SessionId::parse(&fork_id).unwrap();
    assert!(
        srv.store.load(sid).await.is_ok(),
        "the fork's transcript must survive its source being closed"
    );

    srv.stop().await;
}

#[tokio::test]
async fn listing_by_parent_reports_each_child_as_what_it_is() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let original = run_turn(&srv.sock, None, "root").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    let listed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{original}"}},"id":3}}"#
        ),
    )
    .await;
    let sessions = listed["result"]["sessions"].as_array().unwrap();
    let entry = sessions
        .iter()
        .find(|s| s["session_id"] == fork_id.as_str())
        .unwrap_or_else(|| panic!("the fork should be listed under its source: {listed}"));
    assert_eq!(
        entry["session_kind"], "primary",
        "a fork listed under its source must not be reported as a sidechain: {listed}"
    );

    srv.stop().await;
}

// ── session.resume ──────────────────────────────────────────────────────

#[tokio::test]
async fn session_resume_reattaches_and_reports_state() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("stored answer")],
        ask_settings(),
        Duration::from_secs(42),
    )
    .await;

    let sid = run_turn(&srv.sock, None, "remembered question").await;

    // An already-live session is reported as such, not recreated.
    let active = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(active["result"]["status"], "already_active", "{active}");
    assert!(active["result"]["message_count"].as_u64().unwrap() >= 2);
    assert_eq!(active["result"]["scene"], "coding");
    assert_eq!(active["result"]["permission"]["mode"], "default");
    assert_eq!(active["result"]["permission"]["prompts"], true);
    assert_eq!(active["result"]["permission"]["prompt_timeout_secs"], 42);

    // Drop it from memory the way a daemon restart / idle eviction would,
    // then reattach: the transcript must come back with it.
    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], sid);

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":4}}"#
        ),
    )
    .await;
    assert_eq!(resumed["result"]["status"], "resumed", "{resumed}");
    assert_eq!(resumed["result"]["session_id"], sid);
    assert_eq!(resumed["result"]["active"], true);
    assert!(
        resumed["result"]["message_count"].as_u64().unwrap() >= 2,
        "resume must report what it reattached to: {resumed}"
    );
    assert!(resumed["result"]["entry_count"].as_u64().unwrap() >= 2);

    // And the reattached engine really does hold the old context.
    seen.lock().unwrap().clear();
    run_turn(&srv.sock, Some(&sid), "and now").await;
    let sent = {
        let requests = seen.lock().unwrap();
        format!("{:?}", requests.last().unwrap().messages)
    };
    assert!(
        sent.contains("remembered question"),
        "resumed session lost its transcript: {sent}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_resume_refuses_an_unknown_id_unless_asked_to_create() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.resume","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(
        missing["error"]["code"],
        codes::SESSION_NOT_FOUND,
        "{missing}"
    );

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.resume","params":{"session_id":"NopeNopeNopeNopeNope12","create_if_missing":true},"id":2}"#,
    )
    .await;
    assert_eq!(created["result"]["status"], "created", "{created}");
    assert_eq!(created["result"]["message_count"], 0);

    srv.stop().await;
}

/// A transcript recorded under `scene`, with nothing in it but its `Meta`
/// line — enough for §3.4's check, which reads that line and nothing else.
async fn inject_session_in_scene(srv: &ScriptedServer, scene: &str) -> base::session::SessionId {
    let sid = base::session::SessionId::new();
    srv.store
        .append(
            sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: None,
                scene: Some(scene.to_string()),
                project_root: None,
                session_kind: history::entry::SessionKind::Primary,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();
    sid
}

/// P1 §3.4: a session recorded under a scene other than this daemon's must
/// be rejected, not silently resumed into the wrong scene.
#[tokio::test]
async fn session_resume_rejects_a_scene_mismatch() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = inject_session_in_scene(&srv, "chat").await;

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(resumed["error"]["code"], codes::SCENE_MISMATCH, "{resumed}");
    assert_eq!(resumed["error"]["data"]["session_id"], sid.to_string());
    assert_eq!(resumed["error"]["data"]["recorded_scene"], "chat");
    assert_eq!(resumed["error"]["data"]["requested_scene"], "coding");

    srv.stop().await;
}

/// §3.4 binds every method that names a session, not the three that happened
/// to have tests. A guard is added per method and the newest one is the
/// easiest to forget — `session.rename` in particular reads like metadata
/// rather than like reaching into a session, and naming a conversation that
/// belongs to another scene is how it would end up labelled in a list it does
/// not belong to.
#[tokio::test]
async fn the_newer_methods_reject_a_scene_mismatch_too() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;
    let sid = inject_session_in_scene(&srv, "chat").await;

    let calls = [
        (
            "session.rename",
            format!(r#"{{"session_id":"{sid}","name":"别的场景的会话"}}"#),
        ),
        (
            "session.setModel",
            format!(r#"{{"session_id":"{sid}","model":"claude-opus-4-6"}}"#),
        ),
        (
            "session.subscribe",
            format!(r#"{{"session_id":"{sid}","resume":true}}"#),
        ),
    ];

    for (method, params) in calls {
        let response = rpc(
            &srv.sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params},"id":1}}"#),
        )
        .await;
        assert_eq!(
            response["error"]["code"],
            codes::SCENE_MISMATCH,
            "{method} did not refuse a session from another scene: {response}"
        );
        assert_eq!(
            response["error"]["data"]["recorded_scene"], "chat",
            "{method}"
        );
        assert_eq!(
            response["error"]["data"]["requested_scene"], "coding",
            "{method}"
        );
    }

    srv.stop().await;
}

/// P5 §5.6/§9: a sidechain that already ran its one-shot task to
/// conclusion (has a `SessionEnd` marker) must not be resumable — a stale
/// `session.resume` against it is rejected with `SIDECHAIN_TERMINAL`
/// instead of silently reattaching to a "finished" transcript.
#[tokio::test]
async fn session_resume_rejects_a_terminal_sidechain() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;
    srv.store
        .append(
            child,
            history::entry::LogEntry::SessionEnd {
                state: history::entry::SessionEndState::Completed,
            },
        )
        .await
        .unwrap();

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{child}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(
        resumed["error"]["code"],
        codes::SIDECHAIN_TERMINAL,
        "{resumed}"
    );
    assert_eq!(resumed["error"]["data"]["session_id"], child.to_string());
    assert_eq!(resumed["error"]["data"]["parent_session_id"], parent);
    assert_eq!(resumed["error"]["data"]["final_state"], "completed");

    srv.stop().await;
}

/// `session.subscribe {resume: true}` opens a cold session, and "cold" must
/// not become a way around the rule that a finished sidechain has no
/// continuation. It refuses through the same mapping `session.resume` uses,
/// which is why the structured `data` is asserted here too: a second copy of
/// that mapping is exactly what would drift.
#[tokio::test]
async fn opening_a_terminal_sidechain_is_refused_the_same_way() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;
    srv.store
        .append(
            child,
            history::entry::LogEntry::SessionEnd {
                state: history::entry::SessionEndState::Failed,
            },
        )
        .await
        .unwrap();

    let opened = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","params":{{"session_id":"{child}","resume":true}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(
        opened["error"]["code"],
        codes::SIDECHAIN_TERMINAL,
        "{opened}"
    );
    assert_eq!(opened["error"]["data"]["parent_session_id"], parent);
    assert_eq!(opened["error"]["data"]["final_state"], "failed");

    // Without `resume` it is the plain refusal, not the sidechain one: the
    // session is simply not in memory, and nothing was asked to open it.
    let plain = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","params":{{"session_id":"{child}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(plain["error"]["code"], codes::SESSION_NOT_FOUND, "{plain}");

    srv.stop().await;
}

/// Sanity counterpart: a sidechain with no `SessionEnd` marker (still
/// running, or cut off externally) must resume normally — the terminal
/// check must not fire on absence.
#[tokio::test]
async fn session_resume_allows_a_non_terminal_sidechain() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{child}"}},"id":1}}"#
        ),
    )
    .await;
    assert!(resumed.get("error").is_none(), "{resumed}");

    srv.stop().await;
}

/// P1 §3.4/§12.2: a pre-v2 `Meta` (no `scene` field) must resume normally
/// and report `scene_inferred: true` rather than being rejected.
#[tokio::test]
async fn session_resume_infers_scene_for_a_pre_v2_meta() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = base::session::SessionId::new();
    srv.store
        .append(
            sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: None,
                scene: None,
                project_root: None,
                session_kind: history::entry::SessionKind::Primary,
                schema_version: 1,
            },
        )
        .await
        .unwrap();

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(resumed["result"]["status"], "resumed", "{resumed}");
    assert_eq!(resumed["result"]["scene_inferred"], true);

    srv.stop().await;
}

// ── session.list sidechain filtering ────────────────────────────────────

/// P1 §5.4: sidechains are hidden from the default list, and returned
/// exactly via `parent_session_id`.
#[tokio::test]
async fn session_list_excludes_sidechains_by_default_and_returns_them_via_parent_filter() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;

    let child_sid = base::session::SessionId::new();
    srv.store
        .append(
            child_sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: Some(parent.clone()),
                scene: Some("coding".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Sidechain,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();

    let default_list = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{},"id":1}"#,
    )
    .await;
    let ids: Vec<String> = default_list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ids.contains(&parent),
        "default list must include the primary session: {ids:?}"
    );
    assert!(
        !ids.contains(&child_sid.to_string()),
        "default list must not include the sidechain: {ids:?}"
    );

    let children = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{parent}"}},"id":2}}"#
        ),
    )
    .await;
    let child_ids: Vec<String> = children["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(child_ids, vec![child_sid.to_string()], "{children}");

    let with_children = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{"include_children":true},"id":3}"#,
    )
    .await;
    let all_ids: Vec<String> = with_children["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(all_ids.contains(&parent));
    assert!(all_ids.contains(&child_sid.to_string()));

    srv.stop().await;
}

/// `SessionInfo.resumable` (§5.6): a primary session is always resumable; a
/// sidechain is resumable until it gets a `SessionEnd` marker, after which
/// `session.list` must report it as `false` too — not just `session.resume`.
#[tokio::test]
async fn session_list_reports_resumable_and_flips_it_off_after_a_terminal_marker() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let before = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{parent}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(
        before["result"]["sessions"][0]["resumable"], true,
        "{before}"
    );

    srv.store
        .append(
            child,
            history::entry::LogEntry::SessionEnd {
                state: history::entry::SessionEndState::Failed,
            },
        )
        .await
        .unwrap();

    let after = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{parent}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(
        after["result"]["sessions"][0]["resumable"], false,
        "{after}"
    );

    let primary_list = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{},"id":3}"#,
    )
    .await;
    let primary = primary_list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == parent)
        .expect("primary session should be listed");
    assert_eq!(primary["resumable"], true, "{primary}");

    srv.stop().await;
}

// ── session.create (P3: multi-project) ──────────────────────────────────

/// A session bound to a project other than the pool's default must use
/// *that* project's merged settings — proven functionally (the model name
/// actually sent to the API), not just via the Meta stamp.
#[tokio::test]
async fn session_create_binds_to_a_different_project_with_its_own_settings() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(other_project.join(".agix")).unwrap();
    std::fs::write(
        other_project.join(".agix").join("settings.json"),
        r#"{"model": {"model_name": "other-project-model"}, "memory_enabled": false}"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = match created["result"]["session_id"].as_str() {
        Some(s) => s.to_string(),
        None => panic!("session.create should succeed: {created}"),
    };
    assert_eq!(
        created["result"]["project_root"],
        other_project.display().to_string()
    );
    assert_eq!(created["result"]["scene"], "coding");
    assert_eq!(created["result"]["session_kind"], "primary");

    let entries = srv
        .store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta { project_root, .. } => {
            assert_eq!(
                project_root.as_deref(),
                Some(other_project.display().to_string().as_str())
            );
        }
        other => panic!("expected Meta as the first entry, got {other:?}"),
    }

    run_turn(&srv.sock, Some(&sid), "go").await;
    let sent_model = seen.lock().unwrap().last().unwrap().model.clone();
    assert_eq!(
        sent_model, "other-project-model",
        "the turn must have used the project's own settings, not the pool default"
    );

    srv.stop().await;
}

/// The `turn_complete` frame carries what the whole turn spent.
///
/// This is the number a daemon client budgets on — the engine's `TurnOutcome`
/// never crosses the socket. The frame is built by hand-copying two fields out
/// of the event, which is exactly the shape of code that keeps forwarding the
/// wrong one: it used to forward the last call's usage on an ordinary end and a
/// hardcoded zero on every other, so a client watching a turn get cancelled was
/// told it had cost nothing.
///
/// Two calls with different costs, because equal ones would let a sum and a
/// last value pass the same assertion.
#[tokio::test]
async fn the_turn_complete_frame_reports_the_whole_turn() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round_costing("Read", "t1", serde_json::json!({"file_path": "x"}), 100, 20),
            text_round_costing("done", 7, 3),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"read it"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");

    let complete = frames
        .iter()
        .filter_map(|f| f.get("params").and_then(|p| p.get("event")))
        .find(|e| e.get("kind").and_then(|k| k.as_str()) == Some("turn_complete"))
        .unwrap_or_else(|| panic!("no turn_complete frame in {frames:#?}"));

    assert_eq!(
        complete["api_calls"], 2,
        "the case needs a turn of more than one call to mean anything: {complete}"
    );
    assert_eq!(
        complete["usage"]["input_tokens"], 107,
        "input tokens are the last call's, not the turn's: {complete}"
    );
    assert_eq!(
        complete["usage"]["output_tokens"], 23,
        "output tokens are the last call's, not the turn's: {complete}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn a_project_bound_sessions_memories_land_in_its_own_project() {
    // The scripted client repeats its last round forever, so the background
    // memory extraction that runs after the turn gets this same array back and
    // has something to persist. `type: "project"` is what routes a memory to
    // the project-local directory rather than the global one.
    let extracted = r#"[{"name":"merge-freeze","description":"freeze window","content":"Thursday.","type":"project","confidence":0.9}]"#;
    let (srv, _seen) =
        start_scripted_server(vec![text_round(extracted)], ask_settings(), Duration::ZERO).await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(&other_project).unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "remember the freeze window").await;

    let own = other_project
        .join(".agix")
        .join("memory")
        .join("merge-freeze.md");
    let pools = srv
        ._dir
        .path()
        .join("work")
        .join(".agix")
        .join("memory")
        .join("merge-freeze.md");
    for _ in 0..60 {
        if own.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        own.exists(),
        "a project memory belongs to the session's own project, not the daemon's"
    );
    assert!(
        !pools.exists(),
        "the daemon's own project must not collect other projects' memories"
    );

    srv.stop().await;
}

/// The transcript directory `project_root` names, resolved the way the store
/// resolves it (macOS temp directories are symlinks, so the sanitized name is
/// built from the canonical path or from nothing at all).
fn transcript_dir(global_root: &std::path::Path, project: Option<&std::path::Path>) -> PathBuf {
    let canonical = project.map(|p| std::fs::canonicalize(p).unwrap());
    history::path::project_dir_for(&global_root.join("projects"), canonical.as_deref())
}

#[tokio::test]
async fn a_project_bound_sessions_transcript_lands_in_that_projects_directory() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(&other_project).unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "first").await;

    let own = transcript_dir(srv._dir.path(), Some(&other_project)).join(format!("{sid}.jsonl"));
    let pools = transcript_dir(srv._dir.path(), Some(&srv._dir.path().join("work")))
        .join(format!("{sid}.jsonl"));
    assert!(
        own.exists(),
        "a session's transcript belongs to its own project, not the daemon's"
    );
    assert!(!pools.exists(), "and not to both");

    // Still listed: `session.list` enumerates every project this daemon has
    // served, which is what keeps a cold session findable after the move.
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":3}"#,
    )
    .await;
    let entry = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("cold session missing from the list: {listed}"));
    assert_eq!(entry["status"], "inactive", "{entry}");

    srv.stop().await;
}

#[tokio::test]
async fn a_no_project_sessions_transcript_lands_in_the_no_project_bucket() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat","project_root":null},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "first").await;

    assert!(
        transcript_dir(srv._dir.path(), None)
            .join(format!("{sid}.jsonl"))
            .exists(),
        "a session bound to no project must not be filed under one"
    );

    srv.stop().await;
}

#[tokio::test]
async fn closing_a_project_bound_parent_still_cascades_to_its_sidechains() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(&other_project).unwrap();
    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let parent = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&parent), "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{parent}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(
        closed["result"]["sidechains_deleted"], 1,
        "a sidechain is found in its parent's project, not the daemon's: {closed}"
    );
    assert!(srv.store.load(child).await.is_err());

    srv.stop().await;
}

#[tokio::test]
async fn a_project_bound_session_sees_that_projects_own_skills() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    let skill_dir = other_project
        .join(".agents")
        .join("skills")
        .join("freeze-check");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: freeze-check\ndescription: Check the merge freeze window\n---\nbody",
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "anything").await;

    // The last request that carried a system prompt: the background memory
    // extraction fires a request of its own after every turn, and it sends
    // none, so `last()` alone would be reading the wrong one.
    let system_of_last = |seen: &SeenRequests| {
        seen.lock()
            .unwrap()
            .iter()
            .rev()
            .find(|req| !req.system.is_empty())
            .expect("some request carried a system prompt")
            .system
            .iter()
            .map(|b| match b {
                model::types::SystemBlock::Text { text, .. } => text.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        system_of_last(&seen).contains("freeze-check"),
        "a session must see the skills of the project it is bound to"
    );

    // And the daemon's own project does not acquire them.
    run_turn(&srv.sock, None, "anything").await;
    assert!(
        !system_of_last(&seen).contains("freeze-check"),
        "one project's skills must not leak into another's sessions"
    );

    srv.stop().await;
}

#[tokio::test]
async fn another_projects_settings_change_reaches_the_daemon_without_a_reload() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(other_project.join(".agix")).unwrap();
    let settings_file = other_project.join(".agix").join("settings.json");
    std::fs::write(
        &settings_file,
        r#"{"model": {"model_name": "first-model"}}"#,
    )
    .unwrap();

    // Reading the effective settings is what builds (and caches) this
    // project's binding — the state the file change has to invalidate.
    let effective = |root: &std::path::Path| {
        format!(
            r#"{{"jsonrpc":"2.0","method":"config.get","params":{{"scene":"coding","project_root":"{}","tier":"effective"}},"id":1}}"#,
            root.display()
        )
    };
    let first = rpc(&srv.sock, &effective(&other_project)).await;
    assert_eq!(
        first["result"]["settings"]["model"]["model_name"], "first-model",
        "{first}"
    );

    std::fs::write(
        &settings_file,
        r#"{"model": {"model_name": "second-model"}}"#,
    )
    .unwrap();

    let mut seen_second = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = rpc(&srv.sock, &effective(&other_project)).await;
        if now["result"]["settings"]["model"]["model_name"] == "second-model" {
            seen_second = true;
            break;
        }
    }
    assert!(
        seen_second,
        "a project this daemon serves must not need a restart to pick up its own settings"
    );

    srv.stop().await;
}

#[tokio::test]
async fn a_project_bound_session_sees_that_projects_own_agent_types() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    let agents_dir = other_project.join(".agix").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("freeze-bot.md"),
        "---\ndescription: Watches the merge freeze\n---\nbody",
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "anything").await;

    // The catalog is rendered into the `Agent` tool's own description.
    let agent_tool_description = |seen: &SeenRequests| {
        seen.lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|req| {
                req.tools
                    .iter()
                    .find(|t| t.name == "Agent")
                    .map(|t| t.description.clone())
            })
            .expect("some request offered the Agent tool")
    };
    assert!(
        agent_tool_description(&seen).contains("freeze-bot"),
        "a session must see the agent types of the project it is bound to"
    );

    run_turn(&srv.sock, None, "anything").await;
    assert!(
        !agent_tool_description(&seen).contains("freeze-bot"),
        "one project's agent types must not leak into another's sessions"
    );

    srv.stop().await;
}

// ── scripts ─────────────────────────────────────────────────────────────

/// A project that binds a script gets it, through the daemon, on the prompt
/// the daemon sends.
///
/// The harness that covers the nine extension points drives `Builder`
/// directly, which is the right place to ask what an adapter does and the
/// wrong place to ask whether a *session* ever gets one: everything between a
/// `scripts` section on disk and an adapter installed on a session belongs to
/// the daemon — parsing it, resolving it against a root, binding it once per
/// session. None of that had a test.
///
/// Two claims in one case, because they fail together. The mark proves the
/// script ran at all. The rewrite proves the daemon judged it the operator's
/// own code: authority follows the file's location, so a script bound against
/// the wrong root is a script demoted to add-only — which looks exactly like a
/// script that chose not to rewrite anything.
#[cfg(feature = "scripts")]
#[tokio::test]
async fn a_projects_own_script_reaches_the_prompt_the_daemon_sends() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let project = srv._dir.path().join("scripted-project");
    std::fs::create_dir_all(project.join(".agix").join("scripts")).unwrap();
    std::fs::write(
        project.join(".agix").join("scripts").join("house.js"),
        // Adds one block and rewrites every block already there. The rewrite
        // is the half only the operator's own code may do.
        r#"function onAssemble(blocks) {
             var out = blocks.map(function (b) {
               return { name: b.name, content: b.content + "\nDAEMON-SCRIPT-EDIT" };
             });
             out.push({ name: "daemon.house", content: "DAEMON-SCRIPT-ADD" });
             return out;
           }"#,
    )
    .unwrap();
    // `timeout_ms` well above the carrier's 100ms default, because the
    // subject here is whether a project's script reaches the prompt at all.
    // The default budget is wall clock, and on a machine running the rest of
    // this suite beside it even this script can miss it — the engine then
    // does exactly what it promises, drops the contribution and leaves the
    // prompt alone, and the case fails for a reason it is not about. The
    // deadline itself is covered where it belongs, by a script written to
    // exceed it (`tests/runner/tests/script_boundaries.rs`).
    std::fs::write(
        project.join(".agix").join("settings.json"),
        r#"{
             "memory_enabled": false,
             "scripts": [
               {"path": ".agix/scripts/house.js", "point": "prompt.assemble",
                "entry": "onAssemble", "timeout_ms": 5000}
             ]
           }"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session.create failed: {created}"))
        .to_string();

    run_turn(&srv.sock, Some(&sid), "hello").await;

    // Marks only: a failure here otherwise prints the whole system prompt,
    // which is a screenful of noise around one missing word.
    let sent = {
        let requests = seen.lock().unwrap();
        let last = requests
            .last()
            .expect("the turn must have called the model");
        format!("{:?}", last.system)
    };
    assert!(
        sent.contains("DAEMON-SCRIPT-ADD"),
        "the project's script never reached the prompt the daemon sent — \
         nothing between the settings file and the session installed it"
    );
    assert!(
        sent.contains("DAEMON-SCRIPT-EDIT"),
        "the script was demoted to add-only, so the daemon judged the \
         project's own file to be from outside it"
    );

    srv.stop().await;
}

/// `session.get` says what this session's scripts have been doing.
///
/// The failure mode of the whole script tier is a script that quietly does
/// nothing, and until this the only account of it was a line on the daemon's
/// stderr — which the person asking "why is my script not working" does not
/// have. A throwing script is the case worth reporting: nothing about the
/// session looks different, and the answer has to come from somewhere.
#[cfg(feature = "scripts")]
#[tokio::test]
async fn session_get_reports_what_the_scripts_did() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let project = srv._dir.path().join("reported-project");
    std::fs::create_dir_all(project.join(".agix").join("scripts")).unwrap();
    std::fs::write(
        project.join(".agix").join("scripts").join("broken.js"),
        "function onAssemble() { throw new Error('nope'); }",
    )
    .unwrap();
    // Generous on purpose: this case is about a script that *throws* being
    // reported, and a script that timed out would be reported as failed too
    // — passing for the wrong reason.
    std::fs::write(
        project.join(".agix").join("settings.json"),
        r#"{
             "memory_enabled": false,
             "scripts": [
               {"path": ".agix/scripts/broken.js", "point": "prompt.assemble",
                "entry": "onAssemble", "timeout_ms": 5000}
             ]
           }"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session.create failed: {created}"))
        .to_string();
    run_turn(&srv.sock, Some(&sid), "hello").await;

    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    let scripts = &detail["result"]["scripts"];
    assert!(
        scripts.is_object(),
        "a session that bound a script must report on it: {detail}"
    );
    assert!(
        scripts["failed"].as_u64().unwrap_or(0) >= 1,
        "the throwing script is not reported as having failed: {scripts}"
    );
    let recent = scripts["recent"].as_array().expect("recent is an array");
    let first = &recent[0];
    assert_eq!(first["point"], "prompt.assemble");
    assert_eq!(first["outcome"], "failed");
    assert!(
        first["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "a failure without a reason sends the reader nowhere: {first}"
    );

    srv.stop().await;
}

/// A session that bound none has no key at all — absent and empty are
/// different answers, and the difference is the first thing a reader needs.
#[cfg(feature = "scripts")]
#[tokio::test]
async fn session_get_says_nothing_about_scripts_when_there_are_none() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "hello").await;
    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert!(
        detail["result"]["scripts"].is_null(),
        "a session with no scripts must not report an empty ledger: {detail}"
    );
    srv.stop().await;
}

/// A binding the daemon cannot honor costs the session every script, not some
/// of them.
///
/// `bind` refuses the set and the daemon logs it; what matters out here is
/// that the session still starts and runs, carrying none of them. A daemon
/// that failed the session instead would turn one typo in a settings file
/// into an agent that will not start.
#[cfg(feature = "scripts")]
#[tokio::test]
async fn one_bad_binding_leaves_the_session_running_with_no_scripts() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let project = srv._dir.path().join("half-scripted-project");
    std::fs::create_dir_all(project.join(".agix").join("scripts")).unwrap();
    std::fs::write(
        project.join(".agix").join("scripts").join("house.js"),
        r#"function onAssemble(blocks) {
             blocks.push({ name: "daemon.house", content: "DAEMON-SCRIPT-ADD" });
             return blocks;
           }"#,
    )
    .unwrap();
    std::fs::write(
        project.join(".agix").join("settings.json"),
        r#"{
             "memory_enabled": false,
             "scripts": [
               {"path": ".agix/scripts/house.js", "point": "prompt.assemble",
                "entry": "onAssemble", "timeout_ms": 5000},
               {"path": ".agix/scripts/missing.js", "point": "tool.result",
                "entry": "onResult", "timeout_ms": 5000}
             ]
           }"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session.create failed: {created}"))
        .to_string();

    run_turn(&srv.sock, Some(&sid), "hello").await;

    let sent = {
        let requests = seen.lock().unwrap();
        format!("{:?}", requests.last().expect("the turn ran").system)
    };
    assert!(
        !sent.contains("DAEMON-SCRIPT-ADD"),
        "the good binding was applied although another one could not be \
         honored — half a configuration is one nobody wrote"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_create_rejects_a_nonexistent_project_root() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let missing = srv._dir.path().join("does-not-exist");
    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            missing.display()
        ),
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::PROJECT_NOT_FOUND, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn session_create_with_null_project_root_is_a_no_project_session() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    // `chat`, not the daemon's `coding`: a no-project session is only
    // meaningful in a scene that doesn't require one, and `coding` now
    // refuses (see `AgentScene::requires_project`).
    let _ = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":0}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat","project_root":null},"id":1}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(created["result"]["project_root"].is_null(), "{created}");

    let entries = srv
        .store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta {
            project_root,
            session_kind,
            ..
        } => {
            assert_eq!(*project_root, None);
            assert_eq!(*session_kind, history::entry::SessionKind::Primary);
        }
        other => panic!("expected Meta as the first entry, got {other:?}"),
    }

    srv.stop().await;
}

#[tokio::test]
async fn session_create_without_project_root_uses_the_pool_default() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{},"id":1}"#,
    )
    .await;
    let default_project = srv._dir.path().join("work");
    assert_eq!(
        created["result"]["project_root"],
        default_project.display().to_string(),
        "{created}"
    );

    srv.stop().await;
}

/// P3's whole point: a project-bound session must still use *its own*
/// project settings after being dropped from memory and reattached — not
/// silently fall back to the pool default the moment it needs rebuilding.
#[tokio::test]
async fn a_project_bound_session_keeps_its_own_settings_across_close_and_resume() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(other_project.join(".agix")).unwrap();
    std::fs::write(
        other_project.join(".agix").join("settings.json"),
        r#"{"model": {"model_name": "other-project-model"}, "memory_enabled": false}"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "first").await;

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], sid);

    run_turn(&srv.sock, Some(&sid), "second, after reattach").await;
    let sent_model = seen.lock().unwrap().last().unwrap().model.clone();
    assert_eq!(
        sent_model, "other-project-model",
        "resuming must rebuild against the session's own recorded project, not the pool default"
    );

    srv.stop().await;
}

// ── session.close / session.delete cascade (P5) ─────────────────────────

async fn inject_sidechain(srv: &ScriptedServer, parent: &str) -> base::session::SessionId {
    let child_sid = base::session::SessionId::new();
    srv.store
        .append(
            child_sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: Some(parent.to_string()),
                scene: Some("coding".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Sidechain,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();
    child_sid
}

#[tokio::test]
async fn session_close_cascades_deletes_its_sidechains() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{parent}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], parent);
    assert_eq!(closed["result"]["sidechains_deleted"], 1, "{closed}");

    assert!(
        srv.store.load(child).await.is_err(),
        "the sidechain's transcript must be gone after the parent closes"
    );
    assert!(
        srv.store
            .load(base::session::SessionId::parse(&parent).unwrap())
            .await
            .is_ok(),
        "session.close must leave the parent's own transcript on disk"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_delete_removes_the_parent_and_its_sidechains() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let deleted = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{parent}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(deleted["result"]["deleted"], true, "{deleted}");
    assert_eq!(deleted["result"]["sidechains_deleted"], 1, "{deleted}");
    assert_eq!(
        deleted["result"]["sidechain_ids"],
        serde_json::json!([child.to_string()])
    );

    assert!(srv.store.load(child).await.is_err());
    assert!(srv
        .store
        .load(base::session::SessionId::parse(&parent).unwrap())
        .await
        .is_err());

    srv.stop().await;
}

#[tokio::test]
async fn session_delete_dry_run_reports_without_deleting_anything() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let previewed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{parent}","dry_run":true}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(previewed["result"]["deleted"], false, "{previewed}");
    assert_eq!(previewed["result"]["sidechains_deleted"], 1, "{previewed}");

    assert!(
        srv.store.load(child).await.is_ok(),
        "dry_run must not actually delete the sidechain"
    );
    assert!(srv
        .store
        .load(base::session::SessionId::parse(&parent).unwrap())
        .await
        .is_ok());

    srv.stop().await;
}

/// P6 review fix: `session.close`/`.delete` skipped the scene-isolation
/// check `session.resume`/`.fork` already had, so a session recorded under
/// a scene this daemon isn't even serving could still be closed/deleted
/// through it. Mirrors `session_resume_rejects_a_scene_mismatch`'s setup —
/// a session recorded under `"chat"`, which this daemon never activated.
#[tokio::test]
async fn session_close_rejects_a_scene_mismatch() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = base::session::SessionId::new();
    srv.store
        .append(
            sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: None,
                scene: Some("chat".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Primary,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(closed["error"]["code"], codes::SCENE_MISMATCH, "{closed}");
    assert_eq!(closed["error"]["data"]["recorded_scene"], "chat");
    assert!(
        srv.store.load(sid).await.is_ok(),
        "a rejected session.close must not touch the transcript"
    );

    let deleted = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(deleted["error"]["code"], codes::SCENE_MISMATCH, "{deleted}");
    assert!(
        srv.store.load(sid).await.is_ok(),
        "a rejected session.delete must not touch the transcript"
    );

    srv.stop().await;
}

// ── scene.* (P4: multi-scene) ────────────────────────────────────────────

#[tokio::test]
async fn scene_list_reports_the_default_scene_as_active() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#,
    )
    .await;
    let scenes = resp["result"]["scenes"].as_array().unwrap();
    let coding = scenes
        .iter()
        .find(|s| s["scene"] == "coding")
        .expect("coding should be registered");
    assert_eq!(coding["active"], true, "{resp}");
    let chat = scenes
        .iter()
        .find(|s| s["scene"] == "chat")
        .expect("chat should be registered even though inactive");
    assert_eq!(chat["active"], false, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn session_create_rejects_an_inactive_scene() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_NOT_FOUND, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_activate_makes_a_new_scene_available_for_session_create() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let activated = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    assert_eq!(activated["result"]["active"], true, "{activated}");

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat"},"id":2}"#,
    )
    .await;
    let sid = match created["result"]["session_id"].as_str() {
        Some(s) => s,
        None => panic!("session.create should succeed once chat is active: {created}"),
    };
    assert_eq!(created["result"]["scene"], "chat");

    let entries = srv
        .store
        .load(base::session::SessionId::parse(sid).unwrap())
        .await
        .unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta { scene, .. } => {
            assert_eq!(scene.as_deref(), Some("chat"));
        }
        other => panic!("expected Meta as the first entry, got {other:?}"),
    }

    srv.stop().await;
}

#[tokio::test]
async fn scene_activate_rejects_an_unknown_scene_name() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"not-a-real-scene"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_NOT_FOUND, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_deactivate_is_blocked_by_active_sessions_then_succeeds_after_close() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat"},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let blocked = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.deactivate","params":{"scene":"chat"},"id":3}"#,
    )
    .await;
    assert_eq!(
        blocked["error"]["code"],
        codes::SCENE_HAS_ACTIVE_SESSIONS,
        "{blocked}"
    );

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":4}}"#
        ),
    )
    .await;

    let deactivated = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.deactivate","params":{"scene":"chat"},"id":5}"#,
    )
    .await;
    assert_eq!(deactivated["result"]["scene"], "chat", "{deactivated}");

    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.list","id":6}"#,
    )
    .await;
    let chat = listed["result"]["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["scene"] == "chat")
        .unwrap();
    assert_eq!(chat["active"], false, "{listed}");

    srv.stop().await;
}

#[tokio::test]
async fn a_reattached_session_comes_back_under_its_own_scene() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat"},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    run_turn(&srv.sock, Some(&sid), "first").await;
    // The last request that carried a tool surface at all: `chat` renames a
    // session after its first turn, and that naming call is a bare request
    // with no tools on it, so `last()` alone would be asking the wrong one.
    let offered_bash = |seen: &SeenRequests| {
        seen.lock()
            .unwrap()
            .iter()
            .rev()
            .find(|req| !req.tools.is_empty())
            .expect("some request carried a tool surface")
            .tools
            .iter()
            .any(|t| t.name == "Bash")
    };
    assert!(
        !offered_bash(&seen),
        "chat's tool whitelist has no Bash in it"
    );

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;

    run_turn(&srv.sock, Some(&sid), "second, after reattach").await;
    assert!(
        !offered_bash(&seen),
        "reattaching must rebuild against the session's own recorded scene, not the pool default"
    );

    srv.stop().await;
}

#[tokio::test]
async fn run_turn_refuses_a_session_recorded_under_a_scene_this_daemon_stopped_serving() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat"},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;
    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.deactivate","params":{"scene":"chat"},"id":4}"#,
    )
    .await;

    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"hi"}},"id":5}}"#
        ),
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_MISMATCH, "{resp}");
    assert_eq!(resp["error"]["data"]["recorded_scene"], "chat", "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn a_chat_session_names_itself_on_a_daemon_whose_own_scene_does_not() {
    // Two rounds: the turn's own answer, then the naming call `chat` makes
    // after a first turn. `coding` — this daemon's own scene — makes neither.
    let (srv, _seen) = start_scripted_server(
        vec![text_round("answered"), text_round("会话标题")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"chat"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"chat"},"id":2}"#,
    )
    .await;
    let chat_sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (_frames, resp) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{chat_sid}","message":"hello"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(
        resp["result"]["name"], "会话标题",
        "a chat session must name itself even when the daemon's own scene would not: {resp}"
    );

    // The control: the same daemon's own scene still does not.
    let (_frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hello"},"id":4}"#,
    )
    .await;
    assert!(
        resp["result"]["name"].is_null(),
        "coding does not auto-name, and reading the session's scene must not change that: {resp}"
    );

    srv.stop().await;
}

// ── permission default = ask, end to end ────────────────────────────────

/// Drive a turn on one connection while answering prompts on another — the
/// turn's own connection is blocked streaming frames, which is exactly the
/// shape a real client is in when a prompt arrives. `answer` is called once
/// per prompt with `(session_id, prompt_id)` and returns the `decision`
/// JSON to send back, or `None` to leave the prompt unanswered.
async fn run_turn_answering_prompts<F>(
    sock: &std::path::Path,
    message: &str,
    options: &str,
    mut answer: F,
) -> (Vec<serde_json::Value>, serde_json::Value)
where
    F: FnMut(usize) -> Option<String>,
{
    let request = format!(
        r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"message":"{message}"{options}}},"id":1}}"#
    );
    let mut conn = UnixStream::connect(sock).await.unwrap();
    conn.write_all(request.as_bytes()).await.unwrap();
    conn.write_all(b"\n").await.unwrap();
    let (r, _w) = conn.split();
    let mut br = BufReader::new(r);

    let mut frames = Vec::new();
    let mut prompt_no = 0usize;
    loop {
        let mut line = String::new();
        let n = br.read_line(&mut line).await.unwrap();
        assert!(n > 0, "connection closed before a final response arrived");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("id").is_some() {
            return (frames, v);
        }
        if v["params"]["event"]["kind"] == "prompt" {
            if let Some(decision) = answer(prompt_no) {
                let sid = v["params"]["session_id"].as_str().unwrap();
                let prompt_id = v["params"]["event"]["prompt_id"].as_str().unwrap();
                let ack = rpc(
                    sock,
                    &format!(
                        r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}","decision":{decision}}},"id":99}}"#
                    ),
                )
                .await;
                assert_eq!(ack["result"]["prompt_id"], prompt_id, "{ack}");
            }
            prompt_no += 1;
        }
        frames.push(v);
    }
}

#[tokio::test]
async fn ask_default_prompts_then_proceeds_once_permitted() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("permitted.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = run_turn_answering_prompts(&srv.sock, "make a file", "", |_| {
        Some(r#"{"type":"permit"}"#.to_string())
    })
    .await;

    let prompts = events_of_kind(&frames, "prompt");
    assert!(
        !prompts.is_empty(),
        "the new default must emit a kind:\"prompt\" frame for an unlisted Bash call: {frames:?}"
    );
    assert_eq!(prompts[0]["prompt_type"], "permission");
    assert_eq!(prompts[0]["tool_name"], "Bash");
    assert!(prompts[0]["prompt_id"].is_string());
    assert_eq!(prompts[0]["tool_use_id"], "tool-1");
    assert!(response["result"].is_object(), "turn failed: {response}");
    let done = events_of_kind(&frames, "turn_complete");
    assert_eq!(done[0]["tool_calls"], 1, "{done:?}");

    let results = events_of_kind(&frames, "tool_result");
    assert!(
        !results.is_empty(),
        "no tool_result after permitting: {frames:?}"
    );
    let rendered = serde_json::to_string(&results).unwrap();
    assert!(
        !rendered.contains("Denied by permission"),
        "a permitted call was denied anyway: {rendered}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn ask_default_blocks_the_tool_when_denied() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("denied.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = run_turn_answering_prompts(&srv.sock, "make a file", "", |_| {
        Some(r#"{"type":"deny","reason":"not on my machine"}"#.to_string())
    })
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(
        rendered.contains("Denied by permission") && rendered.contains("not on my machine"),
        "a denied call must surface the denial to the model: {rendered}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn settings_bypass_permissions_opt_out_restores_allow_all() {
    let mut settings = ask_settings();
    settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;

    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("bypassed.txt"),
        settings,
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "the opt-out must not prompt at all: {frames:?}"
    );
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(
        !rendered.contains("Denied by permission"),
        "the opt-out must not deny either: {rendered}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn per_session_bypass_option_is_clamped_to_the_configured_mode() {
    // N-6: `settings.permission_mode` is a policy, not a default. A client
    // asking for `bypassPermissions` against an `ask`-configured daemon is
    // clamped and still prompted — otherwise anyone who can reach the socket
    // opts themselves out, and `daemon/src/server.rs` is explicit that there
    // is no per-method authorization on it.
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("session-bypassed.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","options":{"permission_mode":"bypassPermissions"}},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        !events_of_kind(&frames, "prompt").is_empty(),
        "a client-requested bypass must be clamped and still prompt: {frames:?}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn allow_client_permission_override_lets_a_client_opt_out() {
    // ...and the operator can still hand that authority over explicitly.
    let mut settings = ask_settings();
    settings.allow_client_permission_override = true;
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("session-bypassed.txt"),
        settings,
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","options":{"permission_mode":"bypassPermissions"}},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "with the override enabled, bypassPermissions must not prompt: {frames:?}"
    );

    srv.stop().await;
}

/// A session's options are part of what it is, not of the call that
/// happened to build its engine: a reload rebuilds the engine, and the next
/// turn — sent with no options at all — must run under the ones the session
/// was created with. The options are also in the session's log, which is how
/// a daemon restart gets them back.
#[tokio::test]
async fn a_rebuilt_session_keeps_the_options_it_was_created_with() {
    let mut settings = ask_settings();
    settings.allow_client_permission_override = true;
    let mut rounds = asking_bash_rounds("first.txt");
    rounds.extend(asking_bash_rounds("second.txt"));
    let (srv, _seen) = start_scripted_server(rounds, settings, Duration::from_secs(5)).await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","options":{"permission_mode":"bypassPermissions"}},"id":1}"#,
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(events_of_kind(&frames, "prompt").is_empty());
    let sid = response["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let reloaded = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":2}"#,
    )
    .await;
    assert!(reloaded["result"].is_object(), "reload failed: {reloaded}");

    let (frames, response) = rpc_streaming(
        &srv.sock,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session.run_turn",
            "params": {"session_id": sid, "message": "make another"},
            "id": 3,
        })
        .to_string(),
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "the rebuilt session prompted, so it lost the mode it was created with: {frames:?}"
    );

    let entries = srv
        .store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    let recorded = entries.iter().find_map(|env| match &env.entry {
        history::entry::LogEntry::Extension { ns, payload, .. } if ns == "agix.session" => {
            Some(payload.clone())
        }
        _ => None,
    });
    assert_eq!(
        recorded
            .as_ref()
            .map(|p| p["options"]["permission_mode"].clone()),
        Some(serde_json::json!("bypassPermissions")),
        "the session's log does not carry its options: {recorded:?}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn settings_permission_rules_finally_reach_the_daemon() {
    // `settings.permission_rules` has been parsed by `Settings::load` for a
    // long time with no production consumer at all. Under the ask default it
    // finally has one: an Allow rule pre-empts the prompt.
    let mut settings = ask_settings();
    settings.permission_rules = vec![base::interface::settings::PermissionRule {
        tool: "Bash(touch ruled.txt)".into(),
        action: base::interface::settings::PermissionAction::Allow,
        tier: None,
    }];

    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("ruled.txt"),
        settings,
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "an explicit allow rule must pre-empt the prompt: {frames:?}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn an_unanswered_prompt_is_denied_instead_of_hanging_forever() {
    // A one-second ceiling stands in for the production default (300s): the
    // property under test is "the turn ends without anyone answering", not
    // the particular duration.
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("never-answered.txt"),
        ask_settings(),
        Duration::from_secs(1),
    )
    .await;

    // Nobody ever calls `session.respondToPrompt`. Without the daemon-side
    // timeout this call never returns — the engine waits on its oneshot
    // forever — so the outer timeout is the regression guard.
    let (frames, response) = tokio::time::timeout(
        Duration::from_secs(30),
        rpc_streaming(
            &srv.sock,
            r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
        ),
    )
    .await
    .expect("an unanswered permission prompt hung the turn forever");

    assert!(
        !events_of_kind(&frames, "prompt").is_empty(),
        "the prompt should still have been offered: {frames:?}"
    );
    assert!(response["result"].is_object(), "turn failed: {response}");
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(
        rendered.contains("Denied by permission") && rendered.contains("no answer"),
        "the timeout must fail closed, with a reason that names itself: {rendered}"
    );

    srv.stop().await;
}

// ── §5.3 一个会话同时只跑一个 turn / §3.2 场景能力 ──────────────────────

/// A session runs one turn at a time, and the second caller is told so
/// rather than being served an internal error.
///
/// The ordering matters more than the code: the message used to be pushed
/// into the agent's input queue *before* the exclusive event channel was
/// claimed, so a rejected second `run_turn` had already handed the user's
/// message to the turn that was still running. The caller saw a failure and
/// the message was answered anyway, by the wrong turn.
#[tokio::test]
async fn a_second_turn_on_a_busy_session_is_refused_and_its_message_is_not_swallowed() {
    let (srv, seen) = start_scripted_server(
        asking_bash_rounds("busy.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    // Park a turn on an unanswered permission prompt: the session is busy
    // for as long as nobody answers.
    let sock = srv.sock.clone();
    let mut conn = UnixStream::connect(&sock).await.unwrap();
    conn.write_all(
        br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"first"},"id":1}"#,
    )
    .await
    .unwrap();
    conn.write_all(b"\n").await.unwrap();

    let (sid, prompt_id) = {
        let (r, _w) = conn.split();
        let mut br = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before the prompt arrived");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "prompt" {
                break (
                    v["params"]["session_id"].as_str().unwrap().to_string(),
                    v["params"]["event"]["prompt_id"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                );
            }
        }
    };

    let requests_before = seen.lock().unwrap().len();

    let busy = rpc(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"ZZ-refused-message-ZZ"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(busy["error"]["code"], codes::SESSION_BUSY, "{busy}");
    assert_eq!(busy["error"]["data"]["session_id"], sid, "{busy}");
    assert!(
        busy["error"]["data"]["current_turn_id"].is_string(),
        "the refusal must name the turn holding the session: {busy}"
    );

    // Let the parked turn finish.
    let ack = rpc(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}","decision":{{"type":"deny","reason":"test"}}}},"id":99}}"#
        ),
    )
    .await;
    assert_eq!(ack["result"]["prompt_id"], prompt_id, "{ack}");

    // The refused message must not have reached the model as part of the
    // turn that was already running.
    let requests: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .skip(requests_before)
        .map(|r| format!("{r:?}"))
        .collect();
    assert!(
        !requests.iter().any(|r| r.contains("ZZ-refused-message-ZZ")),
        "the refused message was fed to the running turn anyway: {requests:?}"
    );

    srv.stop().await;
}

/// Once the parked turn is done the session is idle again — "busy" tracks
/// the turn, it is not a latch.
#[tokio::test]
async fn a_session_accepts_a_new_turn_after_the_previous_one_finishes() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "first").await;
    let second = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"second"}},"id":2}}"#
        ),
    )
    .await
    .1;
    assert!(second["result"].is_object(), "{second}");

    srv.stop().await;
}

/// `coding` requires a project, and says so at creation rather than letting
/// the session exist and fail on its first tool call.
#[tokio::test]
async fn a_scene_that_requires_a_project_refuses_a_session_without_one() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"coding","project_root":null},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::PROJECT_REQUIRED, "{resp}");

    srv.stop().await;
}

/// The capability bits are what a host reads to decide whether to ask for a
/// project or show a team entry point, so they have to be on the wire.
#[tokio::test]
async fn scene_list_reports_the_capability_bits() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#,
    )
    .await;
    let scenes = resp["result"]["scenes"].as_array().unwrap();
    let by_id = |id: &str| {
        scenes
            .iter()
            .find(|s| s["scene"] == id)
            .unwrap_or_else(|| panic!("{id} missing from scene.list: {resp}"))
            .clone()
    };

    assert_eq!(by_id("coding")["requires_project"], true);
    assert_eq!(by_id("coding")["supports_team"], true);
    assert_eq!(by_id("chat")["requires_project"], false);
    assert_eq!(by_id("chat")["supports_team"], false);

    srv.stop().await;
}

// ── §6.1/§6.3 之前只存在于文档里的方法 ────────────────────────────────

#[tokio::test]
async fn ping_answers_without_a_session() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"daemon.ping","id":1}"#,
    )
    .await;
    assert_eq!(resp["result"]["pong"], true, "{resp}");
    assert_eq!(
        resp["result"]["protocol_version"],
        daemon::discovery::INSTANCE_PROTOCOL_VERSION,
        "{resp}"
    );
    assert!(
        resp["result"]["capabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c == "subscription_dropped_notice")),
        "{resp}"
    );

    srv.stop().await;
}

/// `turn_state` is what a client polls to decide whether its send button is
/// live, so it has to be right on both sides of a turn.
#[tokio::test]
async fn session_get_reports_the_summary_and_the_turn_state() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("hello")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hi").await;
    let got = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;

    assert_eq!(got["result"]["session_id"], sid, "{got}");
    assert_eq!(got["result"]["scene"], "coding", "{got}");
    assert_eq!(got["result"]["scene_active"], true, "{got}");
    assert_eq!(got["result"]["turn_state"], "idle", "{got}");
    assert!(got["result"]["current_turn_id"].is_null(), "{got}");
    // `message_count` comes straight from `session.list`'s summary — asserted
    // present, not non-zero: a live session reports 0 until its transcript is
    // read back, which is `session.list`'s existing behavior and not something
    // `session.get` should quietly diverge from.
    assert!(got["result"]["message_count"].is_number(), "{got}");

    srv.stop().await;
}

#[tokio::test]
async fn session_get_on_an_unknown_session_is_not_found() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let got = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.get","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(got["error"]["code"], codes::SESSION_NOT_FOUND, "{got}");

    srv.stop().await;
}

/// Interrupting an idle session is an answer, not an error — the caller
/// wanted it idle and it is.
#[tokio::test]
async fn interrupting_an_idle_session_reports_that_there_was_nothing_to_interrupt() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hi").await;
    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(resp["result"]["interrupted"], false, "{resp}");

    srv.stop().await;
}

/// The documented use: a client that hits `SESSION_BUSY` interrupts, then
/// sends. The session survives the interrupt and serves the next turn.
#[tokio::test]
async fn interrupt_frees_a_busy_session_for_the_next_turn() {
    let (srv, _seen) = start_scripted_server(
        vec![
            asking_bash_rounds("interrupt.txt")
                .into_iter()
                .next()
                .unwrap(),
            text_round("after"),
        ],
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let mut conn = UnixStream::connect(&srv.sock).await.unwrap();
    conn.write_all(
        br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"first"},"id":1}"#,
    )
    .await
    .unwrap();
    conn.write_all(b"\n").await.unwrap();

    let sid = {
        let (r, _w) = conn.split();
        let mut br = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before the prompt arrived");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "prompt" {
                break v["params"]["session_id"].as_str().unwrap().to_string();
            }
        }
    };

    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(resp["result"]["interrupted"], true, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_describe_reports_capabilities_tools_and_effective_settings() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.describe","params":{"scene":"coding"},"id":1}"#,
    )
    .await;

    assert_eq!(resp["result"]["scene"], "coding", "{resp}");
    assert_eq!(resp["result"]["capabilities"]["requires_project"], true);
    assert_eq!(resp["result"]["capabilities"]["supports_team"], true);
    // `coding` sets no whitelist, which is `null` rather than `[]` — the
    // latter would read as "no tools allowed", the opposite of the truth.
    assert!(resp["result"]["tools"]["allowed"].is_null(), "{resp}");
    assert!(
        !resp["result"]["tools"]["deferred"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{resp}"
    );
    assert!(resp["result"]["settings"].is_object(), "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_describe_rejects_an_unknown_scene() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.describe","params":{"scene":"nope"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_NOT_FOUND, "{resp}");

    srv.stop().await;
}

/// A tier with no file on disk is `null`, not `{}` — "nothing configured
/// here" and "configured empty" are different states a config UI has to be
/// able to tell apart.
#[tokio::test]
async fn config_get_distinguishes_an_absent_tier_from_an_empty_one() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let scene_tier = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"coding","tier":"scene"},"id":1}"#,
    )
    .await;
    assert_eq!(scene_tier["result"]["tier"], "scene", "{scene_tier}");
    assert!(scene_tier["result"]["settings"].is_null(), "{scene_tier}");

    let effective = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"coding"},"id":2}"#,
    )
    .await;
    assert_eq!(effective["result"]["tier"], "effective", "{effective}");
    assert!(effective["result"]["settings"].is_object(), "{effective}");

    srv.stop().await;
}

#[tokio::test]
async fn config_get_rejects_an_unknown_tier() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"coding","tier":"nope"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::INVALID_PARAMS, "{resp}");

    srv.stop().await;
}

/// Credentials are redacted unless asked for, at whatever depth they sit.
#[tokio::test]
async fn config_get_redacts_credentials_by_default() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"coding"},"id":1}"#,
    )
    .await;
    let dumped = resp.to_string();
    assert!(
        !dumped.contains("test-key"),
        "the scripted server's auth token leaked into config.get: {dumped}"
    );

    srv.stop().await;
}

// ── §5.2 一条连接上的并发 ────────────────────────────────────────────────

/// A prompt raised by a turn can be answered **on the connection running
/// that turn**.
///
/// This is the case a browser has: one tab, one socket, everything on it.
/// While requests were served inline the connection read nothing during a
/// turn, so the answer never arrived — the turn waited for an answer that
/// waited for the turn, until the prompt timeout denied it. Every other test
/// in this file answers from a second connection and so never saw it.
///
/// The whole test is under a deadline: a regression here does not fail, it
/// hangs until `permission_prompt_timeout`.
#[tokio::test]
async fn a_prompt_is_answerable_on_the_same_connection_that_started_the_turn() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("same-conn.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let mut conn = UnixStream::connect(&srv.sock).await.unwrap();
        conn.write_all(
            br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
        )
        .await
        .unwrap();
        conn.write_all(b"\n").await.unwrap();

        let (r, mut w) = conn.split();
        let mut br = BufReader::new(r);
        let mut answered = false;
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before the turn finished");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();

            if v["params"]["event"]["kind"] == "prompt" && !answered {
                let sid = v["params"]["session_id"].as_str().unwrap();
                let prompt_id = v["params"]["event"]["prompt_id"].as_str().unwrap();
                // Same socket the turn is streaming on.
                w.write_all(format!(
                    r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}","decision":{{"type":"deny","reason":"same-connection test"}}}},"id":2}}"#
                ).as_bytes()).await.unwrap();
                w.write_all(b"\n").await.unwrap();
                answered = true;
            }

            // The turn's own response, id 1, means the turn ran to an end
            // rather than stalling on an unanswerable question.
            if v["id"] == 1 {
                assert!(answered, "the turn ended without ever asking: {v}");
                return v;
            }
        }
    })
    .await;

    let response = outcome.expect(
        "the turn never finished — the connection is not reading while it runs, so its own \
         prompt cannot be answered",
    );
    assert!(response["result"].is_object(), "turn failed: {response}");

    srv.stop().await;
}

/// `session.interrupt` works from the connection running the turn.
///
/// The tab that started a turn is the one with a stop button on it, so this
/// is the case that matters most; it was unreachable for the same reason.
#[tokio::test]
async fn a_turn_can_be_interrupted_from_the_connection_running_it() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("interrupt-same-conn.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let interrupted = tokio::time::timeout(Duration::from_secs(10), async {
        let mut conn = UnixStream::connect(&srv.sock).await.unwrap();
        conn.write_all(
            br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"go"},"id":1}"#,
        )
        .await
        .unwrap();
        conn.write_all(b"\n").await.unwrap();

        let (r, mut w) = conn.split();
        let mut br = BufReader::new(r);
        let mut asked = false;
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();

            // Wait until the turn is definitely in flight, then interrupt it
            // on this same socket.
            if v["params"]["event"]["kind"] == "prompt" && !asked {
                let sid = v["params"]["session_id"].as_str().unwrap();
                w.write_all(format!(
                    r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":2}}"#
                ).as_bytes()).await.unwrap();
                w.write_all(b"\n").await.unwrap();
                asked = true;
            }

            if v["id"] == 2 {
                return v;
            }
        }
    })
    .await
    .expect("session.interrupt was never answered on the connection running the turn");

    assert_eq!(interrupted["result"]["interrupted"], true, "{interrupted}");

    srv.stop().await;
}

// ── a hook that ends the turn ───────────────────────────────────────────

fn hooked_settings(hooks: serde_json::Value) -> Settings {
    let mut s = ask_settings_no_memory();
    s.hooks_config = Some(hooks);
    s
}

/// Runs a turn on `sid` (or a new session) and hands back the
/// `turn_complete` frame, failing if the turn never produced one.
async fn turn_complete_of(
    sock: &std::path::Path,
    session_id: Option<&str>,
    message: &str,
) -> (String, serde_json::Value) {
    let params = match session_id {
        Some(sid) => format!(r#"{{"session_id":"{sid}","message":"{message}"}}"#),
        None => format!(r#"{{"message":"{message}"}}"#),
    };
    let (frames, resp) = tokio::time::timeout(
        Duration::from_secs(20),
        rpc_streaming(
            sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{params},"id":1}}"#),
        ),
    )
    .await
    .expect("the turn never ended");
    assert!(resp["result"].is_object(), "turn failed: {resp}");
    let done = events_of_kind(&frames, "turn_complete");
    assert_eq!(done.len(), 1, "exactly one turn_complete: {frames:?}");
    (
        resp["result"]["session_id"].as_str().unwrap().to_string(),
        done[0].clone(),
    )
}

#[tokio::test]
async fn a_prompt_blocked_by_a_hook_still_ends_the_turn_and_frees_the_session() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("answer")],
        hooked_settings(serde_json::json!({
            "UserPromptSubmit": [
                { "type": "command", "command": "echo '{\"decision\":\"block\",\"message\":\"not now\"}'" }
            ]
        })),
        Duration::ZERO,
    )
    .await;

    let (sid, done) = turn_complete_of(&srv.sock, None, "hello").await;
    assert_eq!(done["stop_reason"], "stopped_by_hook");
    assert_eq!(done["message"], "not now");

    let (_, again) = turn_complete_of(&srv.sock, Some(&sid), "hello again").await;
    assert_eq!(again["stop_reason"], "stopped_by_hook");
    assert!(seen.lock().unwrap().is_empty(), "the model was called");

    srv.stop().await;
}

#[tokio::test]
async fn a_stop_hook_that_discontinues_still_ends_the_turn_and_frees_the_session() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("answer")],
        hooked_settings(serde_json::json!({
            "Stop": [
                { "type": "command", "command": "echo '{\"continue\":false,\"message\":\"enough\"}'" }
            ]
        })),
        Duration::ZERO,
    )
    .await;

    let (sid, done) = turn_complete_of(&srv.sock, None, "hello").await;
    assert_eq!(done["stop_reason"], "stopped_by_hook");
    assert_eq!(done["message"], "enough");

    let (_, again) = turn_complete_of(&srv.sock, Some(&sid), "hello again").await;
    assert_eq!(again["stop_reason"], "stopped_by_hook");

    srv.stop().await;
}

// ── what is on disk while a turn is still running ───────────────────────

/// A step that finished is on disk before the next one starts, so a daemon
/// killed while the model is answering loses only that answer.
#[tokio::test]
async fn a_finished_step_is_on_disk_while_the_next_one_is_still_running() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round("Glob", "tool-1", serde_json::json!({"pattern": "*"})),
            // An empty round never ends: the model is "still answering".
            vec![],
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let mut client = UnixStream::connect(&srv.sock).await.unwrap();
    client
        .write_all(
            br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"look around"},"id":1}"#,
        )
        .await
        .unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _w) = client.split();
    let mut br = BufReader::new(r);
    let sid = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            assert!(
                br.read_line(&mut line).await.unwrap() > 0,
                "connection closed"
            );
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "tool_result" {
                return v["params"]["session_id"].as_str().unwrap().to_string();
            }
        }
    })
    .await
    .expect("the tool never ran");

    let on_disk = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let h = history(&srv.sock, &sid).await;
            let rendered = serde_json::to_string(&h["messages"]).unwrap();
            if rendered.contains("tool-1") && rendered.contains("tool_result") {
                return rendered;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the finished step never reached the transcript while the turn ran");
    assert!(on_disk.contains("look around"), "{on_disk}");

    srv.stop().await;
}

#[tokio::test]
async fn clear_reaches_the_transcript_and_what_follows_it_does_too() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("first answer"), text_round("second answer")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "before the clear").await;
    let (_, done) = turn_complete_of(&srv.sock, Some(&sid), "/clear").await;
    assert_eq!(done["stop_reason"], "command_executed");
    run_turn(&srv.sock, Some(&sid), "after the clear").await;

    let rendered = serde_json::to_string(&history(&srv.sock, &sid).await["messages"]).unwrap();
    assert!(!rendered.contains("before the clear"), "{rendered}");
    assert!(rendered.contains("after the clear"), "{rendered}");
    assert!(rendered.contains("second answer"), "{rendered}");

    srv.stop().await;
}

// ── frames a watching client needs ──────────────────────────────────────

#[tokio::test]
async fn thinking_reaches_the_client_as_its_own_frame() {
    let mut round = vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Thinking {
                thinking: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::ThinkingDelta {
                thinking: "let me see".into(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::SignatureDelta {
                signature: "sig".into(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
    ];
    for event in text_round("answer") {
        round.push(match event {
            StreamEvent::ContentBlockStart { content_block, .. } => {
                StreamEvent::ContentBlockStart {
                    index: 1,
                    content_block,
                }
            }
            StreamEvent::ContentBlockDelta { delta, .. } => {
                StreamEvent::ContentBlockDelta { index: 1, delta }
            }
            StreamEvent::ContentBlockStop { .. } => StreamEvent::ContentBlockStop { index: 1 },
            other => other,
        });
    }
    let (srv, _seen) =
        start_scripted_server(vec![round], ask_settings_no_memory(), Duration::ZERO).await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"think"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");
    let thinking = events_of_kind(&frames, "thinking_delta");
    assert_eq!(thinking.len(), 1, "{frames:?}");
    assert_eq!(thinking[0]["text"], "let me see");
    let text: String = events_of_kind(&frames, "text_delta")
        .iter()
        .map(|f| f["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "answer");

    srv.stop().await;
}

/// A tab watching a session must learn that a turn it did not start has
/// failed; only the caller gets the error response.
#[tokio::test]
async fn a_failed_turn_tells_every_watcher_it_is_over() {
    let failing = vec![StreamEvent::Error {
        error: model::stream::ApiErrorPayload {
            kind: "invalid_request_error".into(),
            message: "boom".into(),
        },
    }];
    let (srv, _seen) = start_scripted_server(
        vec![text_round("first"), failing],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "hello").await;

    let mut watcher = UnixStream::connect(&srv.sock).await.unwrap();
    watcher
        .write_all(
            format!(
                r#"{{"jsonrpc":"2.0","method":"session.subscribe","id":7,"params":{{"session_id":"{sid}"}}}}"#
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    watcher.write_all(b"\n").await.unwrap();
    let (r, _w) = watcher.split();
    let mut watched = BufReader::new(r);
    let mut line = String::new();
    watched.read_line(&mut line).await.unwrap();
    let subscribed: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(subscribed["result"].is_object(), "{subscribed}");

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"again"}},"id":1}}"#
        ),
    )
    .await;
    assert!(resp["error"].is_object(), "the turn should fail: {resp}");
    assert!(
        events_of_kind(&frames, "error").is_empty(),
        "the caller is told by its response, not twice: {frames:?}"
    );

    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(
                watched.read_line(&mut line).await.unwrap() > 0,
                "watcher closed"
            );
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "error" {
                return v;
            }
        }
    })
    .await
    .expect("the watcher never learned the turn had ended");
    assert_eq!(ended["params"]["session_id"], sid.as_str());

    srv.stop().await;
}

#[tokio::test]
async fn a_turn_reports_how_full_the_context_is_after_its_last_call() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round_costing("Glob", "tool-1", serde_json::json!({"pattern": "*"}), 50, 5),
            text_round_costing("done", 120, 7),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (sid, done) = turn_complete_of(&srv.sock, None, "look").await;
    assert_eq!(done["usage"]["input_tokens"], 170, "{done}");
    assert_eq!(done["last_call_usage"]["input_tokens"], 120, "{done}");
    assert_eq!(done["context"]["tokens"], 120, "{done}");
    assert!(done["context"]["window"].as_u64().unwrap() > 0, "{done}");
    assert!(done["context"]["compact_threshold"].is_u64(), "{done}");

    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","id":2,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(detail["result"]["context"], done["context"], "{detail}");

    srv.stop().await;
}

// ── messages sent while a turn is running ───────────────────────────────

/// Start a turn whose Bash call stops at a permission prompt, and return the
/// connection it streams on together with the session and prompt ids. The
/// prompt holds the turn mid-step for as long as the test needs.
async fn turn_held_at_a_prompt(
    sock: &std::path::Path,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
    String,
    String,
) {
    let conn = UnixStream::connect(sock).await.unwrap();
    let (r, mut w) = conn.into_split();
    w.write_all(
        br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","turn_id":"t1"},"id":1}"#,
    )
    .await
    .unwrap();
    w.write_all(b"\n").await.unwrap();
    let mut br = BufReader::new(r);
    let (sid, prompt_id) = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            assert!(
                br.read_line(&mut line).await.unwrap() > 0,
                "connection closed"
            );
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "prompt" {
                break (
                    v["params"]["session_id"].as_str().unwrap().to_string(),
                    v["params"]["event"]["prompt_id"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                );
            }
        }
    })
    .await
    .expect("the turn never asked");
    (br, w, sid, prompt_id)
}

async fn drain_to_response(
    br: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut frames = Vec::new();
        loop {
            let mut line = String::new();
            assert!(
                br.read_line(&mut line).await.unwrap() > 0,
                "connection closed"
            );
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v.get("id").is_some() {
                return (frames, v);
            }
            frames.push(v);
        }
    })
    .await
    .expect("the turn never finished")
}

async fn permit(sock: &std::path::Path, sid: &str, prompt_id: &str) {
    let ack = rpc(
        sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}","decision":{{"type":"permit"}}}},"id":99}}"#
        ),
    )
    .await;
    assert_eq!(ack["result"]["prompt_id"], prompt_id, "{ack}");
}

#[tokio::test]
async fn a_steered_message_reaches_the_running_turn_after_its_tool_step() {
    let (srv, seen) = start_scripted_server(
        asking_bash_rounds("steered.txt"),
        ask_settings_no_memory(),
        Duration::from_secs(30),
    )
    .await;
    let (mut turn, _w, sid, prompt_id) = turn_held_at_a_prompt(&srv.sock).await;

    let refused = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"hello?"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        codes::SESSION_BUSY,
        "the default stays reject: {refused}"
    );

    let steered = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"also add a README","turn_id":"m1","on_busy":"steer"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(steered["result"]["delivery"], "steer", "{steered}");
    assert_eq!(steered["result"]["into_turn_id"], "t1", "{steered}");

    permit(&srv.sock, &sid, &prompt_id).await;
    let (frames, response) = drain_to_response(&mut turn).await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert_eq!(response["result"]["delivery"], "turn");

    let delivered = events_of_kind(&frames, "user_message_delivered");
    assert_eq!(delivered.len(), 1, "{frames:?}");
    assert_eq!(delivered[0]["message_id"], "m1");
    assert_eq!(delivered[0]["content"], "also add a README");
    let kinds: Vec<String> = frames
        .iter()
        .map(|f| {
            f["params"]["event"]["kind"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
    assert!(at("tool_result") < at("user_message_delivered"));
    assert!(at("user_message_delivered") < at("turn_complete"));
    assert_eq!(events_of_kind(&frames, "turn_complete").len(), 1);

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "the tool call, then the answer");
    let second = serde_json::to_value(&requests[1].messages).unwrap();
    let rendered: Vec<String> = second
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.to_string())
        .collect();
    let steer_at = rendered
        .iter()
        .position(|m| m.contains("also add a README"))
        .expect("the answer was asked with the message in it");
    let result_at = rendered
        .iter()
        .position(|m| m.contains("tool_result"))
        .expect("and the tool result");
    assert!(result_at < steer_at, "{rendered:?}");

    let transcript = history(&srv.sock, &sid).await.to_string();
    assert!(transcript.contains("also add a README"), "{transcript}");

    srv.stop().await;
}

#[tokio::test]
async fn a_queued_message_runs_as_the_next_turn_once_the_session_is_free() {
    let mut rounds = asking_bash_rounds("queued.txt");
    rounds.push(text_round("second answer"));
    let (srv, seen) =
        start_scripted_server(rounds, ask_settings_no_memory(), Duration::from_secs(30)).await;
    let (mut turn, _w, sid, prompt_id) = turn_held_at_a_prompt(&srv.sock).await;

    let queued = {
        let sock = srv.sock.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            rpc_streaming(
                &sock,
                &format!(
                    r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"next","turn_id":"t2","on_busy":"queue"}},"id":4}}"#
                ),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !queued.is_finished(),
        "a queued turn waits for the running one"
    );

    permit(&srv.sock, &sid, &prompt_id).await;
    let (_frames, first) = drain_to_response(&mut turn).await;
    assert!(first["result"].is_object(), "{first}");

    let (frames, second) = tokio::time::timeout(Duration::from_secs(20), queued)
        .await
        .expect("the queued turn never ran")
        .unwrap();
    assert_eq!(second["result"]["turn_id"], "t2", "{second}");
    assert_eq!(second["result"]["delivery"], "turn");
    assert_eq!(events_of_kind(&frames, "turn_complete").len(), 1);
    assert_eq!(seen.lock().unwrap().len(), 3);

    srv.stop().await;
}

#[tokio::test]
async fn an_interrupted_turn_hands_back_a_steered_message_it_never_read() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("returned.txt"),
        ask_settings_no_memory(),
        Duration::from_secs(30),
    )
    .await;
    let (mut turn, _w, sid, _prompt_id) = turn_held_at_a_prompt(&srv.sock).await;

    let steered = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"wait, stop","turn_id":"m1","on_busy":"steer"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(steered["result"]["delivery"], "steer", "{steered}");
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":5}}"#
        ),
    )
    .await;

    let (frames, _response) = drain_to_response(&mut turn).await;
    let returned = events_of_kind(&frames, "user_messages_returned");
    assert_eq!(returned.len(), 1, "{frames:?}");
    assert_eq!(returned[0]["messages"][0]["message_id"], "m1");
    assert_eq!(returned[0]["messages"][0]["content"], "wait, stop");
    assert!(events_of_kind(&frames, "user_message_delivered").is_empty());

    let transcript = history(&srv.sock, &sid).await.to_string();
    assert!(!transcript.contains("wait, stop"), "{transcript}");

    srv.stop().await;
}

// ── structured output against a host schema ────────────────────────────

#[tokio::test]
async fn structured_output_is_checked_against_the_host_schema_and_returned_on_turn_complete() {
    let (srv, seen) = start_scripted_server(
        vec![
            tool_round("StructuredOutput", "so-1", serde_json::json!({"wrong": 1})),
            tool_round(
                "StructuredOutput",
                "so-2",
                serde_json::json!({"answer": "42"}),
            ),
            text_round("done"),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
        "additionalProperties": false
    });
    let (frames, response) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"message":"answer in the format","options":{{"output_schema":{schema}}}}},"id":1}}"#
        ),
    )
    .await;
    assert!(response["result"].is_object(), "{response}");

    let results = events_of_kind(&frames, "tool_result");
    assert_eq!(results.len(), 2, "{frames:?}");
    assert_eq!(results[0]["is_error"], true, "{results:?}");
    assert!(
        results[0]["content"]
            .as_str()
            .unwrap()
            .contains("does not match the required schema"),
        "{results:?}"
    );
    assert_eq!(results[1]["is_error"], false, "{results:?}");

    let done = events_of_kind(&frames, "turn_complete");
    assert_eq!(
        done[0]["structured_output"],
        serde_json::json!({"answer": "42"}),
        "{done:?}"
    );

    let first = serde_json::to_value(&seen.lock().unwrap()[0]).unwrap();
    let tool = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "StructuredOutput")
        .expect("announced up front, not left for ToolSearch")
        .clone();
    assert_eq!(tool["input_schema"], schema, "{tool}");

    srv.stop().await;
}

// ── session.compact / session.clear ─────────────────────────────────────

#[tokio::test]
async fn session_compact_compacts_a_short_conversation_on_request() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round(&"a long answer ".repeat(40))],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "first").await;
    for i in 0..5 {
        run_turn(&srv.sock, Some(&sid), &format!("more {i}")).await;
    }

    let (frames, response) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.compact","params":{{"session_id":"{sid}"}},"id":7}}"#
        ),
    )
    .await;
    assert!(response["result"].is_object(), "{response}");
    let compact = events_of_kind(&frames, "compact");
    assert_eq!(compact.len(), 1, "{frames:?}");
    let before = compact[0]["tokens_before"].as_u64().unwrap();
    let after = compact[0]["tokens_after"].as_u64().unwrap();
    assert!(after < before, "{compact:?}");
    assert!(
        compact[0]["messages_after"].as_u64() < compact[0]["messages_before"].as_u64(),
        "{compact:?}"
    );
    let done = events_of_kind(&frames, "turn_complete");
    assert_eq!(done[0]["stop_reason"], "command_executed", "{done:?}");

    let (frames, response) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.clear","params":{{"session_id":"{sid}"}},"id":8}}"#
        ),
    )
    .await;
    assert!(response["result"].is_object(), "{response}");
    assert_eq!(
        events_of_kind(&frames, "turn_complete")[0]["stop_reason"],
        "command_executed"
    );

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.compact","params":{},"id":9}"#,
    )
    .await;
    assert_eq!(missing["error"]["code"], codes::INVALID_PARAMS, "{missing}");

    srv.stop().await;
}

// ── turn and step boundaries ────────────────────────────────────────────

/// A client can draw a turn without guessing where it begins and where one
/// model call ends: `turn_start` comes first, every call is bracketed by
/// `step_start` / `step_end`, and the tool frames of a call fall inside it.
#[tokio::test]
async fn a_turn_and_each_of_its_model_calls_are_bracketed_by_frames() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round_costing("Glob", "tool-1", serde_json::json!({"pattern": "*"}), 7, 3),
            text_round_costing("done", 11, 5),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"look"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");

    let kinds: Vec<String> = frames
        .iter()
        .map(|f| {
            f["params"]["event"]["kind"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .filter(|k| {
            matches!(
                k.as_str(),
                "turn_start"
                    | "step_start"
                    | "step_end"
                    | "tool_use"
                    | "tool_result"
                    | "text_delta"
                    | "turn_complete"
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "turn_start",
            "step_start",
            "tool_use",
            "tool_result",
            "step_end",
            "step_start",
            "text_delta",
            "step_end",
            "turn_complete"
        ],
        "{frames:#?}"
    );

    let ends = events_of_kind(&frames, "step_end");
    assert_eq!(ends[0]["step"], 0);
    assert_eq!(ends[0]["stop_reason"], "tool_use");
    assert_eq!(ends[0]["tool_calls"], 1);
    assert_eq!(ends[0]["usage"]["input_tokens"], 7);
    assert_eq!(ends[1]["step"], 1);
    assert_eq!(ends[1]["usage"]["output_tokens"], 5);
    let turn_ids: std::collections::HashSet<&str> = frames
        .iter()
        .filter_map(|f| f["params"]["turn_id"].as_str())
        .collect();
    assert_eq!(turn_ids.len(), 1, "every frame belongs to the one turn");

    srv.stop().await;
}

/// A tool row can be drawn while the model is still writing the arguments:
/// `tool_use_start` names the call first, the argument fragments follow, and
/// they concatenate to exactly the input `tool_use` then carries.
#[tokio::test]
async fn a_tool_call_is_announced_before_its_arguments_are_complete() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round("Glob", "tool-7", serde_json::json!({"pattern": "*.rs"})),
            text_round("done"),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"find"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");

    let kinds: Vec<&str> = frames
        .iter()
        .filter_map(|f| f["params"]["event"]["kind"].as_str())
        .filter(|k| k.starts_with("tool_"))
        .collect();
    assert_eq!(
        kinds,
        [
            "tool_use_start",
            "tool_args_delta",
            "tool_use",
            "tool_result"
        ],
        "{frames:#?}"
    );
    let start = events_of_kind(&frames, "tool_use_start")[0];
    assert_eq!(start["id"], "tool-7");
    assert_eq!(start["name"], "Glob");
    let args: String = events_of_kind(&frames, "tool_args_delta")
        .iter()
        .map(|f| f["partial_json"].as_str().unwrap())
        .collect();
    let used = events_of_kind(&frames, "tool_use")[0];
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&args).unwrap(),
        used["input"]
    );

    srv.stop().await;
}

// ── live frames and history tell the same story ─────────────────────────

/// What a client needs from a turn, read off the live frames.
fn reduce_frames(frames: &[serde_json::Value]) -> serde_json::Value {
    let text: String = events_of_kind(frames, "text_delta")
        .iter()
        .map(|f| f["text"].as_str().unwrap())
        .collect();
    let calls: Vec<&serde_json::Value> = events_of_kind(frames, "tool_use")
        .iter()
        .map(|f| &f["id"])
        .collect();
    let results: Vec<&serde_json::Value> = events_of_kind(frames, "tool_result")
        .iter()
        .map(|f| &f["id"])
        .collect();
    serde_json::json!({"text": text, "calls": calls, "results": results})
}

/// The same, read off `session.history` for one turn.
fn reduce_history(messages: &[serde_json::Value], turn_id: &str) -> serde_json::Value {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for m in messages.iter().filter(|m| m["turn_id"] == turn_id) {
        for block in m["content"].as_array().unwrap() {
            match block["type"].as_str().unwrap() {
                "text" if m["role"] == "assistant" => {
                    text.push_str(block["text"].as_str().unwrap())
                }
                "tool_use" => calls.push(block["id"].clone()),
                "tool_result" => results.push(block["tool_use_id"].clone()),
                _ => {}
            }
        }
    }
    serde_json::json!({"text": text, "calls": calls, "results": results})
}

/// A tab that watched a turn live and a tab that opened it afterwards from
/// `session.history` must end up showing the same thing — and the positions
/// the live frames report are positions in that history.
#[tokio::test]
async fn the_live_frames_and_the_history_of_a_turn_agree() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round_costing("Glob", "tool-1", serde_json::json!({"pattern": "*"}), 7, 3),
            text_round_costing("all done", 11, 5),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"look"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");
    let sid = resp["result"]["session_id"].as_str().unwrap().to_string();
    let turn_id = frames[0]["params"]["turn_id"].as_str().unwrap().to_string();

    let h = history(&srv.sock, &sid).await;
    let messages = h["messages"].as_array().unwrap();
    assert_eq!(reduce_frames(&frames), reduce_history(messages, &turn_id));

    let origins: Vec<&str> = messages
        .iter()
        .map(|m| m["origin"].as_str().unwrap())
        .collect();
    assert_eq!(origins, ["user", "assistant", "tool_result", "assistant"]);
    assert_eq!(messages[1]["usage"]["input_tokens"], 7);
    assert_eq!(messages[3]["usage"]["output_tokens"], 5);
    for (i, m) in messages.iter().enumerate() {
        assert_eq!(m["seq"], i);
        assert!(m["id"].is_string() && m["ts"].is_string(), "{m}");
    }

    let ends = events_of_kind(&frames, "step_end");
    assert_eq!(ends[0]["seq"], 3, "user, reply, results are on disk");
    let done = events_of_kind(&frames, "turn_complete")[0];
    assert_eq!(done["seq"], h["total"]);

    let sub = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(sub["result"]["last_seq"], h["total"], "{sub}");

    srv.stop().await;
}
