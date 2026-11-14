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
    let session = agent_tool.parent_state();
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
    let task = session.find_running_task(&task.task_id).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
        .await
        .expect("the sub-agent finishes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_sub_agent_and_a_team_member_record_who_spawned_them_and_as_what() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parent = TestParent::new(Arc::new(LineageModel { seen: seen.clone() }));
    let agent_tool = TestParent {
        id: "parent-session-1".into(),
        team_registry: Some(Arc::new(team::registry::TeamRegistry::new())),
        ..parent
    }
    .agent_tool();
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
    let session = agent_tool.parent_state();
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
    let parent = TestParent::new(Arc::new(LineageModel {
        seen: Default::default(),
    }));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let agent_tool = TestParent {
        events: Some(runtime::event_bus::EventBus::new(tx)),
        id: "parent-session-1".into(),
        ..parent
    }
    .agent_tool();

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
    async fn permission_facts(
        &self,
        _: &serde_json::Value,
        _: &base::tool::ToolContext,
    ) -> base::interface::permission::PermissionFacts {
        use base::interface::permission::{PermissionFacts, Reach, Subject};
        PermissionFacts::new(Subject::None, Reach::Unknown).with_question("run the probe?")
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(base::tool::ToolResult::text("probed"))
    }
}

/// With no parent permission wired, a `TeamCreate` member that asks for
/// more than its parent has is judged in the parent's mode, as a persistent
/// member is.
#[tokio::test]
async fn a_team_create_member_is_judged_in_its_mode_held_to_the_parents() {
    use base::settings::PermissionMode;
    for (parent_mode, probe_runs) in [
        (PermissionMode::Default, 0),
        (PermissionMode::BypassPermissions, 1),
    ] {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = Arc::new(InMemoryToolRegistry::new());
        let (mut settings, dir) = parent_settings_for_test("code");
        settings.values.permission_mode = parent_mode;
        let agent_tool = TestParent {
            config: Arc::new(EngineConfig::defaults_for("test-model")),
            tools: tools.clone(),
            template: tools,
            settings: Some(settings),
            // Not interactive: what would be asked is refused instead.
            scene: Arc::new(super::lifecycle::scene_with_probe()),
            permission: Some(PermissionSet::of(Arc::new(
                permissions::policy::Policy::empty(),
            ))),
            ..TestParent::new(Arc::new(ProbeThenTextModel {
                calls: Default::default(),
            }))
        }
        .agent_tool();

        let mut req = DelegateRequest::new(
            "probe",
            DelegateMode::TeamMember {
                seat: TeamSeat {
                    team: "team".into(),
                    member: "member".into(),
                },
                permission_mode: Some(PermissionMode::BypassPermissions),
                lifetime: MemberLifetime::Stage {
                    stage: "stage".into(),
                    extra_tools: vec![Arc::new(AskingProbe { runs: runs.clone() })],
                },
            },
            StopsWith::Call(tokio_util::sync::CancellationToken::new()),
        );
        req.workdir = Workdir::At(dir.clone());
        agent_tool.delegate(req).await.expect("the member runs");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            probe_runs,
            "parent in {parent_mode:?}"
        );
    }
}

