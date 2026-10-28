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
//!
//! Attachments that came with the message are shown read-only through
//! `inspect_with_attachments`; a middleware that only implements `inspect`
//! never sees them.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAttachmentKind {
    /// A path the runtime reads after screening; its content is not read
    /// for the middleware.
    File,
    /// Text the host already read.
    Text,
    /// An image the host already decoded.
    Image,
}

impl PromptAttachmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Text => "text",
            Self::Image => "image",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PromptAttachment<'a> {
    pub kind: PromptAttachmentKind,
    pub name: Option<&'a str>,
    pub media_type: Option<&'a str>,
    /// Bytes: the file's length on disk, the text's UTF-8 length, the
    /// image's decoded length. `None` when a file's metadata can't be read.
    pub size: Option<u64>,
    pub text: Option<&'a str>,
}

impl<'a> PromptAttachment<'a> {
    pub fn file(path: &'a str, media_type: Option<&'a str>, size: Option<u64>) -> Self {
        Self {
            kind: PromptAttachmentKind::File,
            name: Some(path),
            media_type,
            size,
            text: None,
        }
    }

    pub fn text(name: &'a str, content: &'a str) -> Self {
        Self {
            kind: PromptAttachmentKind::Text,
            name: Some(name),
            media_type: None,
            size: Some(content.len() as u64),
            text: Some(content),
        }
    }

    pub fn image(media_type: &'a str, size: u64) -> Self {
        Self {
            kind: PromptAttachmentKind::Image,
            name: None,
            media_type: Some(media_type),
            size: Some(size),
            text: None,
        }
    }
}

#[async_trait]
pub trait PromptMiddleware: Send + Sync {
    async fn inspect(&self, call: &PromptCall<'_>) -> PromptDecision;

    /// What the runtime calls. Attachments can't be rewritten: a `Rewrite`
    /// replaces the text only, a `Block` refuses the whole message.
    async fn inspect_with_attachments(
        &self,
        call: &PromptCall<'_>,
        attachments: &[PromptAttachment<'_>],
    ) -> PromptDecision {
        let _ = attachments;
        self.inspect(call).await
    }
}
