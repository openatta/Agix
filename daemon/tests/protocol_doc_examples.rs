//! The response examples in `docs/daemon_rpc_protocol.md` are real responses.
//!
//! `protocol_doc_matches_dispatch.rs` keeps the *set* of documented methods
//! honest; this keeps the *shapes* honest. A client is written against these
//! examples, and none of them is compiled.
//!
//! So each example marked `// ← result` is read back out of the file and
//! compared, field by field, against what a real daemon answers over a real
//! socket. The example may say *less* than the response — a new field does
//! not break the doc — but it may not say anything different.
//!
//! Every documented method is either compared here or listed in
//! [`NOT_COMPARED`] / [`NO_EXAMPLE`] with the reason, and
//! `every_documented_method_is_compared_or_says_why_not` holds the lists to
//! the document and to this file.
//!
//! `daemon.doctor` is checked by `crates/assembly/src/doctor.rs`'s
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

/// The JSON values each `// ← result` marker in this section's fenced blocks
/// introduces, in document order, with `//` annotations removed.
///
/// One marker may introduce several alternatives (`{"delivered": true}` then
/// `{"delivered": false, …}`); a later `// … → …` line ends them, because what
/// follows it is a request. A marker followed by prose (`// ← result:同 …`)
/// and no JSON introduces nothing.
fn documented_results(doc: &str, method: &str) -> Vec<serde_json::Value> {
    let body = section(doc, method);
    let mut out = Vec::new();
    let mut flush = |json: &mut String| {
        let values = serde_json::Deserializer::from_str(json)
            .into_iter::<serde_json::Value>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|e| {
                panic!("`{method}`'s documented result does not parse as JSON ({e}):\n{json}")
            });
        out.extend(values);
        json.clear();
    };
    for block in body
        .split("```jsonc\n")
        .skip(1)
        .filter_map(|rest| rest.split_once("\n```").map(|(block, _)| block))
    {
        let mut json = String::new();
        let mut collecting = false;
        for line in block.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("// ← result") {
                flush(&mut json);
                collecting = true;
            } else if trimmed.starts_with("//") && trimmed.contains('→') {
                flush(&mut json);
                collecting = false;
            } else if collecting {
                json.push_str(strip_line_comment(line));
                json.push('\n');
            }
        }
        flush(&mut json);
    }
    out
}

/// The first documented result of `method`.
fn documented_result(doc: &str, method: &str) -> serde_json::Value {
    documented_results(doc, method)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("`{method}`'s section no longer carries a `// ← result` block"))
}

/// The documented result with every number turned into a placeholder, for
/// examples whose numbers stand for measurements (token counts, timestamps)
/// rather than claims.
fn numbers_as_measurements(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Number(_) => serde_json::Value::String("…".into()),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(numbers_as_measurements).collect())
        }
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .into_iter()
                .map(|(k, v)| (k, numbers_as_measurements(v)))
                .collect(),
        ),
        other => other,
    }
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
    start_daemon_with(Arc::new(OneTurnClient), |_| {}).await
}

async fn start_daemon_with(
    model_client: Arc<dyn AnthropicClient>,
    configure: impl FnOnce(&mut base::settings::Settings),
) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("doc.sock");
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
    // Post-turn memory extraction would fire a second model call and slow the
    // one turn this test runs; nothing here reads memory.
    config.settings.memory_enabled = false;
    configure(&mut config.settings);

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
        Arc::new(scene::code::CodeScene::default()),
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            model_client: Some(model_client),
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

fn compiled_in(module: &str) -> bool {
    match module {
        "mcp" => cfg!(feature = "mcp"),
        "agents" => cfg!(feature = "agents"),
        "skills" => cfg!(feature = "skills"),
        "scripts" => cfg!(feature = "scripts"),
        "plugin-packages" => cfg!(feature = "plugin-packages"),
        _ => true,
    }
}

