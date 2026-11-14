//! 一次模型调用失败之后，回合循环怎么照恢复策略的回答重发。

use super::support::recall_test_settings;
use crate::agent::{Agent, InputMessage};
use crate::turn::{TurnError, TurnOutcome};
use base::event::AgentEvent;
use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelError, ModelEvent, ModelMessage, ModelStream,
    StreamParams, ToolDef, Usage,
};
use base::interface::recovery_policy::{
    CallRecovery, ModelFailure, Recovery, RecoveryAttempt, RecoveryPolicy, StopRecovery,
    MAX_RECOVERIES_PER_CALL,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

enum Answer {
    /// The stream opens, says `text`, then fails with `error`.
    BreaksOff(&'static str, ModelError),
    /// `stream()` itself fails: nothing came back.
    Refused(ModelError),
    Text(&'static str),
}

/// Answers the conversation's calls from a script, the last answer forever.
/// Calls with a purpose of their own (memory, naming) are not the turn's.
struct Scripted {
    answers: Mutex<VecDeque<Answer>>,
    calls: Mutex<u32>,
    cache_edits: Mutex<Vec<Vec<String>>>,
}

impl Scripted {
    fn new(answers: Vec<Answer>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            calls: Mutex::new(0),
            cache_edits: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }

    /// The cache edits each of the conversation's calls carried.
    fn cache_edits(&self) -> Vec<Vec<String>> {
        self.cache_edits.lock().unwrap().clone()
    }
}

fn ended() -> ModelEvent {
    ModelEvent::EndTurn {
        stop_reason: "end_turn".into(),
        usage: Usage::default(),
    }
}

#[async_trait::async_trait]
impl Model for Scripted {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }

    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<ToolDef>,
        _messages: Vec<ModelMessage>,
        params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        if params.origin.as_ref().and_then(|o| o.purpose()).is_some() {
            return Ok(Box::new(futures::stream::iter(vec![Ok(ended())])));
        }
        *self.calls.lock().unwrap() += 1;
        self.cache_edits
            .lock()
            .unwrap()
            .push(params.cache_edits.clone());
        let answer = {
            let mut answers = self.answers.lock().unwrap();
            match answers.len() {
                1 => match answers.front() {
                    Some(Answer::BreaksOff(text, e)) => Answer::BreaksOff(text, e.clone()),
                    Some(Answer::Refused(e)) => Answer::Refused(e.clone()),
                    Some(Answer::Text(text)) => Answer::Text(text),
                    None => unreachable!(),
                },
                _ => answers.pop_front().expect("the script ran out"),
            }
        };
        let events = match answer {
            Answer::BreaksOff(text, e) => {
                vec![Ok(ModelEvent::TextDelta { text: text.into() }), Err(e)]
            }
            Answer::Refused(e) => return Err(e),
            Answer::Text(text) => {
                vec![Ok(ModelEvent::TextDelta { text: text.into() }), Ok(ended())]
            }
        };
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// Sends the same request again at once, every time it is asked.
struct AlwaysRetrySame {
    after: Duration,
}

impl RecoveryPolicy for AlwaysRetrySame {
    fn on_failure(&self, _failure: &ModelFailure<'_>, _so_far: &CallRecovery) -> Recovery {
        Recovery::RetrySame { after: self.after }
    }

    fn on_early_stop(&self, _reason: &str, _attempt: &RecoveryAttempt) -> StopRecovery {
        StopRecovery::Accept
    }
}

/// Retries against another model, every time it is asked.
struct SwitchModel;

impl RecoveryPolicy for SwitchModel {
    fn on_failure(&self, _failure: &ModelFailure<'_>, _so_far: &CallRecovery) -> Recovery {
        Recovery::RetryWith {
            model: "fallback-model".into(),
        }
    }

    fn on_early_stop(&self, _reason: &str, _attempt: &RecoveryAttempt) -> StopRecovery {
        StopRecovery::Accept
    }
}

fn agent(
    model: Arc<Scripted>,
    policy: Option<Arc<dyn RecoveryPolicy>>,
    root: &std::path::Path,
) -> (Agent, crate::agent::EventReceiver) {
    let mut settings = recall_test_settings(root);
    settings.values.memory_enabled = false;
    let mut builder = crate::agent::Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(model)
        .settings(Arc::new(settings))
        .skip_warmup(true);
    if let Some(policy) = policy {
        builder = builder.recovery_policy(policy);
    }
    let (agent, events, _input) = builder.build().expect("build should succeed");
    (agent, events)
}

async fn turn(agent: &mut Agent, cancel: CancellationToken) -> Result<TurnOutcome, TurnError> {
    agent
        .process_turn(
            InputMessage::User {
                content: "hello".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            cancel,
        )
        .await
}

fn drained(events: &mut crate::agent::EventReceiver) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        out.push(event);
    }
    out
}

fn last_reply(agent: &Agent) -> String {
    let reply = agent
        .session
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant)
        .expect("a reply was written");
    reply
        .content
        .iter()
        .filter_map(|b| match b {
            ModelContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_breaks_off_is_sent_again_and_the_step_starts_over() {
    let tmp = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![
        Answer::BreaksOff("half an ans", ModelError::stream_interrupted("reset")),
        Answer::Text("the whole answer"),
    ]);
    let (mut agent, mut events) = agent(model.clone(), None, tmp.path());

    let outcome = turn(&mut agent, CancellationToken::new())
        .await
        .expect("the retry succeeded");

    assert_eq!(outcome.stop_reason, "end_turn");
    assert_eq!(model.calls(), 2);
    assert_eq!(last_reply(&agent), "the whole answer");
    let events = drained(&mut events);
    let retries: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ModelRetry {
                step,
                attempt,
                delay_ms,
                reason,
                ..
            } => Some((*step, *attempt, *delay_ms, reason.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 1, "{events:?}");
    assert_eq!(retries[0].0, 0);
    assert_eq!(retries[0].1, 1);
    assert_eq!(retries[0].2, 2_000);
    assert!(
        retries[0].3.starts_with("stream_interrupted"),
        "{retries:?}"
    );
    let step_starts: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StepStart { step, .. } => Some(*step),
            _ => None,
        })
        .collect();
    assert_eq!(step_starts, vec![0, 0]);
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_keeps_breaking_off_fails_after_two_resends() {
    let tmp = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![Answer::BreaksOff(
        "half",
        ModelError::timeout("stream idle for 90s"),
    )]);
    let (mut agent, _events) = agent(model.clone(), None, tmp.path());

    let failed = turn(&mut agent, CancellationToken::new()).await;

    assert!(matches!(failed, Err(TurnError::Model(..))), "{failed:?}");
    assert_eq!(model.calls(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_request_refused_before_the_first_byte_is_not_sent_again() {
    let tmp = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![Answer::Refused(ModelError::rate_limited())]);
    let (mut agent, _events) = agent(model.clone(), None, tmp.path());

    let failed = turn(&mut agent, CancellationToken::new()).await;

    assert!(matches!(failed, Err(TurnError::Model(..))), "{failed:?}");
    assert_eq!(model.calls(), 1, "the client's ladder already retried it");
}

#[tokio::test(start_paused = true)]
async fn the_loop_bounds_the_recoveries_whatever_the_policy_answers() {
    let tmp = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![Answer::BreaksOff(
        "half",
        ModelError::stream_interrupted("reset"),
    )]);
    let policy = Arc::new(AlwaysRetrySame {
        after: Duration::ZERO,
    });
    let (mut agent, _events) = agent(model.clone(), Some(policy), tmp.path());

    let failed = turn(&mut agent, CancellationToken::new()).await;

    assert!(matches!(failed, Err(TurnError::Model(..))), "{failed:?}");
    assert_eq!(model.calls(), 1 + MAX_RECOVERIES_PER_CALL);
}

#[tokio::test]
async fn a_cancel_while_waiting_to_resend_ends_the_turn_cancelled() {
    let tmp = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![Answer::BreaksOff(
        "half",
        ModelError::stream_interrupted("reset"),
    )]);
    let policy = Arc::new(AlwaysRetrySame {
        after: Duration::from_secs(3600),
    });
    let (mut agent, mut events) = agent(model.clone(), Some(policy), tmp.path());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let watcher = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if matches!(event, AgentEvent::ModelRetry { .. }) {
                trigger.cancel();
                return true;
            }
        }
        false
    });

    let outcome = tokio::time::timeout(Duration::from_secs(20), turn(&mut agent, cancel))
        .await
        .expect("the wait was not cancelled")
        .expect("a cancelled turn is not a failure");

    assert_eq!(outcome.stop_reason, "cancelled");
    assert_eq!(model.calls(), 1);
    drop(agent);
    assert!(watcher.await.unwrap(), "no model_retry was announced");
}

/// A retry is the request that failed, sent again: the cache edits it
/// carried go with it, whichever model it goes to.
#[tokio::test(start_paused = true)]
async fn a_retry_carries_the_cache_edits_of_the_call_that_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let model = Scripted::new(vec![
        Answer::Refused(ModelError::overloaded()),
        Answer::Text("answered by the fallback"),
    ]);
    let (mut agent, _events) = agent(model.clone(), Some(Arc::new(SwitchModel)), tmp.path());
    agent.compaction.pend_cache_edits(vec!["toolu_old".into()]);

    let outcome = turn(&mut agent, CancellationToken::new())
        .await
        .expect("the retry succeeded");

    assert_eq!(outcome.stop_reason, "end_turn");
    assert_eq!(
        model.cache_edits(),
        vec![vec!["toolu_old".to_string()], vec!["toolu_old".to_string()]]
    );
    assert!(agent.compaction.lend_cache_edits().is_empty());
}
