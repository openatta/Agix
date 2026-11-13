//! Compaction reaches the client as a `session.event` frame.
//!
//! `AgentEvent::CompactAction` carries the strategy and both message counts,
//! telemetry records it, and the transcript gets a `LogEntry::Compact` marker,
//! but none of that reaches a client by itself — and the consequence (the
//! model no longer remembers something) is visible to the user while the cause
//! is not.
//!
//! Driven through the socket rather than by calling the forwarding loop,
//! because the loop is a whitelist: a variant that is emitted and not listed
//! is exactly the failure this covers, and only the real path can tell the
//! difference.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base::interface::scene::{Scene, ScenePromptContext, TokenBudget};
use base::prompt::PromptBlock;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use model::types::MessagesRequest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

/// The code scene with a context budget one token wide, so the first turn
/// crosses it. Compaction is otherwise a function of conversation length,
/// which a test would have to spend a great many turns reaching.
struct HairTriggerScene(scene::code::CodeScene);

impl Scene for HairTriggerScene {
    fn id(&self) -> &str {
        "hair-trigger"
    }

    fn name(&self) -> &str {
        "Hair Trigger"
    }

    fn description(&self) -> &str {
        "code, compacting on the first turn"
    }

    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        self.0.build_system_prompt(ctx)
    }

    fn tools(&self) -> Vec<String> {
        self.0.tools()
    }

    fn token_budget(&self) -> TokenBudget {
        TokenBudget {
            compact_threshold: 1,
            compact_keep_recent: 1,
        }
    }
}

/// One canned assistant turn: says "ok" and stops.
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

/// Every frame and the final response of one `session.run_turn`, in order.
async fn run_one_turn(sock: &Path) -> Vec<serde_json::Value> {
    let mut conn = UnixStream::connect(sock).await.unwrap();
    let (r, mut w) = conn.split();
    let mut reader = BufReader::new(r);

    let exchange = |method: &str, params: serde_json::Value, id: u32| {
        let request = serde_json::json!({
            "jsonrpc":"2.0","method":method,"params":params,"id":id
        });
        format!("{request}\n")
    };

    w.write_all(exchange("session.create", serde_json::json!({}), 1).as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let created: serde_json::Value = serde_json::from_str(&line).unwrap();
    let session_id = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    w.write_all(
        exchange(
            "session.run_turn",
            serde_json::json!({"session_id": session_id, "message": "hello"}),
            2,
        )
        .as_bytes(),
    )
    .await
    .unwrap();

    let mut frames = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap();
        assert!(n > 0, "connection closed before the turn ended");
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        let terminal = frame.get("id").is_some();
        frames.push(frame);
        if terminal {
            return frames;
        }
    }
}

#[tokio::test]
async fn a_compacted_turn_says_so_on_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("compact.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    // A second model call after the turn, for a session nothing reads back.
    config.settings.values.memory_enabled = false;

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
        Arc::new(HairTriggerScene(scene::code::CodeScene::default())),
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            model_client: Some(Arc::new(OneTurnClient)),
            transcripts: daemon::Transcripts::In(store),
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
    let _handle = tokio::spawn(async move {
        let _ = serving.serve_unix(&bind).await;
    });
    for _ in 0..80 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");

    let frames = run_one_turn(&sock).await;

    let compact = frames
        .iter()
        .find(|f| f["params"]["event"]["kind"] == "compact")
        .unwrap_or_else(|| {
            let kinds: Vec<&str> = frames
                .iter()
                .filter_map(|f| f["params"]["event"]["kind"].as_str())
                .collect();
            panic!("no `compact` frame; the turn emitted {kinds:?}")
        });

    let event = &compact["params"]["event"];
    assert!(
        event["strategy"].is_string(),
        "a compact frame names the strategy that ran: {event}"
    );
    assert!(
        event["messages_before"].is_number() && event["messages_after"].is_number(),
        "a compact frame carries both message counts: {event}"
    );
    assert_eq!(
        compact["method"], "session.event",
        "compaction is a session frame, not a daemon-level notification"
    );
}
