//! Turn execution through the agent: goal rounds, tool failures, hooks, permissions, cancellation.

use super::*;

/// Test tool: always succeeds, records nothing — just needs to exist so
/// the mock model's `ToolUse` call has something real to dispatch to.
pub(super) struct ProbeTool;
#[async_trait::async_trait]
impl base::tool::Tool for ProbeTool {
    fn name(&self) -> &str {
        "Probe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult::text("probe-ok"))
    }
}

/// Second stand-in, so a scene can contribute two tools and have the
/// deferred policy apply to one of them but not the other.
pub(super) struct SecondProbeTool;
#[async_trait::async_trait]
impl base::tool::Tool for SecondProbeTool {
    fn name(&self) -> &str {
        "SecondProbe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult::text("second-probe-ok"))
    }
}

/// Test tool that never finishes on its own — it parks until the turn's
/// token fires. Stands in for whatever a user presses Esc on: a long
/// command, a slow subprocess, a model that won't stop talking.
pub(super) struct HangingTool;
#[async_trait::async_trait]
impl base::tool::Tool for HangingTool {
    fn name(&self) -> &str {
        "Hang"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        ctx.cancel.cancelled().await;
        Err(base::error::ToolError::Cancelled)
    }
}

/// Mock model: 1st call emits a `Probe` tool call, every later call just
/// ends the turn normally. Used to prove the discontinue path actually
/// stops the outer loop — if it didn't, the agent would call the model
/// a 2nd time asking what to do with the tool result.
pub(super) struct ToolThenStopModel {
    pub(super) calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(super) tool: &'static str,
}
#[async_trait::async_trait]
impl Model for ToolThenStopModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let call_no = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events: Vec<
            Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
        > = if call_no == 0 {
            vec![
                Ok(base::interface::model::ModelEvent::ToolUse {
                    id: "toolu_1".into(),
                    name: self.tool.into(),
                    input: serde_json::json!({}),
                }),
                Ok(base::interface::model::ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Default::default(),
                }),
            ]
        } else {
            vec![
                Ok(base::interface::model::ModelEvent::TextDelta {
                    text: "done".into(),
                }),
                Ok(base::interface::model::ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ]
        };
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// Without an owner driving them, `run` starts a goal's rounds itself,
/// one turn each, until the round limit marks the goal blocked.
#[tokio::test]
async fn run_starts_goal_rounds_until_the_limit() {
    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Nope",
    });
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let state = agent.session_state();
    state
        .goal_create(
            "finish".into(),
            Some(2),
            base::context::goal::TurnOrigin::Human,
        )
        .unwrap();
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let engine = tokio::spawn(async move { agent.run(cancel).await });
    input_tx
        .send(InputMessage::User {
            content: "go".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    let turns = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut turns = Vec::new();
        let mut completed = 0;
        while completed < 3 {
            match event_rx.recv().await {
                Some(AgentEvent::TurnStart { turn_id }) => turns.push(turn_id),
                Some(AgentEvent::TurnComplete { .. }) => completed += 1,
                Some(_) => {}
                None => break,
            }
        }
        turns
    })
    .await
    .expect("three turns should run");
    assert_eq!(turns[0], "t1");
    assert!(turns[1].starts_with("goal-1-1-"), "{turns:?}");
    assert!(turns[2].starts_with("goal-1-2-"), "{turns:?}");

    for _ in 0..100 {
        if state.goal_view()["phase"] == "blocked" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(state.goal_view()["phase"], "blocked");
    stop.cancel();
    let _ = engine.await;
}

#[tokio::test]
async fn structured_output_calls_from_earlier_turns_do_not_cut_this_one_short() {
    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Probe",
    });
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(model)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    for i in 0..5 {
        let id = format!("so_{i}");
        agent
            .session
            .push_message(base::interface::model::ModelMessage {
                role: base::interface::model::MessageRole::Assistant,
                content: vec![base::interface::model::ModelContentBlock::ToolUse {
                    id: id.clone(),
                    name: "StructuredOutput".into(),
                    input: serde_json::json!({}),
                }],
            });
        agent
            .session
            .push_message(base::interface::model::ModelMessage {
                role: base::interface::model::MessageRole::User,
                content: vec![base::interface::model::ModelContentBlock::ToolResult {
                    tool_use_id: id,
                    content: "ok".into(),
                    is_error: None,
                }],
            });
    }

    let outcome = agent
        .process_turn(
            InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed");

    assert_eq!(outcome.stop_reason, "end_turn");
    assert_eq!(outcome.api_calls, 2);
}