/// The hook agent's model: on its first call it asks the delegator for a
/// hook agent judging its own session, and keeps what the delegator said.
struct GuardThatAsksForAGuard {
    delegator: Arc<std::sync::OnceLock<Arc<dyn base::interface::delegate::Delegator>>>,
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
            let delegator = self.delegator.get().expect("wired").clone();
            let nested = finished_text(delegator.delegate(judge_request(own_session)).await)
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

fn judge_request(judged_session: String) -> DelegateRequest {
    let mode = DelegateMode::Hook { judged_session };
    let stop = StopsWith::Call(tokio_util::sync::CancellationToken::new());
    let mut req = DelegateRequest::new("judge", mode, stop);
    req.workdir = Workdir::At(std::env::temp_dir());
    req
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_agent_runs_with_every_slot_held_and_starts_no_hook_agent_of_its_own() {
    let delegator_cell = Arc::new(std::sync::OnceLock::new());
    let nested = Arc::new(std::sync::Mutex::new(None));
    let slots = SubagentSlots::new(1);
    let agent_tool = TestParent {
        slots: slots.clone(),
        ..TestParent::new(Arc::new(GuardThatAsksForAGuard {
            delegator: delegator_cell.clone(),
            asked: Arc::default(),
            nested: nested.clone(),
        }))
    }
    .agent_tool();
    let delegator: Arc<dyn base::interface::delegate::Delegator> = agent_tool.delegator.clone();
    let _ = delegator_cell.set(delegator.clone());
    let _held = slots.slots.clone().try_acquire_owned().unwrap();

    let verdict = finished_text(
        delegator
            .delegate(judge_request("parent-session".into()))
            .await,
    )
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

/// The session's delegator, set once the delegator that holds the hook runner
/// exists, as `Builder::build` sets its own.
#[derive(Default)]
struct LateDelegator(std::sync::OnceLock<Arc<dyn base::interface::delegate::Delegator>>);

#[async_trait]
impl base::interface::delegate::Delegator for LateDelegator {
    async fn delegate(&self, req: DelegateRequest) -> Result<Delegated, DelegateError> {
        self.0.get().expect("wired").delegate(req).await
    }

    async fn stop_member(&self, seat: &TeamSeat) {
        self.0.get().expect("wired").stop_member(seat).await
    }

    fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
        self.0.get().expect("wired").live_members()
    }
}

/// A hook agent's own tool calls are not judged by the agent hook it runs
/// for, so one that fails closed does not refuse them all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fail_closed_agent_hook_does_not_block_its_own_agents_tools() {
    let (mut parent, _tools, runs, dir) = wired_parent("code");
    if let Some(settings) = parent.settings.as_mut() {
        settings.values.permission_mode = base::settings::PermissionMode::BypassPermissions;
    }
    let late = Arc::new(LateDelegator::default());
    let guard: hooks::HookConfig = serde_json::from_value(serde_json::json!({
        "type": "agent", "prompt": "judge", "on_error": "block"
    }))
    .unwrap();
    let runner = Arc::new(
        hooks::HookRunner::new(std::collections::HashMap::from([(
            hooks::HookEvent::PreToolUse,
            vec![guard],
        )]))
        .with_agent_executor(Arc::new(
            runtime::hook_executors::DelegatorHookExecutor::new(late.clone(), dir.clone()),
        )),
    );
    let agent_tool = TestParent {
        hooks: Arc::downgrade(&runner),
        permission: Some(PermissionSet::of(Arc::new(
            permissions::policy::Policy::empty(),
        ))),
        ..parent
    }
    .agent_tool();
    let delegator: Arc<dyn base::interface::delegate::Delegator> = agent_tool.delegator.clone();
    let _ = late.0.set(delegator.clone());

    finished_text(
        delegator
            .delegate(judge_request("parent-session".into()))
            .await,
    )
    .expect("the hook agent finishes");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
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
    let agent_tool = TestParent {
        settings: Some(settings),
        host: Some(runtime::agent::HostContracts {
            exec: providers,
            token_counter: counter.clone(),
            rules: rules.clone(),
            ..host_contracts()
        }),
        ..TestParent::new(Arc::new(LineageModel {
            seen: Default::default(),
        }))
    }
    .agent_tool();

    let ran = agent_tool
        .wait_for(
            "look",
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            None,
            None,
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

/// The one dispatch: a waited delegate finishes with its answer, a
/// background one is a task of the parent session, and a narrowed selector
/// narrows what the delegate is offered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_delegator_runs_every_mode_through_one_dispatch() {
    let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(NamedTool("Read")));
    tools.register(Arc::new(NamedTool("Bash")));
    let agent_tool = TestParent::on(
        Arc::new(super::lifecycle::ToolCensusModel {
            offered: offered.clone(),
        }),
        tools,
    )
    .agent_tool();
    let call = || StopsWith::Call(tokio_util::sync::CancellationToken::new());
    match agent_tool
        .delegate(DelegateRequest::new("look", DelegateMode::Wait, call()))
        .await
    {
        Ok(Delegated::Finished(outcome)) => {
            assert_eq!(outcome.end, DelegateEnd::Completed);
            assert!(!outcome.session_id.is_empty());
        }
        Ok(Delegated::Started(_)) => panic!("a waited delegate was started in the background"),
        Err(e) => panic!("the waited delegate did not run: {e}"),
    }

    let background = DelegateRequest::new(
        "look",
        DelegateMode::Background {
            notify_parent: false,
        },
        call(),
    );
    let Ok(Delegated::Started(started)) = agent_tool.delegate(background).await else {
        panic!("the background delegate was not started");
    };
    let task = agent_tool
        .parent_state()
        .find_running_task(&started.task_id)
        .expect("a task of the parent session");
    tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
        .await
        .expect("the background delegate finishes");

    offered.lock().unwrap().clear();
    let mut narrowed = DelegateRequest::new("look", DelegateMode::Wait, call());
    narrowed.tools = ToolSelector::from_lists(&["Read".to_string()], &[]);
    agent_tool.delegate(narrowed).await.expect("it runs");
    let offered = offered.lock().unwrap();
    assert!(
        offered[0].contains(&"Read".to_string()) && !offered[0].contains(&"Bash".to_string()),
        "{offered:?}"
    );
}

struct KeepsEverything;
impl base::interface::memory_contracts::RetrievalHook for KeepsEverything {}

/// A delegate is assembled on the contracts its parent resolved, the same
/// values and not copies of them: the scenes an agent type's `scene:` may
/// name, the memory store with the retriever and hooks that filter what is
/// recalled from it, the standing instructions, the clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delegate_is_assembled_on_its_parents_host_contracts() {
    let probe = Arc::new(CapturesHost(Default::default()));
    let mut registry = scene::SceneRegistry::new();
    registry.register_builtin();
    let parent = runtime::agent::HostContracts {
        scene_registry: Some(Arc::new(registry)),
        memory_retriever: Some(Arc::new(
            base::interface::memory_contracts::SubstringRetriever,
        )),
        retrieval_hooks: vec![Arc::new(KeepsEverything)],
        modules: vec![probe.clone()],
        ..host_contracts()
    };
    let agent_tool = TestParent {
        host: Some(parent.clone()),
        ..test_parent()
    }
    .agent_tool();
    let _delegate = agent_tool
        .build_team_member_agent(
            std::env::temp_dir(),
            None,
            base::settings::PermissionMode::Plan,
            None,
        )
        .await
        .expect("the delegate builds");
    let seen = probe
        .0
        .lock()
        .unwrap()
        .take()
        .expect("the delegate ran its parent's modules");

