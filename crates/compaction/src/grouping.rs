//! Message grouping by API round.
//!
//! Groups messages at API-response granularity. Each API call within a user
//! turn produces its own round, so tool-call loops are split into finer units
//! for compact strategies (snip, micro-compact, etc.) to operate on.

use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};

/// A group of messages belonging to one API round.
#[derive(Debug, Clone)]
pub struct ApiRound {
    /// Index of the first message in this round (in the original flat array).
    pub start: usize,
    /// Messages in this round.
    pub messages: Vec<ModelMessage>,
    /// Approximate token count of all messages in this round.
    pub estimated_tokens: usize,
}

/// Group flat messages into API rounds.
///
/// A new round starts at:
/// 1. User text messages (start of a user turn), OR
/// 2. Assistant messages that follow a User ToolResult — i.e. subsequent API
///    calls within the same user turn.
///
/// Tool result messages (User role with ToolResult blocks) always stay in the
/// same round as their preceding assistant message.
pub fn group_by_api_round(messages: &[ModelMessage]) -> Vec<ApiRound> {
    if messages.is_empty() {
        return vec![];
    }

    let mut rounds: Vec<ApiRound> = Vec::new();
    let mut current: Vec<ModelMessage> = Vec::new();
    let mut start_idx = 0;
    let mut prev_was_tool_result = false;

    for (i, msg) in messages.iter().enumerate() {
        let is_user_text = msg.role == MessageRole::User
            && msg
                .content
                .first()
                .is_some_and(|b| matches!(b, ModelContentBlock::Text { .. }));
        let is_tool_result = msg.role == MessageRole::User
            && msg
                .content
                .first()
                .is_some_and(|b| matches!(b, ModelContentBlock::ToolResult { .. }));
        let is_assistant = msg.role == MessageRole::Assistant;

        // New round starts on:
        // - User text (fresh user input)
        // - Assistant after tool results (next API call in loop)
        let is_round_start = is_user_text || (is_assistant && prev_was_tool_result);

        if is_round_start && !current.is_empty() {
            rounds.push(finish_round(&current, start_idx));
            current = Vec::new();
            start_idx = i;
        }
        current.push(msg.clone());
        prev_was_tool_result = is_tool_result;
    }

    if !current.is_empty() {
        rounds.push(finish_round(&current, start_idx));
    }

    rounds
}

fn finish_round(messages: &[ModelMessage], start: usize) -> ApiRound {
    let tokens = estimate_tokens(messages);
    ApiRound {
        start,
        messages: messages.to_vec(),
        estimated_tokens: tokens,
    }
}

/// Estimate tokens for a slice of messages.
///
/// Delegates to the workspace's one estimator
/// ([`model::tokens::estimate_message_tokens`]), the same one
/// `session::SessionManager::token_count` falls back to, so the number that
/// decides *whether* to compact and the number that decides *how much* to
/// drop are the same number.
///
/// Note this is additive per message and per block, which
/// `DefaultCompactor::snip` relies on: the token cost of a concatenation of
/// rounds equals the sum of the rounds' `estimated_tokens`.
pub fn estimate_tokens(msgs: &[ModelMessage]) -> usize {
    model::tokens::estimate_message_tokens(msgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_user(s: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: s.to_string(),
            }],
        }
    }

    fn tool_result(id: &str, content: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: Some(false),
            }],
        }
    }

    fn assistant_text(s: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::Text {
                text: s.to_string(),
            }],
        }
    }

    fn assistant_tool_use(id: &str, name: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
            }],
        }
    }

    #[test]
    fn groups_two_user_messages() {
        let msgs = vec![
            text_user("hello"),
            assistant_text("hi"),
            text_user("how are you"),
            assistant_text("good"),
        ];
        let rounds = group_by_api_round(&msgs);
        assert_eq!(rounds.len(), 2);
        assert_eq!(rounds[0].messages.len(), 2);
        assert_eq!(rounds[1].messages.len(), 2);
    }

    #[test]
    fn tool_results_stay_in_same_round_as_preceding_assistant() {
        // New grouping: round splits at assistant-after-tool-result boundaries.
        // Round 0: [user text, assistant tool_use, tool_result] — user + first API response + result
        // Round 1: [assistant text] — second API response (after tool result)
        // Round 2: [next user text]
        let msgs = vec![
            text_user("read file"),
            assistant_tool_use("t1", "Read"),
            tool_result("t1", "file content"),
            assistant_text("done"),
            text_user("edit it"),
        ];
        let rounds = group_by_api_round(&msgs);
        assert_eq!(rounds.len(), 3, "expected 3 rounds");
        // Round 0: user text + assistant tool_use + tool result (tool result stays in same round)
        assert_eq!(rounds[0].messages.len(), 3);
        // Round 1: assistant text (second API response, starts after tool result boundary)
        assert_eq!(rounds[1].messages.len(), 1);
        // Round 2: user text
        assert_eq!(rounds[2].messages.len(), 1);
    }

    #[test]
    fn multi_turn_tool_loop() {
        // Simulate: user → assistant → result → assistant → result → assistant → user
        // Round splits at each assistant-after-tool-result boundary.
        // Round 0: [user text, assistant tool_use t1, tool result t1]
        // Round 1: [assistant tool_use t2, tool result t2]
        // Round 2: [assistant text]
        // Round 3: [user text]
        let msgs = vec![
            text_user("do it"),
            assistant_tool_use("t1", "Read"),
            tool_result("t1", "content"),
            assistant_tool_use("t2", "Edit"),
            tool_result("t2", "edited"),
            assistant_text("all done"),
            text_user("next"),
        ];
        let rounds = group_by_api_round(&msgs);
        assert_eq!(rounds.len(), 4);
        assert_eq!(rounds[0].messages.len(), 3); // user + assistant t1 + result t1
        assert_eq!(rounds[1].messages.len(), 2); // assistant t2 + result t2
        assert_eq!(rounds[2].messages.len(), 1); // assistant text
        assert_eq!(rounds[3].messages.len(), 1); // user text
    }

    #[test]
    fn empty_messages() {
        let rounds = group_by_api_round(&[]);
        assert!(rounds.is_empty());
    }
}
