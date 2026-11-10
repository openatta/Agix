//! Tests: what every spawn path — foreground, background, team member —
//! gives a delegate alike.

use super::*;

/// `(parent_session_id, agent_type)` of one model call.
type Lineage = (Option<String>, Option<String>);

/// Answers at once, noting the lineage each turn's call carries.
struct LineageModel {
    seen: Arc<std::sync::Mutex<Vec<Lineage>>>,
}

#[async_trait]
impl Model for LineageModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        if let Some(origin) = params
            .origin
            .filter(|o| matches!(o.kind, base::interface::model::CallKind::Turn { .. }))
        {
            self.seen
                .lock()
                .unwrap()
                .push((origin.parent_session_id, origin.agent_type));
        }
        Ok(Box::new(futures::stream::iter(vec![
            Ok(ModelEvent::TextDelta {
                text: "done".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ])))
    }
}

fn parent_ctx() -> ToolContext {
    let mut ctx = platform::tool_context_for_test(std::env::temp_dir());
    ctx.session_id = "parent-session-1".into();
    ctx.turn_no = 3;
    ctx.tool_use_id = "toolu_bg".into();
    ctx
}

async fn start_in_background(agent_tool: &AgentTool, ctx: ToolContext) {
    let session = ctx.session.clone();
    base::tool::Tool::call(
        agent_tool,
        serde_json::json!({
            "prompt": "look", "description": "look",
            "subagent_type": "explore", "background": true
        }),
        ctx,
        ProgressSender::noop("t"),
    )
    .await
    .expect("the background sub-agent is started");
    let task = session.task_infos().pop().expect("a registered task");
    let task = session
        .find_running_task(task["task_id"].as_str().unwrap())
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
        .await
        .expect("the sub-agent finishes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_sub_agent_and_a_team_member_record_who_spawned_them_and_as_what() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent_tool = agent_tool_with_model(Arc::new(LineageModel { seen: seen.clone() }));
    agent_tool.set_parent_session_id("parent-session-1".into());
    agent_tool.set_team_registry(Arc::new(team::registry::TeamRegistry::new()));
    let expected = (
        Some("parent-session-1".to_string()),
        Some("explore".to_string()),
    );

    start_in_background(&agent_tool, parent_ctx()).await;
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        std::slice::from_ref(&expected)
    );

    seen.lock().unwrap().clear();
    let session = Arc::new(base::context::SessionState::new(std::env::temp_dir()));
    agent_tool
        .spawn_or_message_team_member(
            "team".into(),
            "member".into(),
            "work".into(),
            Some("explore".into()),
            std::env::temp_dir(),
            session,
            None,
        )
        .await
        .expect("the member is spawned");
    wait_until_idle(&agent_tool, &team_key("team", "member")).await;
    assert_eq!(seen.lock().unwrap().as_slice(), [expected]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_sub_agents_progress_names_the_call_that_started_it() {
    let agent_tool = agent_tool_with_model(Arc::new(LineageModel {
        seen: Default::default(),
    }));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent_tool.set_event_sender(runtime::event_bus::EventBus::new(tx));

    start_in_background(&agent_tool, parent_ctx()).await;

    let mut progress = 0;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::SubagentProgress {
            parent_session_id,
            parent_turn,
            parent_tool_use_id,
            ..
        } = event
        {
            progress += 1;
            assert_eq!(parent_session_id, "parent-session-1");
            assert_eq!(parent_turn, 3);
            assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_bg"));
        }
    }
    assert!(progress > 0, "the sub-agent's events reach the parent");
}

/// `Probe`, leaving the decision to the session's mode.
struct AskingProbe {
    runs: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl base::tool::Tool for AskingProbe {
    fn name(&self) -> &str {
        "Probe"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({})
    }
    async fn check_permissions(
        &self,
        _input: &Value,
        _ctx: &ToolContext,
    ) -> base::tool::PermissionDecision {
        base::tool::PermissionDecision::ask("run the probe?")
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(base::tool::ToolResult {
            content: ToolResultContent::Text("probed".into()),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }
}

/// With no parent permission wired, a `TeamCreate` member that asks for
/// more than its parent has is judged in the parent's mode, as a persistent
/// member is.
#[tokio::test]
async fn a_team_create_member_is_judged_in_its_mode_held_to_the_parents() {
    use base::interface::agent_spawner::{AgentSpawner as _, SpawnMode, SpawnRequest};
    use base::interface::settings::PermissionMode;
    for (parent_mode, probe_runs) in [
        (PermissionMode::Default, 0),
        (PermissionMode::BypassPermissions, 1),
    ] {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = Arc::new(InMemoryToolRegistry::new());
        let (mut settings, dir) = parent_settings_for_test("code");
        settings.permission_mode = parent_mode;
        let agent_tool = AgentTool::with_parent_tools(
            Arc::new(ProbeThenTextModel {
                calls: Default::default(),
            }),
            Arc::new(EngineConfig::defaults_for("test-model")),
            tools.clone(),
            tools,
            &[],
            &[],
        )
        .with_settings(Arc::new(settings));
        // Not interactive: what would be asked is refused instead.
        agent_tool.set_scene(Arc::new(super::lifecycle::SceneWithProbe));

        crate::spawner::RuntimeAgentSpawner::new(Arc::new(agent_tool))
            .spawn(SpawnRequest {
                mode: SpawnMode::TeamMember {
                    extra_tools: vec![Arc::new(AskingProbe { runs: runs.clone() })],
                    permission_mode: Some(PermissionMode::BypassPermissions),
                    seat: None,
                },
                ..SpawnRequest::new(
                    "probe",
                    dir.clone(),
                    tokio_util::sync::CancellationToken::new(),
                )
            })
            .await
            .expect("the member runs");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            probe_runs,
            "parent in {parent_mode:?}"
        );
    }
}
