//! `Elicitation` — the one way the engine asks a human something.
//!
//! Three questions in this engine need a person: may this tool call proceed,
//! what did you mean, and shall I import the configuration I found. Each grew
//! its own mechanism — a channel protocol for the first, a process-level
//! callback trait for the third, and for the second a tool that hands its own
//! question back to the model and calls that an answer. A host wanting to
//! answer all three had to learn all three, and a host that could answer none
//! of them got a different failure from each.
//!
//! # Silence is not consent
//!
//! With no implementation registered, every kind of question is *declined*
//! with a reason, never granted. That is the whole reason this is a trait with
//! a default rather than an `Option` each caller interprets: the fallback is
//! written once, here, and it fails closed.
//!
//! # Why the answer is a `Value`
//!
//! The three questions have genuinely different answers — a permission
//! decision, a chosen option or free text, an import choice — and a trait
//! object cannot be generic over them. So the payload is serde-shaped and
//! [`ElicitKind`] documents which type belongs to it. Every call site parses
//! with the type it expects and treats a parse failure exactly like a
//! decline, which keeps a confused implementation from being read as consent.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// What is being asked, and how the answer will be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitKind {
    /// May this tool call run? The answer is the host's permission decision
    /// (`runtime::agent::PermissionDecision`'s wire form).
    Authorization {
        tool_name: String,
        /// Paths the call would touch, for a host that wants to show them.
        paths: Vec<PathBuf>,
        /// The `tool_use` block asking, when the asker knows it.
        tool_use_id: Option<String>,
    },
    /// The model wants something clarified before it continues. The answer is
    /// either one of the request's option keys or free text, as a JSON string.
    Clarification {
        /// Short label for the question, for a host with room for one.
        header: Option<String>,
    },
    /// Shall configuration detected from another agent tool be imported? The
    /// answer is a `base::interface::import_callback::ImportDecision`.
    Import {
        /// The detected sources, named for display.
        sources: Vec<String>,
    },
}

/// One offered answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitOption {
    /// What the answer carries if this one is picked.
    pub key: String,
    /// What the human reads.
    pub label: String,
}

/// One question put to a human.
#[derive(Debug, Clone)]
pub struct ElicitRequest {
    /// Correlates the answer with the question for hosts whose transport is
    /// asynchronous. Unique within a session.
    pub id: String,
    pub kind: ElicitKind,
    /// The question itself, already phrased for a person.
    pub message: String,
    /// Offered answers. Empty means free-form.
    pub options: Vec<ElicitOption>,
}

/// What came back.
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitOutcome {
    /// A human answered. The shape is [`ElicitKind`]'s to define.
    Answered(serde_json::Value),
    /// Nobody answered, or the answer was no. Always carries a reason: a
    /// caller that has to explain a refusal to a model, a log or a user
    /// should never have to invent one.
    Declined { reason: String },
}

impl ElicitOutcome {
    /// An answer, from whatever type the request's kind calls for.
    pub fn answered<T: Serialize>(answer: &T) -> Self {
        match serde_json::to_value(answer) {
            Ok(v) => Self::Answered(v),
            // Only reachable if a kind's answer type is not serializable,
            // which is a bug in the caller — but declining is still the safe
            // reading of "we could not produce an answer".
            Err(e) => Self::Declined {
                reason: format!("the answer could not be encoded: {e}"),
            },
        }
    }

    pub fn declined(reason: impl Into<String>) -> Self {
        Self::Declined {
            reason: reason.into(),
        }
    }

    /// Read the answer as the type this request's kind defines.
    ///
    /// `None` for a decline **and** for an answer that does not parse — a
    /// caller cannot act on an answer it does not understand, and guessing
    /// would be guessing about consent.
    pub fn answer_as<T: DeserializeOwned>(&self) -> Option<T> {
        match self {
            Self::Answered(v) => serde_json::from_value(v.clone()).ok(),
            Self::Declined { .. } => None,
        }
    }

    /// The reason a question went unanswered, for a caller reporting it.
    pub fn decline_reason(&self) -> Option<&str> {
        match self {
            Self::Declined { reason } => Some(reason),
            Self::Answered(_) => None,
        }
    }
}

/// The host's ability to ask a person something.
#[async_trait]
pub trait Elicitation: Send + Sync {
    /// Put `request` to a human and return their answer.
    ///
    /// May take as long as a person takes; callers impose their own timeout
    /// and cancellation. Implementations that cannot serve a particular
    /// [`ElicitKind`] must decline it with a reason rather than answer it
    /// wrongly — a host wired only for permission prompts saying "declined:
    /// this host has no way to ask" is useful, and one inventing an answer
    /// is not.
    async fn ask(&self, request: ElicitRequest) -> ElicitOutcome;
}

/// Declines everything, with a reason naming the absence.
///
/// The default when no host registered anything. Every path that asks a human
/// gets an explicit refusal instead of hanging, timing out, or — the failure
/// this exists to prevent — treating silence as a yes.
pub struct DeclineAll;

#[async_trait]
impl Elicitation for DeclineAll {
    async fn ask(&self, request: ElicitRequest) -> ElicitOutcome {
        let what = match &request.kind {
            ElicitKind::Authorization { tool_name, .. } => {
                format!("authorize `{tool_name}`")
            }
            ElicitKind::Clarification { .. } => "answer a question".to_string(),
            ElicitKind::Import { .. } => "confirm an import".to_string(),
        };
        ElicitOutcome::declined(format!(
            "no one is available to {what}: this engine was built without an \
             elicitation handler, and an unanswered question is not consent"
        ))
    }
}

/// Answers every question the same way, and remembers what it was asked.
///
/// For tests, and as the shape an embedder copies: the second implementation
/// that makes [`Elicitation`] a contract rather than one type's interface.
pub struct FixedElicitation {
    answer: ElicitOutcome,
    asked: std::sync::Mutex<Vec<ElicitRequest>>,
}

impl FixedElicitation {
    pub fn new(answer: ElicitOutcome) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Every request put to it, in order.
    pub fn asked(&self) -> Vec<ElicitRequest> {
        self.asked.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait]
impl Elicitation for FixedElicitation {
    async fn ask(&self, request: ElicitRequest) -> ElicitOutcome {
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request);
        self.answer.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kind: ElicitKind) -> ElicitRequest {
        ElicitRequest {
            id: "q1".into(),
            kind,
            message: "well?".into(),
            options: vec![],
        }
    }

    #[tokio::test]
    async fn nothing_registered_declines_every_kind_with_a_reason() {
        let kinds = [
            ElicitKind::Authorization {
                tool_name: "Bash".into(),
                paths: vec![],
                tool_use_id: None,
            },
            ElicitKind::Clarification { header: None },
            ElicitKind::Import { sources: vec![] },
        ];
        for kind in kinds {
            let out = DeclineAll.ask(req(kind.clone())).await;
            let reason = out
                .decline_reason()
                .unwrap_or_else(|| panic!("{kind:?} must be declined, never answered"));
            assert!(
                !reason.trim().is_empty(),
                "a decline without a reason is a shrug: {kind:?}"
            );
        }
    }

    /// The property the `Value` payload has to earn: an answer that does not
    /// parse as the kind's type reads as no answer, not as a default one.
    #[test]
    fn an_answer_of_the_wrong_shape_is_not_an_answer() {
        let out = ElicitOutcome::Answered(serde_json::json!({"type": "not-a-decision"}));
        assert_eq!(out.answer_as::<String>(), None);
        assert_eq!(
            ElicitOutcome::answered(&"picked".to_string()).answer_as::<String>(),
            Some("picked".to_string())
        );
    }
}
