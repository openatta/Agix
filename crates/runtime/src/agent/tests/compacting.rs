//! Compaction triggered through the agent: what is restated afterwards and when it is skipped.

use super::*;

/// Compacts by keeping the messages for which `keep` holds, reported as
/// `strategy`.
pub(super) struct KeepingCompactor {
    pub(super) keep: fn(&base::interface::model::ModelMessage) -> bool,
    pub(super) strategy: compaction::compact::CompactStrategy,
}

#[async_trait::async_trait]
impl compaction::compact::Compactor for KeepingCompactor {
    async fn compact(
        &self,
        messages: Vec<base::interface::model::ModelMessage>,
        _max_tokens: usize,
        _keep_rounds: usize,
    ) -> Result<
        (
            Vec<base::interface::model::ModelMessage>,
            compaction::compact::CompactResult,
        ),
        compaction::compact::CompactError,
    > {
        let before = messages.len();
        let kept: Vec<_> = messages.into_iter().filter(|m| (self.keep)(m)).collect();
        let result = compaction::compact::CompactResult {
            strategy: self.strategy,
            messages_before: before,
            messages_after: kept.len(),
            tokens_before: 0,
            tokens_after: 0,
            projection: None,
        };
        Ok((kept, result))
    }

    fn wants_llm_summary(
        &self,
        _messages: &[base::interface::model::ModelMessage],
        _max_tokens: usize,
        _keep_rounds: usize,
    ) -> bool {
        false
    }
}

pub(super) fn text_message(
    role: base::interface::model::MessageRole,
    text: &str,
) -> base::interface::model::ModelMessage {
    base::interface::model::ModelMessage {
        role,
        content: vec![base::interface::model::ModelContentBlock::Text { text: text.into() }],
    }
}

pub(super) fn is_opening(m: &base::interface::model::ModelMessage) -> bool {
    crate::turn::holds_session_context(std::slice::from_ref(m))
}

pub(super) const FILLER: &str = "FILLER";

pub(super) fn is_filler(m: &base::interface::model::ModelMessage) -> bool {
    m.content.iter().any(|b| {
        matches!(b, base::interface::model::ModelContentBlock::Text { text } if text.starts_with(FILLER))
    })
}

pub(super) fn compacting_agent(
    keep: fn(&base::interface::model::ModelMessage) -> bool,
    strategy: compaction::compact::CompactStrategy,
) -> Agent {
    compacting_agent_and_events(keep, strategy).0
}

pub(super) fn compacting_agent_and_events(
    keep: fn(&base::interface::model::ModelMessage) -> bool,
    strategy: compaction::compact::CompactStrategy,
) -> (Agent, EventReceiver) {
    use base::interface::model::MessageRole;
    let (mut agent, rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel))
        .settings(Arc::new(test_settings()))
        .compactor(Arc::new(KeepingCompactor { keep, strategy }))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let opening = format!(
        "{}# currentDate\nToday.\n</system-reminder>",
        crate::turn::SESSION_CONTEXT_OPENING
    );
    agent
        .session
        .push_message(text_message(MessageRole::User, &opening));
    agent.session.push_message(text_message(
        MessageRole::User,
        &format!("{FILLER} {}", "padding words ".repeat(200)),
    ));
    agent
        .session
        .push_message(text_message(MessageRole::User, "hello"));
    agent
        .session
        .push_message(text_message(MessageRole::Assistant, "hi"));
    agent.prompt.session_context_injected = true;
    (agent, rx)
}

pub(super) async fn compacted_with(
    keep: fn(&base::interface::model::ModelMessage) -> bool,
) -> bool {
    let mut agent = compacting_agent(keep, compaction::compact::CompactStrategy::MicroCompact);
    agent
        .run_compaction(
            1,
            1,
            crate::turn::CompactTrigger::Auto,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("a compaction");
    agent.prompt.session_context_injected
}

pub(super) fn in_plan_mode_with_a_task(agent: &Agent) {
    agent
        .session_state
        .set_permission_mode(base::permission::PermissionMode::Plan);
    agent.session_state.register_task(
        "ag-7",
        base::context::TaskType::Agent,
        CancellationToken::new(),
        serde_json::json!({"description": "survey the parser"}),
    );
}

pub(super) fn transcript_text(agent: &Agent) -> String {
    agent
        .session
        .messages()
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            base::interface::model::ModelContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) async fn reminders_after_compaction(
    strategy: compaction::compact::CompactStrategy,
) -> String {
    let mut agent = compacting_agent(|m| !is_opening(m) && !is_filler(m), strategy);
    in_plan_mode_with_a_task(&agent);
    agent
        .run_compaction(
            1,
            1,
            crate::turn::CompactTrigger::Auto,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("a compaction");
    transcript_text(&agent)
}

#[tokio::test]
async fn a_compaction_that_drops_turns_restates_plan_mode_and_running_tasks() {
    let text = reminders_after_compaction(compaction::compact::CompactStrategy::Snip).await;
    assert!(text.contains("Plan mode is still active"), "{text}");
    assert!(
        text.contains("task:ag-7 — agent: survey the parser"),
        "{text}"
    );
}

#[tokio::test]
async fn a_compaction_that_keeps_every_turn_leaves_plan_mode_and_tasks_unsaid() {
    let text = reminders_after_compaction(compaction::compact::CompactStrategy::MicroCompact).await;
    assert!(!text.contains("Plan mode is still active"), "{text}");
    assert!(!text.contains("task:ag-7"), "{text}");
}

#[tokio::test]
async fn a_compaction_that_kept_the_opening_context_does_not_ask_for_it_again() {
    assert!(compacted_with(|m| !is_filler(m)).await);
    assert!(!compacted_with(|m| !is_opening(m) && !is_filler(m)).await);
}

pub(super) fn compact_frames(rx: &mut EventReceiver) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|e| matches!(e, AgentEvent::CompactAction { .. }))
        .count()
}

