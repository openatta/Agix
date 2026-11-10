//! What the protocol document says goes *to* the daemon and what comes back
//! outside a `result`, checked against a real daemon.
//!
//! `protocol_doc_examples.rs` compares each method's documented `result`. This
//! file holds the other three shapes a client is written against:
//!
//! - every `// → params` request example is sent as written (placeholders
//!   filled in) and must not be refused as `INVALID_PARAMS` or
//!   `METHOD_NOT_FOUND`;
//! - every `kind` in the §7 and §8 tables is provoked on a daemon with a
//!   scripted model and the frame's field set is compared with the row;
//! - every `error.data` shape in §9.1 is provoked and compared.
//!
//! What cannot be provoked in a test is listed in [`NOT_SENT`],
//! [`SESSION_EVENTS_NOT_PROVOKED`] and [`DAEMON_EVENTS_NOT_PROVOKED`] with the
//! reason, and the tests hold those lists to the document and to what was
//! actually observed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base::settings::Settings;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::rpc::codes;
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

use super::protocol_doc_examples::{protocol_doc, section, strip_line_comment, OneTurnClient};

struct Harness {
    sock: PathBuf,
    project: PathBuf,
    store: Arc<dyn history::store::HistoryStore>,
    dir: tempfile::TempDir,
}

async fn start(
    model: Arc<dyn AnthropicClient>,
    scene: Arc<dyn base::interface::scene::Scene>,
    session_cap: usize,
    configure: impl FnOnce(&mut Settings),
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wire.sock");
    let project = dir.path().join("work");
    std::fs::create_dir_all(&project).unwrap();

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = session_cap;
    config.settings.memory_enabled = false;
    configure(&mut config.settings);

    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &project,
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .unwrap(),
    );
    let pool = daemon::assemble::pool(
        &config,
        scene,
        daemon::Assembly {
            cwd: Some(project.clone()),
            model_client: Some(model),
            transcripts: daemon::Transcripts::In(store.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let serving = server;
    let bind = sock.clone();
    tokio::spawn(async move {
        let _ = serving.serve_unix(&bind).await;
    });
    for _ in 0..80 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");
    Harness {
        sock,
        project,
        store,
        dir,
    }
}

async fn start_code(model: Arc<dyn AnthropicClient>) -> Harness {
    start(
        model,
        Arc::new(scene::code::CodeScene::default()),
        8,
        |_| {},
    )
    .await
}

fn bypass(settings: &mut Settings) {
    settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;
}

struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    pushed: Vec<Value>,
    next_id: u64,
}

impl Conn {
    async fn open(sock: &Path) -> Self {
        let (r, w) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(r),
            writer: w,
            pushed: Vec::new(),
            next_id: 1,
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "method": method, "params": params, "id": id});
        self.writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        id
    }

    async fn read(&mut self) -> Value {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(30), self.reader.read_line(&mut line))
            .await
            .expect("no frame within 30s")
            .unwrap();
        assert!(n > 0, "connection closed");
        serde_json::from_str(&line).unwrap()
    }

    async fn response(&mut self, id: u64) -> Value {
        loop {
            let frame = self.read().await;
            if frame.get("id") == Some(&json!(id)) {
                return frame;
            }
            self.pushed.push(frame);
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.response(id).await
    }
}

async fn call(sock: &Path, method: &str, params: Value) -> Value {
    Conn::open(sock).await.call(method, params).await
}

async fn ok(sock: &Path, method: &str, params: Value) -> Value {
    let response = call(sock, method, params).await;
    assert!(
        response.get("result").is_some(),
        "{method} failed: {response}"
    );
    response["result"].clone()
}

async fn new_session(h: &Harness) -> String {
    ok(&h.sock, "session.create", json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A connection that has subscribed and keeps every pushed frame's `params`.
struct Watch {
    frames: Arc<Mutex<Vec<Value>>>,
}

impl Watch {
    async fn spawn(sock: &Path, method: &str, params: Value, pushes: &'static str) -> Self {
        let mut conn = Conn::open(sock).await;
        let response = conn.call(method, params).await;
        assert!(response.get("result").is_some(), "{method}: {response}");
        let frames: Arc<Mutex<Vec<Value>>> = Arc::default();
        let mut seen = std::mem::take(&mut conn.pushed);
        let into = frames.clone();
        tokio::spawn(async move {
            // The write half stays open for as long as the watcher listens:
            // a daemon closes a connection whose peer has stopped writing.
            let Conn {
                mut reader,
                writer: _writer,
                ..
            } = conn;
            loop {
                for frame in seen.drain(..) {
                    if frame["method"] == pushes {
                        into.lock().unwrap().push(frame["params"].clone());
                    }
                }
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                seen.push(serde_json::from_str(&line).unwrap());
            }
        });
        Self { frames }
    }

    async fn session(sock: &Path, session_id: &str) -> Self {
        Self::spawn(
            sock,
            "session.subscribe",
            json!({"session_id": session_id}),
            "session.event",
        )
        .await
    }

    async fn daemon(sock: &Path) -> Self {
        Self::spawn(sock, "daemon.subscribeEvents", json!({}), "daemon.event").await
    }

    fn snapshot(&self) -> Vec<Value> {
        self.frames.lock().unwrap().clone()
    }

    async fn wait_for(&self, what: &str, hit: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..1200 {
            if let Some(found) = self.snapshot().into_iter().find(|f| hit(f)) {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("never saw {what}; saw {:#?}", self.snapshot());
    }

    async fn wait_kind(&self, kind: &str) -> Value {
        self.wait_for(kind, |f| event_kind(f) == kind).await
    }
}

fn event_kind(frame: &Value) -> &str {
    frame["event"]["kind"]
        .as_str()
        .or_else(|| frame["kind"].as_str())
        .unwrap_or("")
}

fn text_round(text: &str) -> Vec<StreamEvent> {
    let mut round = text_blocks(0, text);
    round.push(end_turn());
    round
}

fn text_blocks(index: u32, text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index,
            delta: BlockDelta::TextDelta { text: text.into() },
        },
        StreamEvent::ContentBlockStop { index },
    ]
}

fn end_turn() -> StreamEvent {
    StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: Some(base::message::StopReason::EndTurn),
            stop_sequence: None,
        },
        usage: Some(Usage::default()),
    }
}

fn tool_blocks(index: u32, tool: &str, id: &str, input: &Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: tool.to_string(),
                input: Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index },
    ]
}

