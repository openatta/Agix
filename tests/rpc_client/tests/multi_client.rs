//! Several browser tabs, one daemon, one session.
//!
//! The daemon serves connections, not owners. A conversation open in three
//! tabs is one session with three watchers, and every property here follows
//! from that: they all see the same stream, any of them can answer a
//! permission ask, and closing one changes nothing for the others — in
//! particular, a broken writer must not tear the session down and take its
//! sidechain transcripts with it.

use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{
    BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage as WireUsage,
};
use model::types::MessagesRequest;
use rpc_client::{DaemonRpcClient, TurnItem};
use tokio_util::sync::CancellationToken;

/// Emits text in chunks with a gap, so a test can act while a turn is still
/// running.
struct DrippingClient {
    chunks: Vec<String>,
    gap: Duration,
}

impl AnthropicClient for DrippingClient {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
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

/// Calls `Bash` until it sees its own tool result, so the turn stops on a
/// permission ask.
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
                        partial_json: serde_json::json!({"command": "touch multi.txt"}).to_string(),
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
impl base::interface::permission::Permission for AllowAllPermission {
    async fn check(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &std::path::Path,
        _: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

struct AskEverythingPermission;

#[async_trait::async_trait]
impl base::interface::permission::Permission for AskEverythingPermission {
    async fn check(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &std::path::Path,
        _: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Prompt {
            prompt_id: uuid::Uuid::new_v4().to_string(),
            message: "may I?".into(),
            paths: Vec::new(),
        }
    }
}

struct Harness {
    server: Arc<DaemonServer>,
    sock: std::path::PathBuf,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
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
        let sock = dir.path().join("multi.sock");
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
            Arc::new(scene::code::CodeScene::default()),
            daemon::Assembly {
                cwd: Some(cwd.clone()),
                model_client: Some(client),
                transcripts: daemon::Transcripts::In(store),
                permission: Some(if allow_all {
                    Arc::new(AllowAllPermission) as Arc<dyn base::interface::permission::Permission>
                } else {
                    Arc::new(AskEverythingPermission)
                }),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");

        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let s = server.clone();
        let listen = sock.clone();
        let task = tokio::spawn(async move {
            let _ = s.serve_unix(&listen).await;
        });
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self {
            server,
            sock,
            _dir: dir,
            task,
        }
    }

    async fn dripping(chunks: &[&str], gap: Duration) -> Self {
        Self::start(
            Arc::new(DrippingClient {
                chunks: chunks.iter().map(|c| c.to_string()).collect(),
                gap,
            }),
            true,
        )
        .await
    }

    async fn tab(&self) -> DaemonRpcClient {
        DaemonRpcClient::connect(&self.sock).await.unwrap()
    }

    async fn stop(self) {
        self.server.shutdown_token().cancel();
        let _ = self.task.await;
    }
}

/// Wait for a `session.event` of `kind` on a connection that is only
/// watching — no request of its own in flight.
async fn wait_for_kind(
    client: &mut DaemonRpcClient,
    kind: &str,
    within: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let frame = match tokio::time::timeout(left, client.next_frame()).await {
            Err(_) => return None,
            Ok(Ok(Some(frame))) => frame,
            Ok(_) => return None,
        };
        if frame["method"] == "session.event" && frame["params"]["event"]["kind"] == kind {
            return Some(frame["params"]["event"].clone());
        }
    }
}

/// The core of it: a tab that did not start the turn still sees the tokens.
#[tokio::test]
async fn a_watching_tab_sees_the_stream_of_a_turn_it_did_not_start() {
    let h = Harness::dripping(&["one ", "two ", "three "], Duration::from_millis(80)).await;

    let mut owner = h.tab().await;
    let created = owner
        .call("session.create", serde_json::json!({}))
        .await
        .unwrap();
    let sid = created.result.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The second tab subscribes before anything runs.
    let mut watcher = h.tab().await;
    let sub = watcher.session_subscribe(&sid).await.unwrap();
    let sub = sub.result.expect("subscribe should succeed");
    assert_eq!(sub["session_id"], sid);
    assert!(sub["last_seq"].is_number(), "{sub}");

    let turn_sid = sid.clone();
    let runner = tokio::spawn(async move {
        owner
            .session_run_turn(&turn_sid, "go", "t1", None)
            .await
            .unwrap()
    });

    let seen = wait_for_kind(&mut watcher, "text_delta", Duration::from_secs(5)).await;
    assert!(
        seen.is_some(),
        "the watching tab never received a token — frames are still going only to the initiator"
    );

    let turn = runner.await.unwrap();
    assert_eq!(turn.text, "one two three ", "the initiator's own view");

    h.stop().await;
}

/// Closing the tab that started a turn does not end the turn, and does not
/// touch the session the other tabs are looking at.
#[tokio::test]
async fn closing_the_initiating_tab_leaves_the_turn_and_the_session_alone() {
    let h = Harness::dripping(&["a ", "b ", "c ", "d "], Duration::from_millis(80)).await;

    let mut owner = h.tab().await;
    let created = owner
        .call("session.create", serde_json::json!({}))
        .await
        .unwrap();
    let sid = created.result.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut watcher = h.tab().await;
    watcher.session_subscribe(&sid).await.unwrap();

    // Start a turn, read one frame, then drop the connection mid-stream.
    {
        let mut stream = owner
            .begin_turn(Some(&sid), "go", "t1", None)
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("a frame")
            .unwrap()
            .expect("not end of stream");
        assert!(matches!(first, TurnItem::Event(_)));
    }
    drop(owner);

    // The watcher keeps receiving, and the turn reaches its end.
    let completed = wait_for_kind(&mut watcher, "turn_complete", Duration::from_secs(10)).await;
    assert!(
        completed.is_some(),
        "the turn stopped when its initiator disconnected"
    );

    // And the session is still there, idle, for whoever is still looking.
    let got = watcher
        .call(
            "session.get",
            serde_json::json!({ "session_id": sid.clone() }),
        )
        .await
        .unwrap();
    let got = got.result.expect("the session should still exist");
    assert_eq!(got["turn_state"], "idle", "{got}");

    h.stop().await;
}

/// A tab that arrives after the ask still gets the question, with the same
/// id, and answering it from there works.
#[tokio::test]
async fn a_late_tab_is_handed_the_pending_permission_ask() {
    let h = Harness::start(Arc::new(AskingClient), false).await;

    let mut owner = h.tab().await;
    let created = owner
        .call("session.create", serde_json::json!({}))
        .await
        .unwrap();
    let sid = created.result.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let turn_sid = sid.clone();
    let runner = tokio::spawn(async move {
        let mut stream = owner
            .begin_turn(Some(&turn_sid), "make a file", "t1", None)
            .await
            .unwrap();
        while let Some(item) = stream.next().await.unwrap() {
            if matches!(item, TurnItem::Response(_)) {
                break;
            }
        }
    });

    // Arrive once the turn is actually sitting on the ask.
    //
    // Sleeping a fixed 300ms instead assumed the turn had got that far, which
    // is a guess about machine speed rather than a fact about the daemon: on a
    // loaded machine it had not, the subscribe came back with no pending
    // prompt, and the test failed for a reason that had nothing to do with
    // what it tests. Subscribing until the ask shows up waits for the
    // condition itself, so a slow machine only makes it slower.
    const ASK_APPEARS_WITHIN: Duration = Duration::from_secs(10);
    let deadline = std::time::Instant::now() + ASK_APPEARS_WITHIN;
    let (mut late, sub) = loop {
        let mut tab = h.tab().await;
        let sub = tab
            .session_subscribe(&sid)
            .await
            .unwrap()
            .result
            .expect("subscribe");
        if sub["pending_prompts"]
            .as_array()
            .is_some_and(|p| !p.is_empty())
        {
            break (tab, sub);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the turn never reached its permission ask within {ASK_APPEARS_WITHIN:?}: {sub}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let pending = sub["pending_prompts"].as_array().expect("an array");
    assert_eq!(
        pending.len(),
        1,
        "a tab opened mid-ask must be handed the question, not a session that looks stuck: {sub}"
    );
    let prompt_id = pending[0]["params"]["event"]["prompt_id"]
        .as_str()
        .expect("the replayed frame carries its prompt_id")
        .to_string();

    // Answering from the late tab settles the turn the other tab started.
    let ack = late
        .call(
            "session.respondToPrompt",
            serde_json::json!({
                "session_id": sid,
                "prompt_id": prompt_id,
                "decision": {"type": "deny", "reason": "multi-client test"},
            }),
        )
        .await
        .unwrap();
    assert!(ack.error.is_none(), "{ack:?}");

    tokio::time::timeout(Duration::from_secs(10), runner)
        .await
        .expect("the turn should finish once someone answers")
        .unwrap();

    h.stop().await;
}

/// Unsubscribing stops the frames, and unsubscribing twice is not an error.
#[tokio::test]
async fn unsubscribing_stops_the_frames_and_is_idempotent() {
    let h = Harness::dripping(&["x "], Duration::ZERO).await;

    let mut owner = h.tab().await;
    let created = owner
        .call("session.create", serde_json::json!({}))
        .await
        .unwrap();
    let sid = created.result.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut watcher = h.tab().await;
    watcher.session_subscribe(&sid).await.unwrap();
    let off = watcher.session_unsubscribe(&sid).await.unwrap();
    assert_eq!(off.result.unwrap()["subscribed"], false);
    let again = watcher.session_unsubscribe(&sid).await.unwrap();
    assert!(again.error.is_none(), "unsubscribing twice is not an error");

    owner
        .session_run_turn(&sid, "go", "t1", None)
        .await
        .unwrap();
    assert!(
        wait_for_kind(&mut watcher, "text_delta", Duration::from_millis(300))
            .await
            .is_none(),
        "frames kept arriving after unsubscribe"
    );

    h.stop().await;
}

/// Subscribing to a session that does not exist is an error rather than a
/// subscription that will never deliver anything.
#[tokio::test]
async fn subscribing_to_an_unknown_session_is_refused() {
    let h = Harness::dripping(&["x"], Duration::ZERO).await;

    let mut tab = h.tab().await;
    let resp = tab
        .session_subscribe("NopeNopeNopeNopeNope12")
        .await
        .unwrap();
    assert_eq!(
        resp.error.map(|e| e.code),
        Some(daemon::rpc::codes::SESSION_NOT_FOUND)
    );

    h.stop().await;
}
