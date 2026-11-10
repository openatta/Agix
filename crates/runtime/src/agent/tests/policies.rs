//! Hook runner construction and the host-supplied extension points: wrappers, retrievers, policies, assemblers.

use super::*;

#[test]
fn build_hook_runner_defaults_to_empty_without_hooks_config() {
    let settings = test_settings();
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let runner = build_hook_runner(
        &settings,
        model,
        Arc::new(crate::module::LateAgentSpawner::default()),
        None,
    );
    assert!(runner.is_empty());
}

#[test]
fn build_hook_runner_parses_hooks_config_into_real_hooks() {
    let mut settings = test_settings();
    settings.hooks_config = Some(serde_json::json!({
        "PreToolUse": [
            { "type": "command", "command": "echo hi" }
        ]
    }));
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let runner = build_hook_runner(
        &settings,
        model,
        Arc::new(crate::module::LateAgentSpawner::default()),
        None,
    );
    assert!(!runner.is_empty());
    assert!(runner.has_hooks_for(hooks::HookEvent::PreToolUse));
}

/// The unwired-event warning must stay a *warning*. Refusing the hook, or
/// dropping it from the runner, would be a behavior change dressed up as
/// a diagnostic — and it would break anyone who has such a hook
/// configured today against the day the event gets wired up.
#[test]
fn build_hook_runner_keeps_hooks_for_unwired_events_and_only_warns() {
    let mut settings = test_settings();
    settings.hooks_config = Some(serde_json::json!({
        "ConfigChange": [
            { "type": "command", "command": "echo reconfigured" }
        ],
        "PreToolUse": [
            { "type": "command", "command": "echo hi" }
        ]
    }));
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let runner = build_hook_runner(
        &settings,
        model,
        Arc::new(crate::module::LateAgentSpawner::default()),
        None,
    );
    assert!(
        runner.has_hooks_for(hooks::HookEvent::ConfigChange),
        "an unwired event's hook must still be registered — the engine warns, it does \
         not silently discard"
    );
    assert!(
        runner.has_hooks_for(hooks::HookEvent::PreToolUse),
        "an unwired event in the same config must not affect the wired ones"
    );
}

#[test]
fn build_hook_runner_degrades_to_empty_on_malformed_hooks_config() {
    let mut settings = test_settings();
    // Wrong shape: hooks value must be a map of event -> Vec<HookConfig>.
    settings.hooks_config = Some(serde_json::json!("not-a-hooks-map"));
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let runner = build_hook_runner(
        &settings,
        model,
        Arc::new(crate::module::LateAgentSpawner::default()),
        None,
    );
    assert!(runner.is_empty());
}