    let same = |a: Option<&Arc<scene::SceneRegistry>>, b: Option<&Arc<scene::SceneRegistry>>| matches!((a, b), (Some(a), Some(b)) if Arc::ptr_eq(a, b));
    assert!(
        same(seen.scene_registry.as_ref(), parent.scene_registry.as_ref()),
        "the delegate cannot resolve the scenes its parent can"
    );
    assert!(Arc::ptr_eq(&seen.memory_store, &parent.memory_store));
    assert!(
        matches!(
            (&seen.memory_retriever, &parent.memory_retriever),
            (Some(a), Some(b)) if Arc::ptr_eq(a, b)
        ),
        "the delegate recalls with another retriever"
    );
    assert_eq!(seen.retrieval_hooks.len(), 1);
    assert!(Arc::ptr_eq(
        &seen.retrieval_hooks[0],
        &parent.retrieval_hooks[0]
    ));
    assert!(Arc::ptr_eq(
        &seen.instruction_provider,
        &parent.instruction_provider
    ));
    assert!(Arc::ptr_eq(&seen.environment, &parent.environment));
}

fn team_of(members: &[&str]) -> Value {
    serde_json::json!({
        "name": "crew",
        "agents": members
            .iter()
            .map(|m| serde_json::json!({ "label": m, "prompt": format!("MEMBER-MARK {m}") }))
            .collect::<Vec<_>>()
    })
}

