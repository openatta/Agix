//! Two session pools in one process are as separate as two daemons.
//!
//! A host can assemble several pools side by side — one per isolated agent —
//! each with its own data root, project and scenes. Nothing one of them is
//! configured with or creates may show up in the other: not its sessions, not
//! the skills its data root holds, not its scenes, not where its MCP OAuth
//! tokens live. Process-wide state is what would break that, and
//! `the_engine_keeps_no_new_process_wide_state` keeps more of it from arriving.

use std::sync::Arc;
use std::time::Duration;

use base::interface::prompt::PromptBlock;
use base::interface::scene::{AgentScene, ScenePromptContext, TokenBudget};
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::{DaemonServer, SessionPool};
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const BETA_ONLY_SCENE: &str = "beta-only";

struct BetaScene;

impl AgentScene for BetaScene {
    fn id(&self) -> &str {
        BETA_ONLY_SCENE
    }
    fn name(&self) -> &str {
        "Beta only"
    }
    fn description(&self) -> &str {
        "a scene only the beta pool's host defines"
    }
    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        scene::scene::coding::CodingScene.build_system_prompt(ctx)
    }
    fn tools(&self) -> Vec<String> {
        scene::scene::coding::CodingScene.tools()
    }
    fn token_budget(&self) -> TokenBudget {
        scene::scene::coding::CodingScene.token_budget()
    }
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
    pool: Arc<SessionPool>,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Pool {
    async fn start(name: &str, scenes: Vec<Arc<dyn AgentScene>>) -> Self {
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
            "coding",
            &StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        config.settings.memory_enabled = false;

        let model = Arc::new(MockAnthropicClient::new());
        model.push_turn(one_turn());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::scene::coding::CodingScene),
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
    let b = Pool::start("beta", vec![Arc::new(BetaScene)]).await;

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

    assert_ne!(
        a.pool.mcp_oauth().store_dir(),
        b.pool.mcp_oauth().store_dir(),
        "MCP OAuth tokens persist under each pool's own root"
    );
}

/// A `static` holding something that can change after it is first set is
/// shared by every pool in the process. The ones that exist are listed here
/// with the reason they may be process-wide; a new one fails this test until
/// it is either moved onto the pool or added here with its reason.
#[test]
fn the_engine_keeps_no_new_process_wide_state() {
    const ALLOWED: &[(&str, &str, &str)] = &[
        ("crates/model/src/tokens.rs", "static B:", "the BPE table: a constant, loaded once"),
        ("crates/model/src/tokens.rs", "Mutex<HashMap<u64, usize>>", "token counts keyed by a hash of the text: the same answer for every pool"),
        ("crates/model/src/tokens.rs", "TokenCounter", "the default counter: always the same tiktoken counter"),
        ("crates/scene/src/scene/coding.rs", "SECTION_CACHE", "prompt sections keyed by a fingerprint of their inputs"),
        ("crates/team/src/team_memory.rs", "PATTERNS", "compiled regexes"),
        ("crates/tools/src/bash.rs", "SUFFIXES", "a constant table"),
        ("crates/mcp/src/manager.rs", "REGISTRY", "the official MCP registry: a constant"),
        ("crates/core/src/interface/exec/local/network.rs", "PROXIED", "an HTTP client; it reads the process environment's proxy, which every pool shares anyway"),
        ("crates/core/src/interface/exec/local/network.rs", "DIRECT", "an HTTP client with no per-pool configuration"),
        ("crates/core/src/interface/exec/local/sandbox.rs", "AVAILABLE", "whether this machine can sandbox at all"),
        ("crates/permissions/src/settings_patch.rs", "WRITE_LOCK", "serialises writes to settings files, which pools may share"),
        ("daemon/src/session_pool.rs", "OTEL", "the OpenTelemetry exporter installs process-wide providers; see assemble::pool"),
    ];

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
    for dir in ["crates", "daemon/src"] {
        visit(&root.join(dir), &root, &mut found);
    }
    let unexpected: Vec<_> = found
        .into_iter()
        .filter(|(file, line)| {
            !ALLOWED
                .iter()
                .any(|(allowed, name, _why)| file == allowed && line.contains(name))
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "process-wide mutable state, shared by every pool in the process:\n{}",
        unexpected
            .iter()
            .map(|(file, line)| format!("  {file}: {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn visit(dir: &std::path::Path, root: &std::path::Path, found: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            visit(&path, root, found);
            continue;
        }
        if !path.extension().is_some_and(|e| e == "rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let code = text.split("#[cfg(test)]").next().unwrap_or("");
        for line in code.lines().map(str::trim) {
            let is_static = line.starts_with("static ") || line.starts_with("pub static ");
            let mutable = ["Mutex<", "RwLock<", "RefCell<", "OnceLock<", "OnceCell<"]
                .iter()
                .any(|cell| line.contains(cell));
            if is_static && mutable {
                found.push((rel.clone(), line.to_string()));
            }
        }
    }
}
