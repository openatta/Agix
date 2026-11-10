//! Tests: what every spawn path — foreground, background, team member —
//! gives a delegate alike.

use super::*;
use base::interface::agent_spawner::{AgentSpawner, SpawnMode, SpawnRequest};

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
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
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
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }

    fn description(&self) -> &str {
        "asking probe (test tool)"
    }

    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
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
    use base::settings::PermissionMode;
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
            .spawn(
                SpawnRequest::new(
                    "probe",
                    dir.clone(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .mode(SpawnMode::TeamMember {
                    extra_tools: vec![Arc::new(AskingProbe { runs: runs.clone() })],
                    permission_mode: Some(PermissionMode::BypassPermissions),
                    seat: None,
                }),
            )
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

/// The hook agent's model: on its first call it asks the spawner for a hook
/// agent judging its own session, and keeps what the spawner said.
struct GuardThatAsksForAGuard {
    spawner: Arc<std::sync::OnceLock<Arc<dyn AgentSpawner>>>,
    asked: Arc<std::sync::atomic::AtomicBool>,
    nested: Arc<std::sync::Mutex<Option<Result<String, String>>>>,
}

#[async_trait]
impl Model for GuardThatAsksForAGuard {
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
        let own_session = params.origin.map(|o| o.session_id).unwrap_or_default();
        if !self.asked.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let spawner = self.spawner.get().expect("wired").clone();
            let nested = spawner
                .spawn(
                    SpawnRequest::new(
                        "judge",
                        std::env::temp_dir(),
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .mode(SpawnMode::Hook {
                        judged_session: own_session,
                    }),
                )
                .await
                .map_err(|e| e.to_string());
            *self.nested.lock().unwrap() = Some(nested);
        }
        Ok(Box::new(futures::stream::iter(vec![
            Ok(ModelEvent::TextDelta {
                text: "{\"ok\": true}".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ])))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_agent_runs_with_every_slot_held_and_starts_no_hook_agent_of_its_own() {
    let spawner_cell = Arc::new(std::sync::OnceLock::new());
    let nested = Arc::new(std::sync::Mutex::new(None));
    let slots = SubagentSlots::new(1);
    let agent_tool = agent_tool_with_model(Arc::new(GuardThatAsksForAGuard {
        spawner: spawner_cell.clone(),
        asked: Arc::default(),
        nested: nested.clone(),
    }))
    .with_subagent_slots(slots.clone());
    let spawner: Arc<dyn AgentSpawner> = Arc::new(crate::spawner::RuntimeAgentSpawner::new(
        Arc::new(agent_tool),
    ));
    let _ = spawner_cell.set(spawner.clone());
    let _held = slots.slots.clone().try_acquire_owned().unwrap();

    let verdict = spawner
        .spawn(
            SpawnRequest::new(
                "judge",
                std::env::temp_dir(),
                tokio_util::sync::CancellationToken::new(),
            )
            .mode(SpawnMode::Hook {
                judged_session: "parent-session".into(),
            }),
        )
        .await
        .map_err(|e| e.to_string());

    assert_eq!(verdict.as_deref(), Ok("{\"ok\": true}"));
    let nested = nested.lock().unwrap().clone().expect("the guard asked");
    assert!(
        nested
            .as_ref()
            .is_err_and(|e| e.contains("does not judge the agent running for another")),
        "{nested:?}"
    );
}

/// Counts the conversations it was asked to judge.
#[derive(Default)]
struct CountingCounter(std::sync::atomic::AtomicUsize);

impl base::interface::token_counter::TokenCounter for CountingCounter {
    fn count_text(&self, text: &str) -> usize {
        text.len()
    }
    fn count_block(&self, _: &base::interface::model::ModelContentBlock) -> usize {
        1
    }
    fn count_messages(&self, messages: &[ModelMessage]) -> usize {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        messages.len()
    }
}

/// Counts the times its index was asked for.
#[derive(Default)]
struct CountingRules(std::sync::atomic::AtomicUsize);

impl base::interface::instruction_provider::RuleProvider for CountingRules {
    fn id(&self) -> &str {
        "counting"
    }
    fn rules(&self) -> Vec<base::rules::RuleEntry> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Vec::new()
    }
}

/// A delegate is built with the host's answers its parent was: the git its
/// first turn asks for to collect the repository context reaches the
/// parent's `Process`, its size is judged by the parent's counter, and its
/// rule index comes from the parent's provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delegate_runs_on_its_parents_host_contracts() {
    use base::interface::exec::in_process::ScriptedProcess;
    let process = ScriptedProcess::new();
    let providers = base::interface::exec::ExecProviders {
        process: Arc::new(process.clone()),
        ..base::interface::exec::ExecProviders::in_process()
    };
    let counter = Arc::new(CountingCounter::default());
    let rules = Arc::new(CountingRules::default());
    let (settings, dir) = parent_settings_for_test("code");
    let agent_tool = agent_tool_with_model(Arc::new(LineageModel {
        seen: Default::default(),
    }))
    .with_settings(Arc::new(settings))
    .with_exec(providers)
    .with_token_counter(counter.clone())
    .with_rule_provider(rules.clone());

    let ran = agent_tool
        .run_sub(
            "look".into(),
            Arc::new(InMemoryToolRegistry::new()),
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            PermissionSet::without_grants(Arc::new(AlwaysPermit)),
            None,
            None,
            Vec::new(),
        )
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    ran.expect("the delegate runs");
    assert!(
        process.seen().iter().any(|argv| argv.starts_with("git ")),
        "the delegate's git did not reach the parent's providers: {:?}",
        process.seen()
    );
    assert!(
        counter.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the delegate's size was not judged by the parent's counter"
    );
    assert!(
        rules.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the delegate's rule index did not come from the parent's provider"
    );
}
