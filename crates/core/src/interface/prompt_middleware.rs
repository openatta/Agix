//! `PromptMiddleware` — what an embedding program says about a user message
//! before it enters the conversation.
//!
//! The Rust counterpart of a `UserPromptSubmit` hook, for rules that live in
//! the embedder's own code: it sees every user message, the one that opens a
//! turn and the ones steered into a running turn at a step boundary, and lets
//! it through, rewrites it, or refuses it. It runs after the hooks configured
//! in settings, in the order the middlewares were added, so the embedder's
//! rules are the last word: nothing configured can rewrite back what they
//! decided.

use async_trait::async_trait;

use crate::permission::PermissionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDelivery {
    /// The message opens a turn.
    Turn,
    /// The message joins the running turn at a step boundary.
    Steer,
}

impl PromptDelivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Steer => "steer",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PromptCall<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub content: &'a str,
    pub delivery: PromptDelivery,
    pub permission_mode: PermissionMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptDecision {
    Continue,
    Rewrite(String),
    /// A turn ends as a `UserPromptSubmit` block ends it (`stopped_by_hook`,
    /// no model call); a steered message is left out and the turn goes on.
    Block {
        message: Option<String>,
    },
}

#[async_trait]
pub trait PromptMiddleware: Send + Sync {
    async fn inspect(&self, call: &PromptCall<'_>) -> PromptDecision;
}