/// A sub-agent already holding the session's only slot asks for its team's
/// members without waiting, so they are refused, and the refusal is what
/// its team reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_team_started_by_a_sub_agent_holding_the_only_slot_is_refused_its_members() {
    use super::scripted::*;
    let members = Gate::new(2, std::time::Duration::from_secs(5));
    let model = Arc::new(
        RoutedModel::default()
            .route(
                "ROOT-MARK",
                [
                    call(
                        "Agent",
                        serde_json::json!({
                            "prompt": "LEAD-MARK run a team", "description": "lead",
                            "background": true
                        }),
                    ),
                    text("root done"),
                ],
            )
            .route(
                "LEAD-MARK",
                [call("TeamCreate", team_of(&["a", "b"])), text("lead done")],
            )
            .route(
                "MEMBER-MARK",
                [
                    Step::Gather(members.clone(), "member done".into()),
                    Step::Gather(members.clone(), "member done".into()),
                ],
            ),
    );
    let mut root = assembled_root(model.clone(), |s| {
        s.execution.team_enabled = true;
        s.execution.max_concurrent_subagents = 1;
    });
    root.turn("ROOT-MARK start the lead").await;
    root.tasks_settled(std::time::Duration::from_secs(60)).await;

    assert_eq!(
        members.peak(),
        0,
        "members ran beside the lead holding the only slot"
    );
    let lead = model.seen_with("LEAD-MARK");
    let reported = lead.last().map(|s| s.text.clone()).unwrap_or_default();
    assert!(
        reported.contains("max_concurrent_subagents"),
        "the team did not report its members being refused a slot: {reported}"
    );
}

/// A root session holds no slot, so the members of a team it starts wait
/// for one: every member runs, never more at once than the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_sessions_team_waits_for_slots_and_every_member_runs() {
    use super::scripted::*;
    let members = Gate::new(3, std::time::Duration::from_secs(3));
    let model = Arc::new(
        RoutedModel::default()
            .route(
                "ROOT-MARK",
                [
                    call("TeamCreate", team_of(&["a", "b", "c"])),
                    text("root done"),
                ],
            )
            .route(
                "MEMBER-MARK",
                (0..3).map(|_| Step::Gather(members.clone(), "MEMBER-FINISHED".into())),
            ),
    );
    let mut root = assembled_root(model.clone(), |s| {
        s.execution.team_enabled = true;
        s.execution.max_concurrent_subagents = 2;
    });
    root.turn("ROOT-MARK run the team").await;

    assert!(
        members.peak() <= 2,
        "{} members ran at once under a cap of 2",
        members.peak()
    );
    let reported = model
        .seen_with("ROOT-MARK")
        .last()
        .map(|s| s.text.clone())
        .unwrap_or_default();
    assert_eq!(
        reported.matches("MEMBER-FINISHED").count(),
        3,
        "not every member ran: {reported}"
    );
}

/// A team stage's member stopped mid-turn says it was stopped, rather than
/// handing its partial text back as an answer, and its task ends stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stage_member_stopped_mid_turn_ends_cancelled() {
    let started = Arc::new(tokio::sync::Notify::new());
    let agent_tool = TestParent::new(Arc::new(AnswersOnlyCancellation {
        started: started.clone(),
        dropped: Default::default(),
    }))
    .agent_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mode = DelegateMode::TeamMember {
        seat: TeamSeat {
            team: "crew".into(),
            member: "a".into(),
        },
        permission_mode: None,
        lifetime: MemberLifetime::Stage {
            stage: "stage".into(),
            extra_tools: Vec::new(),
        },
    };
    let run = agent_tool.delegate(DelegateRequest::new(
        "work",
        mode,
        StopsWith::Call(cancel.clone()),
    ));
    tokio::pin!(run);
    tokio::select! {
        _ = &mut run => panic!("the model never answers"),
        _ = started.notified() => {}
    }
    cancel.cancel();
    let Ok(Delegated::Finished(outcome)) = run.await else {
        panic!("a stage member finishes");
    };
    assert_eq!(outcome.end, DelegateEnd::Cancelled);
    let tasks = agent_tool.parent_state().task_infos();
    assert!(
        tasks
            .iter()
            .any(|t| t.kind == base::context::TaskType::Teammate
                && t.status == base::context::task::TaskStatus::Stopped),
        "{tasks:?}"
    );
}

