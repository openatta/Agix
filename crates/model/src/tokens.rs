//! Token 估算 —— 不加载任何词表的字符类启发式。
//!
//! Anthropic 不公开 tokenizer，本地数只能是近似；真实用量以 API 返回的 usage 为准。
//! 这里按字符类计数：ASCII 单词按长度折算，数字每 3 位一个，标点连串每 3 个一个，
//! 空格与缩进不计，连续换行算一个，CJK 每字一个。对照 `cl100k_base`，代码与 JSON 高估约 8–12%，
//! 英文文档约 4%，中文文档低估约 4%——用作上下文预算与压缩阈值足够，且偏高一侧更安全。

use crate::types::{MessagesRequest, SystemBlock};
use base::interface::model::{ModelContentBlock, ModelMessage, IMAGE_TOKEN_ESTIMATE};
use base::message::{ContentBlock, ToolResultContent};
use std::sync::OnceLock;

const SHORT_WORD: usize = 12;
const CHARS_PER_WORD_TOKEN: usize = 5;
const DIGITS_PER_TOKEN: usize = 3;
const PUNCT_PER_TOKEN: usize = 3;

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F
        | 0x3040..=0x30FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF
        | 0xFF00..=0xFFEF
        | 0x20000..=0x2FFFF)
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphabetic() || (!c.is_ascii() && c.is_alphabetic() && !is_cjk(c))
}

fn run_len(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    pred: impl Fn(char) -> bool,
) -> usize {
    let mut len = 1;
    while chars.next_if(|&d| pred(d)).is_some() {
        len += 1;
    }
    len
}

/// Estimated token count of `s`.
pub fn count_text(s: &str) -> usize {
    let mut n = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if is_word_char(c) {
            let mut len = 1;
            let mut ascii = c.is_ascii();
            while let Some(d) = chars.next_if(|&d| is_word_char(d)) {
                ascii &= d.is_ascii();
                len += 1;
            }
            n += match (ascii, len) {
                (true, l) if l <= SHORT_WORD => 1,
                (true, l) => l.div_ceil(CHARS_PER_WORD_TOKEN),
                (false, l) => l.div_ceil(2),
            };
        } else if c.is_ascii_digit() {
            n += run_len(&mut chars, |d| d.is_ascii_digit()).div_ceil(DIGITS_PER_TOKEN);
        } else if c == ' ' || c == '\t' {
            // cl100k folds spaces into the next token, indentation included.
            run_len(&mut chars, |d| d == ' ' || d == '\t');
        } else if c == '\n' || c == '\r' {
            run_len(&mut chars, |d| d == '\n' || d == '\r');
            n += 1;
        } else if is_cjk(c) {
            n += 1;
        } else if c.is_ascii() {
            n += run_len(&mut chars, |d| d.is_ascii_punctuation()).div_ceil(PUNCT_PER_TOKEN);
        } else {
            n += 2;
        }
    }
    n
}

/// Estimate input tokens for a slice of [`ModelMessage`]s — the protocol-agnostic
/// message type that `session` and `compaction` operate on.
///
/// This is the **single** estimator for the context-budget path:
/// `session::SessionManager::token_count`,
/// `compaction::grouping::estimate_tokens` and
/// `compaction::compact::analyze_context` all delegate here, so the number
/// that trips compaction, the number that decides which rounds to drop, and
/// the number reported in telemetry are the same number.
///
/// Counts message *content* only. Per-message API framing (role, JSON
/// punctuation) is a handful of tokens per message and is deliberately not
/// modelled — same omission as [`estimate_input_tokens`].
pub fn estimate_message_tokens(messages: &[ModelMessage]) -> usize {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(estimate_model_block_tokens)
        .sum()
}

/// Estimate input tokens for a single [`ModelContentBlock`].
pub fn estimate_model_block_tokens(block: &ModelContentBlock) -> usize {
    match block {
        ModelContentBlock::Text { text } => count_text(text),
        // Flat estimate. Counting the base64 payload would read a 1 MB
        // screenshot as hundreds of thousands of tokens and trip compaction on
        // the first image the user pastes — see `IMAGE_TOKEN_ESTIMATE`.
        ModelContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
        // Thinking is billed as text. The signature is an opaque blob that the
        // API requires us to echo back; it is not counted.
        ModelContentBlock::Thinking { text, .. } => count_text(text),
        ModelContentBlock::RedactedThinking { data, .. } => count_text(data),
        ModelContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => count_text(tool_use_id) + count_text(content),
        ModelContentBlock::ToolUse { id, name, input } => {
            count_text(id) + count_text(name) + count_text(&input.to_string())
        }
    }
}