/// An automatic compaction with nothing to drop leaves everything as it
/// was: no recovery spliced in, no frame, no marker, nothing forgotten.
#[tokio::test]
async fn an_automatic_compaction_with_nothing_to_drop_changes_nothing() {
    let (mut agent, mut rx) =
        compacting_agent_and_events(|_| true, compaction::compact::CompactStrategy::NoOp);
    in_plan_mode_with_a_task(&agent);
    let before = transcript_text(&agent);
    let outcome = agent
        .run_compaction(
            1,
            1,
            crate::turn::CompactTrigger::Auto,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert_eq!(outcome, Err(crate::turn::NotCompacted::NothingToDrop));
    assert_eq!(transcript_text(&agent), before);
    assert!(agent.prompt.session_context_injected);
    assert_eq!(compact_frames(&mut rx), 0);
}

/// Dropping a few tokens and putting back more in reminders is not a
/// compaction.
#[tokio::test]
async fn a_compaction_whose_recovery_outweighs_what_it_dropped_is_not_applied() {
    let (mut agent, mut rx) = compacting_agent_and_events(
        |m| !is_opening(m),
        compaction::compact::CompactStrategy::Snip,
    );
    in_plan_mode_with_a_task(&agent);
    let before = agent.session.messages().len();
    let outcome = agent
        .run_compaction(
            1,
            1,
            crate::turn::CompactTrigger::Auto,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert_eq!(outcome, Err(crate::turn::NotCompacted::NothingToDrop));
    assert_eq!(agent.session.messages().len(), before);
    assert!(agent.prompt.session_context_injected);
    assert_eq!(compact_frames(&mut rx), 0);
}

/// The size-refusal path is the same compaction as the others: it says
/// so on the event stream, and a dropped opening context is asked for
/// again on the next turn.
#[tokio::test]
async fn compacting_after_a_size_refusal_is_a_full_compaction() {
    let (mut agent, mut rx) = compacting_agent_and_events(
        |m| !is_opening(m) && !is_filler(m),
        compaction::compact::CompactStrategy::Snip,
    );
    agent
        .compact_for_retry(
            &base::interface::model::ModelError::new(
                base::interface::model::ModelErrorKind::ContextTooLong,
                "prompt is too long",
            ),
            &CancellationToken::new(),
        )
        .await
        .expect("compacted");
    assert!(!agent.prompt.session_context_injected);
    assert_eq!(compact_frames(&mut rx), 1);
}

/// Automatic compaction aims the conversation at what the threshold
/// leaves beside the per-request overhead, so the request that follows
/// is under the threshold rather than over it again by the overhead.
#[tokio::test]
async fn automatic_compaction_leaves_room_for_the_request_overhead() {
    struct Cascade;

    #[async_trait::async_trait]
    impl compaction::compact::Compactor for Cascade {
        async fn compact(
            &self,
            messages: Vec<base::interface::model::ModelMessage>,
            max_tokens: usize,
            keep_rounds: usize,
        ) -> Result<
            (
                Vec<base::interface::model::ModelMessage>,
                compaction::compact::CompactResult,
            ),
            compaction::compact::CompactError,
        > {
            compaction::compact::DefaultCompactor
                .compact(messages, max_tokens, keep_rounds)
                .await
        }

        fn wants_llm_summary(
            &self,
            _messages: &[base::interface::model::ModelMessage],
            _max_tokens: usize,
            _keep_rounds: usize,
        ) -> bool {
            false
        }
    }

    let (mut agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel))
        .compactor(Arc::new(Cascade))
        .settings(Arc::new({
            let mut s = test_settings();
            s.values.compaction.threshold_tokens = Some(6_000);
            s.values.compaction.keep_recent = Some(50);
            s
        }))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    for i in 0..40 {
        agent.session.push_message(text_message(
            base::interface::model::MessageRole::User,
            &format!("question {i}: {}", "words ".repeat(50)),
        ));
        agent.session.push_message(text_message(
            base::interface::model::MessageRole::Assistant,
            &format!("answer {i}: {}", "words ".repeat(50)),
        ));
    }
    agent.compaction.request_overhead_tokens = 2_000;
    assert!(agent.context_tokens() > 6_000);
    agent
        .compact_if_needed(&tokio_util::sync::CancellationToken::new())
        .await;
    assert!(
        agent.context_tokens() <= 6_000,
        "{} tokens after compacting to a 6000-token threshold",
        agent.context_tokens()
    );
}

/// Answers every compaction with nothing dropped, or with an error, and
/// counts how often it was asked.
struct Unshrinkable {
    asked: Arc<std::sync::atomic::AtomicUsize>,
    fails: bool,
}

#[async_trait::async_trait]
impl compaction::compact::Compactor for Unshrinkable {
    async fn compact(
        &self,
        messages: Vec<base::interface::model::ModelMessage>,
        _max_tokens: usize,
        _keep_rounds: usize,
    ) -> Result<
        (
            Vec<base::interface::model::ModelMessage>,
            compaction::compact::CompactResult,
        ),
        compaction::compact::CompactError,
    > {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fails {
            return Err(compaction::compact::CompactError::Internal(
                "summarizer down".into(),
            ));
        }
        let n = messages.len();
        Ok((
            messages,
            compaction::compact::CompactResult {
                strategy: compaction::compact::CompactStrategy::NoOp,
                messages_before: n,
                messages_after: n,
                tokens_before: 0,
                tokens_after: 0,
                projection: None,
            },
        ))
    }

    fn wants_llm_summary(
        &self,
        _messages: &[base::interface::model::ModelMessage],
        _max_tokens: usize,
        _keep_rounds: usize,
    ) -> bool {
        false
    }
}

fn over_the_threshold(fails: bool) -> (Agent, Arc<std::sync::atomic::AtomicUsize>) {
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel))
        .compactor(Arc::new(Unshrinkable {
            asked: asked.clone(),
            fails,
        }))
        .settings(Arc::new({
            let mut s = test_settings();
            s.values.compaction.threshold_tokens = Some(6_000);
            s.values.compaction.keep_recent = Some(50);
            s.values.compaction.reactive = false;
            s
        }))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    push_rounds(&mut agent, 40);
    agent.compaction.request_overhead_tokens = 2_000;
    assert!(agent.context_tokens() > 6_000);
    (agent, asked)
}