/// `Builder::build()` enables file watching, so a configured
/// `FileChanged` hook fires in a real session. This exercises the actual
/// wiring: build an `Agent` with a `FileChanged` hook configured and an
/// instruction file, edit that file, and confirm the hook really runs.
#[tokio::test]
async fn build_starts_file_watching_for_the_instruction_file_when_a_hook_wants_it() {
    // No sleeps guessing at machine speed: the hook is observed in this
    // process through an injected executor, and the file is rewritten
    // until the notification arrives — so a watcher that registers late
    // costs another loop rather than a failure.
    struct RecordingPromptExecutor {
        fired: tokio::sync::mpsc::Sender<()>,
    }

    #[async_trait::async_trait]
    impl hooks::runner::PromptHookExecutor for RecordingPromptExecutor {
        async fn execute(
            &self,
            _prompt: &str,
            _model: Option<&str>,
            _payload: &hooks::HookInput,
        ) -> Result<String, String> {
            let _ = self.fired.try_send(());
            Ok("{}".to_string())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let instruction_path = dir.path().join("AGENTS.md");
    std::fs::write(&instruction_path, "# instructions").unwrap();

    let (fired_tx, mut fired_rx) = tokio::sync::mpsc::channel(1);
    let mut hooks_settings: hooks::HooksSettings = Default::default();
    hooks_settings.insert(
        hooks::HookEvent::FileChanged,
        vec![hooks::config::HookConfig::Prompt {
            prompt: "the instruction file changed".into(),
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

    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());
    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(test_settings()))
        .hooks(runner)
        .instruction(instruction_path.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert!(agent.hooks().has_hooks_for(hooks::HookEvent::FileChanged));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut edits = 0u32;
    let fired = loop {
        edits += 1;
        std::fs::write(&instruction_path, format!("# instructions, edit {edits}")).unwrap();
        match tokio::time::timeout(std::time::Duration::from_millis(500), fired_rx.recv()).await {
            Ok(Some(())) => break true,
            Ok(None) => break false,
            Err(_) if std::time::Instant::now() >= deadline => break false,
            Err(_) => continue,
        }
    };

    assert!(
        fired,
        "editing the watched instruction file should have fired the FileChanged hook \
         ({edits} edits over 30s without one)"
    );
}

/// A wrapper gives one tool a deadline, and the tool
/// actually feels it. Without the wrapper `Hang` waits for the session's
/// own cancellation, which this test never sends.
#[tokio::test(flavor = "multi_thread")]
async fn a_registered_wrapper_can_put_a_deadline_on_one_tool() {
    use base::interface::tool_middleware::{
        NextDispatch, ToolCall, ToolExec, ToolMiddleware, ToolOutcome,
    };

    struct DeadlineFor {
        tool: &'static str,
        after: std::time::Duration,
        wrapped: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ToolMiddleware for DeadlineFor {
        async fn around(
            &self,
            call: &ToolCall,
            exec: &mut ToolExec,
            next: NextDispatch<'_>,
        ) -> ToolOutcome {
            if call.name == self.tool {
                self.wrapped
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                exec.with_timeout(self.after);
            }
            next.run(call, exec).await
        }
    }

    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(HangingTool));
    let wrapped = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Hang",
    });
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .tool_middleware(Arc::new(DeadlineFor {
            tool: "Hang",
            after: std::time::Duration::from_millis(50),
            wrapped: wrapped.clone(),
        }))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let session = CancellationToken::new();
    let engine = {
        let session = session.clone();
        tokio::spawn(async move { agent.run(session).await })
    };
    input_tx
        .send(InputMessage::User {
            content: "hang please".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    // The turn completes without the session ever being cancelled, which
    // it could not do if `Hang` were still waiting on the session token.
    let stop = tokio::time::timeout(std::time::Duration::from_secs(20), async {
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
        "the wrapper's deadline must end the hanging tool — if this times out, the \
         signal it installed is not reaching dispatch",
    );
    assert_eq!(stop, "end_turn");
    assert_eq!(
        wrapped.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the wrapper must have seen the call it was registered for"
    );

    session.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// A deployment can guarantee a string never reaches the
/// model, and the guarantee holds through the path a tool result actually
/// takes — including when the tool failed.
#[tokio::test(flavor = "multi_thread")]
async fn a_result_transformer_is_the_last_thing_to_touch_a_tool_result() {
    use base::interface::tool_result::RedactLiterals;

    struct LeaksATokenTool;
    #[async_trait::async_trait]
    impl base::tool::Tool for LeaksATokenTool {
        fn name(&self) -> &str {
            "Leak"
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
            Ok(base::tool::ToolResult {
                content: base::tool::ToolResultContent::Text(
                    "ANTHROPIC_API_KEY=sk-live-abc123".into(),
                ),
                ..Default::default()
            })
        }
    }

    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(LeaksATokenTool));

    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Leak",
    });
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .tool_result_transformer(Arc::new(RedactLiterals::new(["sk-live-abc123"])))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "run it".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    let result_text = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::ToolResult { content, .. }) => break content,
                Some(AgentEvent::TurnComplete { .. }) => {
                    panic!("the turn ended without a tool result")
                }
                Some(_) => continue,
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("the tool should have run");

    assert!(
        !result_text.contains("sk-live-abc123"),
        "the secret reached the result the model is shown: {result_text}"
    );
    assert!(result_text.contains("<redacted>"), "{result_text}");

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// Recall goes through the contract, so a host can replace the
/// judgement with something deterministic — and a hook can see both ends
/// of it — without touching the engine.
#[tokio::test(flavor = "multi_thread")]
async fn a_substituted_retriever_and_its_hooks_decide_what_is_recalled() {
    use base::interface::memory_contracts::{
        MemoryRetriever, MemoryStorage, RetrievalHook, RetrievalRequest,
    };

    struct Records {
        asked: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl MemoryRetriever for Records {
        async fn retrieve(
            &self,
            _storage: &dyn MemoryStorage,
            _model: &dyn Model,
            request: &RetrievalRequest,
        ) -> Vec<String> {
            self.asked.lock().unwrap().push(request.query.clone());
            vec!["from-the-substitute".to_string()]
        }
    }

    struct Expands;
    impl RetrievalHook for Expands {
        fn before_retrieve(&self, request: &mut RetrievalRequest) {
            request.query = format!("[expanded] {}", request.query);
        }
    }

    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model: Arc<dyn Model> = Arc::new(PlainReplyModel);
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .memory_retriever(Arc::new(Records {
            asked: asked.clone(),
        }))
        .retrieval_hook(Arc::new(Expands))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "what did we decide about deploys".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::TurnComplete { .. }) => break,
                Some(_) => continue,
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("the turn should complete");

    let asked = asked.lock().unwrap().clone();
    assert_eq!(
        asked.len(),
        1,
        "recall must have gone through the substitute"
    );
    assert_eq!(
        asked[0], "[expanded] what did we decide about deploys",
        "the hook must have seen the question before the retriever did"
    );

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// Emits many text deltas, so a per-chunk hook would be obvious.
pub(super) struct ChattyModel;

#[async_trait::async_trait]
impl Model for ChattyModel {
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
        let mut events: Vec<
            Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
        > = (0..64)
            .map(|i| {
                Ok(base::interface::model::ModelEvent::TextDelta {
                    text: format!("chunk {i} "),
                })
            })
            .collect();
        events.push(Ok(base::interface::model::ModelEvent::EndTurn {
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        }));
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// The interceptor sees the request and the finished
/// message, and is *not* called per chunk. Sixty-four deltas produce one
/// message, and the counts say so.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_interceptor_sees_whole_things_never_chunks() {
    use base::interface::model_interceptor::{ModelInterceptor, ModelRequestView};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counting {
        requests: AtomicUsize,
        messages: AtomicUsize,
    }

    impl ModelInterceptor for Counting {
        fn on_request(&self, request: &mut ModelRequestView) {
            self.requests.fetch_add(1, Ordering::SeqCst);
            request.params.max_tokens = 4096;
        }
        fn on_message(&self, _message: &mut base::interface::model::ModelMessage) {
            self.messages.fetch_add(1, Ordering::SeqCst);
        }
    }

    let counting = Arc::new(Counting::default());
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(ChattyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .model_interceptor(counting.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "say a lot".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    let deltas = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut deltas = 0usize;
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::TextDelta { .. }) => deltas += 1,
                Some(AgentEvent::TurnComplete { .. }) => break deltas,
                Some(_) => continue,
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("the turn should complete");

    assert!(deltas > 20, "the model streamed {deltas} deltas");
    assert_eq!(
        counting.requests.load(Ordering::SeqCst),
        1,
        "one request, one call"
    );
    let messages = counting.messages.load(Ordering::SeqCst);
    assert_eq!(
        messages, 1,
        "{deltas} chunks must produce one interception, not {messages}"
    );

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// A host changes "how many steps before stopping"
/// with a policy, and the loop is not touched.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_policy_decides_when_the_loop_stops() {
    use base::interface::turn_policy::{TurnPolicy, TurnProgress, TurnStep};

    struct StopAfterOne;
    impl TurnPolicy for StopAfterOne {
        fn before_model_call(&self, progress: &TurnProgress<'_>) -> TurnStep {
            if progress.api_calls >= 1 {
                return TurnStep::stop("host_said_enough");
            }
            TurnStep::Continue
        }
    }

    // Asks for a tool every time, so only a stop condition ends this.
    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Probe",
    });
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(ProbeTool));

    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .turn_policy(Arc::new(StopAfterOne))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "go".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    let stop = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                Some(_) => continue,
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("the policy should have ended the turn");

    assert_eq!(
        stop, "host_said_enough",
        "the policy's reason must reach the host, not be translated into the engine's"
    );

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

#[test]
fn the_settings_compaction_numbers_reach_the_sessions_context_budget() {
    let mut settings = test_settings();
    settings.compaction.threshold_tokens = Some(1234);
    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(settings))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let budget = agent
        .policies
        .budget_policy
        .context_budget(&agent.scene.token_budget());
    assert_eq!(budget.compact_threshold, 1234);
    assert_eq!(
        budget.compact_keep_recent,
        agent.scene.token_budget().compact_keep_recent,
        "what settings leave out stays the scene's"
    );
}

/// A deployment configures a ceiling on request size and the engine
/// honours it — distinct from `TokenBudget`, which only says when to start
/// compacting.
#[tokio::test(flavor = "multi_thread")]
async fn a_context_hard_cap_ends_a_turn_the_scene_would_have_allowed() {
    use base::interface::budget_policy::{Capped, EngineBudget};

    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Probe",
    });
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(ProbeTool));

    // Zero: the system prompt alone is over it, so the first request is
    // one this deployment has said must never be sent.
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .budget_policy(Arc::new(Capped {
            inner: EngineBudget::new(None),
            context_hard_cap: 0,
        }))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "go".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    let stop = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::TurnComplete { stop_reason, .. }) => break stop_reason,
                Some(_) => continue,
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("the cap should have ended the turn");

    assert_eq!(stop, "context_exceeded");

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// The egress policy is computed correctly *and* it reaches a tool, so
/// this goes through a real turn and asks a tool what its own provider
/// says rather than testing the computation alone.
#[tokio::test(flavor = "multi_thread")]
async fn the_egress_policy_reaches_the_tool_that_has_to_obey_it() {
    use base::interface::exec::Origin;

    struct EgressProbe;
    #[async_trait::async_trait]
    impl base::tool::Tool for EgressProbe {
        fn name(&self) -> &str {
            "Probe"
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
            let blocked = ctx.exec.network.permits("blocked.test", Origin::Agent);
            let allowed = ctx.exec.network.permits("allowed.test", Origin::Agent);
            let endpoint = ctx.exec.network.permits("blocked.test", Origin::Operator);
            Ok(base::tool::ToolResult::text(format!(
                "blocked={blocked} allowed={allowed} endpoint={endpoint}"
            )))
        }
    }

    let mut settings = test_settings();
    settings.sandbox.network_mode = base::context::config::NetworkModeConfig::Allowlist;
    settings.sandbox.allowed_domains = vec!["allowed.test".into()];

    let model: Arc<dyn Model> = Arc::new(ToolThenStopModel {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        tool: "Probe",
    });
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(EgressProbe));

    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(settings))
        .tools(tools)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "go".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    let verdict = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::ToolResult { content, .. }) if content.contains("blocked=") => {
                    break content
                }
                Some(_) => continue,
                None => panic!("event channel closed before the tool answered"),
            }
        }
    })
    .await
    .expect("the probe should have run");

    assert_eq!(
        verdict, "blocked=false allowed=true endpoint=true",
        "the settings have to arrive at the tool's own provider: a configured \
         allowlist that the model cannot feel is the defect this work order \
         was written for"
    );

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// An embedder registers a diagnostic of its own and
/// reads the engine's health back — after handing the agent off to its
/// own task, which is when a supervisor actually asks.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_registers_a_health_check_and_reads_the_report_back() {
    use base::interface::health::{CheckResult, HealthCheck, HealthStatus};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Queue(Arc<AtomicBool>);
    impl HealthCheck for Queue {
        fn name(&self) -> &str {
            "acme.queue"
        }
        fn check(&self) -> CheckResult {
            if self.0.load(Ordering::SeqCst) {
                CheckResult::degraded("queue backed up")
                    .with_details(serde_json::json!({ "depth": 4096 }))
            } else {
                CheckResult::ok("queue drained")
            }
        }
    }

    let backed_up = Arc::new(AtomicBool::new(false));
    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .health_check(Arc::new(Queue(backed_up.clone())))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let health = agent.health();
    // `run()` takes the agent by `&mut self` for the life of the session,
    // so a handle taken before that is the only way to ask afterwards.
    let mut agent = agent;
    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });

    let report = health.report();
    assert_eq!(report.status, HealthStatus::Ok);
    assert_eq!(report.checks.len(), 1);
    assert_eq!(report.checks[0].name, "acme.queue");

    backed_up.store(true, Ordering::SeqCst);
    let report = health.report();
    assert_eq!(
        report.status,
        HealthStatus::Degraded,
        "the report is asked fresh every time, not cached at build"
    );
    assert_eq!(report.to_json()["checks"][0]["details"]["depth"], 4096);

    engine.abort();
}

