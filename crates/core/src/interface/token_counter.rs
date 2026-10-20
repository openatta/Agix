//! `TokenCounter` — how the engine decides how big a conversation is.
//!
//! Every budget decision rests on one number: how many tokens the context
//! currently holds. Compaction triggers on it, strategies choose what to drop
//! by it, and the warning shown to the user quotes it. That number came from
//! one hardcoded algorithm — a local `cl100k_base` tokenizer standing in for
//! Anthropic's, which nobody publishes — with no way for a host to supply a
//! better one.
//!
//! A better one is not hypothetical. A host talking to a provider with a
//! published tokenizer can count exactly. A host willing to spend a call on
//! `/v1/messages/count_tokens` can be exact for Anthropic too. Both are
//! strictly more accurate than an estimate documented to run 5–15% high, and
//! neither could be plugged in.
//!
//! # An estimate, not a measurement
//!
//! Implementations are not required to be exact, and callers must not treat
//! the result as if they were: it decides when to compact, not what is billed.
//! An implementation that is *cheap* and slightly wrong is a better default
//! than one that is exact and costs a network round-trip per turn, which is
//! why the shipped default is the local estimate.

use crate::interface::model::{ModelContentBlock, ModelMessage};

/// Counts the tokens a piece of conversation will cost.
pub trait TokenCounter: Send + Sync {
    /// Tokens in a plain string.
    fn count_text(&self, text: &str) -> usize;

    /// Tokens in one content block, including whatever that block type costs
    /// beyond its text.
    fn count_block(&self, block: &ModelContentBlock) -> usize;

    /// Tokens in a conversation.
    ///
    /// Content only: per-message API framing (role, JSON punctuation) is a
    /// handful of tokens per message and is deliberately not modelled, so an
    /// implementation that ignores it agrees with the default.
    fn count_messages(&self, messages: &[ModelMessage]) -> usize {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .map(|b| self.count_block(b))
            .sum()
    }
}

/// Charges a fixed number of tokens per block and per character.
///
/// The second implementation, and the one tests want: a budget test that has
/// to reach a compaction threshold should say so in one line rather than by
/// generating enough prose to trip a real tokenizer, and a test asserting
/// *which* strategy ran should not change its answer when the tokenizer's
/// table does.
pub struct FixedTokenCounter {
    pub per_char: usize,
    pub per_block: usize,
}

impl FixedTokenCounter {
    pub fn new(per_char: usize, per_block: usize) -> Self {
        Self {
            per_char,
            per_block,
        }
    }
}

impl TokenCounter for FixedTokenCounter {
    fn count_text(&self, text: &str) -> usize {
        text.len() * self.per_char
    }

    fn count_block(&self, block: &ModelContentBlock) -> usize {
        let text = match block {
            ModelContentBlock::Text { text } => text.len(),
            ModelContentBlock::Thinking { text, .. } => text.len(),
            ModelContentBlock::RedactedThinking { data, .. } => data.len(),
            ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => tool_use_id.len() + content.len(),
            ModelContentBlock::ToolUse { id, name, input } => {
                id.len() + name.len() + input.to_string().len()
            }
            ModelContentBlock::Image { .. } => 0,
        };
        self.per_block + text * self.per_char
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_count_is_the_sum_of_its_blocks() {
        let counter = FixedTokenCounter::new(1, 10);
        let messages = vec![ModelMessage {
            role: crate::interface::model::MessageRole::User,
            content: vec![
                ModelContentBlock::Text {
                    text: "abcd".into(),
                },
                ModelContentBlock::Text { text: "ef".into() },
            ],
        }];
        assert_eq!(counter.count_messages(&messages), (10 + 4) + (10 + 2));
    }
}
