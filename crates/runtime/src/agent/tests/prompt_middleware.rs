//! Prompt middleware ordering and attachments, messages arriving mid-turn.

use super::*;

pub(super) type SeenPrompts =
    Arc<std::sync::Mutex<Vec<(base::interface::prompt_middleware::PromptDelivery, String)>>>;

/// Records what it was shown, then answers with `decide`.
pub(super) struct Screen {
    pub(super) seen: SeenPrompts,
    pub(super) decide: fn(&str) -> base::interface::prompt_middleware::PromptDecision,
}

impl Screen {
    fn new(
        decide: fn(&str) -> base::interface::prompt_middleware::PromptDecision,
    ) -> (Arc<Self>, SeenPrompts) {
        let seen = SeenPrompts::default();
        (
            Arc::new(Self {
                seen: seen.clone(),
                decide,
            }),
            seen,
        )
    }
}

#[async_trait::async_trait]
impl base::interface::prompt_middleware::PromptMiddleware for Screen {
    async fn inspect(
        &self,
        call: &base::interface::prompt_middleware::PromptCall<'_>,
    ) -> base::interface::prompt_middleware::PromptDecision {
        self.seen
            .lock()
            .unwrap()
            .push((call.delivery, call.content.to_string()));
        (self.decide)(call.content)
    }
}

pub(super) fn answering_agent(
    settings: base::settings::LoadedSettings,
    builder: impl FnOnce(Builder) -> Builder,
) -> (
    Agent,
    EventReceiver,
    Arc<std::sync::Mutex<Vec<Vec<base::interface::model::ModelMessage>>>>,
) {
    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
        tool: None,
        hold_first: None,
        sent: sent.clone(),
    });
    let (agent, event_rx, _input_tx) = builder(
        Builder::new()
            .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
            .model(model)
            .settings(Arc::new(settings))
            .skip_warmup(true),
    )
    .build()
    .expect("build should succeed");
    (agent, event_rx, sent)
}

