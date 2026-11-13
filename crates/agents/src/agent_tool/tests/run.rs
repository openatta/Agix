//! Tests: run.

use super::*;

/// A `DelegateMode::Background` delegate (the `context: fork` +
/// `background: true` skill path) must return a task id immediately — without waiting for the spawned
/// sub-agent turn to finish — and that id must already be registered on
/// the *caller's* session (the real `session.register_task` /
/// `find_running_task` machinery `TaskOutput`/`TaskStop` also use), in
/// `Running` status. Driving a full turn to actual completion needs a
/// working mock `Model` (`DummyModel` only covers settings-derivation
/// tests elsewhere in this file) — out of scope here: this test is
/// about the background-spawn plumbing (does it return without
/// blocking, does it register under the id it hands back), not turn
/// execution.
#[tokio::test]
async fn spawn_background_returns_immediately_with_a_registered_task() {
    let tools = Arc::new(InMemoryToolRegistry::new());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
    let agent_tool = TestParent {
        config,
        tools: tools.clone(),
        template: tools.clone(),
        ..TestParent::new(model)
    }
    .agent_tool();
    let session = agent_tool.parent_state();

    let start = std::time::Instant::now();
    let task_id = agent_tool
        .start_in_background_for_test("do the thing", &session)
        .await
        .expect("a background spawn itself should not error — it only registers the task");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "a background spawn must return immediately, not block on the sub-agent turn"
    );

    let task = session
        .find_running_task(&task_id)
        .expect("spawn_background must register the task under the id it returns");
    assert!(matches!(
        *task.status.lock().unwrap_or_else(|e| e.into_inner()),
        base::context::RunningStatus::Running
    ));
}

/// A `.agix/agents/*.md` file edited after `with_parent_tools()` takes
/// effect on the running `AgentTool` without rebuilding the `Agent`. This
/// exercises the real `notify` watcher end to end (not just the merge
/// logic in isolation), the same way
/// `skills::manager::editing_a_watched_skill_file_is_picked_up_without_a_manual_reload`
/// does for Skills.
#[test]
fn editing_an_agent_type_file_is_picked_up_without_rebuilding_agent_tool() {
    let dir = std::env::temp_dir().join(format!(
        "agix-agent-watch-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_agent_md(
        &dir,
        "watched-reviewer.md",
        "---\ndescription: original\nmodel: claude-sonnet-4-6\n---\noriginal body",
    );

    let tools = Arc::new(InMemoryToolRegistry::new());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
    let agent_tool = TestParent {
        config,
        tools: tools.clone(),
        template: tools,
        agent_dirs: vec![(dir.clone(), AgentTypeSource::User)],
        ..TestParent::new(model)
    }
    .agent_tool();

    assert_eq!(
        agent_tool
            .delegator
            .agent_type_def("watched-reviewer")
            .unwrap()
            .model
            .as_deref(),
        Some("claude-sonnet-4-6"),
        "sanity check on the initial build-time load"
    );

    write_agent_md(
        &dir,
        "watched-reviewer.md",
        "---\ndescription: updated\nmodel: claude-opus-4-8\n---\nupdated body",
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut picked_up = false;
    while std::time::Instant::now() < deadline {
        if agent_tool
            .delegator
            .agent_type_def("watched-reviewer")
            .and_then(|d| d.model)
            .as_deref()
            == Some("claude-opus-4-8")
        {
            picked_up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        picked_up,
        "watcher never reported the agent-type file change within 5s"
    );
}

/// `run_sub` must return when the sub-agent's model call errors —
/// `DummyModel` always errors. Wrapped in a bounded timeout so a hang
/// fails this test in 10s instead of hanging the whole test binary.
#[tokio::test]
async fn run_sub_returns_promptly_on_a_plain_model_error_instead_of_hanging() {
    let agent_tool = test_agent_tool();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        agent_tool.wait_for(
            "do the thing",
            std::path::PathBuf::from("/tmp"),
            tokio_util::sync::CancellationToken::new(),
            None,
            None,
        ),
    )
    .await
    .expect(
        "run_sub must return within 10s on a plain model error, not hang \
         indefinitely waiting for a TurnComplete event that never comes",
    );
    assert!(
        result.is_err(),
        "DummyModel always errors on .stream(); run_sub should surface that as Err"
    );
}

pub(super) fn agent_tool_with_model(model: Arc<dyn Model>) -> AgentTool {
    TestParent::new(model).agent_tool()
}

pub(super) async fn run_sub_with(agent_tool: &AgentTool) -> Result<String, base::error::ToolError> {
    agent_tool
        .wait_for(
            "do the thing",
            std::path::PathBuf::from("/tmp"),
            tokio_util::sync::CancellationToken::new(),
            None,
            None,
        )
        .await
}

pub(super) struct PanickingModel;
#[async_trait]
impl Model for PanickingModel {
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
        panic!("model exploded");
    }
}

#[tokio::test]
async fn a_panicking_sub_agent_becomes_an_error_for_its_parent() {
    let agent_tool = agent_tool_with_model(Arc::new(PanickingModel));
    let err = run_sub_with(&agent_tool).await.unwrap_err();
    assert!(err.to_string().contains("model exploded"), "{err}");
}

pub(super) struct SetOnDrop(pub(super) Arc<std::sync::atomic::AtomicBool>);
impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(super) struct HangingModel {
    pub(super) started: Arc<tokio::sync::Notify>,
    pub(super) dropped: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait]
impl Model for HangingModel {
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
        let _guard = SetOnDrop(self.dropped.clone());
        self.started.notify_one();
        std::future::pending().await
    }
}

/// Never answers, and gives up only when its call is cancelled.
pub(super) struct AnswersOnlyCancellation {
    pub(super) started: Arc<tokio::sync::Notify>,
    pub(super) dropped: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait]
impl Model for AnswersOnlyCancellation {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let _guard = SetOnDrop(self.dropped.clone());
        self.started.notify_one();
        cancel.cancelled().await;
        Err(base::interface::model::ModelError::cancelled())
    }
}