/// A policy turns an overload from "switch to the
/// fallback" into "fail", without the loop knowing anything about it.
#[tokio::test(flavor = "multi_thread")]
async fn a_recovery_policy_can_refuse_the_fallback_the_engine_would_take() {
    use base::interface::recovery_policy::NeverRecover;

    struct AlwaysOverloaded;

    #[async_trait::async_trait]
    impl Model for AlwaysOverloaded {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _p: Vec<base::prompt::PromptBlock>,
            _t: Vec<base::interface::model::ToolDef>,
            _m: Vec<base::interface::model::ModelMessage>,
            _s: base::interface::model::StreamParams,
            _c: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            Err(base::interface::model::ModelError::Overloaded)
        }
    }

    // A fallback *is* configured, so the engine's own policy would switch
    // to it. The point is that the host's policy wins.
    let mut settings = test_settings();
    settings.model.fallback_model = Some("some-other-model".into());

    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(AlwaysOverloaded) as Arc<dyn Model>)
        .settings(Arc::new(settings))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .recovery_policy(Arc::new(NeverRecover))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "go".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    let saw_error = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::Error { message, .. }) => break message,
                Some(AgentEvent::TurnComplete { stop_reason, .. }) => {
                    panic!(
                        "the turn completed with `{stop_reason}` instead of failing — \
                            the policy's refusal did not reach the loop"
                    )
                }
                Some(_) => continue,
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("the turn should have failed");

    assert!(
        !saw_error.contains("some-other-model"),
        "the engine must not have switched to the fallback: {saw_error}"
    );

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