/// Everything a client can reach without a session, in the order §11 walks
/// them. `daemon.status`'s documented `"sessions": 0` is why this runs
/// before anything is created.
#[tokio::test]
async fn documented_daemon_scene_and_config_results_are_real() {
    let doc = protocol_doc();
    let daemon = start_daemon().await;

    // The example is the default build's; a build without one of those
    // modules does not list it, and that is the documented behaviour.
    let mut ping = documented_result(&doc, "daemon.ping");
    if let Some(modules) = ping.get_mut("modules").and_then(|m| m.as_array_mut()) {
        modules.retain(|m| compiled_in(m.as_str().unwrap_or_default()));
    }
    let actual = daemon.call("daemon.ping", serde_json::json!({})).await;
    assert_documented("daemon.ping", &ping, &actual);
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
        serde_json::json!({"scene": "code", "project_root": null}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "scene.activate",
        serde_json::json!({"scene": "harnix"}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "scene.deactivate",
        serde_json::json!({"scene": "harnix"}),
    )
    .await;

    verify(
        &doc,
        &daemon,
        "config.get",
        serde_json::json!({"scene": "code", "tier": "effective"}),
    )
    .await;
    verify(&doc, &daemon, "config.getProvider", serde_json::json!({})).await;
    verify(&doc, &daemon, "config.reload", serde_json::json!({})).await;

    verify(&doc, &daemon, "mcp.status", serde_json::json!({})).await;
    verify(&doc, &daemon, "commands.list", serde_json::json!({})).await;
    verify(&doc, &daemon, "import.list", serde_json::json!({})).await;

    verify(&doc, &daemon, "project.status", serde_json::json!({})).await;
    verify(
        &doc,
        &daemon,
        "project.trust",
        serde_json::json!({"trusted": true}),
    )
    .await;

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
        serde_json::json!({"scene": "code", "project_root": project}),
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
    verify(
        &doc,
        &daemon,
        "session.export",
        serde_json::json!({"session_id": sid,
                           "path": daemon._dir.path().join("doc.agix.zip"),
                           "format": "zip", "include": ["sidechains", "attachments"]}),
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
            serde_json::json!({"scene": "code", "project_root": project}),
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

/// Documented methods with a `// ← result` example this file does not compare,
/// and why.
const NOT_COMPARED: &[(&str, &str)] = &[
    (
        "session.goal",
        "the example is a goal two automatic rounds in; rounds start on their own, so how \
         many have when the call lands is timing",
    ),
    ("session.setGoal", "shares session.goal's example"),
    ("session.updateGoal", "shares session.goal's example"),
    (
        "session.dropMessages",
        "the positions are for an 8-message history; which positions hold user messages \
         depends on what the engine injected",
    ),
    (
        "session.editMessage",
        "the same positions as session.dropMessages",
    ),
    (
        "session.tasks",
        "the example is the union of every kind's and every status's fields, not one answer",
    ),
    (
        "mcp.auth.start",
        "needs a remote MCP server behind an authorization server",
    ),
    ("mcp.auth.complete", "follows mcp.auth.start"),
    (
        "mcp.auth.setIdentity",
        "needs an enterprise IdP and a server granted through it",
    ),
];

/// Documented methods without a `// ← result` example, and why.
const NO_EXAMPLE: &[(&str, &str)] = &[
    (
        "daemon.doctor",
        "compared by crates/assembly/src/doctor.rs `documented_shape_tests`",
    ),
    (
        "import.run",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
    (
        "mcp.addServer",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
    (
        "mcp.updateServer",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
    (
        "mcp.auth.cancel",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
    (
        "mcp.auth.logout",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
    (
        "plugin.install",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
    (
        "plugin.uninstall",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
    (
        "plugin.enable",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
    (
        "plugin.disable",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
    (
        "plugin.reload",
        "the result is written inline in the prose, not as a `// ← result` block",
    ),
];

/// The methods this file compares, read from its own calls.
fn compared_here() -> std::collections::BTreeSet<String> {
    let src = include_str!("protocol_doc_examples.rs");
    let mut out = std::collections::BTreeSet::new();
    for opener in [
        "verify(",
        "verify_as(",
        "documented_result(",
        "documented_results(",
        "assert_documented(",
    ] {
        for hit in src.split(opener).skip(1) {
            let args: String = hit.chars().take(200).collect();
            if let Some(name) = args.split('"').nth(1) {
                if name.contains('.') && !name.contains(' ') {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

#[test]
fn every_documented_method_is_compared_or_says_why_not() {
    let doc = protocol_doc();
    let compared = compared_here();
    let methods: Vec<String> = doc
        .lines()
        .filter(|line| line.starts_with("#### "))
        .flat_map(|line| {
            line.split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|name| name.contains('.'))
        .collect();

    for method in &methods {
        let has_example = section(&doc, method).contains("// ← result");
        let not_compared = NOT_COMPARED.iter().any(|(m, _)| m == method);
        let no_example = NO_EXAMPLE.iter().any(|(m, _)| m == method);
        if no_example {
            assert!(
                !has_example,
                "{method} is listed in NO_EXAMPLE but its section has a `// ← result` block"
            );
            continue;
        }
        assert!(
            has_example,
            "{method} has no `// ← result` example; write one, or list it in NO_EXAMPLE \
             with the reason"
        );
        assert!(
            compared.contains(method) != not_compared,
            "{method}: compared here = {}, in NOT_COMPARED = {not_compared}. A method is \
             compared against a real daemon or listed with the reason it is not — exactly one",
            compared.contains(method)
        );
    }
    for (listed, _) in NOT_COMPARED.iter().chain(NO_EXAMPLE) {
        assert!(
            methods.iter().any(|m| m == listed),
            "{listed} is listed here but the document has no such method"
        );
    }
}

/// Every value of `v` reduced to a placeholder: the fields it names, not what
/// they hold.
fn shape_of(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(fields) => {
            serde_json::Value::Object(fields.into_iter().map(|(k, v)| (k, shape_of(v))).collect())
        }
        serde_json::Value::Array(_) => serde_json::Value::Array(Vec::new()),
        _ => serde_json::Value::String("…".into()),
    }
}

/// Compare `method`'s live result against another method's documented one,
/// for a section whose example says "same as …".
async fn verify_as(
    doc: &str,
    daemon: &Daemon,
    method: &str,
    documented_by: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let actual = daemon.call(method, params).await;
    assert_documented(method, &documented_result(doc, documented_by), &actual);
    actual
}

/// Scenes, the session's own controls, schedules and the provider patch —
/// each against a daemon whose state the method needs.
#[tokio::test]
async fn documented_scene_session_control_and_schedule_results_are_real() {
    let doc = protocol_doc();
    let daemon = start_daemon().await;
    let project = daemon.project.display().to_string();

    let register = documented_results(&doc, "scene.register");
    let actual = daemon
        .call(
            "scene.register",
            serde_json::json!({
                "scene": {"id": "triage", "name": "Triage", "tools": ["Read", "Grep"]},
                "prompt": "把进来的问题分类。"
            }),
        )
        .await;
    assert_documented("scene.register", &register[0], &actual);
    let actual = daemon
        .call("scene.unregister", serde_json::json!({"scene": "triage"}))
        .await;
    assert_documented(
        "scene.unregister",
        &documented_results(&doc, "scene.unregister")[1],
        &actual,
    );

    let scenes = daemon._dir.path().join("scenes");
    std::fs::create_dir_all(scenes.join("triage")).unwrap();
    std::fs::write(
        scenes.join("triage/scene.json"),
        r#"{"id": "triage", "name": "Triage", "tools": ["Read", "Grep"]}"#,
    )
    .unwrap();
    std::fs::write(scenes.join("triage/prompt.md"), "把进来的问题分类。").unwrap();
    std::fs::create_dir_all(scenes.join("broken")).unwrap();
    std::fs::write(scenes.join("broken/scene.json"), "{ not json").unwrap();
    std::fs::write(scenes.join("broken/prompt.md"), "x").unwrap();
    verify(&doc, &daemon, "scene.reload", serde_json::json!({})).await;

    let created = daemon
        .call(
            "session.create",
            serde_json::json!({"scene": "code", "project_root": project}),
        )
        .await;
    let sid = created["session_id"].as_str().unwrap().to_string();

    verify(
        &doc,
        &daemon,
        "session.setPrompt",
        serde_json::json!({"session_id": sid, "append": "回答一律用中文。", "override": null}),
    )
    .await;
    daemon
        .call(
            "session.run_turn",
            serde_json::json!({"session_id": sid, "message": "hi"}),
        )
        .await;
    let inspected = daemon
        .call(
            "session.inspectContext",
            serde_json::json!({"session_id": sid}),
        )
        .await;
    assert_documented(
        "session.inspectContext",
        &numbers_as_measurements(documented_result(&doc, "session.inspectContext")),
        &inspected,
    );
    verify_as(
        &doc,
        &daemon,
        "session.compact",
        "session.run_turn",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    verify_as(
        &doc,
        &daemon,
        "session.clear",
        "session.run_turn",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    // After the turns: their results carry the session's name, documented null.
    verify(
        &doc,
        &daemon,
        "session.rename",
        serde_json::json!({"session_id": sid, "name": "重构 daemon 装配"}),
    )
    .await;

    // `created_ms` is a timestamp; the example's `0` stands for one.
    let job = daemon
        .call(
            "schedule.create",
            serde_json::json!({"session_id": sid, "cron": "0 9 * * 1-5", "prompt": "日报",
                               "recurring": true, "durable": true}),
        )
        .await;
    assert_documented(
        "schedule.create",
        &numbers_as_measurements(documented_result(&doc, "schedule.create")),
        &job,
    );
    verify(
        &doc,
        &daemon,
        "schedule.list",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    verify(
        &doc,
        &daemon,
        "schedule.delete",
        serde_json::json!({"id": job["job"]["id"]}),
    )
    .await;

    // Last: no turn runs after it. The section documents the result as
    // config.reload's plus `written_to`, and config.reload's example is one
    // particular state of the routing, so only its shape is claimed here.
    let written = daemon
        .call(
            "config.setProvider",
            serde_json::json!({"provider_id": "deepseek",
                               "config": {"api_type": "openai_compatible",
                                          "base_url": "http://127.0.0.1:9/v1",
                                          "api_key": "sk-doc-example",
                                          "default_model": "deepseek-chat"},
                               "default_provider": "deepseek",
                               "project_root": project}),
        )
        .await;
    let mut documented = shape_of(documented_result(&doc, "config.reload"));
    documented["written_to"] = serde_json::json!("…");
    assert_documented("config.setProvider", &documented, &written);
}

/// Its own daemon: the example switches to a provider named `anthropic`, which
/// has to be configured, and no turn may run after the switch — the endpoint
/// is not there.
#[tokio::test]
async fn the_documented_set_model_result_is_real() {
    let doc = protocol_doc();
    let daemon = start_daemon_with(Arc::new(OneTurnClient), |settings| {
        settings.providers.insert(
            "anthropic".into(),
            base::provider::ProviderConfig {
                api_type: Some("anthropic".into()),
                base_url: Some("http://127.0.0.1:9".into()),
                api_key: Some("sk-doc-example".into()),
                default_model: Some("claude-sonnet-4-6".into()),
                ..Default::default()
            },
        );
        settings.default_provider = Some("anthropic".into());
    })
    .await;
    let created = daemon
        .call(
            "session.create",
            serde_json::json!({"scene": "code",
                               "project_root": daemon.project.display().to_string()}),
        )
        .await;
    verify(
        &doc,
        &daemon,
        "session.setModel",
        serde_json::json!({"session_id": created["session_id"],
                           "model": "claude-opus-4-6", "provider": "anthropic"}),
    )
    .await;
}

/// A model that never answers a request whose conversation mentions
/// [`HOLD`], so a sub-agent started with it stays running for as long as the
/// test needs.
#[cfg(feature = "agents")]
struct HoldingClient;

#[cfg(feature = "agents")]
const HOLD: &str = "HOLD-THIS-TASK";

#[cfg(feature = "agents")]
impl AnthropicClient for HoldingClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        let holds = serde_json::to_string(&req.messages).is_ok_and(|m| m.contains(HOLD));
        if holds {
            return Box::pin(futures::stream::pending());
        }
        OneTurnClient.stream_messages(req)
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

#[cfg(feature = "agents")]
#[tokio::test]
async fn documented_task_results_are_real() {
    let doc = protocol_doc();
    let daemon = start_daemon_with(Arc::new(HoldingClient), |_| {}).await;
    let project = daemon.project.display().to_string();
    let created = daemon
        .call(
            "session.create",
            serde_json::json!({"scene": "code", "project_root": project}),
        )
        .await;
    let sid = created["session_id"].as_str().unwrap().to_string();

    let started = verify(
        &doc,
        &daemon,
        "session.startAgent",
        serde_json::json!({"session_id": sid, "prompt": HOLD, "description": "held"}),
    )
    .await;
    let task = started["task_id"].as_str().unwrap().to_string();

    let mut output = serde_json::Value::Null;
    for _ in 0..400 {
        output = daemon
            .call(
                "session.taskOutput",
                serde_json::json!({"session_id": sid, "task_id": task}),
            )
            .await;
        if output["status"] == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_documented(
        "session.taskOutput",
        &documented_result(&doc, "session.taskOutput"),
        &output,
    );

    // The task is `running` before its engine is built; it takes messages once
    // the engine is there.
    let delivered = documented_results(&doc, "session.messageTask");
    let mut actual = serde_json::Value::Null;
    for _ in 0..400 {
        actual = daemon
            .call(
                "session.messageTask",
                serde_json::json!({"session_id": sid, "task_id": task, "message": "还在吗"}),
            )
            .await;
        if actual["delivered"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_documented("session.messageTask", &delivered[0], &actual);
    let actual = daemon
        .call(
            "session.messageTask",
            serde_json::json!({"session_id": sid, "task_id": "no-such-task", "message": "x"}),
        )
        .await;
    assert_documented("session.messageTask", &delivered[1], &actual);

    verify(
        &doc,
        &daemon,
        "session.stopTask",
        serde_json::json!({"session_id": sid, "task_id": task}),
    )
    .await;
}