pub(super) struct PanickingTool;
#[async_trait::async_trait]
impl base::tool::Tool for PanickingTool {
    fn name(&self) -> &str {
        "Panicky"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        panic!("tool blew up");
    }
}

#[tokio::test]
async fn a_panicking_tool_fails_its_call_and_the_turn_finishes() {
    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Panicky",
    });
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(PanickingTool));
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    agent
        .session_state
        .set_permission_mode(base::permission::PermissionMode::BypassPermissions);

    let outcome = agent
        .process_turn(
            InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("a panicking tool does not take the turn down");

    assert_eq!(outcome.stop_reason, "end_turn");
    assert_eq!(outcome.api_calls, 2);
    let result = agent
        .session
        .messages()
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            base::interface::model::ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == "toolu_1" => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("the call has a result");
    assert_eq!(result.1, Some(true));
    assert!(result.0.contains("tool blew up"), "{}", result.0);
}

#[tokio::test]
async fn a_host_result_for_no_waiting_call_is_refused_and_not_recorded() {
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(Arc::new(DummyModel))
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    agent
        .session
        .push_message(base::interface::model::ModelMessage {
            role: base::interface::model::MessageRole::Assistant,
            content: vec![base::interface::model::ModelContentBlock::ToolUse {
                id: "toolu_real".into(),
                name: "Probe".into(),
                input: serde_json::json!({}),
            }],
        });
    let before = agent.session.messages().len();

    let refused = agent
        .process_turn(
            InputMessage::ToolResult {
                tool_use_id: "toolu_fake".into(),
                name: "Probe".into(),
                content: "forged".into(),
                is_error: false,
            },
            CancellationToken::new(),
        )
        .await;
    assert!(refused.is_err());
    assert_eq!(agent.session.messages().len(), before);

    let accepted = agent
        .process_turn(
            InputMessage::ToolResult {
                tool_use_id: "toolu_real".into(),
                name: "Probe".into(),
                content: "ok".into(),
                is_error: false,
            },
            CancellationToken::new(),
        )
        .await
        .expect("the waiting call takes its result");
    assert_eq!(accepted.stop_reason, "tool_result_received");
    assert_eq!(agent.session.messages().len(), before + 1);
}