/// Records the prompt blocks of the first request and ends the turn.
pub(super) struct RecordingModel(pub(super) Arc<std::sync::Mutex<Vec<base::prompt::PromptBlock>>>);

#[async_trait::async_trait]
impl Model for RecordingModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let mut first = self.0.lock().unwrap();
        if first.is_empty() {
            *first = prompt_blocks;
        }
        drop(first);
        let events: Vec<
            Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
        > = vec![Ok(base::interface::model::ModelEvent::EndTurn {
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        })];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// Runs one turn and returns the prompt blocks the model was given.
pub(super) async fn prompt_blocks_for_one_turn(
    assembler: Option<Arc<dyn base::interface::prompt_assembler::PromptAssembler>>,
) -> Vec<base::prompt::PromptBlock> {
    let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut builder = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(RecordingModel(recorded.clone())) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .skip_warmup(true);
    if let Some(a) = assembler {
        builder = builder.prompt_assembler(a);
    }
    let (mut agent, mut event_rx, input_tx) = builder.build().expect("build should succeed");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "go".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::TurnComplete { .. }) | None => break,
                Some(_) => continue,
            }
        }
    })
    .await;
    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;

    let blocks = recorded.lock().unwrap().clone();
    assert!(!blocks.is_empty(), "the model was never called");
    blocks
}