fn tool_round(tool: &str, id: &str, input: Value) -> Vec<StreamEvent> {
    let mut round = tool_blocks(0, tool, id, &input);
    round.push(StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: Some(base::message::StopReason::ToolUse),
            stop_sequence: None,
        },
        usage: Some(Usage::default()),
    });
    round
}

fn scripted(rounds: Vec<Vec<StreamEvent>>) -> Arc<MockAnthropicClient> {
    let model = Arc::new(MockAnthropicClient::new());
    for round in rounds {
        model.push_turn(round);
    }
    model
}

/// Run one `session.run_turn` to its final response on a connection of its
/// own, handing back every frame that preceded it.
async fn run_turn(sock: &Path, params: Value) -> (Vec<Value>, Value) {
    let mut conn = Conn::open(sock).await;
    let response = conn.call("session.run_turn", params).await;
    (conn.pushed, response)
}

// ---------------------------------------------------------------- requests

struct RequestExample {
    methods: Vec<String>,
    ordinal: usize,
    params: Value,
}

fn headed_methods(heading: &str) -> Vec<String> {
    heading
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|name| name.contains('.') && !name.contains(' '))
        .map(str::to_string)
        .collect()
}

/// `{ "type": "object", … }` says "and more"; as JSON it is the object
/// without the ellipsis.
fn without_bare_ellipsis(json: &str) -> String {
    let mut out = String::new();
    let (mut in_string, mut escaped) = (false, false);
    for c in json.chars() {
        if in_string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '…' => {}
            '}' | ']' => {
                let kept = out.trim_end().trim_end_matches(',').len();
                out.truncate(kept);
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Every JSON value a `// → …` marker introduces in a method's section, with
/// the methods it is addressed to: the ones the marker names, else the ones
/// whose last segment it mentions (`// → params (getProvider)`), else all of
/// the heading's.
fn documented_requests(doc: &str) -> Vec<RequestExample> {
    let mut out = Vec::new();
    for heading in doc.lines().filter(|l| l.starts_with("#### ")) {
        let methods = headed_methods(heading);
        let Some(first) = methods.first() else {
            continue;
        };
        for block in section(doc, first)
            .split("```jsonc\n")
            .skip(1)
            .filter_map(|rest| rest.split_once("\n```").map(|(block, _)| block))
        {
            let mut targets: Vec<String> = Vec::new();
            let mut json = String::new();
            let mut collecting = false;
            let flush = |json: &mut String, targets: &[String], out: &mut Vec<RequestExample>| {
                let values = serde_json::Deserializer::from_str(&without_bare_ellipsis(json))
                    .into_iter::<Value>()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap_or_else(|e| {
                        panic!("`{first}`'s request example does not parse ({e}):\n{json}")
                    });
                for params in values {
                    out.push(RequestExample {
                        methods: targets.to_vec(),
                        ordinal: out.len(),
                        params,
                    });
                }
                json.clear();
            };
            for line in block.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("// ←") {
                    flush(&mut json, &targets, &mut out);
                    collecting = false;
                } else if trimmed.starts_with("//") && trimmed.contains('→') {
                    flush(&mut json, &targets, &mut out);
                    collecting = true;
                    let named: Vec<String> = methods
                        .iter()
                        .filter(|m| trimmed.contains(m.as_str()))
                        .cloned()
                        .collect();
                    let by_suffix: Vec<String> = methods
                        .iter()
                        .filter(|m| trimmed.contains(m.rsplit('.').next().unwrap()))
                        .cloned()
                        .collect();
                    targets = if !named.is_empty() {
                        named
                    } else if !by_suffix.is_empty() && by_suffix.len() < methods.len() {
                        by_suffix
                    } else {
                        methods.clone()
                    };
                } else if collecting {
                    json.push_str(strip_line_comment(line));
                    json.push('\n');
                }
            }
            flush(&mut json, &targets, &mut out);
        }
    }
    out
}

/// Request examples that cannot be sent in a test, each with the reason.
/// Keyed by the method and the example's position among that method's.
const NOT_SENT: &[(&str, &str)] = &[
    (
        "mcp.auth.complete#0",
        "needs a sign-in in progress, and only a real authorization server starts one",
    ),
    (
        "mcp.auth.setIdentity#0",
        "needs an enterprise IdP configured and a token it issued",
    ),
    (
        "mcp.auth.setIdentity#1",
        "needs an enterprise IdP configured and a token it issued",
    ),
    (
        "plugin.install#0",
        "needs a plugin archive whose checksum matches; the example's path is a stand-in",
    ),
];

/// Documented ids and paths stand for this machine's; fill in the real ones.
fn fill(value: &mut Value, key: Option<&str>, session_id: &str, h: &Harness) {
    match value {
        Value::String(s) if key == Some("session_id") && s.contains('…') => {
            *s = session_id.to_string()
        }
        Value::String(s) if key == Some("project_root") && s.starts_with("/Users/x/") => {
            *s = h.project.display().to_string()
        }
        Value::String(s) if key == Some("redirect_uri") && s.contains('…') => {
            *s = "https://app.example/oauth/done".into()
        }
        Value::String(s) if key == Some("path") && s.starts_with("/abs/") => {
            *s = h.dir.path().join("export.agix.zip").display().to_string()
        }
        Value::Array(items) => items.iter_mut().for_each(|v| fill(v, None, session_id, h)),
        Value::Object(fields) => fields
            .iter_mut()
            .for_each(|(k, v)| fill(v, Some(k), session_id, h)),
        _ => {}
    }
}

/// What a method needs to exist before its example makes sense.
async fn arrange(h: &Harness, method: &str, session_id: &str) {
    match method {
        "session.updateGoal" => {
            ok(
                &h.sock,
                "session.setGoal",
                json!({"session_id": session_id, "objective": "finish"}),
            )
            .await;
        }
        "session.dropMessages" | "session.editMessage" | "session.fork" | "session.history" => {
            for n in 0..4 {
                ok(
                    &h.sock,
                    "session.run_turn",
                    json!({"session_id": session_id, "message": format!("m{n}")}),
                )
                .await;
            }
        }
        "mcp.auth.start" => {
            ok(&h.sock, "project.trust", json!({"trusted": true})).await;
            ok(
                &h.sock,
                "mcp.addServer",
                json!({"name": "docs",
                       "config": {"type": "streamable_http", "url": "http://127.0.0.1:9/mcp"}}),
            )
            .await;
        }
        "import.run" => std::fs::write(h.project.join("CLAUDE.md"), "# notes\n").unwrap(),
        _ => {}
    }
}

#[tokio::test]
async fn documented_request_examples_are_accepted_by_a_real_daemon() {
    let doc = protocol_doc();
    let h = start_code(Arc::new(OneTurnClient)).await;
    // Routing a session to a configured provider replaces the scripted model,
    // and the MCP server `mcp.auth.start` needs makes every later project
    // write wait out its failed connection: those examples get a daemon of
    // their own.
    let with_providers = start(
        Arc::new(OneTurnClient),
        Arc::new(scene::code::CodeScene::default()),
        8,
        |settings| {
            settings.default_provider = Some("anthropic".into());
            for (id, model) in [
                ("deepseek", "deepseek-chat"),
                ("anthropic", "claude-opus-4-6"),
            ] {
                settings.providers.insert(
                    id.into(),
                    base::provider::ProviderConfig {
                        api_type: Some("anthropic".into()),
                        base_url: Some("http://127.0.0.1:9".into()),
                        api_key: Some("unused".into()),
                        default_model: Some(model.into()),
                        models: vec![model.into()],
                        ..Default::default()
                    },
                );
            }
        },
    )
    .await;
    let examples = documented_requests(&doc);
    assert!(
        examples.len() >= 45,
        "the request examples are no longer being read out of the document: {}",
        examples.len()
    );

    let mut sent = 0;
    let mut refused = Vec::new();
    for example in &examples {
        for method in &example.methods {
            let position = examples
                .iter()
                .filter(|e| e.methods.contains(method) && e.ordinal < example.ordinal)
                .count();
            let key = format!("{method}#{position}");
            if NOT_SENT.iter().any(|(k, _)| *k == key) {
                continue;
            }
            let h = if matches!(
                method.as_str(),
                "session.create" | "session.setModel" | "mcp.auth.start"
            ) {
                &with_providers
            } else {
                &h
            };
            let session_id = new_session(h).await;
            arrange(h, method, &session_id).await;
            let mut params = example.params.clone();
            fill(&mut params, None, &session_id, h);
            if method.starts_with("plugin.") && params["name"] == "…" {
                params["name"] = json!("demo-plugin");
            }
            if params.get("total").is_some() {
                let history = ok(
                    &h.sock,
                    "session.history",
                    json!({"session_id": session_id}),
                )
                .await;
                params["total"] = history["total"].clone();
                if params.get("at").is_some_and(Value::is_array) {
                    let humans: Vec<Value> = history["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|m| m["origin"] == "user")
                        .take(2)
                        .map(|m| m["seq"].clone())
                        .collect();
                    params["at"] = json!(humans);
                }
            }
            let response = call(&h.sock, method, params.clone()).await;
            sent += 1;
            let code = response["error"]["code"].as_i64();
            let bad = [
                i64::from(codes::INVALID_PARAMS),
                i64::from(codes::METHOD_NOT_FOUND),
            ];
            if code.is_some_and(|c| bad.contains(&c)) {
                refused.push(format!("{key}: {params}\n    -> {response}"));
            }
        }
    }
    assert!(
        refused.is_empty(),
        "documented requests the daemon does not accept:\n{}",
        refused.join("\n")
    );
    eprintln!("sent {sent} documented request examples");
    for (key, _) in NOT_SENT {
        let (method, _) = key.split_once('#').unwrap();
        assert!(
            examples
                .iter()
                .any(|e| e.methods.iter().any(|m| m == method)),
            "{key} is listed in NOT_SENT but the document has no such example"
        );
    }
}

// -------------------------------------------------------------- error.data

fn documented_error_data(doc: &str) -> BTreeMap<String, Vec<Value>> {
    let start = doc.find("### 9.1").expect("the doc has a §9.1");
    let body = &doc[start..];
    let block = body
        .split("```jsonc\n")
        .nth(1)
        .and_then(|rest| rest.split_once("\n```"))
        .expect("§9.1 has its example block")
        .0;
    let mut by_code: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut label: Option<String> = None;
    let mut json = String::new();
    let mut flush = |label: &Option<String>, json: &mut String| {
        if let Some(label) = label {
            let values = serde_json::Deserializer::from_str(json)
                .into_iter::<Value>()
                .collect::<Result<Vec<_>, _>>()
                .unwrap_or_else(|e| panic!("§9.1's {label} example does not parse ({e}):\n{json}"));
            by_code.entry(label.clone()).or_default().extend(values);
        }
        json.clear();
    };
    for line in block.lines() {
        let trimmed = line.trim();
        let code = trimmed.strip_prefix("// ").filter(|name| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        });
        if let Some(code) = code {
            flush(&label, &mut json);
            label = Some(code.to_string());
        } else {
            json.push_str(strip_line_comment(line));
            json.push('\n');
        }
    }
    flush(&label, &mut json);
    by_code
}

fn keys_of(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

fn assert_data_shape(code: &str, documented: &[Value], response: &Value) {
    let data = &response["error"]["data"];
    assert!(
        documented.iter().any(|d| keys_of(d) == keys_of(data)),
        "{code}: error.data is {data}; the document shows {documented:?}\n{response}"
    );
}

struct Held {
    conn: Conn,
    run_id: u64,
    session: String,
    prompt: Value,
}

/// A turn whose Bash call has stopped at a permission prompt, which holds it
/// mid-step for as long as the test needs.
async fn hold_at_prompt(h: &Harness, extra: Value) -> Held {
    let mut conn = Conn::open(&h.sock).await;
    let mut params = json!({"message": "make a file", "turn_id": "t1"});
    for (k, v) in extra.as_object().into_iter().flatten() {
        params[k] = v.clone();
    }
    let run_id = conn.send("session.run_turn", params).await;
    loop {
        let frame = conn.read().await;
        let is_prompt = frame["params"]["event"]["kind"] == "prompt";
        let session = frame["params"]["session_id"].as_str().map(str::to_string);
        let prompt = frame["params"]["event"].clone();
        conn.pushed.push(frame);
        if is_prompt {
            return Held {
                conn,
                run_id,
                session: session.unwrap(),
                prompt,
            };
        }
    }
}

async fn answer_prompt(sock: &Path, session: &str, prompt: &Value) {
    let params = if prompt["prompt_type"] == "question" {
        json!({"session_id": session, "prompt_id": prompt["prompt_id"],
               "prompt_type": "question", "answer": {"type": "answer", "text": "a"}})
    } else {
        json!({"session_id": session, "prompt_id": prompt["prompt_id"],
               "decision": {"type": "permit"}})
    };
    ok(sock, "session.respondToPrompt", params).await;
}

/// Read a turn to its final response, answering whatever it asks on the way.
async fn drive(conn: &mut Conn, run_id: u64, sock: &Path) -> Value {
    loop {
        let frame = conn.read().await;
        if frame.get("id") == Some(&json!(run_id)) {
            return frame;
        }
        if frame["params"]["event"]["kind"] == "prompt" {
            answer_prompt(
                sock,
                frame["params"]["session_id"].as_str().unwrap(),
                &frame["params"]["event"],
            )
            .await;
        }
        conn.pushed.push(frame);
    }
}

fn asks_for_a_file() -> Vec<Vec<StreamEvent>> {
    vec![
        tool_round("Bash", "tool-1", json!({"command": "touch made.txt"})),
        text_round("done"),
    ]
}

/// Never answers a request that asks it to "keep thinking", so the sub-agent
/// started with that prompt stays a running task.
struct HoldsOne {
    held: std::sync::atomic::AtomicUsize,
}

impl AnthropicClient for HoldsOne {
    fn stream_messages(&self, req: model::types::MessagesRequest) -> model::client::EventStream {
        let asked = serde_json::to_string(&req.messages).unwrap_or_default();
        if asked.contains("keep thinking") {
            self.held.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Box::pin(futures::stream::pending());
        }
        OneTurnClient.stream_messages(req)
    }

    fn count_tokens<'a>(
        &'a self,
        _req: &'a model::types::MessagesRequest,
    ) -> model::client::CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

async fn transcript_in_scene(
    h: &Harness,
    scene: &str,
    parent: Option<&str>,
) -> base::session::SessionId {
    let sid = base::session::SessionId::new();
    h.store
        .append(
            sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: parent.map(str::to_string),
                scene: Some(scene.to_string()),
                project_root: None,
                session_kind: if parent.is_some() {
                    history::entry::SessionKind::Sidechain
                } else {
                    history::entry::SessionKind::Primary
                },
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();
    sid
}

#[tokio::test]
async fn documented_error_data_is_what_the_errors_carry() {
    let doc = protocol_doc();
    let documented = documented_error_data(&doc);
    for code in [
        "SESSION_BUSY",
        "SCENE_MISMATCH",
        "SIDECHAIN_TERMINAL",
        "METHOD_NOT_FOUND",
    ] {
        assert!(
            documented.contains_key(code),
            "§9.1 no longer has a `// {code}` example"
        );
    }

    let h = start_code(scripted(asks_for_a_file())).await;
    let mut held = hold_at_prompt(&h, json!({})).await;
    let busy = call(
        &h.sock,
        "session.run_turn",
        json!({"session_id": held.session, "message": "again"}),
    )
    .await;
    assert_eq!(busy["error"]["code"], codes::SESSION_BUSY, "{busy}");
    assert_data_shape("SESSION_BUSY", &documented["SESSION_BUSY"], &busy);
    assert_eq!(busy["error"]["data"]["current_turn_id"], "t1");
    let rebuild = call(
        &h.sock,
        "session.setPrompt",
        json!({"session_id": held.session, "append": "Be brief."}),
    )
    .await;
    assert_eq!(rebuild["error"]["code"], codes::SESSION_BUSY, "{rebuild}");
    assert_data_shape("SESSION_BUSY", &documented["SESSION_BUSY"], &rebuild);
    answer_prompt(&h.sock, &held.session, &held.prompt).await;
    let finished = drive(&mut held.conn, held.run_id, &h.sock).await;
    assert!(finished.get("result").is_some(), "{finished}");

    let model = Arc::new(HoldsOne {
        held: Default::default(),
    });
    let h = start_code(model.clone()).await;
    let session = new_session(&h).await;
    let started = ok(
        &h.sock,
        "session.startAgent",
        json!({"session_id": session, "prompt": "keep thinking"}),
    )
    .await;
    for _ in 0..400 {
        if model.held.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    ok(
        &h.sock,
        "session.run_turn",
        json!({"session_id": session, "message": "hello"}),
    )
    .await;
    let blocked = call(
        &h.sock,
        "session.setPrompt",
        json!({"session_id": session, "append": "Be brief."}),
    )
    .await;
    assert_eq!(blocked["error"]["code"], codes::SESSION_BUSY, "{blocked}");
    assert_data_shape("SESSION_BUSY", &documented["SESSION_BUSY"], &blocked);
    assert_eq!(
        blocked["error"]["data"]["running_task_ids"],
        json!([started["task_id"]]),
        "{blocked}"
    );

    let foreign = transcript_in_scene(&h, "harnix", None).await;
    let mismatch = call(
        &h.sock,
        "session.resume",
        json!({"session_id": foreign.to_string()}),
    )
    .await;
    assert_eq!(
        mismatch["error"]["code"],
        codes::SCENE_MISMATCH,
        "{mismatch}"
    );
    assert_data_shape("SCENE_MISMATCH", &documented["SCENE_MISMATCH"], &mismatch);

    let parent = new_session(&h).await;
    assert_eq!(
        documented["SIDECHAIN_TERMINAL"][0]["final_state"].as_str(),
        Some("completed")
    );
    for state in [
        history::entry::SessionEndState::Completed,
        history::entry::SessionEndState::Failed,
    ] {
        let child = transcript_in_scene(&h, "code", Some(&parent)).await;
        h.store
            .append(child, history::entry::LogEntry::SessionEnd { state })
            .await
            .unwrap();
        let terminal = call(
            &h.sock,
            "session.resume",
            json!({"session_id": child.to_string()}),
        )
        .await;
        assert_eq!(
            terminal["error"]["code"],
            codes::SIDECHAIN_TERMINAL,
            "{terminal}"
        );
        assert_data_shape(
            "SIDECHAIN_TERMINAL",
            &documented["SIDECHAIN_TERMINAL"],
            &terminal,
        );
    }

    ok(&h.sock, "scene.activate", json!({"scene": "harnix"})).await;
    ok(
        &h.sock,
        "session.create",
        json!({"scene": "harnix", "project_root": h.project.display().to_string()}),
    )
    .await;
    let without_data = [
        (
            "INVALID_PARAMS",
            codes::INVALID_PARAMS,
            call(&h.sock, "session.get", json!({})).await,
        ),
        (
            "SESSION_NOT_FOUND",
            codes::SESSION_NOT_FOUND,
            call(&h.sock, "session.get", json!({"session_id": "Nope"})).await,
        ),
        (
            "SCENE_NOT_FOUND",
            codes::SCENE_NOT_FOUND,
            call(&h.sock, "scene.describe", json!({"scene": "no-such-scene"})).await,
        ),
        (
            "PROJECT_NOT_FOUND",
            codes::PROJECT_NOT_FOUND,
            call(
                &h.sock,
                "session.create",
                json!({"project_root": h.dir.path().join("missing").display().to_string()}),
            )
            .await,
        ),
        (
            "SCENE_HAS_ACTIVE_SESSIONS",
            codes::SCENE_HAS_ACTIVE_SESSIONS,
            call(&h.sock, "scene.deactivate", json!({"scene": "harnix"})).await,
        ),
    ];
    for (name, code, response) in without_data {
        assert_eq!(response["error"]["code"], code, "{name}: {response}");
        assert!(
            response["error"].get("data").is_none_or(Value::is_null),
            "{name} carries data, which §9.1 says it does not: {response}"
        );
    }
}

/// `METHOD_NOT_FOUND` carries `data.module` only in a build without the
/// module, which the default build is not.
#[tokio::test]
async fn a_method_of_a_module_this_build_lacks_names_the_module_in_error_data() {
    let doc = protocol_doc();
    let documented = documented_error_data(&doc);
    assert!(
        documented["METHOD_NOT_FOUND"]
            .iter()
            .any(|d| keys_of(d) == BTreeSet::from(["module".to_string()])),
        "§9.1's METHOD_NOT_FOUND example is not `{{ \"module\": … }}`"
    );
    let missing = [("mcp", "mcp.status"), ("agents", "session.startAgent")]
        .into_iter()
        .find(|(module, _)| !super::protocol_doc_examples::compiled_in(module));
    let Some((module, method)) = missing else {
        eprintln!(
            "every module is compiled in; the data shape is checked against the document only"
        );
        return;
    };
    let h = start_code(Arc::new(OneTurnClient)).await;
    let response = call(&h.sock, method, json!({})).await;
    assert_eq!(
        response["error"]["code"],
        codes::METHOD_NOT_FOUND,
        "{response}"
    );
    assert_eq!(response["error"]["data"]["module"], module, "{response}");
    assert_data_shape(
        "METHOD_NOT_FOUND",
        &documented["METHOD_NOT_FOUND"],
        &response,
    );
}

// ------------------------------------------------------------------ events

#[derive(Debug, Clone, Default)]
struct FieldSpec {
    required: BTreeSet<String>,
    optional: BTreeSet<String>,
}

/// The names in `a,b?,c:[…],d[]` at its own nesting level; `?` marks optional.
fn parse_fields(body: &str) -> FieldSpec {
    let mut spec = FieldSpec::default();
    let mut depth = 0i32;
    let mut token = String::new();
    let mut tokens = Vec::new();
    for c in body.chars() {
        match c {
            '[' | '{' | '(' => depth += 1,
            ']' | '}' | ')' => depth -= 1,
            _ => {}
        }
        if c == ',' && depth == 0 {
            tokens.push(std::mem::take(&mut token));
        } else {
            token.push(c);
        }
    }
    tokens.push(token);
    for token in tokens {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let name: String = token
            .chars()
            .take_while(|c| !matches!(c, ':' | '[' | '?'))
            .collect();
        if token[name.len()..].starts_with('?') {
            spec.optional.insert(name);
        } else {
            spec.required.insert(name);
        }
    }
    spec
}

/// Each top-level `{…}` in a payload cell is one alternative: `prompt` has two.
fn field_specs(cell: &str) -> Vec<FieldSpec> {
    let cell = cell.replace('`', "");
    let (mut depth, mut start) = (0usize, 0usize);
    let mut specs = Vec::new();
    for (i, c) in cell.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = i + 1;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    specs.push(parse_fields(&cell[start..i]));
                }
            }
            _ => {}
        }
    }
    specs
}

struct DocumentedKind {
    specs: Vec<FieldSpec>,
    turn_id_null: bool,
}

/// The `| kind | payload | … |` rows between `from` and `to` in the document.
fn documented_kinds(doc: &str, from: &str, to: &str) -> BTreeMap<String, DocumentedKind> {
    let start = doc
        .find(from)
        .unwrap_or_else(|| panic!("no `{from}` in the doc"));
    let rest = &doc[start..];
    let rest = &rest[..rest[from.len()..]
        .find(to)
        .map_or(rest.len(), |i| i + from.len())];
    let mut kinds = BTreeMap::new();
    for row in rest
        .lines()
        .filter(|l| l.starts_with("| `") && !l.starts_with("| `kind`"))
    {
        let cells: Vec<&str> = row.split('|').collect();
        let kind = cells[1].trim().trim_matches('`').to_string();
        kinds.insert(
            kind,
            DocumentedKind {
                specs: field_specs(cells[2]),
                turn_id_null: row.contains("`turn_id` 为 `null`"),
            },
        );
    }
    kinds
}

fn assert_fields(what: &str, specs: &[FieldSpec], event: &Value) {
    let mut keys = keys_of(event);
    keys.remove("kind");
    let fits = specs.iter().any(|s| {
        let allowed: BTreeSet<String> = s.required.union(&s.optional).cloned().collect();
        s.required.is_subset(&keys) && keys.is_subset(&allowed)
    });
    assert!(
        fits,
        "{what}: the frame has fields {keys:?}; the document's row allows {specs:#?}\n{event}"
    );
}

#[derive(Default)]
struct Observed {
    session: BTreeMap<String, Vec<Value>>,
    daemon: BTreeMap<String, Vec<Value>>,
}

impl Observed {
    /// Frames as a connection received them, `session.event` ones kept.
    fn pushed(&mut self, frames: &[Value]) {
        for frame in frames {
            match frame["method"].as_str() {
                Some("session.event") => self.session_params(&frame["params"]),
                Some("daemon.event") => self.daemon_params(&frame["params"]),
                _ => {}
            }
        }
    }

    fn session_params(&mut self, params: &Value) {
        let kind = params["event"]["kind"].as_str().unwrap_or("?").to_string();
        self.session.entry(kind).or_default().push(params.clone());
    }

    fn daemon_params(&mut self, params: &Value) {
        let kind = params["kind"].as_str().unwrap_or("?").to_string();
        self.daemon.entry(kind).or_default().push(params.clone());
    }

    fn watched(&mut self, watch: &Watch) {
        for params in watch.snapshot() {
            if params.get("event").is_some() {
                self.session_params(&params);
            } else {
                self.daemon_params(&params);
            }
        }
    }
}

const SESSION_EVENTS_NOT_PROVOKED: &[(&str, &str)] = &[
    (
        "team_progress",
        "needs a team definition and a model that drives the Team tool through its stages",
    ),
    (
        "subscription_dropped",
        "needs a connection that stops reading while the session overflows its queue; \
         whether it overflows is a matter of timing",
    ),
];

const DAEMON_EVENTS_NOT_PROVOKED: &[(&str, &str)] = &[
    (
        "mcp_connected",
        "needs a real MCP server process; the only fixture builds a crate inside the test \
         (`daemon_e2e.rs`, ignored by default)",
    ),
    (
        "mcp_auth_required",
        "needs an OAuth authorization server (`mcp_oauth.rs` runs one in-process)",
    ),
    (
        "mcp_auth_completed",
        "needs an OAuth authorization server (`mcp_oauth.rs` runs one in-process)",
    ),
    (
        "mcp_auth_failed",
        "needs an OAuth authorization server (`mcp_oauth.rs` runs one in-process)",
    ),
    (
        "import_detected",
        "sent by the daemon binary's own startup (`main.rs`), before any client can have subscribed",
    ),
    (
        "events_lagged",
        "needs a subscriber that stops reading while the daemon floods its channel; timing",
    ),
];

/// The code scene, with a context budget one token wide so the first turn
/// compacts, and with the tools the code scene leaves out.
struct TunedScene {
    inner: scene::code::CodeScene,
    compacts_at_once: bool,
    extra_tools: &'static [&'static str],
}

