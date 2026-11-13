//! Event channels and sinks, permission wire format, mid-turn steering.

use super::*;

#[test]
fn channel_types_construct() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let _sender: EventSender = crate::event_bus::EventBus::new(tx);
}

/// Answers in one text block and stops. Enough to make a turn that emits
/// a handful of events and finishes immediately.
pub(super) struct PlainReplyModel;

#[async_trait::async_trait]
impl Model for PlainReplyModel {
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
        let events: Vec<
            Result<base::interface::model::ModelEvent, base::interface::model::ModelError>,
        > = vec![
            Ok(base::interface::model::ModelEvent::TextDelta {
                text: "hello".into(),
            }),
            Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// Drives one turn to completion and returns the events the host's
/// channel saw.
pub(super) async fn run_one_turn_with_sinks(
    sinks: Vec<Arc<dyn base::interface::event_sink::EventSink>>,
) -> Vec<AgentEvent> {
    let mut builder = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(PlainReplyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .skip_warmup(true);
    for sink in sinks {
        builder = builder.event_sink(sink);
    }
    let (mut agent, mut event_rx, input_tx) = builder.build().expect("build should succeed");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "say something".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();

    let observed = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut seen = Vec::new();
        loop {
            match event_rx.recv().await {
                Some(ev) => {
                    let done = matches!(ev, AgentEvent::TurnComplete { .. });
                    seen.push(ev);
                    if done {
                        break seen;
                    }
                }
                None => panic!("event channel closed before the turn completed"),
            }
        }
    })
    .await
    .expect("the turn should complete");

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    observed
}

/// A host can take the event stream in-process, without standing a
/// forwarding task between the engine's channel and its own world.
#[tokio::test(flavor = "multi_thread")]
async fn a_registered_sink_sees_the_same_stream_as_the_returned_channel() {
    let sink = base::interface::event_sink::CollectingSink::new();
    let observed = run_one_turn_with_sinks(vec![sink.clone()]).await;
    assert!(
        observed.len() >= 2,
        "a turn should emit more than just its completion"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while sink.len() < observed.len() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let as_json = |evs: &[AgentEvent]| {
        evs.iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        as_json(&sink.events())[..observed.len()],
        as_json(&observed)[..],
        "the sink must see the same events, in the same order, as the channel"
    );
}

/// Registering a sink is observation, not control.
///
/// The sink here blocks forever rather than merely being slow, so the
/// test needs no timing threshold to be meaningful: if the turn were
/// waiting on it at any point, the turn would never complete and the
/// helper's timeout would fire.
#[tokio::test(flavor = "multi_thread")]
async fn a_stuck_sink_cannot_stop_a_turn_from_completing() {
    #[derive(Default)]
    struct Gate {
        released: std::sync::Mutex<bool>,
        wake: std::sync::Condvar,
    }
    struct Wedged(Arc<Gate>);
    impl base::interface::event_sink::EventSink for Wedged {
        fn emit(&self, _event: &AgentEvent) {
            let mut released = self.0.released.lock().unwrap();
            while !*released {
                released = self.0.wake.wait(released).unwrap();
            }
        }
    }

    let gate = Arc::new(Gate::default());
    let observed = run_one_turn_with_sinks(vec![Arc::new(Wedged(gate.clone()))]).await;
    assert!(
        observed.len() >= 2,
        "the turn ran to completion past a sink that never returns"
    );

    // Let the lane's task out before the runtime goes away.
    *gate.released.lock().unwrap() = true;
    gate.wake.notify_all();
}

#[test]
fn permission_decision_wire_format_round_trips() {
    let permit = PermissionDecision::Permit;
    let s = serde_json::to_value(&permit).unwrap();
    assert_eq!(s, serde_json::json!({"type": "permit"}));
    assert!(matches!(
        serde_json::from_value::<PermissionDecision>(s).unwrap(),
        PermissionDecision::Permit
    ));

    let deny = PermissionDecision::Deny {
        reason: "no".into(),
    };
    let s = serde_json::to_value(&deny).unwrap();
    assert_eq!(s, serde_json::json!({"type": "deny", "reason": "no"}));
    match serde_json::from_value::<PermissionDecision>(s).unwrap() {
        PermissionDecision::Deny { reason } => assert_eq!(reason, "no"),
        other => panic!("expected Deny, got {other:?}"),
    }

    let permit_always_session = PermissionDecision::PermitAlways {
        scope: PersistScope::Session,
    };
    let s = serde_json::to_value(&permit_always_session).unwrap();
    assert_eq!(
        s,
        serde_json::json!({"type": "permit_always", "scope": "session"})
    );
    match serde_json::from_value::<PermissionDecision>(s).unwrap() {
        PermissionDecision::PermitAlways {
            scope: PersistScope::Session,
        } => {}
        other => panic!("expected PermitAlways{{scope: Session}}, got {other:?}"),
    }

    let permit_always_local = PermissionDecision::PermitAlways {
        scope: PersistScope::Local,
    };
    let s = serde_json::to_value(&permit_always_local).unwrap();
    assert_eq!(
        s,
        serde_json::json!({"type": "permit_always", "scope": "local"})
    );
    match serde_json::from_value::<PermissionDecision>(s).unwrap() {
        PermissionDecision::PermitAlways {
            scope: PersistScope::Local,
        } => {}
        other => panic!("expected PermitAlways{{scope: Local}}, got {other:?}"),
    }

    // Also verify direct JSON string parsing of the exact wire shapes
    // named in the RPC docs, not just round-tripping our own output.
    let from_str: PermissionDecision =
        serde_json::from_str(r#"{"type":"permit_always","scope":"local"}"#).unwrap();
    assert!(matches!(
        from_str,
        PermissionDecision::PermitAlways {
            scope: PersistScope::Local
        }
    ));
}

/// Released by the test, so a tool step lasts exactly as long as the
/// test needs to land a message in the middle of it.
pub(super) struct GateTool(pub(super) Arc<tokio::sync::Notify>);
#[async_trait::async_trait]
impl base::tool::Tool for GateTool {
    fn name(&self) -> &str {
        "Gate"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }

    fn description(&self) -> &str {
        "gate tool (test tool)"
    }

    fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn permission_facts(
        &self,
        _: &serde_json::Value,
        _: &base::tool::ToolContext,
    ) -> base::interface::permission::PermissionFacts {
        base::interface::permission::PermissionFacts::new(
            base::interface::permission::Subject::None,
            base::interface::permission::Reach::SessionOnly,
        )
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        tokio::select! {
            _ = self.0.notified() => Ok(base::tool::ToolResult::text("gate opened")),
            _ = ctx.cancel.cancelled() => Err(base::error::ToolError::Cancelled),
        }
    }
}

/// Calls `tool` once if given, then answers; records what every call
/// was sent. `hold_first` keeps the first call open until released.
pub(super) struct StepRecordingModel {
    pub(super) tool: Option<&'static str>,
    pub(super) hold_first: Option<Arc<tokio::sync::Notify>>,
    pub(super) sent: Arc<std::sync::Mutex<Vec<Vec<base::interface::model::ModelMessage>>>>,
}
#[async_trait::async_trait]
impl Model for StepRecordingModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<base::interface::model::ModelMessage>,
        params: base::interface::model::StreamParams,
        _cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        let turn_call = matches!(
            params.origin.as_ref().map(|o| &o.kind),
            Some(base::interface::model::CallKind::Turn { .. })
        );
        if !turn_call {
            return Ok(Box::new(futures::stream::iter(vec![Ok(
                ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                },
            )])));
        }
        let call_no = {
            let mut sent = self.sent.lock().unwrap();
            sent.push(messages);
            sent.len() - 1
        };
        if call_no == 0 {
            if let Some(hold) = &self.hold_first {
                hold.notified().await;
            }
        }
        let events = match (call_no, self.tool) {
            (0, Some(tool)) => vec![
                Ok(ModelEvent::ToolUse {
                    id: "toolu_1".into(),
                    name: tool.into(),
                    input: serde_json::json!({}),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Default::default(),
                }),
            ],
            _ => vec![
                Ok(ModelEvent::TextDelta {
                    text: format!("answer {call_no}"),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ],
        };
        Ok(Box::new(futures::stream::iter(events)))
    }
}

pub(super) fn text_of(m: &base::interface::model::ModelMessage) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            base::interface::model::ModelContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) async fn next_event(
    rx: &mut EventReceiver,
    mut want: impl FnMut(&AgentEvent) -> bool,
    what: &str,
) -> AgentEvent {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(e) if want(&e) => break e,
                Some(_) => continue,
                None => panic!("event channel closed while waiting for {what}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

pub(super) fn steer(
    steering: &crate::steering::SteerHandle,
    id: &str,
    text: &str,
) -> crate::steering::SteerAck {
    steering.offer(id, text)
}

#[tokio::test]
async fn notices_reach_the_model_at_turn_start_and_at_step_boundaries() {
    let hold = Arc::new(tokio::sync::Notify::new());
    let sent: Arc<std::sync::Mutex<Vec<Vec<base::interface::model::ModelMessage>>>> =
        Default::default();
    let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
        tool: None,
        hold_first: Some(hold.clone()),
        sent: sent.clone(),
    });
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(model)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let state = agent.session_state();
    state.post_notice(None, "notice from before the turn");

    let poster = async {
        while sent.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        state.post_notice(None, "notice from during the turn");
        hold.notify_one();
    };
    let turn = agent.run_turn("the question".into(), "t1".into(), CancellationToken::new());
    let (outcome, ()) = tokio::join!(turn, poster);
    outcome.expect("the turn runs");

    let text = |m: &base::interface::model::ModelMessage| format!("{:?}", m.content);
    let sent = sent.lock().unwrap();
    assert_eq!(sent.len(), 2, "the turn goes on to answer the notice");
    let first: Vec<String> = sent[0].iter().map(text).collect();
    let notice_at = first.iter().position(|t| t.contains("notice from before"));
    let question_at = first.iter().position(|t| t.contains("the question"));
    assert!(notice_at.unwrap() < question_at.unwrap(), "{first:?}");
    assert!(text(sent[1].last().unwrap()).contains("notice from during the turn"));
}

#[tokio::test]
async fn compacting_on_request_works_far_below_the_threshold() {
    let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
        tool: None,
        hold_first: None,
        sent: Default::default(),
    });
    let (mut agent, mut event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(model)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    for i in 0..40 {
        agent
            .session
            .push_message(base::interface::model::ModelMessage {
                role: base::interface::model::MessageRole::User,
                content: vec![base::interface::model::ModelContentBlock::Text {
                    text: format!("question {i}: {}", "words ".repeat(50)),
                }],
            });
        agent
            .session
            .push_message(base::interface::model::ModelMessage {
                role: base::interface::model::MessageRole::Assistant,
                content: vec![base::interface::model::ModelContentBlock::Text {
                    text: format!("answer {i}: {}", "words ".repeat(50)),
                }],
            });
    }
    let before = agent.session.messages().len();
    assert!(
        agent.context_tokens() < agent.context_budget().compact_threshold,
        "the point is that the budget would not have compacted this"
    );

    let done = agent
        .compact_now(&CancellationToken::new())
        .await
        .expect("something was compacted");
    assert_eq!(done.messages_before, before);
    assert!(done.messages_after < before, "{done:?}");
    assert!(done.tokens_after < done.tokens_before, "{done:?}");
    assert_eq!(agent.session.messages().len(), done.messages_after);

    let event = next_event(
        &mut event_rx,
        |e| matches!(e, AgentEvent::CompactAction { .. }),
        "the compaction event",
    )
    .await;
    assert!(matches!(
        event,
        AgentEvent::CompactAction { tokens_before, tokens_after, .. }
            if tokens_before == done.tokens_before && tokens_after == done.tokens_after
    ));

    let mut empty = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(StepRecordingModel {
            tool: None,
            hold_first: None,
            sent: Default::default(),
        }))
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed")
        .0;
    assert_eq!(
        empty.compact_now(&CancellationToken::new()).await,
        None,
        "nothing to compact"
    );
}

