//! Tests: lifecycle.

use super::*;

pub(super) fn hanging_team_parent(
    cap: usize,
) -> (
    TestParent,
    Arc<tokio::sync::Notify>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut config = EngineConfig::defaults_for("test-model");
    config.team_max_persistent_members = cap;
    let parent = TestParent {
        config: Arc::new(config),
        ..TestParent::new(Arc::new(AnswersOnlyCancellation {
            started: started.clone(),
            dropped: dropped.clone(),
        }))
    }
    .with_team();
    (parent, started, dropped)
}

pub(super) fn hanging_team_tool(
    cap: usize,
) -> (
    AgentTool,
    Arc<tokio::sync::Notify>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let (parent, started, dropped) = hanging_team_parent(cap);
    (parent.agent_tool(), started, dropped)
}

pub(super) async fn message_member(
    agent_tool: &AgentTool,
    session: &Arc<base::context::SessionState>,
    name: &str,
) -> Result<String, base::error::ToolError> {
    agent_tool
        .spawn_or_message_team_member(
            "team".into(),
            name.into(),
            "work".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_member_past_the_cap_is_refused_while_every_member_is_busy() {
    let (agent_tool, started, _) = hanging_team_tool(1);
    let session = agent_tool.parent_state();
    message_member(&agent_tool, &session, "a").await.unwrap();
    started.notified().await;

    let refused = message_member(&agent_tool, &session, "b")
        .await
        .expect_err("the only place is taken by a busy member");
    assert!(
        refused
            .to_string()
            .contains("team member limit reached: 1 persistent member is busy"),
        "{refused}"
    );
    assert!(refused.to_string().contains("Message it,"), "{refused}");
    assert_eq!(
        agent_tool
            .delegator
            .members
            .live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        1
    );
    message_member(&agent_tool, &session, "a")
        .await
        .expect("a member already there can still be messaged");
}

#[tokio::test]
async fn a_stage_member_is_in_its_sessions_telemetry_by_session_and_stage() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let agent_tool = TestParent {
        host: Some(runtime::agent::HostContracts {
            telemetry: TelemetryHandle::new(tx),
            ..host_contracts()
        }),
        ..test_parent()
    }
    .agent_tool();
    let _ = agent_tool
        .run_stage_member(
            "work",
            tokio_util::sync::CancellationToken::new(),
            None,
            Vec::new(),
        )
        .await;

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(serde_json::to_value(event).unwrap());
    }
    let of = |kind: &str| {
        events
            .iter()
            .find(|e| e["type"] == kind)
            .unwrap_or_else(|| panic!("no {kind} in {events:?}"))
            .clone()
    };
    let spawned = of("agent_spawned");
    let completed = of("agent_completed");
    assert_eq!(spawned["stage_name"], "stage", "{spawned}");
    assert_eq!(spawned["session_id"], agent_tool.delegator.env.parent.id);
    let member = spawned["agent_id"].as_str().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["session_id"] == member && e["type"] != "agent_spawned"),
        "agent_id is not the member's session: {events:?}"
    );
    assert_eq!(completed["agent_id"], member);
}