/// The engine's default [`TokenCounter`]: everything above, behind the
/// contract, so a host with a better answer can supply one.
///
/// Stateless, so constructing one costs nothing and any number of them agree.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicCounter;

impl base::interface::token_counter::TokenCounter for HeuristicCounter {
    fn count_text(&self, text: &str) -> usize {
        count_text(text)
    }

    fn count_block(&self, block: &ModelContentBlock) -> usize {
        estimate_model_block_tokens(block)
    }

    fn count_messages(&self, messages: &[ModelMessage]) -> usize {
        estimate_message_tokens(messages)
    }
}

/// The counter used wherever nothing more specific was supplied.
pub fn default_counter() -> std::sync::Arc<dyn base::interface::token_counter::TokenCounter> {
    static C: OnceLock<std::sync::Arc<dyn base::interface::token_counter::TokenCounter>> =
        OnceLock::new();
    C.get_or_init(|| std::sync::Arc::new(HeuristicCounter))
        .clone()
}

/// 估算一段 `MessagesRequest` 的输入 token 数（不含模型输出）。
///
/// 覆盖：
/// - system blocks 文本
/// - tools[] 的 description + input_schema（schema 序列化成 JSON 字符串）
/// - messages[] 的所有 ContentBlock
///
/// 不覆盖（轻微低估）：
/// - role / 字段名等 API 框架开销（实际占 ~5-15 token）
/// - JSON 结构本身（`{` `}` `,` 等）
pub fn estimate_input_tokens(req: &MessagesRequest) -> usize {
    let mut total = 0usize;

    // system
    for block in &req.system {
        let SystemBlock::Text { text, .. } = block;
        total += count_text(text);
    }

    // tools
    for tool in &req.tools {
        total += count_text(&tool.description);
        total += count_text(&tool.input_schema.to_string());
        total += count_text(&tool.name);
    }

    // messages
    for msg in &req.messages {
        for block in &msg.content {
            total += estimate_block_tokens(block);
        }
    }

    total
}