impl TunedScene {
    fn compacting() -> Arc<Self> {
        Arc::new(Self {
            inner: Default::default(),
            compacts_at_once: true,
            extra_tools: &[],
        })
    }

    fn notifying() -> Arc<Self> {
        Arc::new(Self {
            inner: Default::default(),
            compacts_at_once: false,
            extra_tools: &["PushNotification"],
        })
    }
}

impl base::interface::scene::Scene for TunedScene {
    fn id(&self) -> &str {
        "tuned"
    }

    fn name(&self) -> &str {
        "Tuned"
    }

    fn description(&self) -> &str {
        "the code scene, tuned for a test"
    }

    fn build_system_prompt(
        &self,
        ctx: &base::interface::scene::ScenePromptContext,
    ) -> Vec<base::prompt::PromptBlock> {
        self.inner.build_system_prompt(ctx)
    }

    fn tools(&self) -> Vec<String> {
        let mut tools = self.inner.tools();
        tools.extend(self.extra_tools.iter().map(|t| t.to_string()));
        tools
    }

    fn token_budget(&self) -> base::interface::scene::TokenBudget {
        if self.compacts_at_once {
            base::interface::scene::TokenBudget {
                compact_threshold: 1,
                compact_keep_recent: 1,
            }
        } else {
            self.inner.token_budget()
        }
    }
}

