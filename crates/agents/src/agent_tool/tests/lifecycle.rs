//! Tests: lifecycle.

use super::*;

pub(super) fn hanging_team_tool(
    cap: usize,
) -> (
    AgentTool,
    Arc<tokio::sync::Notify>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tools = Arc::new(InMemoryToolRegistry::new());
    let mut config = EngineConfig::defaults_for("test-model");
    config.team_max_persistent_members = cap;
    let agent_tool = AgentTool::with_parent_tools(
        Arc::new(AnswersOnlyCancellation {
            started: started.clone(),
            dropped: dropped.clone(),
        }),
        Arc::new(config),
        tools.clone(),
        tools,
        &[],
        &[],
    );
    agent_tool.set_team_registry(Arc::new(team::registry::TeamRegistry::new()));
    (agent_tool, started, dropped)
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
    let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
        "/tmp",
    )));
    message_member(&agent_tool, &session, "a").await.unwrap();
    started.notified().await;

    let refused = message_member(&agent_tool, &session, "b")
        .await
        .expect_err("the only place is taken by a busy member");
    assert!(
        refused.to_string().contains("team member limit reached"),
        "{refused}"
    );
    assert_eq!(
        agent_tool
            .inner
            .team_members
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
async fn a_persistent_member_is_in_its_sessions_telemetry_from_start_to_stop() {
    let agent_tool = team_enabled_agent_tool();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    agent_tool.set_telemetry_handle(TelemetryHandle::new(tx));
    let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
        "/tmp",
    )));
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
    let (agent_tool, started, dropped) = hanging_team_tool(4);
    let ended = tokio_util::sync::CancellationToken::new();
    agent_tool.set_session_ended(ended.clone());
    let session = Arc::new(base::context::SessionState::new(std::path::PathBuf::from(
        "/tmp",
    )));
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
        Err(base::interface::model::ModelError::Internal("done".into()))
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
    let session = Arc::new(base::context::SessionState::new(std::env::temp_dir()));
    let tid = agent_tool
        .spawn_background(
            "look into it".into(),
            Arc::new(InMemoryToolRegistry::new()),
            std::env::temp_dir(),
            tokio_util::sync::CancellationToken::new(),
            None,
            None,
            Vec::new(),
            session.clone(),
        )
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

/// A scene that brings a tool of its own, the way an embedder's does: a
/// fresh instance on every call.
pub(super) struct SceneWithProbe;
impl base::interface::scene::Scene for SceneWithProbe {
    fn id(&self) -> &str {
        "with-probe"
    }
    fn name(&self) -> &str {
        "With probe"
    }
    fn description(&self) -> &str {
        "scene contributing scene_probe"
    }
    fn build_system_prompt(
        &self,
        _: &base::interface::scene::ScenePromptContext,
    ) -> Vec<base::prompt::PromptBlock> {
        vec![]
    }
    fn tools(&self) -> Vec<String> {
        vec![]
    }
    fn token_budget(&self) -> base::interface::scene::TokenBudget {
        base::interface::scene::TokenBudget {
            compact_threshold: 150_000,
            compact_keep_recent: 20,
        }
    }
    fn extra_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        vec![Arc::new(NamedTool("scene_probe"))]
    }
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
async fn a_spawn_on_a_model_asks_for_that_model() {
    use base::interface::agent_spawner::AgentSpawner;
    let asked_for = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tools = Arc::new(InMemoryToolRegistry::new());
    let agent_tool = AgentTool::with_parent_tools(
        Arc::new(ModelNameModel {
            asked_for: asked_for.clone(),
        }),
        Arc::new(EngineConfig::defaults_for("test-model")),
        tools.clone(),
        tools,
        &[],
        &[],
    );
    let spawner = crate::spawner::RuntimeAgentSpawner::new(Arc::new(agent_tool));

    let answer = spawner
        .spawn(
            base::interface::agent_spawner::SpawnRequest::new(
                "review",
                std::path::PathBuf::from("/tmp"),
                tokio_util::sync::CancellationToken::new(),
            )
            .model(Some("claude-haiku-4-5".into())),
        )
        .await
        .unwrap();
    assert_eq!(answer, "{\"ok\":true}");
    let asked_for = asked_for.lock().unwrap();
    assert!(!asked_for.is_empty());
    assert!(
        asked_for.iter().all(|m| m == "claude-haiku-4-5"),
        "{asked_for:?}"
    );
}

/// The `AgentTool` of a session on `SceneWithProbe`: its registry already
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
    let agent_tool = AgentTool::with_parent_tools(
        Arc::new(ToolCensusModel {
            offered: offered.clone(),
        }),
        Arc::new(EngineConfig::defaults_for("test-model")),
        tools,
        template,
        &[],
        types,
    );
    agent_tool.set_scene(Arc::new(SceneWithProbe));
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
    let session = ctx.session.clone();
    base::tool::Tool::call(
        &agent_tool,
        serde_json::json!({ "prompt": "look", "description": "look", "background": true }),
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
    assert!(
        offered_once(&offered.lock().unwrap(), "scene_probe"),
        "{:?}",
        offered.lock().unwrap()
    );

    offered.lock().unwrap().clear();
    use base::interface::agent_spawner::AgentSpawner as _;
    crate::spawner::RuntimeAgentSpawner::new(Arc::new(agent_tool))
        .spawn(base::interface::agent_spawner::SpawnRequest::new(
            "look",
            std::env::temp_dir(),
            tokio_util::sync::CancellationToken::new(),
        ))
        .await
        .expect("a spawner sub-agent is built and runs");
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
async fn the_session_cap_refuses_one_sub_agent_too_many_but_not_a_team_member() {
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let slots = SubagentSlots::new(1);
    let agent_tool = agent_tool_with_model(Arc::new(HangingModel {
        started: started.clone(),
        dropped: dropped.clone(),
    }))
    .with_subagent_slots(slots.clone());

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

    let member = agent_tool.run_team_member(
        "member task".into(),
        Arc::new(InMemoryToolRegistry::new()),
        std::path::PathBuf::from("/tmp"),
        tokio_util::sync::CancellationToken::new(),
        None,
        None,
        None,
        None,
    );
    tokio::select! {
        _ = member => panic!("the model never answers"),
        _ = started.notified() => {}
    }
    assert_eq!(slots.running(), 1, "a team member holds no slot");

    drop(first);
    assert_eq!(slots.running(), 0, "an ended sub-agent gives its slot back");
}