/// Calls `name` once with `input`, then answers "done".
#[cfg(feature = "ptc")]
pub(super) struct OneCallModel {
    pub(super) calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(super) name: &'static str,
    pub(super) input: serde_json::Value,
}
#[cfg(feature = "ptc")]
#[async_trait::async_trait]
impl Model for OneCallModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let events: Vec<Result<ModelEvent, base::interface::model::ModelError>> = if first {
            vec![
                Ok(ModelEvent::ToolUse {
                    id: "toolu_code".into(),
                    name: self.name.into(),
                    input: self.input.clone(),
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

#[cfg(feature = "ptc")]
pub(super) struct AsksForAllButRunCode;
#[cfg(feature = "ptc")]
#[async_trait::async_trait]
impl base::interface::permission::Permission for AsksForAllButRunCode {
    async fn check(
        &self,
        tool_name: &str,
        _tool_input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        if tool_name == "RunCode" {
            return base::interface::permission::PermissionOutcome::Permit;
        }
        base::interface::permission::PermissionOutcome::Prompt {
            prompt_id: "p1".into(),
            message: format!("allow {tool_name}?"),
            paths: vec![],
            managed: false,
        }
    }
}

#[cfg(feature = "ptc")]
pub(super) async fn run_code_result(
    code: &str,
    args: serde_json::Value,
    permission: Option<Arc<dyn base::interface::permission::Permission>>,
) -> (String, bool) {
    let template = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&template);
    let model: Arc<dyn Model> = Arc::new(OneCallModel {
        calls: Default::default(),
        name: "RunCode",
        input: serde_json::json!({"code": code, "args": args}),
    });
    let mut builder = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(template)
        .skip_warmup(true);
    if let Some(p) = permission {
        builder = builder.permission(p);
    }
    let (mut agent, mut event_rx, _input_tx) = builder.build().expect("build should succeed");
    assert!(
        agent.tooling.tools.get("RunCode").is_some(),
        "the code scene offers RunCode"
    );
    agent
        .process_turn(
            InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the turn runs");
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::ToolResult {
            name,
            content,
            is_error,
            ..
        } = event
        {
            if name == "RunCode" {
                return (content, is_error.unwrap_or(false));
            }
        }
    }
    panic!("no RunCode result");
}

#[cfg(feature = "ptc")]
#[tokio::test(flavor = "multi_thread")]
async fn a_program_reads_files_through_the_sessions_own_tools() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
    let (content, is_error) = run_code_result(
        "const texts = await Promise.all(['a.txt', 'b.txt'].map(f =>
            tools.Read({file_path: args.dir + '/' + f})));
         return texts.map(t => t.includes('alpha') ? 'a' : t.includes('beta') ? 'b' : '?');",
        serde_json::json!({"dir": dir.path()}),
        None,
    )
    .await;
    assert!(!is_error, "{content}");
    assert!(content.starts_with(r#"["a","b"]"#), "{content}");
    assert!(content.contains("(2 tool calls, 0 failed)"), "{content}");
}

#[cfg(feature = "ptc")]
#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_needs_approval_fails_instead_of_asking() {
    let (content, _) = run_code_result(
        "try { await tools.Read({file_path: '/etc/hosts'}); return 'ran'; }
         catch (e) { return e.message; }",
        serde_json::json!({}),
        Some(Arc::new(AsksForAllButRunCode)),
    )
    .await;
    assert!(
        content.contains("a program cannot ask for"),
        "the program should see the refusal, got: {content}"
    );
}

#[tokio::test]
async fn task_created_fires_when_a_running_task_starts() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("task_created.json");
    let mut settings = test_settings();
    settings.hooks_config = Some(serde_json::json!({
        "TaskCreated": [
            { "type": "command", "command": format!("cat > '{}'", seen.display()) }
        ]
    }));
    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(Arc::new(DummyModel))
        .settings(Arc::new(settings))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    agent.session_state.register_task(
        "t-1",
        base::context::TaskType::Agent,
        CancellationToken::new(),
        serde_json::json!({"agent_type": "explore"}),
    );