/// Runs one turn whose tool step is held open while `steered` is sent,
/// then lets it finish. Returns every event up to `TurnComplete` and what
/// each model call was sent.
pub(super) async fn steer_during_a_tool_step(
    settings: base::settings::LoadedSettings,
    builder: impl FnOnce(Builder) -> Builder,
    steered: &[(&str, &str)],
) -> (
    Vec<AgentEvent>,
    Vec<Vec<base::interface::model::ModelMessage>>,
) {
    let gate = Arc::new(tokio::sync::Notify::new());
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(GateTool(gate.clone())));
    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
        tool: Some("Gate"),
        hold_first: None,
        sent: sent.clone(),
    });
    let (mut agent, mut event_rx, input_tx) = builder(
        Builder::new()
            .scene(Arc::new(scene::code::spec(
                scene::code::Interaction::Interactive,
            )))
            .model(model)
            .settings(Arc::new(settings))
            .tools(tools)
            .skip_warmup(true),
    )
    .build()
    .expect("build should succeed");
    let steering = agent.steer_handle();
    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });

    input_tx
        .send(InputMessage::User {
            content: "start".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();
    let mut events = Vec::new();
    loop {
        let e = next_event(&mut event_rx, |_| true, "the tool call").await;
        let started = matches!(&e, AgentEvent::ToolUse { name, .. } if name == "Gate");
        events.push(e);
        if started {
            break;
        }
    }
    for (id, text) in steered {
        assert_eq!(
            steer(&steering, id, text),
            crate::steering::SteerAck::Accepted
        );
    }
    gate.notify_one();
    loop {
        let e = next_event(&mut event_rx, |_| true, "the end of the turn").await;
        let done = matches!(e, AgentEvent::TurnComplete { .. });
        events.push(e);
        if done {
            break;
        }
    }

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
    let sent = sent.lock().unwrap().clone();
    (events, sent)
}

pub(super) fn prompt_hook(command: &str) -> base::settings::LoadedSettings {
    let mut settings = test_settings();
    settings.values.hooks_config = Some(serde_json::json!({
        "UserPromptSubmit": [ { "type": "command", "command": command } ]
    }));
    settings
}

pub(super) fn steer_outcomes(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::UserMessageDelivered {
                message_id,
                content,
                ..
            } => Some(format!("delivered {message_id} {content}")),
            AgentEvent::UserMessageRefused {
                message_id,
                content,
                message,
                ..
            } => Some(format!("refused {message_id} {content} {message:?}")),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_steered_message_the_prompt_hook_blocks_is_refused_and_the_turn_goes_on() {
    let settings = prompt_hook(
        r#"grep -q secret && echo '{"decision":"block","message":"no secrets"}' || true"#,
    );
    let (events, sent) = steer_during_a_tool_step(
        settings,
        |b| b,
        &[("m1", "the secret plan"), ("m2", "also check the tests")],
    )
    .await;

    assert_eq!(
        steer_outcomes(&events),
        [
            "refused m1 the secret plan Some(\"no secrets\")",
            "delivered m2 also check the tests",
        ]
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnComplete { stop_reason, .. }) if stop_reason == "end_turn"
    ));
    assert_eq!(sent.len(), 2, "the turn went on to its next call");
    let second: Vec<String> = sent[1].iter().map(text_of).collect();
    assert!(
        !second.iter().any(|t| t.contains("secret")),
        "the refused message never reached the transcript: {second:?}"
    );
    assert_eq!(second.last().unwrap(), "also check the tests");
}

#[tokio::test]
async fn a_steered_message_reaches_the_transcript_as_the_prompt_hook_rewrote_it() {
    let settings = prompt_hook(
        r#"grep -q '"delivery":"steer"' && echo '{"updated_input":"REWRITTEN"}' || true"#,
    );
    let (events, sent) = steer_during_a_tool_step(settings, |b| b, &[("m1", "original")]).await;

    assert_eq!(steer_outcomes(&events), ["delivered m1 REWRITTEN"]);
    assert_eq!(
        text_of(sent[0].last().unwrap()),
        "start",
        "a turn says \"turn\""
    );
    assert_eq!(text_of(sent[1].last().unwrap()), "REWRITTEN");
}

#[tokio::test]
async fn without_a_prompt_hook_steered_messages_are_delivered_as_sent() {
    let (events, sent) = steer_during_a_tool_step(
        test_settings(),
        |b| b,
        &[("m1", "the secret plan"), ("m2", "also check the tests")],
    )
    .await;

    assert_eq!(
        steer_outcomes(&events),
        [
            "delivered m1 the secret plan",
            "delivered m2 also check the tests",
        ]
    );
    assert_eq!(sent.len(), 2);
}
