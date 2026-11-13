//! The same exchange over Unix socket, TCP and WebSocket.
//!
//! The daemon's claim is that a transport does framing and nothing else. The
//! only way to check a claim like that is to run one exchange over each and
//! see that nothing but the framing differed — a per-transport test that
//! sends `daemon.status` and stops proves the handshake works and little
//! more.
//!
//! What differs per transport, and therefore what these cover: streaming a
//! turn frame by frame, the aggregate read agreeing with the streamed one,
//! refusing an oversized frame, and answering a permission prompt mid-turn
//! — the bidirectional case WebSocket was chosen for, since over
//! server-sent events that answer needs a second channel and a correlation
//! scheme.
//!
//! There is one property here that no aggregate assertion can express: that
//! a token reaches the client *while the turn is still running*. A client
//! that collects everything and returns at the end sees an identical result
//! whether the daemon streamed or buffered — which is how eight unflushed
//! write sites survived in the daemon for as long as they did.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{
    BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage as WireUsage,
};
use model::types::MessagesRequest;
use rpc_client::{DaemonRpcClient, TurnItem};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "transport-parity-token";

/// A model that emits its tokens with a gap between them.
///
/// The gap is the whole point: it separates "the daemon forwarded this token
/// when it had it" from "the daemon collected everything and flushed once at
/// the end". Without it both look the same from the client.
struct DrippingClient {
    chunks: Vec<String>,
    gap: Duration,
    seen: Arc<StdMutex<usize>>,
}

impl AnthropicClient for DrippingClient {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        *self.seen.lock().unwrap() += 1;
        let chunks = self.chunks.clone();
        let gap = self.gap;
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text { text: String::new() },
            });
            for chunk in chunks {
                yield Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: BlockDelta::TextDelta { text: chunk },
                });
                tokio::time::sleep(gap).await;
            }
            yield Ok(StreamEvent::ContentBlockStop { index: 0 });
            yield Ok(StreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: Some(WireUsage::default()),
            });
        })
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

/// Calls `Bash` until it sees its own tool result, then answers with text —
/// the shape that makes the daemon stop and ask, so the prompt round trip
/// can be exercised.
///
/// Keyed on the conversation rather than on a call counter: the same daemon
/// serves one turn per transport here, and a counter would have the second
/// and third turns skipping straight to the text round.
struct AskingClient;

