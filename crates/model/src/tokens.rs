//! Token 估算 —— 用 tiktoken `cl100k_base` 做近似（Anthropic 不公开精确 tokenizer，
//! cl100k 实测高估约 5-15%，足以用作 autoCompact 阈值判断）。
//!
//! 真实 token 数应当走 Anthropic 的 `/v1/messages/count_tokens` 端点，但那需要
//! 网络调用 + API 配额；本地估算是它的廉价代理。

use crate::types::{MessagesRequest, SystemBlock};
use base::interface::model::{ModelContentBlock, ModelMessage, IMAGE_TOKEN_ESTIMATE};
use base::message::{ContentBlock, ToolResultContent};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use tiktoken_rs::CoreBPE;

/// 进程级 BPE 单例。`cl100k_base` BPE 表会从 tiktoken-rs 内嵌资源里加载（数 MB）；
/// 第一次访问后缓存。
fn bpe() -> &'static CoreBPE {
    static B: OnceLock<CoreBPE> = OnceLock::new();
    B.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base BPE table bundled and valid"))
}

// ── Memoized text counting ──
//
// The compaction path re-counts the *same* conversation several times per turn
// (budget check → warning check → threshold check → per-round grouping →
// per-strategy re-check), and a near-full context is on the order of a megabyte
// of text. Running the BPE over all of it every time is the difference between
// "free" and "hundreds of milliseconds per turn", which is what kept the
// compaction path on `len() / 4` in the first place.
//
// So: memoize by content hash. Old messages are immutable once pushed, so the
// same block hashes to the same key on every pass and is encoded exactly once.
// Only bodies above `CACHE_MIN_LEN` are cached — short strings encode faster
// than the hash+lookup would cost, and caching them would evict the large tool
// results that actually matter.
//
// A 64-bit hash collision would yield a slightly wrong *estimate*; nothing here
// is load-bearing beyond a budget threshold, so that trade is deliberate.
const CACHE_MIN_LEN: usize = 512;
const CACHE_MAX_ENTRIES: usize = 4096;

fn text_cache() -> &'static Mutex<HashMap<u64, usize>> {
    static C: OnceLock<Mutex<HashMap<u64, usize>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Token count of `s` under the shared BPE, memoized for large inputs.
fn count_text(bpe: &CoreBPE, s: &str) -> usize {
    if s.len() < CACHE_MIN_LEN {
        return bpe.encode_with_special_tokens(s).len();
    }
    let key = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.hash(&mut h);
        h.finish()
    };
    if let Ok(cache) = text_cache().lock() {
        if let Some(&n) = cache.get(&key) {
            return n;
        }
    }
    let n = bpe.encode_with_special_tokens(s).len();
    if let Ok(mut cache) = text_cache().lock() {
        // Crude bound: a full cache is dropped wholesale rather than evicted by
        // recency. The working set is one conversation; when it turns over
        // completely, re-encoding it once is cheaper than tracking LRU order.
        if cache.len() >= CACHE_MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(key, n);
    }
    n
}

// ── Interface-level estimation (the compaction/session path) ──

/// Estimate input tokens for a slice of [`ModelMessage`]s — the protocol-agnostic
/// message type that `session` and `compaction` operate on.
///
/// This is the **single** estimator for the context-budget path. It previously
/// had three independent implementations that disagreed with each other and with
/// this module's real tokenizer:
/// `session::SessionManager::token_count` (`len() / 4`, flat 50 for every
/// non-text block), `compaction::grouping::estimate_tokens` (`len() / 4` plus
/// per-block overheads), and `compaction::compact::analyze_context`. All three
/// now delegate here, so the number that trips compaction, the number that
/// decides which rounds to drop, and the number reported in telemetry are the
/// same number.
///
/// Counts message *content* only. Per-message API framing (role, JSON
/// punctuation) is a handful of tokens per message and is deliberately not
/// modelled — same omission as [`estimate_input_tokens`].
pub fn estimate_message_tokens(messages: &[ModelMessage]) -> usize {
    let bpe = bpe();
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| estimate_model_block_tokens_with(b, bpe))
        .sum()
}

/// Estimate input tokens for a single [`ModelContentBlock`].
pub fn estimate_model_block_tokens(block: &ModelContentBlock) -> usize {
    estimate_model_block_tokens_with(block, bpe())
}

/// The engine's default [`TokenCounter`]: everything above, behind the
/// contract, so a host with a better answer can supply one.
///
/// Stateless — the BPE table and the memo cache are process-wide already —
/// so constructing one costs nothing and any number of them agree.
#[derive(Debug, Default, Clone, Copy)]
pub struct TiktokenCounter;

impl base::interface::token_counter::TokenCounter for TiktokenCounter {
    fn count_text(&self, text: &str) -> usize {
        count_text(bpe(), text)
    }

