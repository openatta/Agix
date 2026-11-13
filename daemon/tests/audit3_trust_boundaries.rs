//! 0.3.12 发布前对抗式审查（信任边界与权限）核实过的缺陷，每条都从宿主一侧的
//! socket 看外部结果。修好之前标 ignore，修好之后留作回归用例。

#![cfg(feature = "skills")]

use std::sync::Arc;
use std::time::Duration;

use base::interface::scene::{SceneSpec, ToolSurface};
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const NO_SHELL: &str = "audit3-no-shell";

/// A scene whose whitelist leaves Bash out.
fn no_shell_scene() -> SceneSpec {
    let mini = scene::mini::spec();
    let mut s = SceneSpec::new(NO_SHELL, "No shell", mini.prompt);
    s.tools = ToolSurface::allowing(&["Read", "Skill"]);
    s.token_budget = mini.token_budget;
    s
}

fn stop(reason: base::message::StopReason) -> StreamEvent {
    StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: Some(reason),
            stop_sequence: None,
        },
        usage: Some(Usage::default()),
    }
}

fn tool_round(id: &str, name: &str, input: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: name.into(),
                input: Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        stop(base::message::StopReason::ToolUse),
    ]
}

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
            delta: BlockDelta::TextDelta { text: text.into() },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        stop(base::message::StopReason::EndTurn),
    ]
}

async fn call(sock: &std::path::Path, method: &str, params: Value) -> Value {
    let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
    let mut conn = UnixStream::connect(sock).await.unwrap();
    conn.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let (r, _w) = conn.split();
    let mut reader = BufReader::new(r);
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(120), reader.read_line(&mut line))
            .await
            .expect("the call answered")
            .unwrap();
        assert!(n > 0, "{method}: connection closed before a response");
        let frame: Value = serde_json::from_str(&line).unwrap();
        if frame.get("id").is_some() {
            assert!(frame.get("result").is_some(), "{method} failed: {frame}");
            return frame["result"].clone();
        }
    }
}

/// Everything the model was sent after the first request.
fn later_requests(model: &MockAnthropicClient) -> String {
    (1..model.calls())
        .filter_map(|n| model.nth_request(n))
        .map(|r| serde_json::to_string(&r).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 场景的工具白名单把 `Bash` 拿掉了，skill 正文里的 `` !`cmd` `` 也就不跑：
/// 文档说它"按一次 `Bash` 调用判"，而这个会话里没有 `Bash` 可调。
#[tokio::test]
async fn audit3_a_skills_inline_command_does_not_run_in_a_scene_without_bash() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("agix");
    let scene_root = global.join("code");
    let work = dir.path().join("work");
    for d in [&scene_root, &work] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(
        global.join("settings.json"),
        json!({ "sandbox": {"dangerously_disable_sandbox": true} }).to_string(),
    )
    .unwrap();
    let skill = global.join("skills").join("probe");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: probe\ndescription: probes\n---\nMarker: !`echo AUD\"\"IT3-RAN`\n",
    )
    .unwrap();

    let sock = dir.path().join("agix.sock");
    let paths =
        StaticDaemonPaths::with_project(scene_root, work.clone()).with_global(global.clone());
    let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
    config.settings.values.memory_enabled = false;
    let model = Arc::new(MockAnthropicClient::new());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(work),
            model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
            transcripts: daemon::Transcripts::Nowhere,
            scenes: vec![Arc::new(no_shell_scene())],
            extra_scenes: vec![NO_SHELL.into()],
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let listener = DaemonServer::bind_unix(&sock).unwrap();
    tokio::spawn(async move {
        let _ = server.serve_unix_listener(listener).await;
    });

    let session = call(&sock, "session.create", json!({"scene": NO_SHELL})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    model.push_turn(tool_round("call-skill", "Skill", json!({"skill": "probe"})));
    model.push_turn(text_round("done"));
    model.push_turn(text_round("done"));
    call(
        &sock,
        "session.run_turn",
        json!({"session_id": session, "message": "use the probe skill"}),
    )
    .await;

    let first = model.nth_request(0).expect("the turn reached the model");
    let advertised: Vec<String> = first.tools.iter().map(|t| t.name.clone()).collect();
    assert!(advertised.iter().any(|t| t == "Skill"), "{advertised:?}");
    assert!(!advertised.iter().any(|t| t == "Bash"), "{advertised:?}");

    let later = later_requests(&model);
    assert!(
        later.contains("not run"),
        "the skill's inline command was not refused: {later}"
    );
    assert!(
        !later.contains("AUDIT3-RAN"),
        "a session without Bash ran the skill's shell command: {later}"
    );
}
