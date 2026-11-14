//! 悬空 tool_use 的配对修复。

use crate::turn::request::{awaits_result, pair_dangling_tool_uses};
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};

fn msg(role: MessageRole, content: Vec<ModelContentBlock>) -> ModelMessage {
    ModelMessage { role, content }
}

fn tool_use(id: &str) -> ModelContentBlock {
    ModelContentBlock::ToolUse {
        id: id.into(),
        name: "Bash".into(),
        input: serde_json::json!({}),
    }
}

fn tool_result(id: &str) -> ModelContentBlock {
    ModelContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: "ok".into(),
        is_error: None,
    }
}

fn result_ids(m: &ModelMessage) -> Vec<&str> {
    m.content
        .iter()
        .filter_map(|b| match b {
            ModelContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_transcript_that_ends_on_a_tool_use_gets_a_result_after_it() {
    let out = pair_dangling_tool_uses(vec![
        msg(
            MessageRole::User,
            vec![ModelContentBlock::Text { text: "go".into() }],
        ),
        msg(MessageRole::Assistant, vec![tool_use("a")]),
    ]);
    assert_eq!(out.len(), 3);
    assert_eq!(out[2].role, MessageRole::User);
    assert_eq!(result_ids(&out[2]), ["a"]);
}

#[test]
fn a_partly_answered_batch_gets_the_missing_results_first() {
    let out = pair_dangling_tool_uses(vec![
        msg(MessageRole::Assistant, vec![tool_use("a"), tool_use("b")]),
        msg(
            MessageRole::User,
            vec![
                tool_result("b"),
                ModelContentBlock::Text {
                    text: "next".into(),
                },
            ],
        ),
    ]);
    assert_eq!(out.len(), 2);
    assert_eq!(result_ids(&out[1]), ["a", "b"]);
}

#[test]
fn results_that_answer_no_tool_use_before_them_are_dropped() {
    let out = pair_dangling_tool_uses(vec![
        msg(MessageRole::User, vec![tool_result("ghost")]),
        msg(MessageRole::Assistant, vec![tool_use("a")]),
        msg(
            MessageRole::User,
            vec![tool_result("a"), tool_result("a"), tool_result("x")],
        ),
    ]);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].role, MessageRole::Assistant);
    assert_eq!(result_ids(&out[1]), ["a"]);
}

#[test]
fn results_in_later_user_messages_join_the_first() {
    let out = pair_dangling_tool_uses(vec![
        msg(MessageRole::Assistant, vec![tool_use("a"), tool_use("b")]),
        msg(MessageRole::User, vec![tool_result("a")]),
        msg(MessageRole::User, vec![tool_result("b")]),
    ]);
    assert_eq!(out.len(), 2);
    assert_eq!(result_ids(&out[1]), ["a", "b"]);
}

#[test]
fn a_result_is_awaited_only_for_an_unanswered_call_of_the_last_reply() {
    let transcript = vec![
        msg(MessageRole::Assistant, vec![tool_use("old")]),
        msg(MessageRole::User, vec![tool_result("old")]),
        msg(MessageRole::Assistant, vec![tool_use("a"), tool_use("b")]),
        msg(MessageRole::User, vec![tool_result("a")]),
    ];
    assert!(awaits_result(&transcript, "b"));
    assert!(!awaits_result(&transcript, "a"));
    assert!(!awaits_result(&transcript, "old"));
    assert!(!awaits_result(&transcript, "made-up"));
}

#[test]
fn a_paired_transcript_is_left_alone() {
    let input = vec![
        msg(MessageRole::Assistant, vec![tool_use("a")]),
        msg(MessageRole::User, vec![tool_result("a")]),
        msg(
            MessageRole::Assistant,
            vec![ModelContentBlock::Text {
                text: "done".into(),
            }],
        ),
    ];
    let out = pair_dangling_tool_uses(input.clone());
    assert_eq!(format!("{out:?}"), format!("{input:?}"));
}
