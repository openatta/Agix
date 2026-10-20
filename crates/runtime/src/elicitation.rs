//! The engine's own answer to [`Elicitation`]: ask over the session's event
//! stream and wait for the host to answer on the input channel.
//!
//! This is the mechanism that was inlined in the turn loop — register a
//! one-shot under a prompt id, emit `AgentEvent::PermissionPrompt`, await the
//! answer — with two things it did not have. It is behind the contract, so a
//! library embedder can replace it with a direct call into their own UI
//! instead of pumping a channel protocol. And it cleans up after itself: if
//! the caller gives up (its timeout fires, the turn is cancelled), dropping
//! the future removes the registration, so a late answer for that id is
//! discarded rather than waking a call that already resolved.
//!
//! # What it cannot serve
//!
//! The host protocol this speaks carries exactly one question — may this tool
//! call proceed. There is no wire form for a clarification or an import
//! confirmation, so those are declined, with a reason that says so. Answering
//! them wrongly would be worse than not answering: a made-up answer to "what
//! did you mean" is a made-up instruction.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base::interface::elicitation::{
    ElicitKind, ElicitOption, ElicitOutcome, ElicitRequest, Elicitation,
};

use crate::agent::{EventSender, PermissionDecision};

type Pending = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<PermissionDecision>>>>;

/// Asks over the session's event stream.
pub struct ChannelElicitation {
    events: EventSender,
    pending: Pending,
}

impl ChannelElicitation {
    pub fn new(events: EventSender, pending: Pending) -> Self {
        Self { events, pending }
    }

    /// The answers a host may give an authorization question, in the order a
    /// UI should offer them. The keys are `PermissionDecision`'s own wire
    /// tags, so a host can build the answer from the key it was shown.
    pub fn authorization_options() -> Vec<ElicitOption> {
        vec![
            ElicitOption {
                key: "permit".into(),
                label: "Allow once".into(),
            },
            ElicitOption {
                key: "permit_always".into(),
                label: "Allow every time".into(),
            },
            ElicitOption {
                key: "deny".into(),
                label: "Deny".into(),
            },
        ]
    }
}

/// Removes a prompt registration unless the answer arrived.
///
/// The turn used to do this by hand in each of its give-up branches. Tying it
/// to the future's lifetime instead means a caller that drops the ask for a
/// reason nobody has thought of yet still cannot leave a stale registration
/// behind.
struct Registration {
    id: String,
    pending: Pending,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

#[async_trait]
impl Elicitation for ChannelElicitation {
    async fn ask(&self, request: ElicitRequest) -> ElicitOutcome {
        let (tool_name, paths, tool_use_id) = match request.kind {
            ElicitKind::Authorization {
                tool_name,
                paths,
                tool_use_id,
            } => (tool_name, paths, tool_use_id),
            other => {
                let what = match other {
                    ElicitKind::Clarification { .. } => "a clarification",
                    ElicitKind::Import { .. } => "an import confirmation",
                    ElicitKind::Authorization { .. } => unreachable!(),
                };
                return ElicitOutcome::declined(format!(
                    "this host is connected for permission prompts only and has no way to \
                     put {what} to a person; register an `Elicitation` implementation that \
                     can reach your UI to change that"
                ));
            }
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(request.id.clone(), tx);
        let _registration = Registration {
            id: request.id.clone(),
            pending: self.pending.clone(),
        };

        // Registered before emitting, so an answer that comes back instantly
        // still finds somewhere to land.
        let _ = self.events.send(base::event::AgentEvent::PermissionPrompt {
            prompt_id: request.id,
            tool_name,
            message: request.message,
            paths,
            turn_id: String::new(),
            tool_use_id,
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
            options: ChannelElicitation::authorization_options(),
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

        let tx = loop {
            if let Some(tx) = pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove("p1")
            {
                break tx;
            }
            tokio::task::yield_now().await;
        };
        tx.send(PermissionDecision::Permit).unwrap();

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
            ElicitKind::Clarification { header: None },
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
}