fn thinking_then_a_bash_call() -> Vec<StreamEvent> {
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
                thinking: "a file is wanted".into(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
    ];
    round.extend(tool_blocks(
        1,
        "Bash",
        "tool-1",
        &json!({"command": "touch made.txt"}),
    ));
    round.push(StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: Some(base::message::StopReason::ToolUse),
            stop_sequence: None,
        },
        usage: Some(Usage::default()),
    });
    round
}

async fn steered_and_permitted(h: &Harness, obs: &mut Observed, steer: &str, then: &str) {
    let mut held = hold_at_prompt(h, json!({})).await;
    let steered = call(
        &h.sock,
        "session.run_turn",
        json!({"session_id": held.session, "message": steer,
               "turn_id": "m1", "on_busy": "steer"}),
    )
    .await;
    assert_eq!(steered["result"]["delivery"], "steer", "{steered}");
    if then == "interrupt" {
        ok(
            &h.sock,
            "session.interrupt",
            json!({"session_id": held.session}),
        )
        .await;
    } else {
        answer_prompt(&h.sock, &held.session, &held.prompt).await;
    }
    let response = drive(&mut held.conn, held.run_id, &h.sock).await;
    assert!(response.get("result").is_some(), "{response}");
    obs.pushed(&held.conn.pushed);
}