pub(super) async fn user_turn(agent: &mut Agent, id: &str, text: &str) -> crate::turn::TurnOutcome {
    agent
        .process_turn(
            InputMessage::User {
                content: text.into(),
                attachments: vec![],
                turn_id: id.into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed")
}

#[tokio::test]
async fn a_prompt_middleware_block_ends_the_turn_before_any_model_call() {
    use base::interface::prompt_middleware::PromptDecision;
    let (screen, _) = Screen::new(|text| {
        if text.contains("forbidden") {
            PromptDecision::Block {
                message: Some("not here".into()),
            }
        } else {
            PromptDecision::Continue
        }
    });
    let (mut agent, mut event_rx, sent) =
        answering_agent(test_settings(), |b| b.prompt_middleware(screen));

    let outcome = user_turn(&mut agent, "t1", "a forbidden topic").await;
    assert_eq!(outcome.stop_reason, "stopped_by_hook");
    assert!(sent.lock().unwrap().is_empty(), "no model call");
    let done = next_event(
        &mut event_rx,
        |e| matches!(e, AgentEvent::TurnComplete { .. }),
        "turn_complete",
    )
    .await;
    assert!(matches!(
        done,
        AgentEvent::TurnComplete { ref stop_reason, ref message, .. }
            if stop_reason == "stopped_by_hook" && message.as_deref() == Some("not here")
    ));

    let outcome = user_turn(&mut agent, "t2", "something else").await;
    assert_eq!(outcome.stop_reason, "end_turn");
    assert_eq!(sent.lock().unwrap().len(), 1, "the next turn runs as usual");
}

#[tokio::test]
async fn a_prompt_middleware_rewrite_is_what_the_model_and_the_transcript_get() {
    use base::interface::prompt_middleware::PromptDecision;
    let (screen, _) = Screen::new(|_| PromptDecision::Rewrite("REWRITTEN".into()));
    let (mut agent, _event_rx, sent) =
        answering_agent(test_settings(), |b| b.prompt_middleware(screen));

    user_turn(&mut agent, "t1", "original message").await;

    assert_eq!(
        text_of(sent.lock().unwrap()[0].last().unwrap()),
        "REWRITTEN"
    );
    let texts: Vec<String> = agent.session.messages().iter().map(text_of).collect();
    assert!(texts.iter().any(|t| t == "REWRITTEN"), "{texts:?}");
    assert!(!texts.iter().any(|t| t == "original message"), "{texts:?}");
}

#[tokio::test]
async fn prompt_middlewares_run_after_the_configured_hook_in_the_order_they_were_added() {
    use base::interface::prompt_middleware::{PromptDecision, PromptDelivery};
    let settings = prompt_hook(r#"echo '{"updated_input":"B"}'"#);
    let (first, first_saw) = Screen::new(|text| PromptDecision::Rewrite(format!("{text}1")));
    let (second, second_saw) = Screen::new(|text| PromptDecision::Rewrite(format!("{text}2")));
    let (mut agent, _event_rx, sent) = answering_agent(settings, |b| {
        b.prompt_middleware(first).prompt_middleware(second)
    });

    user_turn(&mut agent, "t1", "A").await;

    assert_eq!(
        *first_saw.lock().unwrap(),
        [(PromptDelivery::Turn, "B".to_string())],
        "the hook's rewrite is what the first middleware sees"
    );
    assert_eq!(
        *second_saw.lock().unwrap(),
        [(PromptDelivery::Turn, "B1".to_string())]
    );
    assert_eq!(text_of(sent.lock().unwrap()[0].last().unwrap()), "B12");
}

#[tokio::test]
async fn a_steered_message_passes_the_prompt_middleware_too() {
    use base::interface::prompt_middleware::{PromptDecision, PromptDelivery};
    let (screen, seen) = Screen::new(|text| {
        if text.contains("secret") {
            PromptDecision::Block { message: None }
        } else {
            PromptDecision::Continue
        }
    });
    let (events, sent) = steer_during_a_tool_step(
        test_settings(),
        |b| b.prompt_middleware(screen),
        &[("m1", "the secret plan"), ("m2", "also check the tests")],
    )
    .await;

    assert_eq!(
        *seen.lock().unwrap(),
        [
            (PromptDelivery::Turn, "start".to_string()),
            (PromptDelivery::Steer, "the secret plan".to_string()),
            (PromptDelivery::Steer, "also check the tests".to_string()),
        ]
    );
    assert_eq!(
        steer_outcomes(&events),
        [
            "refused m1 the secret plan None",
            "delivered m2 also check the tests",
        ]
    );
    assert_eq!(text_of(sent[1].last().unwrap()), "also check the tests");
}

pub(super) type SeenAttachments = Arc<
    std::sync::Mutex<
        Vec<(
            base::interface::prompt_middleware::PromptDelivery,
            Vec<(
                &'static str,
                Option<String>,
                Option<String>,
                Option<u64>,
                Option<String>,
            )>,
        )>,
    >,
>;

/// Records the attachments it was shown and refuses a message that has any.
pub(super) struct NoAttachments(pub(super) SeenAttachments);

#[async_trait::async_trait]
impl base::interface::prompt_middleware::PromptMiddleware for NoAttachments {
    async fn inspect(
        &self,
        _call: &base::interface::prompt_middleware::PromptCall<'_>,
    ) -> base::interface::prompt_middleware::PromptDecision {
        unreachable!("the runtime calls inspect_with_attachments")
    }

    async fn inspect_with_attachments(
        &self,
        call: &base::interface::prompt_middleware::PromptCall<'_>,
        attachments: &[base::interface::prompt_middleware::PromptAttachment<'_>],
    ) -> base::interface::prompt_middleware::PromptDecision {
        use base::interface::prompt_middleware::PromptDecision;
        self.0.lock().unwrap().push((
            call.delivery,
            attachments
                .iter()
                .map(|a| {
                    (
                        a.kind.as_str(),
                        a.name.map(str::to_string),
                        a.media_type.map(str::to_string),
                        a.size,
                        a.text.map(str::to_string),
                    )
                })
                .collect(),
        ));
        if attachments.is_empty() {
            PromptDecision::Continue
        } else {
            PromptDecision::Block {
                message: Some("no attachments here".into()),
            }
        }
    }
}

#[tokio::test]
async fn a_prompt_middleware_sees_the_attachments_of_a_turn_and_none_on_a_steer() {
    use base::interface::prompt_middleware::{PromptDecision, PromptDelivery};
    let dir = tempfile::tempdir().unwrap();
    let picture = dir.path().join("photo.PNG");
    std::fs::write(&picture, [0u8; 42]).unwrap();
    let picture = picture.display().to_string();

    let seen = SeenAttachments::default();
    let (old_style, old_saw) = Screen::new(|_| PromptDecision::Continue);
    let (mut agent, _event_rx, sent) = answering_agent(test_settings(), |b| {
        b.prompt_middleware(old_style)
            .prompt_middleware(Arc::new(NoAttachments(seen.clone())))
    });
    let outcome = agent
        .process_turn(
            InputMessage::User {
                content: "look at these".into(),
                attachments: vec![
                    Attachment::Text {
                        path: "notes.md".into(),
                        content: "héllo".into(),
                    },
                    Attachment::Image {
                        media_type: "image/jpeg".into(),
                        data: "aGVsbG8=".into(),
                    },
                    Attachment::File {
                        path: picture.clone(),
                    },
                ],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed");

    assert_eq!(outcome.stop_reason, "stopped_by_hook");
    assert_eq!(outcome.message.as_deref(), Some("no attachments here"));
    assert!(sent.lock().unwrap().is_empty(), "no model call");
    assert_eq!(
        *old_saw.lock().unwrap(),
        [(PromptDelivery::Turn, "look at these".to_string())],
        "a middleware that only implements inspect still runs"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [(
            PromptDelivery::Turn,
            vec![
                (
                    "text",
                    Some("notes.md".to_string()),
                    None,
                    Some(6),
                    Some("héllo".to_string()),
                ),
                ("image", None, Some("image/jpeg".to_string()), Some(5), None),
                (
                    "file",
                    Some(picture),
                    Some("image/png".to_string()),
                    Some(42),
                    None,
                ),
            ],
        )]
    );

    let steer_seen = SeenAttachments::default();
    let (events, _sent) = steer_during_a_tool_step(
        test_settings(),
        |b| b.prompt_middleware(Arc::new(NoAttachments(steer_seen.clone()))),
        &[("m1", "also this")],
    )
    .await;
    assert_eq!(
        *steer_seen.lock().unwrap(),
        [
            (PromptDelivery::Turn, vec![]),
            (PromptDelivery::Steer, vec![])
        ]
    );
    assert_eq!(steer_outcomes(&events), ["delivered m1 also this"]);
}

#[tokio::test]
async fn a_message_sent_during_a_tool_step_reaches_the_next_model_call() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(GateTool(gate.clone())));
    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
        tool: Some("Gate"),
        hold_first: None,
        sent: sent.clone(),
    });
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });

    input_tx
        .send(InputMessage::User {
            content: "start".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();
    next_event(
        &mut event_rx,
        |e| matches!(e, AgentEvent::ToolUse { name, .. } if name == "Gate"),
        "the tool call",
    )
    .await;

    assert_eq!(
        steer(&input_tx, "m1", "also check the tests").await,
        crate::steering::SteerAck::Accepted
    );
    gate.notify_one();

    let delivered = next_event(
        &mut event_rx,
        |e| matches!(e, AgentEvent::UserMessageDelivered { .. }),
        "the delivery",
    )
    .await;
    assert!(matches!(
        delivered,
        AgentEvent::UserMessageDelivered { ref message_id, ref turn_id, .. }
            if message_id == "m1" && turn_id == "t1"
    ));
    next_event(
        &mut event_rx,
        |e| matches!(e, AgentEvent::TurnComplete { .. }),
        "the end of the turn",
    )
    .await;

    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "one call for the tool, one after it");
    let second = &sent[1];
    let at = second
        .iter()
        .position(|m| text_of(m) == "also check the tests")
        .expect("the second call carries the message");
    let result_at = second
        .iter()
        .position(|m| {
            m.content.iter().any(|b| {
                matches!(
                    b,
                    base::interface::model::ModelContentBlock::ToolResult { .. }
                )
            })
        })
        .expect("and the tool result");
    assert!(
        result_at < at,
        "after the tool result, never between the pair"
    );
    assert_eq!(at, second.len() - 1);

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

#[tokio::test]
async fn a_message_sent_while_the_model_answers_is_answered_in_the_same_turn() {
    let hold = Arc::new(tokio::sync::Notify::new());
    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
        tool: None,
        hold_first: Some(hold.clone()),
        sent: sent.clone(),
    });
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });

    input_tx
        .send(InputMessage::User {
            content: "first".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while sent.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the first call should start");

    assert_eq!(
        steer(&input_tx, "m1", "and one more thing").await,
        crate::steering::SteerAck::Accepted
    );
    hold.notify_one();

    let done = next_event(
        &mut event_rx,
        |e| matches!(e, AgentEvent::TurnComplete { .. }),
        "the end of the turn",
    )
    .await;
    assert!(matches!(
        done,
        AgentEvent::TurnComplete { api_calls: 2, .. }
    ));
    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(text_of(sent[1].last().unwrap()), "and one more thing");

    assert_eq!(
        steer(&input_tx, "m2", "too late").await,
        crate::steering::SteerAck::NotRunning,
        "an idle session takes no steering; the sender keeps the message"
    );

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}

#[tokio::test]
async fn an_interrupted_turn_hands_back_what_it_never_delivered() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(GateTool(gate)));
    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model: Arc<dyn Model> = Arc::new(StepRecordingModel {
        tool: Some("Gate"),
        hold_first: None,
        sent: sent.clone(),
    });
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });

    input_tx
        .send(InputMessage::User {
            content: "start".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();
    next_event(
        &mut event_rx,
        |e| matches!(e, AgentEvent::ToolUse { .. }),
        "the tool call",
    )
    .await;
    assert_eq!(
        steer(&input_tx, "m1", "never mind").await,
        crate::steering::SteerAck::Accepted
    );
    input_tx
        .send(InputMessage::System {
            kind: EngineCommand::CancelTurn { turn_id: None },
            content: String::new(),
        })
        .unwrap();

    let returned = next_event(
        &mut event_rx,
        |e| {
            matches!(
                e,
                AgentEvent::UserMessagesReturned { .. }
                    | AgentEvent::UserMessageDelivered { .. }
                    | AgentEvent::TurnComplete { .. }
            )
        },
        "the hand-back",
    )
    .await;
    match returned {
        AgentEvent::UserMessagesReturned { messages, .. } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].message_id, "m1");
        }
        other => panic!("expected the message back before the turn ended, got {other:?}"),
    }
    let done = next_event(
        &mut event_rx,
        |e| matches!(e, AgentEvent::TurnComplete { .. }),
        "the end of the turn",
    )
    .await;
    assert!(matches!(
        done,
        AgentEvent::TurnComplete { ref stop_reason, .. } if stop_reason == "cancelled"
    ));
    assert_eq!(sent.lock().unwrap().len(), 1, "the model never saw it");

    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;
}