/// Notes the text of every turn's messages.
struct MessageTextModel {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Model for MessageTextModel {
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
        use base::interface::model::{ModelContentBlock, ModelEvent};
        let text: String = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ModelContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        self.seen.lock().unwrap().push(text);
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

/// A host names a type and gives the task alone, as the model does; the
/// delegate is told the type's prompt ahead of it all the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_started_agent_of_a_type_is_given_the_types_prompt() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent_tool = TestParent {
        plugin_agent_types: vec![AgentTypeDefinition {
            name: "typed".into(),
            description: "typed".into(),
            system_prompt: "TYPE-PROMPT".into(),
            ..Default::default()
        }],
        ..TestParent::new(Arc::new(MessageTextModel { seen: seen.clone() }))
    }
    .agent_tool();
    let mut req = DelegateRequest::new(
        "HOST-TASK",
        DelegateMode::Host {
            notify_parent: false,
        },
        StopsWith::Session,
    );
    req.agent_type = Some("typed".into());
    let Ok(Delegated::Started(started)) = agent_tool.delegate(req).await else {
        panic!("a host's agent is started");
    };
    let task = agent_tool
        .parent_state()
        .find_running_task(&started.task_id)
        .expect("its task");
    tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
        .await
        .expect("the agent finishes");
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|t| t.contains("TYPE-PROMPT\n\nTask: HOST-TASK")),
        "{seen:?}"
    );
}

/// A forked skill naming an `agent:` and a team stage's member naming a type
/// ask the delegator directly, not through the `Agent` tool; their delegate is
/// told the type's prompt ahead of the task all the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typed_fork_and_stage_member_are_given_the_types_prompt() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent_tool = TestParent {
        plugin_agent_types: vec![AgentTypeDefinition {
            name: "typed".into(),
            description: "typed".into(),
            system_prompt: "TYPE-PROMPT".into(),
            ..Default::default()
        }],
        ..TestParent::new(Arc::new(MessageTextModel { seen: seen.clone() }))
    }
    .agent_tool();
    let stage = DelegateMode::TeamMember {
        seat: TeamSeat {
            team: "team".into(),
            member: "member".into(),
        },
        permission_mode: None,
        lifetime: MemberLifetime::Stage {
            stage: "stage".into(),
            extra_tools: Vec::new(),
        },
    };
    for (task, mode) in [("FORK-TASK", DelegateMode::Wait), ("STAGE-TASK", stage)] {
        let stop = StopsWith::Call(tokio_util::sync::CancellationToken::new());
        let mut req = DelegateRequest::new(task, mode, stop);
        req.agent_type = Some("typed".into());
        req.workdir = Workdir::At(std::env::temp_dir());
        finished_text(agent_tool.delegate(req).await).expect("the delegate finishes");
        let typed = format!("TYPE-PROMPT\n\nTask: {task}");
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|t| t.contains(&typed)), "{task}: {seen:?}");
    }
}

/// A persistent member of a type is told the type's prompt once, with the
/// message it is created on; later messages reach it as sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typed_persistent_member_is_given_the_types_prompt_once() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent_tool = TestParent {
        plugin_agent_types: vec![AgentTypeDefinition {
            name: "typed".into(),
            description: "typed".into(),
            system_prompt: "TYPE-PROMPT".into(),
            ..Default::default()
        }],
        ..TestParent::new(Arc::new(MessageTextModel { seen: seen.clone() }))
    }
    .with_team()
    .agent_tool();
    for task in ["FIRST-TASK", "SECOND-TASK"] {
        let task_id = agent_tool
            .spawn_or_message_team_member(
                "crew".into(),
                "member".into(),
                task.into(),
                Some("typed".into()),
                std::env::temp_dir(),
                agent_tool.parent_state(),
                None,
            )
            .await
            .expect("the message is queued");
        if let Some(task) = agent_tool.parent_state().find_running_task(&task_id) {
            tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
                .await
                .expect("the message's turn ends");
        }
    }
    let seen = seen.lock().unwrap();
    let last = seen
        .iter()
        .find(|t| t.ends_with("SECOND-TASK"))
        .expect("the second message reached the member's model");
    assert!(last.contains("SECOND-TASK"), "{seen:?}");
    assert!(last.contains("TYPE-PROMPT\n\nTask: FIRST-TASK"), "{seen:?}");
    assert_eq!(last.matches("TYPE-PROMPT").count(), 1, "{seen:?}");
}