    let mut payload = None;
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(&seen) {
            if !text.is_empty() {
                payload = Some(text);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let payload: serde_json::Value = serde_json::from_str(&payload.expect("the hook ran")).unwrap();
    assert_eq!(payload["hook_event_name"], "TaskCreated");
    let task: serde_json::Value =
        serde_json::from_str(payload["reason"].as_str().unwrap()).unwrap();
    assert_eq!(task["task_id"], "t-1");
}

#[tokio::test]
async fn task_completed_fires_when_a_running_task_ends_however_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("task_completed.jsonl");
    let mut settings = test_settings();
    settings.hooks_config = Some(serde_json::json!({
        "TaskCompleted": [
            { "type": "command", "command": format!("cat >> '{}'", seen.display()) }
        ]
    }));
    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(Arc::new(DummyModel))
        .settings(Arc::new(settings))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let endings = [
        (
            "t-done",
            base::context::TaskType::Code,
            base::context::RunningStatus::Completed,
        ),
        (
            "t-failed",
            base::context::TaskType::Agent,
            base::context::RunningStatus::Failed("boom".into()),
        ),
        (
            "t-stopped",
            base::context::TaskType::Teammate,
            base::context::RunningStatus::Cancelled,
        ),
    ];
    for (id, kind, _) in &endings {
        agent.session_state.register_task(
            *id,
            *kind,
            CancellationToken::new(),
            serde_json::json!({}),
        );
    }
    for (id, _, status) in endings {
        agent.session_state.finish_task(id, status);
    }

    let mut tasks: Vec<serde_json::Value> = Vec::new();
    for _ in 0..300 {
        tasks = std::fs::read_to_string(&seen)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .inspect(|p| assert_eq!(p["hook_event_name"], "TaskCompleted"))
            .map(|p| serde_json::from_str(p["reason"].as_str().unwrap()).unwrap())
            .collect();
        if tasks.len() == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let ended = |id: &str| {
        tasks
            .iter()
            .find(|t| t["task_id"] == id)
            .map(|t| (t["kind"].clone(), t["status"].clone()))
            .unwrap_or_else(|| panic!("no TaskCompleted for {id}: {tasks:?}"))
    };
    assert_eq!(
        ended("t-done"),
        (serde_json::json!("code"), serde_json::json!("done"))
    );
    assert_eq!(
        ended("t-failed"),
        (serde_json::json!("agent"), serde_json::json!("failed"))
    );
    assert_eq!(
        ended("t-stopped"),
        (serde_json::json!("teammate"), serde_json::json!("stopped"))
    );
}

#[tokio::test]
async fn an_async_rewake_hook_runs_again_once_a_running_task_ends() {
    let dir = tempfile::tempdir().unwrap();
    let woken = dir.path().join("woken");
    let mut settings = test_settings();
    settings.hooks_config = Some(serde_json::json!({
        "SessionStart": [{
            "type": "command",
            "async_rewake": true,
            "command": format!(
                "input=$(cat); case \"$input\" in *WakeEvent*) echo woken > '{}' ;; \
                 *) echo '{{\"rewake\":true}}' ;; esac",
                woken.display()
            )
        }]
    }));
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(Arc::new(FailingModel))
        .settings(Arc::new(settings))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let session = agent.session_state.clone();
    let task = session.register_task(
        "t-1",
        base::context::TaskType::Agent,
        CancellationToken::new(),
        serde_json::json!({}),
    );
    let cancel = CancellationToken::new();
    let running = tokio::spawn({
        let cancel = cancel.clone();
        async move { agent.run(cancel).await }
    });

    async fn one_turn(
        input: &mpsc::UnboundedSender<InputMessage>,
        events: &mut mpsc::UnboundedReceiver<AgentEvent>,
        turn_id: &str,
    ) {
        input
            .send(InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: turn_id.into(),
            })
            .unwrap();
        loop {
            match events.recv().await.expect("the session is running") {
                AgentEvent::TurnComplete { .. } | AgentEvent::Error { .. } => return,
                _ => {}
            }
        }
    }

    one_turn(&input_tx, &mut event_rx, "t1").await;
    one_turn(&input_tx, &mut event_rx, "t2").await;
    assert!(
        !woken.exists(),
        "nothing it waits on has finished, so the hook is not run again"
    );

    session.finish_task(&task.task_id, base::context::RunningStatus::Completed);
    let mut ran_again = false;
    for turn in 3..50 {
        one_turn(&input_tx, &mut event_rx, &format!("t{turn}")).await;
        if woken.exists() {
            ran_again = true;
            break;
        }
    }
    assert!(ran_again, "the task's end wakes the hook");

    cancel.cancel();
    running.await.unwrap();
}

#[tokio::test]
async fn a_tool_hook_is_told_which_tool_use_it_is_about_and_the_live_mode() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("pre_tool_use.json");
    let mut settings = test_settings();
    settings.hooks_config = Some(serde_json::json!({
        "PreToolUse": [
            { "type": "command", "command": format!("cat > '{}'", seen.display()) }
        ]
    }));
    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Probe",
    });
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(ProbeTool));
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(model)
        .settings(Arc::new(settings))
        .tools(tools)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    agent
        .session_state
        .set_permission_mode(base::permission::PermissionMode::Plan);

    agent
        .process_turn(
            InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed");

    let payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&seen).expect("the hook ran")).unwrap();
    assert_eq!(payload["tool_use_id"], "toolu_1");
    assert_eq!(payload["permission_mode"], "plan");
}

