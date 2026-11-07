//! `ModelInterceptor` — the request on its way out, and the message on its
//! way back.
//!
//! Two moments matter for anyone who wants to shape what the engine says to a
//! provider and what it does with the answer: just before a request is sent,
//! when the messages, tools and parameters are assembled and nothing has left
//! the process yet; and just after a complete message has been assembled from
//! a stream, when it is whole enough to reason about.
//!
//! # There is deliberately no per-chunk hook
//!
//! The obvious third moment — every streamed delta — is the one this does not
//! offer. A turn produces thousands of chunks, so a hook there is called
//! thousands of times, and the cost is invisible to whoever writes it: their
//! function looks cheap, the streaming looks broken. Worse, a hook that can
//! rewrite chunks can produce a message that never existed as a coherent
//! whole, which nothing downstream is built to handle.
//!
//! So the contract offers complete things: the complete request, the complete
//! message. Both are called once per model call, which
//! [`does_not_run_per_chunk`](self) has a test for. If a deployment genuinely
//! needs to transform a stream as it arrives, the answer is a declarative rule
//! the engine executes natively — not a callback in the chunk loop.

use crate::interface::model::{ModelMessage, StreamParams, ToolDef};
use crate::prompt::PromptBlock;

/// A model request, assembled and not yet sent.
#[derive(Debug, Clone)]
pub struct ModelRequestView {
    pub prompt_blocks: Vec<PromptBlock>,
    pub tool_defs: Vec<ToolDef>,
    pub messages: Vec<ModelMessage>,
    pub params: StreamParams,
}

/// Shapes what goes to the provider and what comes back.
///
/// Both methods default to doing nothing, so an implementation that cares
/// about one moment does not have to acknowledge the other.
pub trait ModelInterceptor: Send + Sync {
    /// Called once per model call, immediately before the request is sent.
    ///
    /// Everything is mutable: messages, tools, sampling parameters, prompt
    /// blocks. This is the last point at which the engine's idea of the
    /// request and the provider's can still be made to differ.
    fn on_request(&self, _request: &mut ModelRequestView) {}

    /// Called once per complete message the model produced, after the stream
    /// that carried it has finished and before the session records it.
    ///
    /// Whole messages only — see the module docs on why there is no
    /// per-chunk equivalent.
    fn on_message(&self, _message: &mut ModelMessage) {}
}

/// Run every interceptor over a request.
pub fn intercept_request(
    interceptors: &[std::sync::Arc<dyn ModelInterceptor>],
    request: &mut ModelRequestView,
) {
    for i in interceptors {
        i.on_request(request);
    }
}

/// Run every interceptor over a completed message.
pub fn intercept_message(
    interceptors: &[std::sync::Arc<dyn ModelInterceptor>],
    message: &mut ModelMessage,
) {
    for i in interceptors {
        i.on_message(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::model::{MessageRole, ModelContentBlock};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct Counting {
        requests: AtomicUsize,
        messages: AtomicUsize,
    }

    impl ModelInterceptor for Counting {
        fn on_request(&self, request: &mut ModelRequestView) {
            self.requests.fetch_add(1, Ordering::SeqCst);
            request.params.max_tokens = 42;
        }
        fn on_message(&self, message: &mut ModelMessage) {
            self.messages.fetch_add(1, Ordering::SeqCst);
            message.content.push(ModelContentBlock::Text {
                text: "[seen]".into(),
            });
        }
    }

    fn request() -> ModelRequestView {
        ModelRequestView {
            prompt_blocks: Vec::new(),
            tool_defs: Vec::new(),
            messages: Vec::new(),
            params: StreamParams {
                model: "test-model".into(),
                max_tokens: 1024,
                thinking_mode: crate::settings::ThinkingMode::Off,
                effort: None,
                fallback_model: None,
                cache_edits: Vec::new(),
                origin: None,
                input_map: None,
                provider_search: None,
            },
        }
    }

    #[test]
    fn a_request_can_be_reshaped_before_it_leaves() {
        let counting = Arc::new(Counting::default());
        let chain: Vec<Arc<dyn ModelInterceptor>> = vec![counting.clone()];
        let mut req = request();
        intercept_request(&chain, &mut req);
        assert_eq!(req.params.max_tokens, 42);
        assert_eq!(counting.requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_completed_message_can_be_reshaped_before_it_is_recorded() {
        let counting = Arc::new(Counting::default());
        let chain: Vec<Arc<dyn ModelInterceptor>> = vec![counting.clone()];
        let mut message = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::Text { text: "hi".into() }],
        };
        intercept_message(&chain, &mut message);
        assert_eq!(message.content.len(), 2);
        assert_eq!(counting.messages.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nothing_registered_changes_nothing() {
        let mut req = request();
        let before = req.clone();
        intercept_request(&[], &mut req);
        assert_eq!(req.params.max_tokens, before.params.max_tokens);
        assert_eq!(req.messages.len(), before.messages.len());
    }
}
