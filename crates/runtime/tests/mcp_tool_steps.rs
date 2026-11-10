//! What a tool step asks of the session's MCP servers: whether any of them
//! announced a change, never a fresh listing of every server's tools.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base::context::McpServerInstruction;
use base::interface::mcp_servers::{McpPromptCommand, McpPromptInvocation, McpServers};
use base::interface::model::{
    Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef,
};
use base::prompt::PromptBlock;
use base::settings::{PathSettings, Settings};
use base::tool::{InMemoryToolRegistry, Tool};
use runtime::agent::Builder;
use runtime::module::{Module, ModuleError, SessionContext};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Asked {
    full_listings: AtomicUsize,
    change_checks: AtomicUsize,
}

struct CountingMcp(Arc<Asked>);

#[async_trait]
impl McpServers for CountingMcp {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    async fn refresh_tools(&mut self) {
        self.0.full_listings.fetch_add(1, Ordering::SeqCst);
    }
    async fn refresh_if_changed(&mut self) -> bool {
        self.0.change_checks.fetch_add(1, Ordering::SeqCst);
        false
    }
    fn instructions(&self) -> Vec<McpServerInstruction> {
        Vec::new()
    }
    async fn resolve_resource_refs(&self, _: &str) -> Option<String> {
        None
    }
    fn prompt_commands(&self) -> Vec<McpPromptCommand> {
        Vec::new()
    }
    fn has_prompt_command(&self, _: &str) -> bool {
        false
    }
    async fn invoke_prompt_command(&self, _: &str, _: &str) -> McpPromptInvocation {
        McpPromptInvocation {
            text: String::new(),
            is_error: true,
        }
    }
}

struct CountingModule(Mutex<Option<CountingMcp>>);

impl Module for CountingModule {
    fn id(&self) -> &'static str {
        "counting-mcp"
    }
    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        if let Some(mcp) = self.0.lock().unwrap().take() {
            cx.provide_mcp(Box::new(mcp));
        }
        Ok(())
    }
}

/// Calls `TodoWrite` on its first two replies, then answers.
struct TwoToolSteps(AtomicUsize);

#[async_trait]
impl Model for TwoToolSteps {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _: Vec<PromptBlock>,
        _: Vec<ToolDef>,
        _: Vec<ModelMessage>,
        _: StreamParams,
        _: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let events: Vec<Result<ModelEvent, ModelError>> = if self.0.fetch_add(1, Ordering::SeqCst)
            < 2
        {
            vec![
                Ok(ModelEvent::ToolUse {
                    id: format!("call-{}", self.0.load(Ordering::SeqCst)),
                    name: "TodoWrite".into(),
                    input: serde_json::json!({"todos": [{"content": "look", "status": "pending", "activeForm": "Looking"}]}),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Default::default(),
                }),
            ]
        } else {
            vec![
                Ok(ModelEvent::TextDelta {
                    text: "done".into(),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ]
        };
        Ok(Box::new(futures::stream::iter(events)))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_step_does_not_relist_tools_without_an_announcement() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::defaults_for("test-model");
    settings.paths = PathSettings {
        user_data_dir: dir.path().to_path_buf(),
        global_data_dir: dir.path().to_path_buf(),
        workspace: base::settings::Workspace::project_of(dir.path()),
        local_data_dir: dir.path().to_path_buf(),
        scope: "code".into(),
    };
    settings.memory_enabled = false;
    settings.telemetry_enabled = false;

    let asked = Arc::new(Asked::default());
    let model = Arc::new(TwoToolSteps(AtomicUsize::new(0)));
    let mut agent = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(model.clone())
        .settings(Arc::new(settings))
        .tools({
            let t = Arc::new(InMemoryToolRegistry::new());
            tools::register_builtin_tools(&t);
            t
        })
        .module(Arc::new(CountingModule(Mutex::new(Some(CountingMcp(
            asked.clone(),
        ))))))
        .skip_warmup(true)
        .build()
        .expect("build should succeed")
        .0;

    agent
        .run_turn(
            "find the docs".into(),
            "t1".into(),
            CancellationToken::new(),
        )
        .await
        .expect("the turn completes");

    assert_eq!(
        model.0.load(Ordering::SeqCst),
        3,
        "two tool steps, then the answer"
    );
    assert_eq!(asked.full_listings.load(Ordering::SeqCst), 0);
    assert!(asked.change_checks.load(Ordering::SeqCst) >= 2);
}
