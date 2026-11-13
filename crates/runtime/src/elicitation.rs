//! The engine's own answer to [`Elicitation`]: ask over the session's event
//! stream and wait for the host to answer on the input channel.
//!
//! An authorization goes out as `AgentEvent::PermissionPrompt` and comes back
//! as `InputMessage::PermissionResponse`; a clarification, when the host said
//! it can answer one, goes out as `AgentEvent::QuestionPrompt` and comes back
//! as `InputMessage::QuestionResponse`. Either way a one-shot is registered
//! under the prompt id before the event is emitted, and dropping the ask (its
//! timeout fired, the turn was cancelled) removes the registration, so a late
//! answer for that id is discarded rather than waking a call that already
//! resolved. A library embedder can replace all of this with a direct call
//! into their own UI.
//!
//! # What it cannot serve
//!
//! There is no wire form for an import confirmation, and a host that did not
//! say it answers questions gets no clarifications; both are declined, with a
//! reason that says so. Answering them wrongly would be worse than not
//! answering: a made-up answer to "what did you mean" is a made-up
//! instruction.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base::interface::elicitation::{
    ElicitKind, ElicitOption, ElicitOutcome, ElicitRequest, Elicitation,
};

use crate::agent::{EventSender, PermissionDecision, QuestionAnswer};

type Waiting<T> = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<T>>>>;
type Pending = Waiting<PermissionDecision>;

/// Questions put to the host and not yet answered, keyed by `prompt_id`.
pub type PendingQuestions = Waiting<QuestionAnswer>;

/// Asks over the session's event stream.
pub struct ChannelElicitation {
    events: EventSender,
    pending: Pending,
    questions: Option<PendingQuestions>,
}

impl ChannelElicitation {
    pub fn new(events: EventSender, pending: Pending) -> Self {
        Self {
            events,
            pending,
            questions: None,
        }
    }

    /// Put clarifications to the host too, waiting on `pending` for the answers.
    pub fn with_questions(mut self, pending: PendingQuestions) -> Self {
        self.questions = Some(pending);
        self
    }

    /// The answers a host may give an authorization question, in the order a
    /// UI should offer them. The keys are `PermissionDecision`'s own wire
    /// tags, so a host can build the answer from the key it was shown.
    /// "Allow every time" only where the answer `keeps_grants`.
    pub fn authorization_options(keeps_grants: bool) -> Vec<ElicitOption> {
        let option = |key: &str, label: &str| ElicitOption {
            key: key.into(),
            label: label.into(),
        };
        let mut options = vec![option("permit", "Allow once")];
        if keeps_grants {
            options.push(option("permit_always", "Allow every time"));
        }
        options.push(option("deny", "Deny"));
        options
    }

    async fn authorize(
        &self,
        request: ElicitRequest,
        tool_name: String,
        paths: Vec<PathBuf>,
        tool_use_id: Option<String>,
    ) -> ElicitOutcome {
        let (tx, rx) = tokio::sync::oneshot::channel();
        // Registered before emitting, so an answer that comes back instantly
        // still finds somewhere to land.
        let _registration = Registration::new(&request.id, &self.pending, tx);
        let _ = self.events.send(base::event::AgentEvent::PermissionPrompt {
            prompt_id: request.id,
            tool_name,
            message: request.message,
            paths,
            turn_id: String::new(),
            tool_use_id,
            task_id: None,
        });

        match rx.await {
            Ok(decision) => ElicitOutcome::answered(&decision),
            // The only registration is the one above, and it is removed on
            // drop — so this means the host's side went away. Fail closed.
            Err(_) => {
                ElicitOutcome::declined("permission request channel closed without an answer")
            }
        }
    }

