//! A scene the host defines is served like a built-in one: it can be named in
//! `extra_scenes`, sessions can be created in it, and turning it off does not
//! lose it.

use std::sync::Arc;
use std::time::Duration;

use base::interface::prompt::PromptBlock;
use base::interface::scene::{AgentScene, ScenePromptContext, TokenBudget};
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const HOST_SCENE: &str = "host-review";

struct HostScene;

impl AgentScene for HostScene {
    fn id(&self) -> &str {
        HOST_SCENE
    }
    fn name(&self) -> &str {
        "Review"
    }
    fn description(&self) -> &str {
        "a scene this host defines"
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

async fn call(
    sock: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let request = serde_json::json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
    let mut conn = UnixStream::connect(sock).await.unwrap();
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
            return frame;
        }
    }
}

#[tokio::test]
async fn a_host_scene_is_served_and_survives_being_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("scenes.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
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
            cwd: Some(cwd),
            model_client: Some(Arc::new(MockAnthropicClient::new()) as Arc<dyn AnthropicClient>),
            scenes: vec![Arc::new(HostScene)],
            extra_scenes: vec![HOST_SCENE.into()],
            ..Default::default()
        },
    )
    .await
    .expect("a host scene named in extra_scenes is known by then");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let serving = server.clone();
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

    let created = call(
        &sock,
        "session.create",
        serde_json::json!({"scene": HOST_SCENE}),
    )
    .await;
    assert_eq!(created["result"]["scene"], HOST_SCENE, "{created}");
    let sid = created["result"]["session_id"].clone();
    let closed = call(
        &sock,
        "session.close",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    assert!(closed["result"].is_object(), "{closed}");

    let off = call(
        &sock,
        "scene.deactivate",
        serde_json::json!({"scene": HOST_SCENE}),
    )
    .await;
    assert!(off["result"].is_object(), "{off}");
    let on = call(
        &sock,
        "scene.activate",
        serde_json::json!({"scene": HOST_SCENE}),
    )
    .await;
    assert_eq!(
        on["result"]["active"], true,
        "a host scene turned off must be possible to turn on again: {on}"
    );
}