#[tokio::test]
async fn a_persistent_member_is_in_its_sessions_telemetry_from_start_to_stop() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let agent_tool = TestParent {
        host: Some(runtime::agent::HostContracts {
            telemetry: TelemetryHandle::new(tx),
            ..host_contracts()
        }),
        ..test_parent().with_team()
    }
    .agent_tool();
    let session = agent_tool.parent_state();
    message_member(&agent_tool, &session, "a").await.unwrap();
    wait_until_idle(&agent_tool, &team_key("team", "a")).await;
    agent_tool.stop_team_member("team", "a").await;

    let mut kinds = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !kinds.contains(&"agent_completed".to_string()) {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("agent_completed within 5s")
            .expect("the handle is still open");
        kinds.push(event.kind().to_string());
    }
    let spawned = kinds.iter().position(|k| k == "agent_spawned");
    assert!(spawned.is_some_and(|i| i < kinds.len() - 1), "{kinds:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_end_of_the_parent_session_stops_a_member_mid_turn() {
    let (parent, started, dropped) = hanging_team_parent(4);
    let ended = tokio_util::sync::CancellationToken::new();
    let agent_tool = TestParent {
        ended: ended.clone(),
        ..parent
    }
    .agent_tool();
    let session = agent_tool.parent_state();
    message_member(&agent_tool, &session, "a").await.unwrap();
    started.notified().await;

    ended.cancel();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the member kept its turn running after the session ended"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_parent_call_aborts_the_foreground_sub_agent() {
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent_tool = agent_tool_with_model(Arc::new(HangingModel {
        started: started.clone(),
        dropped: dropped.clone(),
    }));
    tokio::select! {
        _ = run_sub_with(&agent_tool) => panic!("the model never answers"),
        _ = started.notified() => {}
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sub-agent kept running after its parent dropped the call"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Blocks its thread until a second call is in flight beside it (or the
/// test gives up and releases it), and records how many were at once.
#[derive(Default)]
pub(super) struct BlockingModel {
    pub(super) in_flight: std::sync::atomic::AtomicUsize,
    pub(super) peak: std::sync::atomic::AtomicUsize,
    pub(super) released: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl Model for BlockingModel {
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
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(now, SeqCst);
        while self.peak.load(SeqCst) < 2 && !self.released.load(SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        self.in_flight.fetch_sub(1, SeqCst);
        Err(base::interface::model::ModelError::protocol("done"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_sub_agents_of_one_turn_run_on_separate_threads() {
    let model = Arc::new(BlockingModel::default());
    let agent_tool = agent_tool_with_model(model.clone());
    // Polled one after the other, the first call would block its thread
    // for good; the timeout only keeps that failure from hanging the run.
    let both = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        futures::future::join(run_sub_with(&agent_tool), run_sub_with(&agent_tool)),
    )
    .await;
    model
        .released
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(both.is_ok(), "the two sub-agents never ran side by side");
    assert_eq!(
        model.peak.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "two sub-agents whose model calls block their thread were polled one after the other"
    );
}

/// First call: waits for `go`, then answers. Later calls: record the last
/// user message they were shown, then answer.
pub(super) struct SteerableModel {
    pub(super) calls: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) started: Arc<tokio::sync::Notify>,
    pub(super) go: Arc<tokio::sync::Notify>,
    pub(super) seen_later: Arc<std::sync::Mutex<Vec<String>>>,
}
#[async_trait]
impl Model for SteerableModel {
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
        use base::interface::model::ModelEvent;
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.started.notify_one();
            self.go.notified().await;
        } else {
            let last_user = messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, MessageRole::User))
                .map(|m| format!("{:?}", m.content))
                .unwrap_or_default();
            self.seen_later.lock().unwrap().push(last_user);
        }
        Ok(Box::new(futures::stream::iter(vec![
            Ok(ModelEvent::TextDelta {
                text: "report".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ])))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_sub_agent_reads_its_messages_and_reports_back_when_done() {
    let started = Arc::new(tokio::sync::Notify::new());
    let go = Arc::new(tokio::sync::Notify::new());
    let seen_later = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent_tool = agent_tool_with_model(Arc::new(SteerableModel {
        calls: Default::default(),
        started: started.clone(),
        go: go.clone(),
        seen_later: seen_later.clone(),
    }));
    let session = agent_tool.parent_state();
    let tid = agent_tool
        .start_in_background_for_test("look into it", &session)
        .await
        .unwrap();
    started.notified().await;
    tools::task_message::deliver(&session, &tid, "also check the lexer".into())
        .expect("a running sub-agent takes messages");
    go.notify_one();

    let task = session.find_running_task(&tid).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
        .await
        .expect("the sub-agent finishes");
    assert!(
        seen_later.lock().unwrap()[0].contains("also check the lexer"),
        "the message joins the sub-agent's turn: {:?}",
        seen_later.lock().unwrap()
    );
    let notices = session.take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("<task-notification>"));
    assert!(notices[0].contains(&tid));
    assert!(notices[0].contains("<status>done</status>"));
    assert!(notices[0].contains("report"));
}

/// A scene that brings a tool of its own, the way an embedder's does.
pub(super) fn scene_with_probe() -> SceneSpec {
    let mut s = SceneSpec::new(
        "with-probe",
        "With probe",
        Arc::new(base::interface::scene::FixedPrompt(vec![])),
    );
    s.extra_tools = vec![Arc::new(NamedTool("scene_probe"))];
    s
}

/// Answers at once, noting the tools each request offered.
pub(super) struct ToolCensusModel {
    pub(super) offered: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}
#[async_trait]
impl Model for ToolCensusModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        self.offered
            .lock()
            .unwrap()
            .push(tools.into_iter().map(|t| t.name).collect());
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

/// Answers at once, noting the model each request asked for.
pub(super) struct ModelNameModel {
    pub(super) asked_for: Arc<std::sync::Mutex<Vec<String>>>,
}
#[async_trait]
impl Model for ModelNameModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        self.asked_for.lock().unwrap().push(params.model);
        Ok(Box::new(futures::stream::iter(vec![
            Ok(ModelEvent::TextDelta {
                text: "{\"ok\":true}".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ])))
    }
}

#[tokio::test]
async fn a_cap_of_zero_refuses_persistent_members_and_leaves_one_shot_delegates_alone() {
    let asked_for = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut config = EngineConfig::defaults_for("test-model");
    config.team_max_persistent_members = 0;
    let agent_tool = TestParent {
        config: Arc::new(config),
        ..TestParent::new(Arc::new(ModelNameModel {
            asked_for: asked_for.clone(),
        }))
    }
    .with_team()
    .agent_tool();
    let session = agent_tool.parent_state();

    let refused = message_member(&agent_tool, &session, "a")
        .await
        .expect_err("no persistent member may start at a cap of 0");
    assert!(
        refused
            .to_string()
            .contains("persistent team members are turned off"),
        "{refused}"
    );
    assert!(asked_for.lock().unwrap().is_empty());

    let req = DelegateRequest::new(
        "review",
        DelegateMode::Wait,
        StopsWith::Call(tokio_util::sync::CancellationToken::new()),
    );
    let answer = finished_text(agent_tool.delegate(req).await).unwrap();
    assert_eq!(answer, "{\"ok\":true}");
}

#[tokio::test]
async fn a_spawn_on_a_model_asks_for_that_model() {
    let asked_for = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tools = Arc::new(InMemoryToolRegistry::new());
    let agent_tool = TestParent {
        config: Arc::new(EngineConfig::defaults_for("test-model")),
        tools: tools.clone(),
        template: tools,
        ..TestParent::new(Arc::new(ModelNameModel {
            asked_for: asked_for.clone(),
        }))
    }
    .agent_tool();
    let mut req = DelegateRequest::new(
        "review",
        DelegateMode::Wait,
        StopsWith::Call(tokio_util::sync::CancellationToken::new()),
    );
    req.model = Some("claude-haiku-4-5".into());
    let answer = finished_text(agent_tool.delegate(req).await).unwrap();
    assert_eq!(answer, "{\"ok\":true}");
    let asked_for = asked_for.lock().unwrap();
    assert!(!asked_for.is_empty());
    assert!(
        asked_for.iter().all(|m| m == "claude-haiku-4-5"),
        "{asked_for:?}"
    );
}

/// The `AgentTool` of a session on `scene_with_probe`: its registry already
/// holds the scene's tool, as `Builder::build()` leaves it, and the template
/// it was built from does not.
pub(super) fn agent_tool_on_a_scene_with_a_tool(
    types: &[AgentTypeDefinition],
) -> (AgentTool, Arc<std::sync::Mutex<Vec<Vec<String>>>>) {
    let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let template = Arc::new(InMemoryToolRegistry::new());
    template.register(Arc::new(NamedTool("Read")));
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(NamedTool("Read")));
    tools.register(Arc::new(NamedTool("scene_probe")));
    let agent_tool = TestParent {
        config: Arc::new(EngineConfig::defaults_for("test-model")),
        tools,
        template,
        plugin_agent_types: (types).to_vec(),
        scene: Arc::new(scene_with_probe()),
        ..TestParent::new(Arc::new(ToolCensusModel {
            offered: offered.clone(),
        }))
    }
    .agent_tool();
    (agent_tool, offered)
}

pub(super) fn offered_once(offered: &[Vec<String>], name: &str) -> bool {
    offered
        .first()
        .is_some_and(|tools| tools.iter().filter(|t| *t == name).count() == 1)
}

#[tokio::test]
async fn a_foreground_sub_agent_on_its_parents_scene_gets_the_scenes_tools_once() {
    let (agent_tool, offered) = agent_tool_on_a_scene_with_a_tool(&[]);
    let result = base::tool::Tool::call(
        &agent_tool,
        serde_json::json!({ "prompt": "look", "description": "look" }),
        platform::tool_context_for_test(std::env::temp_dir()),
        ProgressSender::noop("t"),
    )
    .await
    .expect("the sub-agent is built and runs");
    assert!(!result.is_error, "{:?}", result.content);
    let offered = offered.lock().unwrap();
    assert!(offered_once(&offered, "scene_probe"), "{offered:?}");
    assert!(offered_once(&offered, "Read"), "{offered:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_sub_agent_and_a_spawned_one_get_the_scenes_tools_once() {
    let (agent_tool, offered) = agent_tool_on_a_scene_with_a_tool(&[]);
    let ctx = platform::tool_context_for_test(std::env::temp_dir());
    let session = agent_tool.parent_state();
    base::tool::Tool::call(
        &agent_tool,
        serde_json::json!({ "prompt": "look", "description": "look", "background": true }),
        ctx,
        ProgressSender::noop("t"),
    )
    .await
    .expect("the background sub-agent is started");
    let task = session.task_infos().pop().expect("a registered task");
    let task = session.find_running_task(&task.task_id).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
        .await
        .expect("the sub-agent finishes");
    assert!(
        offered_once(&offered.lock().unwrap(), "scene_probe"),
        "{:?}",
        offered.lock().unwrap()
    );

    offered.lock().unwrap().clear();
    agent_tool
        .wait_for(
            "look",
            std::env::temp_dir(),
            tokio_util::sync::CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("a waited sub-agent is built and runs");
    assert!(
        offered_once(&offered.lock().unwrap(), "scene_probe"),
        "{:?}",
        offered.lock().unwrap()
    );
}

#[tokio::test]
async fn an_agent_types_tool_lists_narrow_the_scenes_tools_too() {
    let types = [
        AgentTypeDefinition {
            name: "reader".into(),
            description: "reads".into(),
            allowed_tools: vec!["Read".into()],
            ..Default::default()
        },
        AgentTypeDefinition {
            name: "no-probe".into(),
            description: "no probe".into(),
            disallowed_tools: vec!["scene_probe".into()],
            ..Default::default()
        },
    ];
    for agent_type in ["reader", "no-probe"] {
        let (agent_tool, offered) = agent_tool_on_a_scene_with_a_tool(&types);
        base::tool::Tool::call(
            &agent_tool,
            serde_json::json!({
                "prompt": "look", "description": "look", "subagent_type": agent_type
            }),
            platform::tool_context_for_test(std::env::temp_dir()),
            ProgressSender::noop("t"),
        )
        .await
        .expect("the sub-agent is built and runs");
        let offered = offered.lock().unwrap();
        assert!(
            offered[0].contains(&"Read".to_string())
                && !offered[0].contains(&"scene_probe".to_string()),
            "{agent_type}: {offered:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_cap_refuses_one_sub_agent_too_many_and_a_root_team_member_waits_for_one() {
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let slots = SubagentSlots::new(1);
    let agent_tool = TestParent {
        slots: slots.clone(),
        ..TestParent::new(Arc::new(HangingModel {
            started: started.clone(),
            dropped: dropped.clone(),
        }))
    }
    .agent_tool();

    let mut first = Box::pin(run_sub_with(&agent_tool));
    tokio::select! {
        _ = &mut first => panic!("the model never answers"),
        _ = started.notified() => {}
    }
    assert_eq!(slots.running(), 1);

    let refused = run_sub_with(&agent_tool).await.unwrap_err();
    assert!(
        refused.to_string().contains("max_concurrent_subagents"),
        "{refused}"
    );

    let mut member = Box::pin(agent_tool.run_stage_member(
        "member task",
        tokio_util::sync::CancellationToken::new(),
        None,
        Vec::new(),
    ));
    tokio::select! {
        biased;
        refused = &mut member => panic!("the member did not wait for a slot: {refused:?}"),
        _ = tokio::task::yield_now() => {}
    }

    drop(first);
    tokio::select! {
        _ = &mut member => panic!("the model never answers"),
        _ = started.notified() => {}
    }
    assert_eq!(
        slots.running(),
        1,
        "the member runs on the slot the first sub-agent gave back"
    );
    drop(member);
    assert_eq!(slots.running(), 0, "an ended member gives its slot back");
}