#[tokio::test]
async fn post_tool_use_hook_discontinue_ends_the_turn_early() {
    // A PostToolUse hook returning `continue: false` must stop
    // the turn right after the tool that triggered it, instead of
    // looping back to the model for another round. Proven two ways:
    // stop_reason == "stopped_by_hook", and the model is called exactly
    // once (a working-but-broken discontinue would still call it a 2nd
    // time to hand back the tool result).
    let mut settings = test_settings();
    settings.hooks_config = Some(serde_json::json!({
        "PostToolUse": [
            { "type": "command", "command": "echo '{\"continue\":false}'" }
        ]
    }));
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: call_count.clone(),
        tool: "Probe",
    });
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(ProbeTool));

    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(settings))
        .tools(tools)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let outcome = agent
        .process_turn(
            InputMessage::User {
                content: "please use the probe tool".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed");

    assert_eq!(outcome.stop_reason, "stopped_by_hook");
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "model should only have been called once — the turn must not have looped back for a 2nd round"
    );
}

/// Returns `EndTurn` with no `ToolUse` on the very first call whose
/// first message text contains `"You are selecting memories"` — that's
/// `select_memories_from`'s fixed system-prompt opener, the one
/// reliable way to tell "this is the memory-prefetch call" apart from
/// the turn's own main model call using only the `Model` trait's public
/// interface. The prefetch branch returns a stream that never yields
/// anything (`futures::stream::pending()`) — simulating a slow/stuck
/// provider call — so a test asserting the *turn* still completes
/// promptly is actually exercising the abort-on-no-tool-use path, not
/// just getting lucky with timing.
pub(super) struct AnswersDirectlySlowPrefetchModel;

#[async_trait::async_trait]
impl Model for AnswersDirectlySlowPrefetchModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<base::interface::model::ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let is_prefetch = messages.first().is_some_and(|m| {
            m.content.iter().any(|b| {
                matches!(
                    b,
                    base::interface::model::ModelContentBlock::Text { text }
                        if text.contains("You are selecting memories")
                )
            })
        });
        if is_prefetch {
            return Ok(Box::new(futures::stream::pending()));
        }
        let events: Vec<
            Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
        > = vec![
            Ok(base::interface::model::ModelEvent::TextDelta {
                text: "no tools needed here".into(),
            }),
            Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// A turn that answers directly (no tool calls) aborts the memory
/// prefetch rather than awaiting it — the result would only be injected
/// when `has_tool_uses` is true — so a slow prefetch cannot hold up the
/// response.
#[tokio::test]
async fn turn_without_tool_use_does_not_wait_on_a_slow_memory_prefetch() {
    let mut settings = test_settings();
    settings.memory_enabled = true;
    let model: Arc<dyn Model> = Arc::new(AnswersDirectlySlowPrefetchModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());

    // >5 entries so `select_memories_from` doesn't short-circuit
    // (skip the model call entirely) before ever reaching our mock's
    // prefetch branch — see its own "headers.len() <= max_results"
    // early return.
    let tmp = tempfile::tempdir().unwrap();
    let memory_store = Arc::new(base::memory::MemoryStore::new(
        tmp.path().join("user"),
        tmp.path().join("local"),
    ));
    let memories: Vec<base::memory::DurableMemory> = (0..8)
        .map(|i| base::memory::DurableMemory {
            name: format!("mem-{i}"),
            description: format!("test memory {i}"),
            memory_type: Default::default(),
            content: "content".into(),
            source_session_id: String::new(),
            confidence: 0.8,
            last_seen: String::new(),
            recall_count: 0,
        })
        .collect();
    memory_store.persist_batch(memories).unwrap();

    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(settings))
        .memory_store(memory_store)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.process_turn(
            InputMessage::User {
                content: "just answer, don't use any tools".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        ),
    )
    .await
    .expect(
        "turn should have completed well within 5s — it must not have waited on the \
         (deliberately never-resolving) memory prefetch",
    )
    .expect("process_turn should succeed");

    assert_eq!(outcome.stop_reason, "end_turn");
}

/// Always asks. Enough to drive a real permission prompt through the
/// engine loop.
pub(super) struct PromptingPermission;
#[async_trait::async_trait]
impl base::interface::permission::Permission for PromptingPermission {
    async fn check(
        &self,
        tool_name: &str,
        _tool_input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Prompt {
            prompt_id: "p1".into(),
            message: format!("allow {tool_name}?"),
            paths: vec![],
            managed: false,
        }
    }
}