async fn the_turn_and_its_tool_call(obs: &mut Observed) {
    let h = start_code(scripted(vec![
        thinking_then_a_bash_call(),
        text_round("done"),
    ]))
    .await;
    steered_and_permitted(&h, obs, "also add a README", "permit").await;
}

async fn a_steered_message_handed_back(obs: &mut Observed) {
    let h = start_code(scripted(vec![
        thinking_then_a_bash_call(),
        text_round("done"),
    ]))
    .await;
    steered_and_permitted(&h, obs, "wait, stop", "interrupt").await;
}

async fn a_steered_message_a_hook_refuses(obs: &mut Observed) {
    let hook = r#"grep -q refuse-me && echo '{"decision":"block","message":"not now"}'; true"#;
    let h = start(
        scripted(vec![thinking_then_a_bash_call(), text_round("done")]),
        Arc::new(scene::code::CodeScene::default()),
        8,
        |settings| {
            settings.hooks_config = Some(json!({
                "UserPromptSubmit": [{"type": "command", "command": hook}]
            }));
        },
    )
    .await;
    steered_and_permitted(&h, obs, "refuse-me", "permit").await;
}

async fn a_question_to_the_user(obs: &mut Observed) {
    let h = start_code(scripted(vec![
        tool_round(
            "AskUserQuestion",
            "tool-1",
            json!({"question": "which branch?", "header": "Branch",
                   "options": [{"key": "a", "label": "main"}, {"key": "b", "label": "dev"}]}),
        ),
        text_round("thanks"),
    ]))
    .await;
    let mut conn = Conn::open(&h.sock).await;
    let id = conn
        .send(
            "session.run_turn",
            json!({"message": "ask", "options": {"prompt_types": ["permission", "question"]}}),
        )
        .await;
    let response = drive(&mut conn, id, &h.sock).await;
    assert!(response.get("result").is_some(), "{response}");
    obs.pushed(&conn.pushed);
}