/// The assembler is replaceable outright, not merely
/// contributed to. Nothing the kernel would have assembled survives.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_can_replace_prompt_assembly_outright() {
    use base::interface::prompt_assembler::{AssemblyRequest, PromptAssembler};

    struct OnlyThis;
    impl PromptAssembler for OnlyThis {
        fn assemble(&self, _request: &AssemblyRequest<'_>) -> Vec<base::prompt::PromptBlock> {
            vec![base::prompt::PromptBlock::system("only this").named("host.only")]
        }
    }

    let blocks = prompt_blocks_for_one_turn(Some(Arc::new(OnlyThis))).await;
    assert_eq!(
        blocks.len(),
        1,
        "the kernel's stages still reached the request: {:?}",
        blocks.iter().map(|b| &b.name).collect::<Vec<_>>()
    );
    assert_eq!(blocks[0].content, "only this");
}

/// The second implementation, in a real session: same content, one block
/// and one cache breakpoint instead of several.
#[tokio::test(flavor = "multi_thread")]
async fn the_merging_assembler_folds_a_real_session_prompt_into_one_block() {
    use base::interface::prompt_assembler::MergedSystemPrompt;

    let default_blocks = prompt_blocks_for_one_turn(None).await;
    assert!(
        default_blocks.len() > 1,
        "this assertion is only meaningful when the default produces several blocks"
    );

    let merged = prompt_blocks_for_one_turn(Some(Arc::new(MergedSystemPrompt::default()))).await;
    assert_eq!(merged.len(), 1);
    for block in &default_blocks {
        assert!(
            merged[0].content.contains(&block.content),
            "folding dropped a stage: {:?}",
            block.name
        );
    }
}
