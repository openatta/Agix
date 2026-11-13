//! MCP as the kernel sees it: a session assembled with [`McpModule`] offers
//! the servers' tools, runs their prompts as slash commands, inlines their
//! resources, and routes their elicitation URLs to the session's hooks.
//!
//! [`McpModule`]: mcp::McpModule

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use base::interface::model::{Model, ModelContentBlock, ModelMessage};
use base::interface::scene::SceneSpec;
use base::settings::*;
use mcp::manager::McpManager;
use runtime::agent::Builder;

struct DummyModel;

#[async_trait]
impl Model for DummyModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        Err(base::interface::model::ModelError::protocol(
            "DummyModel does not stream",
        ))
    }
}

/// Answers every request directly and keeps the text of every message it
/// was sent.
struct RecordingAnswerModel {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Model for RecordingAnswerModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let mut seen = self.seen.lock().unwrap();
        for m in &messages {
            for b in &m.content {
                if let ModelContentBlock::Text { text } = b {
                    seen.push(text.clone());
                }
            }
        }
        let events = vec![
            Ok(base::interface::model::ModelEvent::TextDelta {
                text: "answering directly".into(),
            }),
            Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

fn recall_test_settings(root: &std::path::Path) -> base::settings::LoadedSettings {
    base::settings::LoadedSettings::new(
        Settings {
            model: ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: "https://api.example.com".into(),
                auth_token: "test".into(),
                model_name: "test-model".into(),
                max_tokens: 4096,
                thinking_mode: ThinkingMode::Off,
                effort: None,
                fallback_model: None,
            },
            execution: Default::default(),
            compaction: Default::default(),
            sandbox: Default::default(),
            plugins: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            path_style: Default::default(),
            permission_mode: PermissionMode::Default,
            allow_client_permission_override: false,
            telemetry_enabled: false,
            hooks_config: None,
            project_trust: Default::default(),
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
            language: None,
            scripts: Vec::new(),
        },
        PathSettings {
            user_data_dir: root.join("user-data"),
            global_data_dir: root.join("global-data"),
            workspace: base::settings::Workspace::project_of(&root.join("local-data")),
            local_data_dir: root.join("local-data"),
            scope: "code".into(),
        },
    )
}

fn test_settings() -> base::settings::LoadedSettings {
    recall_test_settings(&std::env::temp_dir().join("agix-mcp-module-tests"))
}

// `McpManager::all_prompts` / `execute_prompt` / `list_resources` /
// `read_resource` must be reachable from a session, not only MCP *tools*.
// These drive a whole turn through `run_turn`, so they cover the
// registration in `Builder::build`, the slash-command interception, and
// the pre-push `@`-reference resolution together.

#[tokio::test]
async fn build_registers_mcp_tool_adapters_into_the_executable_tool_registry() {
    // `build_tool_defs()` (`turn/request.rs`) advertises every `mcp.tool_adapters()`
    // entry to the model as a callable tool, so `Builder::build()` must
    // register each per-tool adapter, not just the 3 generic List/Read/Dispatch
    // MCP tools — `turn/tool_exec.rs::execute_tool_inner` looks up `ctx.tools`, a clone
    // of this same registry, and would answer "Tool not found".
    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<SceneSpec> = Arc::new(scene::code::spec(scene::code::Interaction::Interactive));

    let mock_client = Arc::new(mcp::client::MockMcpClient::new(
        "test-server",
        vec![mcp::client::McpToolMeta {
            name: "do-thing".into(),
            description: Some("does a thing".into()),
            input_schema: serde_json::json!({"type": "object"}),
            hints: Default::default(),
        }],
    ));
    let mut mcp_manager = McpManager::from_clients(vec![mock_client]).await;
    mcp_manager.refresh_tools().await;
    assert_eq!(
        mcp_manager.tool_adapters().len(),
        1,
        "sanity check: mock client should have produced one adapter"
    );

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .module(Arc::new(mcp::McpModule::new(mcp_manager)))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert!(
        agent.tools().get("mcp__test-server__do-thing").is_some(),
        "MCP tool adapter should be registered in the executable tool registry, found: {:?}",
        agent.tools().names()
    );
}

/// An MCP tool result containing an elicitation URL must reach the hook
/// system. This exercises the real path end to end: an MCP tool call
/// returns a result with an `elicitation://` URL in it, and a configured
/// `Elicitation` hook must actually fire.
#[tokio::test]
async fn build_wires_mcp_elicitation_urls_to_the_elicitation_hook() {
    // Observed through an injected hook executor, in this process. The
    // previous version configured a `command` hook that appended to a file
    // and polled for it, which measured how long the machine took to fork
    // a shell under a full `cargo test --workspace` — not what this test is
    // about, and it timed out at sixty seconds saying so. An `http` hook is
    // not an option either: the SSRF guard blocks loopback, correctly.
    struct RecordingPromptExecutor {
        fired: tokio::sync::mpsc::Sender<serde_json::Value>,
    }

    #[async_trait::async_trait]
    impl hooks::runner::PromptHookExecutor for RecordingPromptExecutor {
        async fn execute(
            &self,
            _prompt: &str,
            _model: Option<&str>,
            payload: &hooks::HookInput,
            _stop: &tokio_util::sync::CancellationToken,
        ) -> Result<String, String> {
            let _ = self
                .fired
                .send(payload.tool_input.clone().unwrap_or_default())
                .await;
            Ok("{}".to_string())
        }
    }

    let (fired_tx, mut fired_rx) = tokio::sync::mpsc::channel(1);
    let mut hooks_settings: hooks::HooksSettings = Default::default();
    hooks_settings.insert(
        hooks::HookEvent::Elicitation,
        vec![hooks::config::HookConfig::Prompt {
            prompt: "an elicitation happened".into(),
            timeout: None,
            model: None,
            if_pattern: None,
            on_error: hooks::OnError::Allow,
            may_approve: false,
        }],
    );
    let runner = Arc::new(
        hooks::HookRunner::new(hooks_settings)
            .with_prompt_executor(Arc::new(RecordingPromptExecutor { fired: fired_tx })),
    );

    let dir = tempfile::tempdir().unwrap();
    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<SceneSpec> = Arc::new(scene::code::spec(scene::code::Interaction::Interactive));

    let mock_client = Arc::new(mcp::client::MockMcpClient::new(
        "test-server",
        vec![mcp::client::McpToolMeta {
            name: "authorize".into(),
            description: Some("needs out-of-band authorization".into()),
            input_schema: serde_json::json!({"type": "object"}),
            hints: Default::default(),
        }],
    ));
    mock_client.push_response(
        "authorize",
        mcp::client::McpCallResult {
            content: vec![mcp::client::McpContent::Text(
                "Please authorize at elicitation://example.com/auth/123".into(),
            )],
            is_error: false,
            meta: None,
        },
    );
    let mut mcp_manager = McpManager::from_clients(vec![mock_client]).await;
    mcp_manager.refresh_tools().await;

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .hooks(runner)
        .module(Arc::new(mcp::McpModule::new(mcp_manager)))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert!(agent.hooks().has_hooks_for(hooks::HookEvent::Elicitation));

    let tool = agent
        .tools()
        .get("mcp__test-server__authorize")
        .expect("MCP tool adapter registered");
    let ctx = platform::tool_context_for_test(dir.path().to_path_buf());
    let progress = base::tool::ProgressSender::noop("test-tool-use");
    tool.call(serde_json::json!({}), ctx, progress)
        .await
        .expect("mock tool call succeeds");

    // The callback dispatches through `tokio::spawn`, so this arrives
    // after `call` returns. Waiting on the dispatch itself means a slow
    // machine makes the test slower and never makes it fail.
    let payload = tokio::time::timeout(std::time::Duration::from_secs(30), fired_rx.recv())
        .await
        .expect(
            "an MCP tool result containing an elicitation URL should have fired the \
             Elicitation hook",
        )
        .expect("the executor sends before returning");
    assert_eq!(
        payload["url"],
        serde_json::json!("elicitation://example.com/auth/123"),
        "the hook must be told which URL, got: {payload}"
    );
    assert_eq!(payload["server_name"], serde_json::json!("test-server"));
}

fn prompt_arg(name: &str, required: bool) -> mcp::client::McpPromptArg {
    mcp::client::McpPromptArg {
        name: name.into(),
        description: None,
        required: Some(required),
    }
}

/// A mock MCP server exposing one prompt (two required args) and one
/// readable resource.
async fn mcp_manager_for_turn_tests(down: bool) -> McpManager {
    let client = Arc::new(
        mcp::client::MockMcpClient::new("github", Vec::new())
            .with_prompts(vec![mcp::client::McpPromptMeta {
                name: "review_pr".into(),
                description: Some("Review a pull request".into()),
                arguments: vec![prompt_arg("repo", true), prompt_arg("pr", true)],
            }])
            .with_prompt_body(
                "review_pr",
                "PROMPT-BODY: review PR {pr} in {repo} thoroughly.",
            )
            .with_resource_text("doc://guide/install", "RESOURCE-BODY: run cargo install."),
    );
    let mut mgr = McpManager::from_clients(vec![client]).await;
    mgr.refresh_prompts().await;
    if down {
        // Collect the prompt list while the server is up, *then* knock it
        // over — that is what a mid-session disconnect actually looks
        // like, and the case where the command is still offered but the
        // invocation must degrade rather than fail.
        let prompts = mgr.all_prompts().to_vec();
        let dead = Arc::new(mcp::client::MockMcpClient::new("github", Vec::new()).down());
        let mut dead_mgr = McpManager::from_clients(vec![dead]).await;
        dead_mgr.set_prompts_for_test(prompts);
        return dead_mgr;
    }
    mgr
}

async fn run_one_turn(
    mcp_manager: McpManager,
    input: &str,
) -> (runtime::turn::TurnOutcome, String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model: Arc<dyn base::interface::model::Model> =
        Arc::new(RecordingAnswerModel { seen: seen.clone() });
    let scene: Arc<base::interface::scene::SceneSpec> =
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive));
    let mut settings = recall_test_settings(tmp.path());
    // Keep the transcript to just what this test injects.
    settings.values.memory_enabled = false;

    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(settings))
        .module(Arc::new(mcp::McpModule::new(mcp_manager)))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let outcome = agent
        .run_turn(
            input.into(),
            "t1".into(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("a broken MCP server must never fail the turn");
    let transcript = seen.lock().unwrap().join("\n");
    (outcome, transcript, tmp)
}

#[tokio::test]
async fn mcp_prompt_is_registered_as_a_slash_command() {
    let tmp = tempfile::tempdir().unwrap();
    let model: Arc<dyn base::interface::model::Model> = Arc::new(RecordingAnswerModel {
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let scene: Arc<base::interface::scene::SceneSpec> =
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive));
    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .module(Arc::new(mcp::McpModule::new(
            mcp_manager_for_turn_tests(false).await,
        )))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let cmd = agent
        .commands()
        .resolve("mcp__github__review_pr")
        .expect("MCP prompt should be a slash command");
    assert_eq!(cmd.description(), "Review a pull request");
    assert!(agent
        .commands()
        .list()
        .iter()
        .any(|(n, _)| *n == "mcp__github__review_pr"));
}

#[tokio::test]
async fn invoking_an_mcp_prompt_command_injects_the_servers_messages() {
    let (outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(false).await,
        "/mcp__github__review_pr repo=acme/widgets pr=42",
    )
    .await;
    assert_eq!(outcome.stop_reason, "end_turn");
    assert!(
        transcript.contains("PROMPT-BODY: review PR 42 in acme/widgets thoroughly."),
        "the server's prompt messages never reached the model: {transcript}"
    );
}

#[tokio::test]
async fn a_missing_required_prompt_argument_surfaces_a_clear_error() {
    let (outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(false).await,
        "/mcp__github__review_pr repo=acme/widgets",
    )
    .await;
    assert_eq!(outcome.stop_reason, "end_turn", "the turn must not fail");
    assert!(
        transcript.contains("missing required argument: pr"),
        "no clear argument error reached the model: {transcript}"
    );
    assert!(
        !transcript.contains("PROMPT-BODY"),
        "the prompt must not have been called at all: {transcript}"
    );
}

#[tokio::test]
async fn a_down_mcp_server_degrades_to_visible_text_without_failing_the_turn() {
    let (outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(true).await,
        "/mcp__github__review_pr repo=acme/widgets pr=42",
    )
    .await;
    assert_eq!(outcome.stop_reason, "end_turn");
    assert!(
        transcript.contains("could not be run") && transcript.contains("is not connected"),
        "the failure was not surfaced as text the model can explain: {transcript}"
    );
}

#[tokio::test]
async fn an_at_reference_inlines_the_resource_into_the_user_message() {
    let (_outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(false).await,
        "summarise @github:doc://guide/install for me",
    )
    .await;
    assert!(
        transcript.contains("RESOURCE-BODY: run cargo install."),
        "the resource was never inlined: {transcript}"
    );
    assert!(transcript.contains("<mcp-resource server=\"github\""));
    // The original reference stays put so the model can correlate.
    assert!(transcript.contains("summarise @github:doc://guide/install for me"));
}

#[tokio::test]
async fn at_signs_that_are_not_resource_references_never_trigger_a_fetch() {
    for text in [
        "email me at xbitshans@gmail.com when done",
        "why does @Component({ selector: 'x' }) fail here?",
        "fix fn f<'a>(x: &'a str) -> &'a str { x }",
        "run npm i @anthropic-ai/sdk @types/node",
    ] {
        let (_outcome, transcript, _tmp) =
            run_one_turn(mcp_manager_for_turn_tests(false).await, text).await;
        assert!(
            !transcript.contains("<mcp-resources>"),
            "false-positive `@` fetch on {text:?}: {transcript}"
        );
    }
}

#[tokio::test]
async fn an_unresolvable_at_reference_degrades_to_visible_text() {
    let (outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(false).await,
        "read @github:doc://guide/does-not-exist",
    )
    .await;
    assert_eq!(outcome.stop_reason, "end_turn", "the turn must not fail");
    assert!(
        transcript.contains("error=\"true\"") && transcript.contains("no such resource"),
        "the bad URI was not surfaced as visible text: {transcript}"
    );
}