async fn a_turn_that_fails_for_a_watcher(obs: &mut Observed) {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn_with_errors(vec![Err(model::error::AnthropicError::Auth(
        "bad key".into(),
    ))]);
    let h = start_code(model).await;
    let session = new_session(&h).await;
    let watch = Watch::session(&h.sock, &session).await;
    let failed = call(
        &h.sock,
        "session.run_turn",
        json!({"session_id": session, "message": "hi"}),
    )
    .await;
    assert_eq!(failed["error"]["code"], codes::ENGINE_ERROR, "{failed}");
    watch.wait_kind("error").await;
    obs.watched(&watch);
}

async fn a_compacted_turn(obs: &mut Observed) {
    let h = start(Arc::new(OneTurnClient), TunedScene::compacting(), 8, |_| {}).await;
    let (frames, response) = run_turn(&h.sock, json!({"message": "hello"})).await;
    assert!(response.get("result").is_some(), "{response}");
    obs.pushed(&frames);
}

async fn a_sub_agent_inside_a_turn(obs: &mut Observed) {
    let h = start(
        scripted(vec![
            tool_round(
                "Agent",
                "toolu_agent",
                json!({"description": "say hello", "prompt": "Reply with: hello",
                       "subagent_type": "general-purpose"}),
            ),
            text_round("hello"),
            text_round("done"),
        ]),
        Arc::new(scene::code::CodeScene::default()),
        8,
        bypass,
    )
    .await;
    let (frames, response) = run_turn(&h.sock, json!({"message": "delegate"})).await;
    assert!(response.get("result").is_some(), "{response}");
    obs.pushed(&frames);
}