    async fn clarify(
        &self,
        questions: &PendingQuestions,
        request: ElicitRequest,
        header: Option<String>,
        tool_use_id: Option<String>,
    ) -> ElicitOutcome {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _registration = Registration::new(&request.id, questions, tx);
        let _ = self.events.send(base::event::AgentEvent::QuestionPrompt {
            prompt_id: request.id,
            question: request.message,
            header,
            options: request.options,
            turn_id: String::new(),
            tool_use_id,
            task_id: None,
        });

        match rx.await {
            Ok(QuestionAnswer::Answer { text }) => ElicitOutcome::answered(&text),
            Ok(QuestionAnswer::Decline { reason }) => {
                ElicitOutcome::declined(format!("the user declined to answer: {reason}"))
            }
            Err(_) => ElicitOutcome::declined("question channel closed without an answer"),
        }
    }
}

/// Removes a prompt registration unless the answer arrived.
///
/// Tied to the future's lifetime, so a caller that drops the ask for a reason
/// nobody has thought of yet still cannot leave a stale registration behind.
struct Registration<T> {
    id: String,
    pending: Waiting<T>,
}

impl<T> Registration<T> {
    fn new(id: &str, pending: &Waiting<T>, tx: tokio::sync::oneshot::Sender<T>) -> Self {
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), tx);
        Self {
            id: id.to_string(),
            pending: pending.clone(),
        }
    }
}

impl<T> Drop for Registration<T> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// Wake the question `prompt_id` with `answer`. A `prompt_id` nobody waits
/// on — already answered, timed out, never asked — is ignored.
pub(crate) fn resolve_question_response(
    pending: &PendingQuestions,
    prompt_id: &str,
    answer: QuestionAnswer,
) {
    if let Some(sender) = pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(prompt_id)
    {
        let _ = sender.send(answer);
    }
}

fn cannot_carry(what: &str) -> ElicitOutcome {
    ElicitOutcome::declined(format!(
        "this host is connected for permission prompts only and has no way to \
         put {what} to a person; register an `Elicitation` implementation that \
         can reach your UI to change that"
    ))
}