fn estimate_block_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text, .. } => count_text(text),
        ContentBlock::ToolUse { name, input, id } => {
            count_text(name) + count_text(id) + count_text(&input.to_string())
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            let id_tokens = count_text(tool_use_id);
            let body_tokens = match content {
                ToolResultContent::Text(t) => count_text(t),
                ToolResultContent::Blocks(blocks) => blocks.iter().map(estimate_block_tokens).sum(),
            };
            id_tokens + body_tokens
        }
        ContentBlock::Thinking { thinking, .. } => count_text(thinking),
        ContentBlock::RedactedThinking { data, .. } => count_text(data),
        // Flat per-image estimate, shared with the interface-level estimator so
        // the wire-format and context-budget views of the same image agree.
        ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
        // Cache edits are metadata-only (delete-tool-result operations), negligible token cost
        ContentBlock::CacheEdits { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageParam, ToolDef};
    use base::message::Role;
    use serde_json::json;

    fn empty_request() -> MessagesRequest {
        MessagesRequest::minimal("test", "")
    }

    #[test]
    fn empty_messages_zero_tokens_in_user() {
        let mut req = empty_request();
        req.messages = vec![]; // 完全空
        assert_eq!(estimate_input_tokens(&req), 0);
    }

    #[test]
    fn user_message_text_counted() {
        let req = MessagesRequest::minimal("test", "hello world");
        let n = estimate_input_tokens(&req);
        assert!((2..=5).contains(&n), "expected 2-5 tokens, got {n}");
    }

    #[test]
    fn longer_text_more_tokens() {
        let short = MessagesRequest::minimal("test", "hi");
        let long = MessagesRequest::minimal(
            "test",
            "hello world this is a longer message with many tokens",
        );
        assert!(estimate_input_tokens(&long) > estimate_input_tokens(&short));
    }

    #[test]
    fn system_block_counted() {
        let mut req = empty_request();
        req.system = vec![SystemBlock::text("system instructions go here")];
        let n_with = estimate_input_tokens(&req);
        req.system.clear();
        let n_without = estimate_input_tokens(&req);
        assert!(n_with > n_without, "system block should increase count");
    }

    #[test]
    fn tools_contribute_tokens() {
        let mut req = empty_request();
        req.tools = vec![ToolDef {
            name: "Bash".into(),
            description: "Run a shell command. Use sparingly.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }),
            cache_control: None,
            defer_loading: None,
            strict: None,
        }];
        let n = estimate_input_tokens(&req);
        // tools 至少应当 > 5 token
        assert!(n > 5, "expected >5 tokens for one ToolDef, got {n}");
    }

    #[test]
    fn tool_use_block_counted() {
        let mut req = empty_request();
        req.messages = vec![MessageParam {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_01".into(),
                name: "Bash".into(),
                input: json!({"command": "ls -la"}),
            }],
        }];
        let n = estimate_input_tokens(&req);
        assert!(n > 3, "expected >3 tokens for tool_use, got {n}");
    }

    #[test]
    fn tool_result_text_counted() {
        let mut req = empty_request();
        req.messages = vec![MessageParam {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: ToolResultContent::Text("a\nb\nc".into()),
                is_error: false,
            }],
        }];
        let n = estimate_input_tokens(&req);
        assert!(n > 3);
    }

    #[test]
    fn image_block_costs_1500() {
        let mut req = empty_request();
        req.messages = vec![MessageParam {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: base::message::ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "tiny".into(), // 数据本身不再额外计 —— 我们用固定估算
                },
            }],
        }];
        let n = estimate_input_tokens(&req);
        assert!(n >= 1500, "image should cost ≈1500 tokens; got {n}");
    }

    use base::interface::model::MessageRole;

    fn model_user(blocks: Vec<ModelContentBlock>) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: blocks,
        }
    }

    #[test]
    fn message_tokens_are_real_tokens_not_char_quarters() {
        // "len() / 4" would call this 7; a long word is several tokens, not one.
        let msgs = vec![model_user(vec![ModelContentBlock::Text {
            text: "antidisestablishmentarianism".into(),
        }])];
        let n = estimate_message_tokens(&msgs);
        assert!(n >= 4, "expected real tokenization, got {n}");
    }

    #[test]
    fn image_block_is_flat_estimate_not_base64_length() {
        // A ~1 MB base64 payload. Under `len() / 4` this reads as ~250_000
        // tokens, which trips compaction on the first pasted screenshot.
        let msgs = vec![model_user(vec![ModelContentBlock::Image {
            media_type: "image/png".into(),
            data: "A".repeat(1_000_000),
        }])];
        assert_eq!(estimate_message_tokens(&msgs), IMAGE_TOKEN_ESTIMATE);
    }

    #[test]
    fn empty_messages_are_zero_tokens() {
        assert_eq!(estimate_message_tokens(&[]), 0);
    }

    #[test]
    fn tool_use_counts_its_serialized_input() {
        let small = vec![model_user(vec![ModelContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
        }])];
        let large = vec![model_user(vec![ModelContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "grep -rn pattern ./src --include='*.rs'"}),
        }])];
        assert!(
            estimate_message_tokens(&large) > estimate_message_tokens(&small),
            "tool input must contribute tokens"
        );
    }

    #[test]
    fn estimates_track_the_shape_of_text() {
        assert_eq!(count_text(""), 0);
        assert_eq!(count_text("hello world"), 2);
        assert_eq!(count_text("1234567"), 3);
        assert_eq!(count_text("这是中文"), 4);
        assert_eq!(count_text("fn main() {}"), 4);
        assert_eq!(count_text("    indented"), count_text("indented"));
        assert_eq!(count_text("a\n\n\nb"), 3);
    }

    #[test]
    fn multibyte_text_does_not_panic_and_costs_tokens() {
        let msgs = vec![model_user(vec![ModelContentBlock::Text {
            text: "这是一段用于测试的中文内容".repeat(100),
        }])];
        assert!(estimate_message_tokens(&msgs) > 0);
    }
}

#[cfg(test)]
mod counter_contract_tests {
    use super::*;
    use base::interface::token_counter::TokenCounter;

    /// The contract's default implementation must be the algorithm the free
    /// functions already use — otherwise "the number that trips compaction
    /// and the number that decides what to drop are the same number" stops
    /// being true the moment a caller moves to the trait.
    #[test]
    fn the_default_counter_agrees_with_the_free_functions() {
        let counter = HeuristicCounter;
        let messages = vec![ModelMessage {
            role: base::interface::model::MessageRole::User,
            content: vec![
                ModelContentBlock::Text {
                    text: "a moderately long piece of prose, long enough to matter".repeat(20),
                },
                ModelContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "git status"}),
                },
                ModelContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            ],
        }];

        assert_eq!(
            counter.count_messages(&messages),
            estimate_message_tokens(&messages)
        );
        for block in &messages[0].content {
            assert_eq!(
                counter.count_block(block),
                estimate_model_block_tokens(block)
            );
        }
        assert_eq!(counter.count_text("hello there"), count_text("hello there"));
    }

    #[test]
    fn the_default_counter_is_the_one_handed_out() {
        let messages = vec![ModelMessage {
            role: base::interface::model::MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: "hello".into(),
            }],
        }];
        assert_eq!(
            default_counter().count_messages(&messages),
            estimate_message_tokens(&messages)
        );
    }
}