async fn a_sub_agent_the_host_starts(obs: &mut Observed) {
    let h = start(
        Arc::new(OneTurnClient),
        Arc::new(scene::code::CodeScene::default()),
        8,
        bypass,
    )
    .await;
    let session = new_session(&h).await;
    let watch = Watch::session(&h.sock, &session).await;
    ok(
        &h.sock,
        "session.startAgent",
        json!({"session_id": session, "prompt": "say hi"}),
    )
    .await;
    watch.wait_kind("task_finished").await;
    obs.watched(&watch);
}

async fn a_shell_command_that_runs_on(obs: &mut Observed) {
    let h = start(
        scripted(vec![
            tool_round(
                "Bash",
                "toolu_cmd",
                json!({"command": "for i in 1 2 3 4; do echo line-$i; sleep 0.6; done"}),
            ),
            text_round("ran"),
        ]),
        Arc::new(scene::code::CodeScene::default()),
        8,
        bypass,
    )
    .await;
    let session = new_session(&h).await;
    let watch = Watch::session(&h.sock, &session).await;
    ok(
        &h.sock,
        "session.run_turn",
        json!({"session_id": session, "message": "run it"}),
    )
    .await;
    watch.wait_kind("task_progress").await;
    watch.wait_kind("task_finished").await;
    obs.watched(&watch);
}

async fn a_goal_and_an_extension_entry(obs: &mut Observed) {
    let h = start_code(Arc::new(OneTurnClient)).await;
    let session = new_session(&h).await;
    let watch = Watch::session(&h.sock, &session).await;
    ok(
        &h.sock,
        "session.setGoal",
        json!({"session_id": session, "objective": "finish", "max_rounds": 1}),
    )
    .await;
    ok(
        &h.sock,
        "session.appendEntry",
        json!({"session_id": session, "ns": "com.example.cards",
               "event": "job_done", "payload": {}}),
    )
    .await;
    watch.wait_kind("goal_changed").await;
    watch.wait_kind("extension_entry").await;
    obs.watched(&watch);
}

fn write_skill(project: &Path, name: &str) {
    let dir = project.join(".agents/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} for the test\n---\nbody"),
    )
    .unwrap();
}

async fn a_skill_that_appears_between_turns(obs: &mut Observed) {
    let h = start_code(Arc::new(OneTurnClient)).await;
    let other = h.dir.path().join("other-project");
    write_skill(&other, "already-here");
    let session = ok(
        &h.sock,
        "session.create",
        json!({"project_root": other.display().to_string()}),
    )
    .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let watch = Watch::session(&h.sock, &session).await;
    ok(
        &h.sock,
        "session.run_turn",
        json!({"session_id": session, "message": "first"}),
    )
    .await;
    write_skill(&other, "freeze-check");
    for n in 0..40 {
        ok(
            &h.sock,
            "session.run_turn",
            json!({"session_id": session, "message": format!("again {n}")}),
        )
        .await;
        if watch
            .snapshot()
            .iter()
            .any(|f| event_kind(f) == "skills_changed")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    watch.wait_kind("skills_changed").await;
    obs.watched(&watch);
}

async fn a_notification_from_the_model(obs: &mut Observed) {
    let h = start(
        scripted(vec![
            tool_round(
                "PushNotification",
                "toolu_note",
                json!({"message": "deploy ready", "status": "proactive"}),
            ),
            text_round("sent"),
        ]),
        TunedScene::notifying(),
        8,
        bypass,
    )
    .await;
    let (frames, response) = run_turn(&h.sock, json!({"message": "tell me"})).await;
    assert!(response.get("result").is_some(), "{response}");
    obs.pushed(&frames);
}

async fn observe_session_events() -> Observed {
    let mut obs = Observed::default();
    the_turn_and_its_tool_call(&mut obs).await;
    a_steered_message_handed_back(&mut obs).await;
    a_steered_message_a_hook_refuses(&mut obs).await;
    a_question_to_the_user(&mut obs).await;
    a_turn_that_fails_for_a_watcher(&mut obs).await;
    a_compacted_turn(&mut obs).await;
    a_sub_agent_inside_a_turn(&mut obs).await;
    a_sub_agent_the_host_starts(&mut obs).await;
    a_shell_command_that_runs_on(&mut obs).await;
    a_goal_and_an_extension_entry(&mut obs).await;
    a_skill_that_appears_between_turns(&mut obs).await;
    a_notification_from_the_model(&mut obs).await;
    obs
}

/// The documented JSON of §7.1 / §7.2 says the same fields as their table rows.
fn documented_example(doc: &str, heading: &str) -> Value {
    let start = doc
        .find(heading)
        .unwrap_or_else(|| panic!("no `{heading}`"));
    let block = doc[start..]
        .split("```jsonc\n")
        .nth(1)
        .and_then(|rest| rest.split_once("\n```"))
        .unwrap()
        .0;
    let text: String = block
        .lines()
        .map(strip_line_comment)
        .collect::<Vec<_>>()
        .join("\n")
        .replace("{…}", "{}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{heading}'s example: {e}\n{text}"))
}