#[async_trait]
impl Elicitation for ChannelElicitation {
    async fn ask(&self, request: ElicitRequest) -> ElicitOutcome {
        match request.kind.clone() {
            ElicitKind::Authorization {
                tool_name,
                paths,
                tool_use_id,
            } => self.authorize(request, tool_name, paths, tool_use_id).await,
            ElicitKind::Clarification {
                header,
                tool_use_id,
            } => match &self.questions {
                Some(questions) => self.clarify(questions, request, header, tool_use_id).await,
                None => cannot_carry("a clarification"),
            },
            ElicitKind::Import { .. } => cannot_carry("an import confirmation"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::elicitation::ElicitKind;

    fn bus() -> (
        EventSender,
        tokio::sync::mpsc::UnboundedReceiver<base::event::AgentEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (crate::event_bus::EventBus::new(tx), rx)
    }

    fn authorization(id: &str) -> ElicitRequest {
        ElicitRequest {
            id: id.into(),
            kind: ElicitKind::Authorization {
                tool_name: "Bash".into(),
                paths: vec![],
                tool_use_id: None,
            },
            message: "run `rm -rf /`?".into(),
            options: ChannelElicitation::authorization_options(true),
        }
    }

    fn clarification(id: &str) -> ElicitRequest {
        ElicitRequest {
            id: id.into(),
            kind: ElicitKind::Clarification {
                header: Some("Branch".into()),
                tool_use_id: Some(id.into()),
            },
            message: "which branch?".into(),
            options: vec![ElicitOption {
                key: "a".into(),
                label: "main".into(),
            }],
        }
    }

    async fn registered<T>(pending: &Waiting<T>, id: &str) -> tokio::sync::oneshot::Sender<T> {
        loop {
            if let Some(tx) = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(id) {
                break tx;
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn an_answer_on_the_input_channel_comes_back_as_the_decision() {
        let (events, mut rx) = bus();
        let pending: Pending = Default::default();
        let asker = ChannelElicitation::new(events, pending.clone());

        let ask = tokio::spawn(async move { asker.ask(authorization("p1")).await });

        let emitted = rx.recv().await.expect("the question must reach the host");
        assert!(matches!(
            emitted,
            base::event::AgentEvent::PermissionPrompt { ref prompt_id, .. } if prompt_id == "p1"
        ));

        registered(&pending, "p1")
            .await
            .send(PermissionDecision::Permit)
            .unwrap();

        let outcome = ask.await.unwrap();
        assert!(matches!(
            outcome.answer_as::<PermissionDecision>(),
            Some(PermissionDecision::Permit)
        ));
    }

    /// The registration must not outlive the ask. A caller that gives up and
    /// drops the future leaves nothing for a late answer to wake.
    #[tokio::test]
    async fn giving_up_on_the_ask_removes_the_registration() {
        let (events, _rx) = bus();
        let pending: Pending = Default::default();
        let asker = ChannelElicitation::new(events, pending.clone());

        {
            let ask = asker.ask(authorization("p2"));
            tokio::pin!(ask);
            // Poll once so the registration is made, then never poll again.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), &mut ask).await;
            assert!(pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("p2"));
        }

        assert!(
            !pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("p2"),
            "a dropped ask must take its registration with it"
        );
    }

    #[tokio::test]
    async fn the_kinds_this_host_cannot_carry_are_declined_by_name() {
        let (events, _rx) = bus();
        let asker = ChannelElicitation::new(events, Default::default());

        for kind in [
            ElicitKind::Clarification {
                header: None,
                tool_use_id: None,
            },
            ElicitKind::Import { sources: vec![] },
        ] {
            let outcome = asker
                .ask(ElicitRequest {
                    id: "q".into(),
                    kind,
                    message: "?".into(),
                    options: vec![],
                })
                .await;
            assert!(
                outcome
                    .decline_reason()
                    .is_some_and(|r| r.contains("no way to put")),
                "an unservable kind must say why, not answer: {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_host_that_answers_questions_is_asked_and_its_answer_comes_back() {
        let (events, mut rx) = bus();
        let questions: PendingQuestions = Default::default();
        let asker =
            ChannelElicitation::new(events, Default::default()).with_questions(questions.clone());

        let ask = tokio::spawn(async move { asker.ask(clarification("tool-1")).await });

        match rx.recv().await.expect("the question must reach the host") {
            base::event::AgentEvent::QuestionPrompt {
                prompt_id,
                question,
                header,
                options,
                tool_use_id,
                ..
            } => {
                assert_eq!(prompt_id, "tool-1");
                assert_eq!(question, "which branch?");
                assert_eq!(header.as_deref(), Some("Branch"));
                assert_eq!(options.len(), 1);
                assert_eq!(tool_use_id.as_deref(), Some("tool-1"));
            }
            other => panic!("expected a question, got {other:?}"),
        }

        resolve_question_response(
            &questions,
            "tool-1",
            QuestionAnswer::Answer { text: "a".into() },
        );
        assert_eq!(
            ask.await.unwrap().answer_as::<String>().as_deref(),
            Some("a")
        );
        assert!(questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }

    #[tokio::test]
    async fn a_declined_question_comes_back_with_the_hosts_reason() {
        let (events, _rx) = bus();
        let questions: PendingQuestions = Default::default();
        let asker =
            ChannelElicitation::new(events, Default::default()).with_questions(questions.clone());

        let ask = tokio::spawn(async move { asker.ask(clarification("tool-2")).await });
        registered(&questions, "tool-2")
            .await
            .send(QuestionAnswer::Decline {
                reason: "not now".into(),
            })
            .unwrap();

        let outcome = ask.await.unwrap();
        assert!(
            outcome
                .decline_reason()
                .is_some_and(|r| r.contains("declined") && r.contains("not now")),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn an_answer_for_a_question_nobody_waits_on_is_ignored() {
        let questions: PendingQuestions = Default::default();
        resolve_question_response(
            &questions,
            "gone",
            QuestionAnswer::Answer { text: "a".into() },
        );
        assert!(questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }
}