    fn count_block(&self, block: &ModelContentBlock) -> usize {
        estimate_model_block_tokens_with(block, bpe())
    }

    fn count_messages(&self, messages: &[ModelMessage]) -> usize {
        estimate_message_tokens(messages)
    }
}

/// The counter used wherever nothing more specific was supplied.
pub fn default_counter() -> std::sync::Arc<dyn base::interface::token_counter::TokenCounter> {
    static C: OnceLock<std::sync::Arc<dyn base::interface::token_counter::TokenCounter>> =
        OnceLock::new();
    C.get_or_init(|| std::sync::Arc::new(TiktokenCounter))
        .clone()
}

fn estimate_model_block_tokens_with(block: &ModelContentBlock, bpe: &CoreBPE) -> usize {
    match block {
        ModelContentBlock::Text { text } => count_text(bpe, text),
        // Flat estimate. Encoding the base64 payload would read a 1 MB
        // screenshot as ~250_000 tokens and trip compaction on the first image
        // the user pastes — see `IMAGE_TOKEN_ESTIMATE`'s doc comment.
        ModelContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
        // Thinking is billed as text. The signature is an opaque blob that the
        // API requires us to echo back; it is not counted here, matching the
        // estimator this replaced.
        ModelContentBlock::Thinking { text, .. } => count_text(bpe, text),
        ModelContentBlock::RedactedThinking { data, .. } => count_text(bpe, data),
        ModelContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => count_text(bpe, tool_use_id) + count_text(bpe, content),
        ModelContentBlock::ToolUse { id, name, input } => {
            count_text(bpe, id) + count_text(bpe, name) + count_text(bpe, &input.to_string())
        }
    }
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
    let bpe = bpe();
    let mut total = 0usize;

    // system
    for block in &req.system {
        let SystemBlock::Text { text, .. } = block;
        total += bpe.encode_with_special_tokens(text).len();
    }

    // tools
    for tool in &req.tools {
        total += bpe.encode_with_special_tokens(&tool.description).len();
        let schema_str = tool.input_schema.to_string();
        total += bpe.encode_with_special_tokens(&schema_str).len();
        total += bpe.encode_with_special_tokens(&tool.name).len();
    }

    // messages
    for msg in &req.messages {
        for block in &msg.content {
            total += estimate_block_tokens(block, bpe);
        }
    }

    total
}

fn estimate_block_tokens(block: &ContentBlock, bpe: &CoreBPE) -> usize {
    match block {
        ContentBlock::Text { text, .. } => count_text(bpe, text),
        ContentBlock::ToolUse { name, input, id } => {
            count_text(bpe, name) + count_text(bpe, id) + count_text(bpe, &input.to_string())
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            let id_tokens = count_text(bpe, tool_use_id);
            let body_tokens = match content {
                ToolResultContent::Text(t) => count_text(bpe, t),
                ToolResultContent::Blocks(blocks) => {
                    blocks.iter().map(|b| estimate_block_tokens(b, bpe)).sum()
                }
            };
            id_tokens + body_tokens
        }
        ContentBlock::Thinking { thinking, .. } => count_text(bpe, thinking),
        ContentBlock::RedactedThinking { data, .. } => count_text(bpe, data),
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
        // "hello world" 是 2 个 token (按 cl100k)
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

    // ── Interface-level estimator (`estimate_message_tokens`) ──

    use base::interface::model::MessageRole;

    fn model_user(blocks: Vec<ModelContentBlock>) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: blocks,
        }
    }

    #[test]
    fn message_tokens_are_real_tokens_not_char_quarters() {
        // "len() / 4" would call this 1 token; the tokenizer sees 4+.
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
    fn memoized_and_unmemoized_paths_agree() {
        // Same content, once below the cache threshold and once above it — the
        // memoized path must not change the answer.
        let unit = "the quick brown fox jumps over the lazy dog. ";
        let short = unit.to_string();
        let long = unit.repeat(64); // > CACHE_MIN_LEN
        assert!(short.len() < CACHE_MIN_LEN && long.len() > CACHE_MIN_LEN);

        let direct = bpe().encode_with_special_tokens(&long).len();
        let first = estimate_message_tokens(&[model_user(vec![ModelContentBlock::Text {
            text: long.clone(),
        }])]);
        let second =
            estimate_message_tokens(&[model_user(vec![ModelContentBlock::Text { text: long }])]);
        assert_eq!(first, direct);
        assert_eq!(second, direct, "cache hit must return the same count");
        let _ = short;
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
        let counter = TiktokenCounter;
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
        assert_eq!(
            counter.count_text("hello there"),
            count_text(bpe(), "hello there")
        );
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