fn push_rounds(agent: &mut Agent, rounds: usize) {
    for i in 0..rounds {
        agent.session.push_message(text_message(
            base::interface::model::MessageRole::User,
            &format!("question {i}: {}", "words ".repeat(50)),
        ));
        agent.session.push_message(text_message(
            base::interface::model::MessageRole::Assistant,
            &format!("answer {i}: {}", "words ".repeat(50)),
        ));
    }
}

async fn checks(agent: &mut Agent, n: usize) {
    for _ in 0..n {
        agent
            .compact_if_needed(&tokio_util::sync::CancellationToken::new())
            .await;
    }
}

/// An automatic compaction that saves nothing is not retried at every
/// step: each miss in a row doubles how many due checks go by first.
#[tokio::test]
async fn automatic_compaction_that_saves_nothing_backs_off() {
    let (mut agent, asked) = over_the_threshold(false);
    let asked = || asked.load(std::sync::atomic::Ordering::SeqCst);
    checks(&mut agent, 1).await;
    assert_eq!(asked(), 1);
    checks(&mut agent, 2).await;
    assert_eq!(asked(), 1, "held for two checks");
    checks(&mut agent, 1).await;
    assert_eq!(asked(), 2);
    checks(&mut agent, 4).await;
    assert_eq!(asked(), 2, "held for four checks");
    checks(&mut agent, 1).await;
    assert_eq!(asked(), 3);
}

/// A wait ends early once the context has grown well past where the last
/// attempt left it.
#[tokio::test]
async fn a_backed_off_compaction_is_tried_again_once_the_context_grows() {
    let (mut agent, asked) = over_the_threshold(false);
    checks(&mut agent, 1).await;
    push_rounds(&mut agent, 10);
    checks(&mut agent, 1).await;
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A compactor that fails counts as a miss like one that saves nothing.
#[tokio::test]
async fn a_failed_automatic_compaction_backs_off_too() {
    let (mut agent, asked) = over_the_threshold(true);
    checks(&mut agent, 3).await;
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(agent.compaction.auto_backoff.misses(), 1);
}
