//! Tests: programs.

use super::*;

/// A wired `AgentTool` in an attended scene whose parent asks about
/// every call, with the events it sends up.
pub(super) fn asking_agent_tool() -> (
    Arc<AgentTool>,
    Arc<std::sync::atomic::AtomicUsize>,
    std::path::PathBuf,
    tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
) {
    let (agent_tool, _tools, runs, dir) = wired_agent_tool("code");
    agent_tool.set_scene(Arc::new(scene::code::CodeScene::default()));
    agent_tool.set_parent_permission(Arc::new(PromptsForEverything));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    agent_tool.set_event_sender(runtime::event_bus::EventBus::new(tx));
    agent_tool.set_parent_pending_permissions(Arc::default());
    (Arc::new(agent_tool), runs, dir, rx)
}

pub(super) fn agent_call_ctx(
    cwd: std::path::PathBuf,
    tool_use_id: &str,
    from_code: bool,
) -> ToolContext {
    let mut ctx = ToolContext::for_test(cwd);
    ctx.tool_use_id = tool_use_id.into();
    ctx.from_code = from_code;
    ctx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sub_agent_a_program_started_is_refused_what_it_would_ask_about() {
    let (agent_tool, runs, dir, mut rx) = asking_agent_tool();
    let result = base::tool::Tool::call(
        agent_tool.as_ref(),
        serde_json::json!({"prompt": "go", "description": "probe"}),
        agent_call_ctx(dir.clone(), "toolu_code.1", true),
        ProgressSender::noop("t"),
    )
    .await
    .expect("the sub-agent runs to its answer");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!result.is_error, "{:?}", result.content);
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the call that needed approval did not run"
    );
    while let Ok(event) = rx.try_recv() {
        assert!(
            !matches!(event, AgentEvent::PermissionPrompt { .. }),
            "a program's sub-agent asked the host: {event:?}"
        );
    }

    let (agent_tool, _runs, dir, mut rx) = asking_agent_tool();
    let direct = tokio::spawn(async move {
        base::tool::Tool::call(
            agent_tool.as_ref(),
            serde_json::json!({"prompt": "go", "description": "probe"}),
            agent_call_ctx(dir, "toolu_direct", false),
            ProgressSender::noop("t"),
        )
        .await
    });
    let asked = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = rx.recv().await {
            if matches!(event, AgentEvent::PermissionPrompt { .. }) {
                return true;
            }
        }
        false
    })
    .await;
    direct.abort();
    assert_eq!(
        asked,
        Ok(true),
        "the same sub-agent started directly asks the host"
    );
}

/// Answers with text after a pause, counting how many answer at once.
pub(super) struct SlowTextModel {
    pub(super) running: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) peak: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Model for SlowTextModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.running.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(now, SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        self.running.fetch_sub(1, SeqCst);
        let events: Vec<Result<ModelEvent, base::interface::model::ModelError>> = vec![
            Ok(ModelEvent::TextDelta {
                text: "done".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_programs_fan_out_past_the_cap_waits_for_a_slot_instead_of_failing() {
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let agent_tool = agent_tool_with_model(Arc::new(SlowTextModel {
        running: Arc::default(),
        peak: peak.clone(),
    }))
    .with_subagent_slots(SubagentSlots::new(2));
    let call = |id: &str, from_code: bool| {
        base::tool::Tool::call(
            &agent_tool,
            serde_json::json!({"prompt": "go", "description": id}),
            agent_call_ctx(std::env::temp_dir(), id, from_code),
            ProgressSender::noop("t"),
        )
    };

    let (a, b, c) = futures::join!(call("a", true), call("b", true), call("c", true));
    for r in [a, b, c] {
        let r = r.unwrap();
        assert!(!r.is_error, "{:?}", r.content);
    }
    assert!(peak.load(std::sync::atomic::Ordering::SeqCst) <= 2);

    let (d, e, f) = futures::join!(call("d", false), call("e", false), call("f", false));
    let refused: Vec<_> = [d, e, f]
        .into_iter()
        .map(Result::unwrap)
        .filter(|r| r.is_error)
        .collect();
    assert_eq!(
        refused.len(),
        1,
        "a direct call past the cap is refused at once"
    );
    assert!(
        format!("{:?}", refused[0].content).contains("max_concurrent_subagents"),
        "{:?}",
        refused[0].content
    );
}

#[tokio::test]
async fn a_program_cannot_start_a_background_team_or_worktree_sub_agent() {
    let agent_tool = test_agent_tool();
    for extra in [
        serde_json::json!({"background": true}),
        serde_json::json!({"team_name": "t", "name": "n"}),
        serde_json::json!({"worktree": "w"}),
    ] {
        let mut input = serde_json::json!({"prompt": "go", "description": "d"});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let err = base::tool::Tool::call(
            &agent_tool,
            input,
            agent_call_ctx(std::env::temp_dir(), "toolu_code.1", true),
            ProgressSender::noop("t"),
        )
        .await
        .expect_err("refused");
        assert!(
            err.to_string().contains("not available from a program"),
            "{extra}: {err}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_background_sub_agent_ends_its_task_failed_and_says_so() {
    let agent_tool = agent_tool_with_model(Arc::new(PanickingModel));
    let ctx = ToolContext::for_test(std::env::temp_dir());
    let session = ctx.session.clone();
    let started = base::tool::Tool::call(
        &agent_tool,
        serde_json::json!({"prompt": "go", "description": "bg", "background": true}),
        ctx,
        ProgressSender::noop("t"),
    )
    .await
    .unwrap();
    let ToolResultContent::Text(text) = &started.content else {
        panic!("{:?}", started.content);
    };
    let task_id = text
        .split("task_id: ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .expect("a task id");
    let task = session.find_running_task(task_id).expect("registered");
    tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
        .await
        .expect("the task ends");
    assert!(
        matches!(
            &*task.status.lock().unwrap(),
            base::context::RunningStatus::Failed(m) if m.contains("model exploded")
        ),
        "{:?}",
        task.status.lock().unwrap()
    );
    assert!(
        session
            .take_notices()
            .iter()
            .any(|n| n.contains("model exploded")),
        "the parent is told"
    );
}