/// A turn blocked on a permission prompt must receive its answer while
/// `Agent::run` is `await`ing it — the reason the input demultiplexer
/// exists. This drives the real loop rather than simulating the message.
#[tokio::test]
async fn a_permission_answer_reaches_a_turn_that_is_blocked_waiting_for_it() {
    // Local rather than reusing `turn/tests/tool_exec.rs`'s ProbeTool — that lives in a
    // private `mod tests` and exporting it just for this would widen its
    // visibility for one caller.
    struct ProbeTool;
    #[async_trait::async_trait]
    impl base::tool::Tool for ProbeTool {
        fn name(&self) -> &str {
            "Probe"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult::text("probe-ran"))
        }
    }

    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool));

    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Probe",
    });
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .permission(Arc::new(PromptingPermission))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let cancel = CancellationToken::new();
    let engine = tokio::spawn(async move { agent.run(cancel).await });

    input_tx
        .send(InputMessage::User {
            content: "use the probe tool".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    // Wait for the engine to actually ask.
    let prompt_id = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::PermissionPrompt { prompt_id, .. }) => break prompt_id,
                Some(_) => continue,
                None => panic!("event channel closed before any permission prompt"),
            }
        }
    })
    .await
    .expect("engine should have emitted a permission prompt");

    // Answer it on the same channel the daemon uses.
    input_tx
        .send(InputMessage::PermissionResponse {
            prompt_id,
            decision: PermissionDecision::Permit,
        })
        .unwrap();

    // Without the demultiplexer this deadlocks until the timeout.
    let completed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                Some(_) => continue,
                None => panic!("event channel closed before the turn completed"),
            }
        }
    })
    .await
    .expect(
        "the turn must complete once its permission answer arrives — if this times out, \
         the answer is not reaching the blocked tool call",
    );
    assert_eq!(completed, "end_turn");

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// `EngineCommand::CancelTurn` must interrupt the turn in flight and
/// leave the session able to take the next message.
///
/// The second half of this test — a normal turn served *after* the
/// cancellation — is what tells a per-turn cancel from a session shutdown.
///
/// It also pins the delivery path. `CancelTurn` is handled in the input
/// demultiplexer, not the main loop, because the main loop is inside
/// `process_turn().await` for the entire time a turn is running; handled
/// there, this test would hang until the timeout with the tool still
/// parked.
#[tokio::test]
async fn cancel_turn_interrupts_the_turn_and_leaves_the_session_usable() {
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(HangingTool));

    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Hang",
    });
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let session_cancel = CancellationToken::new();
    let engine = {
        let session_cancel = session_cancel.clone();
        tokio::spawn(async move { agent.run(session_cancel).await })
    };

    input_tx
        .send(InputMessage::User {
            content: "start something long".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    // Wait until the tool is actually running, so the cancel lands
    // mid-turn rather than before the turn starts.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::ToolUse { name, .. }) if name == "Hang" => break,
                Some(_) => continue,
                None => panic!("event channel closed before the tool ran"),
            }
        }
    })
    .await
    .expect("the hanging tool should have started");

    input_tx
        .send(InputMessage::System {
            kind: EngineCommand::CancelTurn { turn_id: None },
            content: String::new(),
        })
        .unwrap();

    let stop_reason = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                Some(_) => continue,
                None => panic!("event channel closed before the turn ended"),
            }
        }
    })
    .await
    .expect("CancelTurn must reach the running turn while it is still running");
    assert_eq!(stop_reason, "cancelled");
    assert!(
        !session_cancel.is_cancelled(),
        "cancelling a turn must not cancel the session"
    );

    // The session is still live: a second turn runs to completion.
    input_tx
        .send(InputMessage::User {
            content: "still there?".into(),
            attachments: vec![],
            turn_id: "t2".into(),
        })
        .unwrap();
    let second = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                Some(_) => continue,
                None => panic!("event channel closed — the engine died with the turn"),
            }
        }
    })
    .await
    .expect("the session must still serve turns after one was cancelled");
    assert_eq!(second, "end_turn");

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// `CancelTurn` with a `turn_id` lands on that turn: sent before the turn's
/// message, the turn starts cancelled; sent for a turn that has ended, the
/// next one runs as usual.
#[tokio::test]
async fn cancel_turn_lands_on_the_turn_it_names() {
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(super::events_and_steering::PlainReplyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    let cancel = |turn_id: &str| {
        input_tx
            .send(InputMessage::System {
                kind: EngineCommand::CancelTurn {
                    turn_id: Some(turn_id.into()),
                },
                content: String::new(),
            })
            .unwrap()
    };
    let turn = |turn_id: &str| {
        input_tx
            .send(InputMessage::User {
                content: "hello".into(),
                attachments: vec![],
                turn_id: turn_id.into(),
            })
            .unwrap()
    };
    async fn ended(event_rx: &mut EventReceiver) -> String {
        match super::events_and_steering::next_event(
            event_rx,
            |e| matches!(e, AgentEvent::TurnComplete { .. }),
            "the end of the turn",
        )
        .await
        {
            AgentEvent::TurnComplete { stop_reason, .. } => stop_reason,
            _ => unreachable!(),
        }
    }

    cancel("t1");
    turn("t1");
    assert_eq!(ended(&mut event_rx).await, "cancelled");
    cancel("t1");
    turn("t2");
    assert_eq!(ended(&mut event_rx).await, "end_turn");

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// Cancelling the session's own token still cascades into the turn in
/// flight — per-turn tokens are children, not replacements.
#[tokio::test]
async fn cancelling_the_session_still_stops_the_turn_in_flight() {
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(HangingTool));

    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Hang",
    });
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let session_cancel = CancellationToken::new();
    let engine = {
        let session_cancel = session_cancel.clone();
        tokio::spawn(async move { agent.run(session_cancel).await })
    };

    input_tx
        .send(InputMessage::User {
            content: "start something long".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::ToolUse { name, .. }) if name == "Hang" => break,
                Some(_) => continue,
                None => panic!("event channel closed before the tool ran"),
            }
        }
    })
    .await
    .expect("the hanging tool should have started");

    session_cancel.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(10), engine)
        .await
        .expect("the engine must exit when its own token is cancelled")
        .expect("engine task panicked");
}

