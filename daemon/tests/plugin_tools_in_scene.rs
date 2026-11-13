//! A plugin's tools in a scene with a tool whitelist: the ones the plugin
//! admits with `Registrar::admit_tools` reach the model and can be called,
//! the ones it does not are left out like any tool the whitelist omits.

use std::sync::Arc;
use std::time::Duration;

use base::interface::scene::{SceneSpec, ToolSurface};
use base::settings::{PermissionAction, PermissionRule};
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use runtime::plugin::{Plugin, PluginSession, Registrar};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const SCENE: &str = "read-only-host";
const ADMITTED: &str = "demo__x";
const NOT_ADMITTED: &str = "demo__y";

/// A scene whose whitelist names one built-in tool.
fn read_only_scene() -> SceneSpec {
    let mini = scene::mini::spec();
    let mut s = SceneSpec::new(SCENE, "Read only", mini.prompt);
    s.tools = ToolSurface::allowing(&["Read"]);
    s.token_budget = mini.token_budget;
    s
}

struct Echo(&'static str);

#[async_trait::async_trait]
impl base::tool::Tool for Echo {
    fn name(&self) -> &str {
        self.0
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }

    fn description(&self) -> &str {
        "Answers with its own name."
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
        json!({"type": "object"})
    }
    async fn call(
        &self,
        _: serde_json::Value,
        _: base::tool::ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult::text(format!("{} ran", self.0)))
    }
}

struct Demo;

impl Plugin for Demo {
    fn name(&self) -> &str {
        "demo"
    }
    fn register(&self, _: &PluginSession<'_>, r: &mut Registrar) {
        r.tool(Arc::new(Echo(ADMITTED)))
            .tool(Arc::new(Echo(NOT_ADMITTED)))
            .admit_tools(ADMITTED);
    }
}

fn tool_round(id: &str, name: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: name.into(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: "{}".into(),
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
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::EndTurn),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

async fn call(
    sock: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
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
            assert!(frame.get("result").is_some(), "{method} failed: {frame}");
            return frame["result"].clone();
        }
    }
}

/// The requests that carried a tool list: the turn's own rounds, not the
/// memory extraction that may follow it.
fn tool_lists(model: &MockAnthropicClient) -> Vec<Vec<String>> {
    (0..model.calls())
        .filter_map(|n| model.nth_request(n))
        .map(|r| r.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>())
        .filter(|names| !names.is_empty())
        .collect()
}

fn tool_result_of(model: &MockAnthropicClient, id: &str) -> serde_json::Value {
    (0..model.calls())
        .filter_map(|n| model.nth_request(n))
        .map(|r| serde_json::to_value(r).unwrap())
        .find_map(|request| {
            request["messages"]
                .as_array()?
                .iter()
                .filter_map(|m| m["content"].as_array())
                .flatten()
                .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == id)
                .cloned()
        })
        .unwrap_or_else(|| panic!("no request carries a tool_result for {id}"))
}

#[tokio::test]
async fn an_admitted_plugin_tool_passes_the_scene_whitelist_and_the_rest_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("admit.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.settings.values.memory_enabled = false;
    config.settings.rules = [ADMITTED, NOT_ADMITTED]
        .map(|tool| {
            base::settings::LoadedRule::new(
                PermissionRule {
                    tool: tool.into(),
                    action: PermissionAction::Allow,
                },
                base::interface::config_source::Tier::Project,
                false,
            )
        })
        .to_vec();
    let model = Arc::new(MockAnthropicClient::new());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(cwd),
            model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
            scenes: vec![Arc::new(read_only_scene())],
            extra_scenes: vec![SCENE.into()],
            plugins: vec![Arc::new(Demo)],
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
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

    let session = call(&sock, "session.create", json!({"scene": SCENE})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    model.push_turn(tool_round("call-x", ADMITTED));
    model.push_turn(tool_round("call-y", NOT_ADMITTED));
    model.push_turn(text_round("done"));
    call(
        &sock,
        "session.run_turn",
        json!({"session_id": session, "message": "use x, then y"}),
    )
    .await;

    let lists = tool_lists(&model);
    assert_eq!(lists.len(), 3, "{lists:?}");
    let tools = &lists[0];
    assert!(tools.iter().any(|t| t == "Read"), "{tools:?}");
    assert!(tools.iter().any(|t| t == ADMITTED), "{tools:?}");
    assert!(!tools.iter().any(|t| t == NOT_ADMITTED), "{tools:?}");
    assert!(
        lists.iter().all(|l| l == tools),
        "every round advertises the same set: {lists:?}"
    );

    let x = tool_result_of(&model, "call-x");
    assert_ne!(x["is_error"], true, "{x}");
    assert!(x.to_string().contains("demo__x ran"), "{x}");

    let y = tool_result_of(&model, "call-y");
    assert_eq!(y["is_error"], true, "{y}");
    assert!(!y.to_string().contains("demo__y ran"), "{y}");
}
