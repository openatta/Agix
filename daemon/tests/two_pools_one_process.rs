//! Two session pools in one process are as separate as two daemons.
//!
//! A host can assemble several pools side by side — one per isolated agent —
//! each with its own data root, project and scenes. Nothing one of them is
//! configured with or creates may show up in the other: not its sessions, not
//! the skills its data root holds, not its scenes, not where its MCP OAuth
//! tokens live. Process-wide state is what would break that, and
//! `tests/guards/tests/process_state.rs` keeps more of it from arriving.

use std::sync::Arc;
use std::time::Duration;

use base::interface::scene::SceneSpec;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::{DaemonServer, SessionPool};
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const BETA_ONLY_SCENE: &str = "beta-only";

/// A scene only the beta pool's host defines.
fn beta_scene() -> SceneSpec {
    let code = scene::code::spec(scene::code::Interaction::Interactive);
    let mut s = SceneSpec::new(BETA_ONLY_SCENE, "Beta only", code.prompt);
    s.token_budget = code.token_budget;
    s
}

fn one_turn() -> Vec<StreamEvent> {
    vec![
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
    ]
}

struct Pool {
    sock: std::path::PathBuf,
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pool: Arc<SessionPool>,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Pool {
    async fn start(name: &str, scenes: Vec<Arc<SceneSpec>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join(format!("{name}.sock"));
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let skill = dir.path().join("skills").join(format!("pool-{name}-skill"));
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\ndescription: a skill only pool-{name} has\n---\nDo the {name} thing.\n"),
        )
        .unwrap();

        let mut config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&sock),
            "code",
            &StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        config.settings.values.memory_enabled = false;

        let model = Arc::new(MockAnthropicClient::new());
        model.push_turn(one_turn());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(cwd),
                scenes,
                model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
                ..Default::default()
            },
        )
        .await
        .expect("the pool assembles");

        let server = Arc::new(DaemonServer::new(pool.clone(), CancellationToken::new()));
        let bind = sock.clone();
        tokio::spawn(async move {
            let _ = server.serve_unix(&bind).await;
        });
        for _ in 0..80 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(sock.exists(), "pool {name}: socket never bound");
        Self {
            sock,
            pool,
            model,
            _dir: dir,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = serde_json::json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "{method}: connection closed before a response");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                assert!(frame.get("result").is_some(), "{method} failed: {frame}");
                return frame["result"].clone();
            }
        }
    }

    async fn one_turn_session(&self) -> String {
        let id = self.call("session.create", serde_json::json!({})).await["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        self.call(
            "session.run_turn",
            serde_json::json!({"session_id": id, "message": "hello"}),
        )
        .await;
        id
    }

    async fn listed(&self, method: &str, list: &str, key: &str) -> Vec<String> {
        self.call(method, serde_json::json!({})).await[list]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|item| item.get(key).and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect()
    }

    fn first_request(&self) -> String {
        serde_json::to_string(&self.model.nth_request(0).expect("a model call was made")).unwrap()
    }
}

#[tokio::test]
async fn two_pools_in_one_process_see_nothing_of_each_other() {
    let a = Pool::start("alpha", Vec::new()).await;
    let b = Pool::start("beta", vec![Arc::new(beta_scene())]).await;

    let (in_a, in_b) = (a.one_turn_session().await, b.one_turn_session().await);

    let sessions_a = a.listed("session.list", "sessions", "session_id").await;
    let sessions_b = b.listed("session.list", "sessions", "session_id").await;
    assert!(sessions_a.contains(&in_a) && !sessions_a.contains(&in_b));
    assert!(sessions_b.contains(&in_b) && !sessions_b.contains(&in_a));

    let (prompt_a, prompt_b) = (a.first_request(), b.first_request());
    assert!(prompt_a.contains("pool-alpha-skill") && !prompt_a.contains("pool-beta-skill"));
    assert!(prompt_b.contains("pool-beta-skill") && !prompt_b.contains("pool-alpha-skill"));

    let scenes_a = a.listed("scene.list", "scenes", "scene").await;
    let scenes_b = b.listed("scene.list", "scenes", "scene").await;
    assert!(!scenes_a.iter().any(|s| s == BETA_ONLY_SCENE));
    assert!(scenes_b.iter().any(|s| s == BETA_ONLY_SCENE));

    #[cfg(feature = "mcp")]
    assert_ne!(
        a.pool.mcp_auth().credential_dir(),
        b.pool.mcp_auth().credential_dir(),
        "MCP OAuth tokens persist under each pool's own root"
    );
}

#[cfg(feature = "ptc")]
fn run_code_round(code: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: "toolu_program".into(),
                name: "RunCode".into(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: serde_json::json!({ "code": code }).to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::ToolUse),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

/// Each pool has its own `RunCode` budget: one pool's programs holding
/// every slot it has leave another pool's programs free to start.
#[cfg(feature = "ptc")]
#[tokio::test(flavor = "multi_thread")]
async fn a_pool_with_every_program_slot_taken_leaves_the_other_pools_programs_running() {
    let a = Pool::start("alpha", Vec::new()).await;
    let b = Pool::start("beta", Vec::new()).await;
    let slots = a.pool.run_slots();
    let _held = slots
        .clone()
        .acquire_many_owned(slots.available_permits() as u32)
        .await
        .unwrap();
    assert_eq!(slots.available_permits(), 0);

    b.one_turn_session().await;
    b.model
        .push_turn(run_code_round("return 'pool-beta-ran-' + (6 * 7);"));
    b.model.push_turn(one_turn());
    let session = b.call("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    tokio::time::timeout(
        Duration::from_secs(60),
        b.call(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "run it"}),
        ),
    )
    .await
    .expect("pool beta's program waited for a slot pool alpha holds");

    let answered = serde_json::to_string(
        &b.model
            .nth_request(2)
            .expect("the program's result went back"),
    )
    .unwrap();
    assert!(answered.contains("pool-beta-ran-42"), "{answered}");
}