#[tokio::test]
async fn documented_session_event_payloads_are_what_the_daemon_sends() {
    let doc = protocol_doc();
    let documented = documented_kinds(&doc, "## 7. 流式帧", "### 7.1");
    assert!(
        documented.len() >= 25,
        "the §7 table is no longer being read"
    );
    let observed = observe_session_events().await;

    for kind in observed.session.keys() {
        assert!(
            documented.contains_key(kind),
            "the daemon sent a `{kind}` frame that §7 does not list (§7 calls itself the whitelist)"
        );
    }
    for (kind, row) in &documented {
        let skipped = SESSION_EVENTS_NOT_PROVOKED.iter().find(|(k, _)| k == kind);
        match (observed.session.get(kind), skipped) {
            (Some(_), Some(_)) => panic!("`{kind}` is listed as not provoked but was"),
            (None, None) => panic!(
                "no scenario provoked `{}`: provoke it or list it with why not",
                kind
            ),
            (None, Some(_)) => continue,
            (Some(frames), None) => {
                for params in frames {
                    assert_eq!(
                        keys_of(params),
                        BTreeSet::from(["session_id".into(), "turn_id".into(), "event".into()]),
                        "{kind}: the envelope"
                    );
                    if row.turn_id_null {
                        assert!(
                            params["turn_id"].is_null(),
                            "{kind}: turn_id should be null: {params}"
                        );
                    }
                    assert_fields(
                        &format!("session.event {kind}"),
                        &row.specs,
                        &params["event"],
                    );
                }
            }
        }
    }
    for (kind, _) in SESSION_EVENTS_NOT_PROVOKED {
        assert!(
            documented.contains_key(*kind),
            "{kind} is not a documented kind"
        );
    }

    for (heading, kind) in [("### 7.1", "compact"), ("### 7.2", "turn_complete")] {
        let example = documented_example(&doc, heading);
        let spec = &documented[kind].specs[0];
        let table: BTreeSet<String> = spec.required.union(&spec.optional).cloned().collect();
        let mut shown = keys_of(&example);
        shown.remove("kind");
        assert_eq!(
            shown, table,
            "{heading}'s example and the §7 row for `{kind}` disagree"
        );
    }
    let complete = documented_example(&doc, "### 7.2");
    let real = &observed.session["turn_complete"]
        .iter()
        .find(|f| f["event"].get("context").is_some())
        .expect("a turn that called the model")["event"];
    assert_eq!(
        keys_of(&complete["context"]),
        keys_of(&real["context"]),
        "turn_complete.context"
    );
}

async fn daemon_events() -> Observed {
    let mut obs = Observed::default();
    let h = start(
        Arc::new(OneTurnClient),
        Arc::new(scene::code::CodeScene::default()),
        1,
        |_| {},
    )
    .await;
    let watch = Watch::daemon(&h.sock).await;

    let first = new_session(&h).await;
    ok(
        &h.sock,
        "session.run_turn",
        json!({"session_id": first, "message": "hi"}),
    )
    .await;
    let second = new_session(&h).await;
    watch.wait_kind("session_evicted").await;
    ok(&h.sock, "session.resume", json!({"session_id": first})).await;
    ok(&h.sock, "session.close", json!({"session_id": first})).await;
    ok(&h.sock, "session.delete", json!({"session_id": second})).await;
    ok(&h.sock, "project.trust", json!({"trusted": true})).await;
    ok(
        &h.sock,
        "mcp.addServer",
        json!({"name": "ghost",
               "config": {"type": "stdio", "command": "definitely-not-a-real-binary-xyz"}}),
    )
    .await;
    for kind in [
        "session_created",
        "session_resumed",
        "session_closed",
        "session_deleted",
        "turn_started",
        "turn_completed",
        "project_trust_changed",
        "mcp_connect_failed",
    ] {
        watch.wait_kind(kind).await;
    }
    obs.watched(&watch);
    obs
}

#[tokio::test]
async fn documented_daemon_event_payloads_are_what_the_daemon_sends() {
    let doc = protocol_doc();
    let documented = documented_kinds(&doc, "## 8. 异步通知", "## 9.");
    assert!(
        documented.len() >= 14,
        "the §8 table is no longer being read"
    );
    let observed = daemon_events().await;

    for kind in observed.daemon.keys() {
        assert!(
            documented.contains_key(kind),
            "the daemon sent a `{kind}` daemon.event that §8 does not list"
        );
    }
    for (kind, row) in &documented {
        let skipped = DAEMON_EVENTS_NOT_PROVOKED.iter().find(|(k, _)| k == kind);
        match (observed.daemon.get(kind), skipped) {
            (Some(_), Some(_)) => panic!("`{kind}` is listed as not provoked but was"),
            (None, None) => panic!(
                "no scenario provoked `{}`: provoke it or list it with why not",
                kind
            ),
            (None, Some(_)) => {}
            (Some(frames), None) => {
                for params in frames {
                    assert_fields(&format!("daemon.event {kind}"), &row.specs, params);
                }
            }
        }
    }
    for (kind, _) in DAEMON_EVENTS_NOT_PROVOKED {
        assert!(
            documented.contains_key(*kind),
            "{kind} is not a documented kind"
        );
    }
}

/// §4.5's `acceptEdits` row says a read outside the project is asked about, as
/// in `default`, and so is a read-only `git` in a repository the session did
/// not start in.
#[tokio::test]
async fn documented_accept_edits_asks_about_reads_outside_the_project_and_a_nested_repository() {
    let doc = protocol_doc();
    let row = doc
        .lines()
        .find(|l| l.starts_with("| `acceptEdits` |"))
        .expect("§4.5 has an acceptEdits row");
    assert!(
        row.contains("读项目外") && row.contains("`git`"),
        "the row no longer says what these two calls show: {row}"
    );

    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "outside\n").unwrap();
    let h = start(
        scripted(vec![
            tool_round(
                "Read",
                "tool-1",
                json!({"file_path": outside.path().join("secret.txt")}),
            ),
            tool_round("Bash", "tool-2", json!({"command": "cd nested && git log"})),
            text_round("done"),
        ]),
        Arc::new(scene::code::CodeScene::default()),
        8,
        |settings| {
            settings.permission_mode = base::interface::settings::PermissionMode::AcceptEdits
        },
    )
    .await;
    std::fs::create_dir_all(h.dir.path().join("nested/.git")).unwrap();
    std::fs::write(
        h.dir.path().join("nested/.git/HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();

    let session = ok(
        &h.sock,
        "session.create",
        json!({"project_root": h.project.display().to_string()}),
    )
    .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut held = hold_at_prompt(&h, json!({"session_id": session, "message": "look"})).await;
    assert_eq!(held.prompt["tool_name"], "Read", "{}", held.prompt);
    answer_prompt(&h.sock, &held.session, &held.prompt).await;
    let response = drive(&mut held.conn, held.run_id, &h.sock).await;
    assert!(response.get("result").is_some(), "{response}");
    let asked: Vec<&Value> = held
        .conn
        .pushed
        .iter()
        .filter(|f| f["params"]["event"]["kind"] == "prompt")
        .map(|f| &f["params"]["event"]["tool_name"])
        .collect();
    assert_eq!(
        asked,
        [&json!("Read"), &json!("Bash")],
        "the read outside and the nested repository's git are each asked about once"
    );
}