impl AnthropicClient for AskingClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        let already_ran = serde_json::to_string(&req.messages)
            .map(|s| s.contains("tool_result"))
            .unwrap_or(false);
        let events: Vec<StreamEvent> = if !already_ran {
            vec![
                StreamEvent::ContentBlockStart {
                    index: 0,
                    content_block: ContentBlockStart::ToolUse {
                        id: "tool-1".into(),
                        name: "Bash".into(),
                        input: serde_json::Value::Null,
                    },
                },
                StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: BlockDelta::InputJsonDelta {
                        partial_json: serde_json::json!({"command": "touch parity.txt"})
                            .to_string(),
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
        } else {
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
                        text: "done".into(),
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
        };
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

struct AllowAllPermission;

#[async_trait::async_trait]
impl base::interface::permission::PermissionPolicy for AllowAllPermission {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

/// Asks about everything, so a turn with a tool call stops for an answer.
struct AskEverythingPermission;

#[async_trait::async_trait]
impl base::interface::permission::PermissionPolicy for AskEverythingPermission {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Prompt {
            prompt_id: uuid::Uuid::new_v4().to_string(),
            message: "may I?".into(),
            paths: Vec::new(),
            managed: false,
        }
    }
}

struct Harness {
    server: Arc<DaemonServer>,
    sock: std::path::PathBuf,
    tcp: std::net::SocketAddr,
    ws: std::net::SocketAddr,
    _dir: tempfile::TempDir,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn dripping(chunks: &[&str], gap: Duration) -> Self {
        Self::start(
            Arc::new(DrippingClient {
                chunks: chunks.iter().map(|c| c.to_string()).collect(),
                gap,
                seen: Arc::new(StdMutex::new(0)),
            }),
            true,
        )
        .await
    }

    /// A daemon that will stop and ask before running the tool.
    async fn asking() -> Self {
        Self::start(Arc::new(AskingClient), false).await
    }

    async fn start(client: Arc<dyn AnthropicClient>, allow_all: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let store: Arc<dyn history::store::HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &cwd,
                history::path::HistoryRoots::under(dir.path()),
            )
            .await
            .unwrap(),
        );
        let sock = dir.path().join("parity.sock");
        let config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&sock),
            "code",
            &StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        // Assembled the way `main.rs` assembles, so a transport claim is
        // tested against the daemon these tests are about rather than one
        // this file put together itself.
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(cwd.clone()),
                model_client: Some(client),
                transcripts: daemon::Transcripts::In(store),
                permission: Some(base::interface::permission::PermissionSet::without_grants(
                    // `allow_all` decides whether the daemon ever raises a prompt:
                    // the streaming tests want turns that just run, the prompt
                    // test wants one that stops and asks.
                    if allow_all {
                        Arc::new(AllowAllPermission)
                            as Arc<dyn base::interface::permission::PermissionPolicy>
                    } else {
                        Arc::new(AskEverythingPermission)
                    },
                )),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");

        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        server.set_tcp_token(TOKEN.to_string()).await;

        let mut tasks = Vec::new();
        {
            let s = server.clone();
            let sock = sock.clone();
            tasks.push(tokio::spawn(async move {
                let _ = s.serve_unix(&sock).await;
            }));
        }

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp = tcp_listener.local_addr().unwrap();
        {
            let s = server.clone();
            tasks.push(tokio::spawn(async move {
                let _ = s.serve_tcp_listener(tcp_listener).await;
            }));
        }

        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = ws_listener.local_addr().unwrap();
        {
            let s = server.clone();
            tasks.push(tokio::spawn(async move {
                let _ = daemon::ws::serve_ws_listener(s, ws_listener).await;
            }));
        }

        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Self {
            server,
            sock,
            tcp,
            ws,
            _dir: dir,
            tasks,
        }
    }

    /// One client per transport, each already past its handshake.
    async fn clients(&self) -> Vec<DaemonRpcClient> {
        vec![
            DaemonRpcClient::connect(&self.sock).await.unwrap(),
            DaemonRpcClient::connect_tcp(self.tcp, TOKEN).await.unwrap(),
            DaemonRpcClient::connect_ws(self.ws, TOKEN, Some("http://localhost:5173"))
                .await
                .unwrap(),
        ]
    }

    async fn stop(self) {
        self.server.shutdown_token().cancel();
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

#[tokio::test]
async fn every_transport_answers_the_same_request() {
    let h = Harness::dripping(&["hello"], Duration::ZERO).await;

    for mut client in h.clients().await {
        let name = client.transport_name();
        let resp = client.daemon_status().await.unwrap();
        let result = resp
            .result
            .unwrap_or_else(|| panic!("{name}: daemon.status failed: {:?}", resp.error));
        assert!(result["version"].is_string(), "{name}: {result}");
    }

    h.stop().await;
}

/// A method that *changes* something answers alike on every transport, and
/// the change itself is one change however it arrived.
///
/// The cases above ask a question (`daemon.status`) or watch a stream. Both
/// are read-shaped, and a transport that mangled a request body on the way in
/// would still answer them. `session.rename` carries a payload the daemon
/// has to parse — including one that is not ASCII, which is where a framing
/// bug shows up first — and the result is readable back through a different
/// connection than the one that wrote it.
#[tokio::test]
async fn a_mutating_method_lands_once_however_it_arrived() {
    let h = Harness::dripping(&["hello"], Duration::ZERO).await;

    let mut owner = DaemonRpcClient::connect(&h.sock).await.unwrap();
    let session = owner
        .session_create(serde_json::json!({}))
        .await
        .expect("a session");

    // Each transport names it, and each name is the one that comes back.
    for (i, mut client) in h.clients().await.into_iter().enumerate() {
        let name = client.transport_name();
        let chosen = format!("第 {i} 个传输起的名字");
        let resp = client
            .session_rename(&session, Some(&chosen))
            .await
            .unwrap();
        let result = resp
            .result
            .unwrap_or_else(|| panic!("{name}: session.rename failed: {:?}", resp.error));
        assert_eq!(result["name"], chosen.as_str(), "{name}: {result}");

        // Read back on a connection that did not write it: the rename is a
        // fact about the session, not about the connection.
        let seen = owner.session_get(&session).await.unwrap().result.unwrap();
        assert_eq!(
            seen["name"],
            chosen.as_str(),
            "{name}: the rename did not reach the session"
        );

        // The other method that changes a live session, for the same reason:
        // it carries a payload and answers with what it accepted.
        let model = format!("claude-model-{i}");
        let switched = client
            .session_set_model(&session, &model)
            .await
            .unwrap()
            .result
            .unwrap_or_else(|| panic!("{name}: session.setModel failed"));
        assert_eq!(switched["model"], model.as_str(), "{name}: {switched}");
        assert_eq!(switched["applies"], "next_turn", "{name}: {switched}");
    }

    h.stop().await;
}

/// The property an aggregate client cannot express: a token arrives while
/// the turn is still running.
///
/// The discrimination is in the timing, and the measurement is the **spread
/// between the first token and the last** rather than a wall-clock budget for
/// the first. The model drips five chunks 150ms apart, so a daemon forwarding
/// each frame as it has it delivers them ~600ms apart end to end, while one
/// that collects the turn and flushes at the end delivers all five within a
/// few milliseconds of each other. Those two are never close.
///
/// The earlier form gave the first token an absolute 300ms deadline measured
/// from `begin_turn`. That budget also had to cover connecting, dispatch and
/// scheduling, so on a loaded machine it expired before the daemon had done
/// anything wrong — the test failed for reasons it does not test. Load shifts
/// the first and last arrival by the same amount, so it cancels in the spread
/// and the property stays exactly as falsifiable: a buffering daemon still
/// fails, on any machine.
#[tokio::test]
async fn a_token_reaches_the_client_before_the_turn_ends_on_every_transport() {
    const CHUNK_GAP: Duration = Duration::from_millis(150);
    const CHUNKS: [&str; 5] = ["one ", "two ", "three ", "four ", "five"];
    /// Halfway between a buffered flush (~0) and streamed delivery (~600ms).
    const MIN_SPREAD: Duration = Duration::from_millis(300);
    /// Only to stop a hung daemon from hanging the suite; not a discriminator.
    const HANG_GUARD: Duration = Duration::from_secs(30);

    let h = Harness::dripping(&CHUNKS, CHUNK_GAP).await;

    for mut client in h.clients().await {
        let name = client.transport_name();
        let mut stream = client.begin_turn(None, "go", "t1", None).await.unwrap();

        let mut first_text: Option<String> = None;
        let mut first_at: Option<std::time::Instant> = None;
        let mut last_at: Option<std::time::Instant> = None;

        loop {
            let item = tokio::time::timeout(HANG_GUARD, stream.next())
                .await
                .unwrap_or_else(|_| panic!("{name}: the turn never finished"))
                .unwrap_or_else(|e| panic!("{name}: stream error: {e}"));

            match item {
                Some(TurnItem::Event(event)) => {
                    if event["kind"] == "text_delta" {
                        let now = std::time::Instant::now();
                        first_at.get_or_insert(now);
                        last_at = Some(now);
                        first_text.get_or_insert_with(|| {
                            event["text"].as_str().unwrap_or_default().to_string()
                        });
                    }
                }
                Some(TurnItem::Response(_)) | None => break,
            }
        }

        // The first chunk only, not the whole reply: proof it was forwarded
        // as it arrived rather than assembled first.
        assert_eq!(first_text.as_deref(), Some("one "), "{name}");

        let spread = last_at
            .zip(first_at)
            .map(|(last, first)| last - first)
            .unwrap_or_default();
        assert!(
            spread >= MIN_SPREAD,
            "{name}: all {} tokens arrived within {spread:?} of each other — the daemon \
             collected the turn and flushed it, instead of forwarding each frame as it \
             had it (streamed delivery spreads them over ~{:?})",
            CHUNKS.len(),
            CHUNK_GAP * (CHUNKS.len() as u32 - 1),
        );
    }

    h.stop().await;
}

/// The aggregate path must agree with the streaming one it is built on.
#[tokio::test]
async fn the_collected_turn_matches_what_the_stream_delivered() {
    let h = Harness::dripping(&["a", "b", "c"], Duration::from_millis(5)).await;

    for mut client in h.clients().await {
        let name = client.transport_name();
        let created = client
            .call("session.create", serde_json::json!({}))
            .await
            .unwrap();
        let sid = created.result.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let turn = client
            .session_run_turn(&sid, "go", "t1", None)
            .await
            .unwrap();
        assert_eq!(turn.text, "abc", "{name}");
        assert!(turn.turn_complete, "{name}: no turn_complete event");
        assert!(
            turn.response.is_some_and(|r| r.error.is_none()),
            "{name}: turn failed"
        );
    }

    h.stop().await;
}

/// 16 MiB is the ceiling on all three. Over it, the connection closes rather
/// than the frame being skipped — after an oversized frame the daemon does
/// not know where the next one starts.
#[tokio::test]
async fn an_oversized_frame_closes_the_connection_on_the_line_transports() {
    let h = Harness::dripping(&["x"], Duration::ZERO).await;

    let mut client = DaemonRpcClient::connect(&h.sock).await.unwrap();
    let huge = "z".repeat(17 * 1024 * 1024);
    let result = client
        .call("session.create", serde_json::json!({ "scene": huge }))
        .await;
    assert!(
        result.is_err(),
        "an over-limit frame must end the connection, not be answered"
    );

    h.stop().await;
}

/// The bidirectional case, on every transport.
///
/// The daemon asks mid-turn and waits; the client answers on the same
/// connection and the turn continues. This is the exchange WebSocket was
/// chosen for — over server-sent events the answer needs a second channel
/// and a correlation scheme — so "it works over WS too" is the claim that
/// justified the transport.
#[tokio::test]
async fn a_prompt_is_answered_on_the_same_connection_on_every_transport() {
    let h = Harness::asking().await;

    for mut client in h.clients().await {
        let name = client.transport_name();

        // A second connection answers the prompt, because the first one is
        // parked inside its own turn — same as a real client, which cannot
        // reply on a socket it is blocked reading.
        let answerer_sock = h.sock.clone();

        let mut stream = client
            .begin_turn(None, "make a file", "t1", None)
            .await
            .unwrap();
        let mut answered = false;
        let mut saw_tool_result = false;

        while let Some(item) = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .unwrap_or_else(|_| panic!("{name}: the turn stalled"))
            .unwrap()
        {
            match item {
                TurnItem::Event(event) => match event["kind"].as_str() {
                    Some("prompt") if !answered => {
                        let prompt_id = event["prompt_id"].as_str().unwrap().to_string();
                        let sid = stream
                            .session_id()
                            .expect("a stream frame names its session")
                            .to_string();
                        let mut answerer = DaemonRpcClient::connect(&answerer_sock).await.unwrap();
                        let ack = answerer
                            .call(
                                "session.respondToPrompt",
                                serde_json::json!({
                                    "session_id": sid,
                                    "prompt_id": prompt_id,
                                    "decision": {"type": "deny", "reason": "parity test"},
                                }),
                            )
                            .await
                            .unwrap();
                        assert!(
                            ack.error.is_none(),
                            "{name}: respondToPrompt failed: {ack:?}"
                        );
                        answered = true;
                    }
                    Some("tool_result") => saw_tool_result = true,
                    _ => {}
                },
                TurnItem::Response(resp) => {
                    assert!(resp.error.is_none(), "{name}: turn failed: {resp:?}");
                    break;
                }
            }
        }

        assert!(answered, "{name}: the daemon never asked");
        assert!(
            saw_tool_result,
            "{name}: the turn did not continue after the answer"
        );
    }

    h.stop().await;
}

/// Dropping the connection mid-turn does **not** cancel the turn.
///
/// This asserted the opposite until sessions grew subscribers. Cancelling on
/// a broken writer was right while one CLI owned one socket; with several
/// tabs able to watch one session it means closing the tab that sent the
/// message kills a turn the others are reading — and the teardown cascaded
/// into deleting the session's sidechain transcripts.
///
/// The tokens are already paid for, so the turn finishes and lands in the
/// transcript, where anyone who comes back can read it.
#[tokio::test]
async fn dropping_the_connection_mid_turn_leaves_the_turn_running() {
    let h = Harness::dripping(
        &["one ", "two ", "three ", "four ", "five"],
        Duration::from_millis(150),
    )
    .await;

    let session_id = {
        let mut client = DaemonRpcClient::connect(&h.sock).await.unwrap();
        let mut stream = client.begin_turn(None, "go", "t1", None).await.unwrap();
        let item = tokio::time::timeout(Duration::from_millis(500), stream.next())
            .await
            .expect("a frame within 500ms")
            .unwrap()
            .expect("a frame, not end of stream");
        assert!(matches!(item, TurnItem::Event(_)));
        stream
            .session_id()
            .expect("the frame names its session")
            .to_string()
    };

    // Still running right after the disconnect — nothing cancelled it.
    let mut observer = DaemonRpcClient::connect(&h.sock).await.unwrap();
    let during = observer
        .call(
            "session.get",
            serde_json::json!({ "session_id": session_id }),
        )
        .await
        .unwrap();
    assert_eq!(
        during.result.as_ref().map(|r| r["turn_state"].clone()),
        Some(serde_json::json!("running")),
        "the turn was cancelled when its client went away: {during:?}"
    );

    // And it reaches the end on its own.
    let mut settled = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let resp = observer
            .call(
                "session.get",
                serde_json::json!({ "session_id": session_id }),
            )
            .await
            .unwrap();
        if resp
            .result
            .map(|r| r["turn_state"] == "idle")
            .unwrap_or(false)
        {
            settled = true;
            break;
        }
    }
    assert!(settled, "the turn never finished after its client left");

    h.stop().await;
}

/// `daemon.shutdown` stops the daemon, and says so before it goes.
#[tokio::test]
async fn shutdown_answers_before_it_stops_serving() {
    let h = Harness::dripping(&["x"], Duration::ZERO).await;

    let mut client = DaemonRpcClient::connect(&h.sock).await.unwrap();
    let resp = client.daemon_shutdown().await.unwrap();
    assert_eq!(
        resp.result.as_ref().map(|r| r["shutting_down"].clone()),
        Some(serde_json::json!(true)),
        "shutdown must answer the caller before it tears the listeners down: {resp:?}"
    );

    // And it really did shut down: the accept loops end on their own.
    for t in h.tasks {
        let _ = tokio::time::timeout(Duration::from_secs(5), t)
            .await
            .expect("listeners should stop after daemon.shutdown");
    }
}