/// A `UserPromptSubmit` hook's `decision: "block"` refuses to process
/// the message at all: the turn ends immediately with
/// `stopped_by_hook`, and the model must never be called.
#[tokio::test]
async fn user_prompt_submit_hook_block_ends_the_turn_before_any_model_call() {
    let mut settings = test_settings();
    settings.hooks_config = Some(serde_json::json!({
        "UserPromptSubmit": [
            { "type": "command", "command": "echo '{\"decision\":\"block\",\"message\":\"not now\"}'" }
        ]
    }));
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: call_count.clone(),
        tool: "Probe",
    });
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());

    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(settings))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let outcome = agent
        .process_turn(
            InputMessage::User {
                content: "please use the probe tool".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed");

    assert_eq!(outcome.stop_reason, "stopped_by_hook");
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the model must never be called once UserPromptSubmit blocked the message"
    );
}

/// `updated_input` should rewrite the message text that actually gets
/// processed — same field `PreToolUse` uses for rewriting tool input,
/// repurposed for the one thing there is to rewrite here.
#[tokio::test]
async fn user_prompt_submit_hook_rewrites_the_message_content() {
    let mut settings = test_settings();
    settings.hooks_config = Some(serde_json::json!({
        "UserPromptSubmit": [
            { "type": "command", "command": "echo '{\"updated_input\":\"REWRITTEN\"}'" }
        ]
    }));
    // Not `DummyModel` — its `stream()` is `unimplemented!()`, and this
    // test (unlike the block test above) lets the turn actually reach
    // the model call.
    let model: Arc<dyn Model> = Arc::new(AnswersDirectlySlowPrefetchModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());

    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(settings))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    agent
        .process_turn(
            InputMessage::User {
                content: "original message".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed");

    let pushed_text = agent
        .session
        .messages()
        .iter()
        .find_map(|m| {
            m.content.iter().find_map(|b| match b {
                base::interface::model::ModelContentBlock::Text { text }
                    if text == "original message" || text == "REWRITTEN" =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
        })
        .expect("the user message text should be present in session.messages()");
    assert_eq!(
        pushed_text, "REWRITTEN",
        "the hook's updated_input should have replaced the original message content"
    );
}
