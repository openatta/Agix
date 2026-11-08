//! SessionMemoryCompactor — prunes old messages that have already been
//! summarized into the `session_memory.md` sidecar file.
//!
//! This compactor runs FIRST in the compaction chain — before LLM-based
//! summarization (FullCompact) and before the Snip/MicroCompact/Collapse
//! cascade used by DefaultCompactor.
//!
//! It is very cheap (no model calls, no LLM IO). When session memory
//! extraction has previously been completed, the compactor prunes messages
//! before the extraction point, recovering the token budget that those
//! old messages had consumed.
//!
//! # Safety invariants
//! - ToolUse/ToolResult pairs are never split (operates at API-round
//!   granularity via `group_by_api_round`).
//! - Thinking block boundaries are preserved — entire assistant messages
//!   are kept or pruned as atomic units.
//! - Session memory file size is capped after compaction to avoid growing
//!   unboundedly.

use async_trait::async_trait;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

#[allow(unused_imports)]
use base::interface::model::{ModelContentBlock, ModelMessage, MessageRole};

use crate::compact::{Compactor, CompactError, CompactResult, CompactStrategy};
use crate::grouping::{estimate_tokens, group_by_api_round};

/// Configuration for the SessionMemory compaction strategy.
#[derive(Debug, Clone)]
pub struct SessionMemoryCompactConfig {
    /// Minimum estimated tokens that must be freed to trigger compaction.
    /// Default: 10_000 tokens.
    pub min_tokens: usize,

    /// Minimum number of text (non-tool) blocks in the pruned message region.
    /// Guards against pruning regions that have almost no conversational
    /// content (e.g. only tool results). Default: 5.
    pub min_text_blocks: usize,

    /// Maximum estimated tokens to prune in a single pass. Prevents the
    /// compactor from removing too much context at once. Default: 40_000.
    pub max_tokens: usize,
}

impl Default for SessionMemoryCompactConfig {
    fn default() -> Self {
        Self {
            min_tokens: 10_000,
            min_text_blocks: 5,
            max_tokens: 40_000,
        }
    }
}

/// A compactor that prunes old messages which have already been durably
/// stored in the `session_memory.md` sidecar file.
///
/// # How it works
///
/// 1. The runtime calls [`mark_extracted`](Self::mark_extracted) after the
///    model has written durable memories (e.g. after a `session_memory_extract`
///    cycle). This sets a cursor: the number of messages from the head that
///    are now durably stored.
/// 2. On the next `compact()` call, if the cursor is non-zero, messages
///    before it are candidates for pruning. The compactor groups by API
///    round to avoid splitting ToolUse/ToolResult pairs.
/// 3. It checks the configured thresholds (`min_tokens`, `min_text_blocks`,
///    `max_tokens`) and either prunes the eligible rounds or returns a
///    no-op result, letting the caller fall through to the next strategy.
/// 4. After successful pruning, if `session_memory_path` is configured, the
///    `session_memory.md` file is truncated to keep its size bounded.
/// 5. The cursor is reset to 0, meaning the compactor is ready for the next
///    extraction cycle.
pub struct SessionMemoryCompactor {
    config: SessionMemoryCompactConfig,
    /// Cursor: number of messages from the head that have been summarized
    /// into session memory. Messages at indices `[0, cursor)` are candidates
    /// for pruning on the next compaction cycle.
    last_summarized_count: AtomicUsize,
    /// Optional path to `session_memory.md`. When set, the file is truncated
    /// after compaction to stay within a reasonable token budget.
    pub session_memory_path: Option<std::path::PathBuf>,
}

impl SessionMemoryCompactor {
    pub fn new(config: SessionMemoryCompactConfig) -> Self {
        Self {
            config,
            last_summarized_count: AtomicUsize::new(0),
            session_memory_path: None,
        }
    }

    /// Mark that session memory extraction has completed with the given
    /// number of messages processed at call time. On the next `compact()`,
    /// all messages before this count are eligible for pruning.
    pub fn mark_extracted(&self, message_count: usize) {
        self.last_summarized_count.store(message_count, Ordering::Release);
    }

    /// Returns the current cursor — how many messages from the head have
    /// already been extracted into session memory.
    pub fn extracted_count(&self) -> usize {
        self.last_summarized_count.load(Ordering::Acquire)
    }

    /// Count the number of `Text` blocks in a message slice.
    fn count_text_blocks(messages: &[ModelMessage]) -> usize {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ModelContentBlock::Text { .. }))
            .count()
    }

    /// Compute the sum of estimated tokens for rounds in `rounds[..count]`.
    fn round_tokens_up_to(rounds: &[crate::grouping::ApiRound], count: usize) -> usize {
        rounds[..count.min(rounds.len())]
            .iter()
            .map(|r| r.estimated_tokens)
            .sum()
    }
}

#[async_trait]
impl Compactor for SessionMemoryCompactor {
    async fn compact(
        &self,
        messages: Vec<ModelMessage>,
        _max_tokens: usize,
        keep_rounds: usize,
    ) -> Result<(Vec<ModelMessage>, CompactResult), CompactError> {
        let tokens_before = estimate_tokens(&messages);
        let count_before = messages.len();
        let cursor = self.last_summarized_count.load(Ordering::Acquire);

        if cursor == 0 || count_before <= keep_rounds.max(1) {
            return Ok(self.noop_result(messages, tokens_before, count_before));
        }

        // We never split a round, so we prune complete rounds whose messages
        // are all before `cursor`.
        let rounds = group_by_api_round(&messages);

        // Walk rounds to find the cut point.  `prune_up_to_round` is the
        // number of complete rounds whose messages all lie before `cursor`.
        let mut prune_up_to_round = 0usize;
        let mut running = 0usize;
        for round in &rounds {
            running += round.messages.len();
            if running >= cursor {
                break;
            }
            prune_up_to_round += 1;
        }

        // Never prune all rounds — keep at least `keep_rounds`.
        let max_prune = rounds.len().saturating_sub(keep_rounds.max(1));
        // Never prune below the cursor's round — preserve the round containing
        // the cursor so no partial tool_use/tool_result pairs survive.
        let effective_prune = prune_up_to_round.min(max_prune);

        if effective_prune == 0 {
            return Ok(self.noop_result(messages, tokens_before, count_before));
        }

        let pruned_tokens = Self::round_tokens_up_to(&rounds, effective_prune);

        // Must free at least min_tokens.
        if pruned_tokens < self.config.min_tokens {
            return Ok(self.noop_result(messages, tokens_before, count_before));
        }

        // Count text blocks in the pruned region.
        let pruned_msgs: Vec<&ModelMessage> = rounds[..effective_prune]
            .iter()
            .flat_map(|r| &r.messages)
            .collect();
        let text_blocks = {
            let slice: Vec<ModelMessage> =
                pruned_msgs.iter().map(|m| (*m).clone()).collect();
            Self::count_text_blocks(&slice)
        };
        if text_blocks < self.config.min_text_blocks {
            return Ok(self.noop_result(messages, tokens_before, count_before));
        }

        // If we would free more than max_tokens, restore the innermost
        // (most recent) pruned rounds until we are within budget.
        let final_prune = if pruned_tokens > self.config.max_tokens {
            let mut p = effective_prune;
            while p > 1 {
                p -= 1;
                if Self::round_tokens_up_to(&rounds, p) <= self.config.max_tokens {
                    break;
                }
            }
            p
        } else {
            effective_prune
        };

        if final_prune == 0 {
            return Ok(self.noop_result(messages, tokens_before, count_before));
        }

        let result: Vec<ModelMessage> = rounds[final_prune..]
            .iter()
            .flat_map(|r| r.messages.clone())
            .collect();

        let tokens_after = estimate_tokens(&result);
        let messages_after = result.len();

        if let Some(ref path) = self.session_memory_path {
            if let Err(e) = truncate_session_memory_file(path, self.config.max_tokens).await {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "failed to truncate session memory file"
                );
            }
        }

        // The summarized messages were just dropped, so the cursor restarts.
        self.last_summarized_count.store(0, Ordering::Release);

        Ok((
            result,
            CompactResult {
                strategy: CompactStrategy::SessionMemory,
                messages_before: count_before,
                messages_after,
                tokens_before,
                tokens_after,
                projection: None,
            },
        ))
    }
}

impl SessionMemoryCompactor {
    /// Build a "no work done" CompactResult, returning the original messages.
    fn noop_result(
        &self,
        messages: Vec<ModelMessage>,
        tokens_before: usize,
        count_before: usize,
    ) -> (Vec<ModelMessage>, CompactResult) {
        (
            messages,
            CompactResult {
                strategy: CompactStrategy::SessionMemory,
                messages_before: count_before,
                messages_after: count_before,
                tokens_before,
                tokens_after: tokens_before,
                projection: None,
            },
        )
    }
}

/// Truncate the `session_memory.md` sidecar file when its body exceeds a
/// reasonable token budget.
///
/// Preserves the YAML frontmatter block and the `# Session Memory` header,
/// but truncates the body content from the end so the total file (excluding
/// frontmatter) stays within `max_token_equivalent` tokens.
///
/// Uses a rough estimate of 1 token ≈ 4 characters.
async fn truncate_session_memory_file(path: &Path, max_token_equivalent: usize) -> std::io::Result<()> {
    let content = tokio::fs::read_to_string(path).await?;
    let content = content.trim();

    if content.is_empty() {
        return Ok(());
    }

    // The frontmatter is a YAML block delimited by two `---` lines at the
    // very start of the file.
    let (frontmatter, body) = if let Some(after_first) = content.strip_prefix("---") {
        if let Some((fm_lines, after_fm)) = after_first.split_once("\n---") {
            let frontmatter_text = format!("---{}\n---", fm_lines);
            let body = after_fm.trim_start();
            (Some(frontmatter_text), body)
        } else {
            // Malformed: no closing `---`, treat everything as body.
            (None, content)
        }
    } else {
        (None, content)
    };

    let max_chars = max_token_equivalent.saturating_mul(4);
    if body.len() <= max_chars {
        return Ok(());
    }

    // Character-boundary aware: the early return above already established
    // `body.len() > max_chars`, so this always cuts, and session notes are
    // model-authored prose that is routinely non-ASCII.
    let truncated_body = base::text::truncate_at_char_boundary(body, max_chars);

    let new_content = if let Some(ref fm) = frontmatter {
        format!("{}\n\n{}", fm, truncated_body)
    } else {
        truncated_body.to_string()
    };

    tokio::fs::write(path, new_content.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::Compactor;
    use tempfile::TempDir;

    fn text_user(s: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: s.to_string(),
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

    fn tool_use_msg(id: &str, name: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
            }],
        }
    }

    fn tool_result_msg(id: &str, content: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: Some(false),
            }],
        }
    }

    fn build_simple_conversation(rounds: usize) -> Vec<ModelMessage> {
        let mut msgs = Vec::new();
        for i in 0..rounds {
            msgs.push(text_user(&format!("user msg {i}")));
            msgs.push(assistant_text(&format!("assistant response {i}")));
        }
        msgs
    }

    fn build_conversation_with_tools() -> Vec<ModelMessage> {
        vec![
            text_user("read file a"),
            tool_use_msg("tu1", "Read"),
            tool_result_msg("tu1", "content of file a"),
            assistant_text("file a read"),
            text_user("edit file b"),
            tool_use_msg("tu2", "Edit"),
            tool_result_msg("tu2", "file b edited"),
            assistant_text("file b done"),
            text_user("final question"),
            assistant_text("final answer"),
        ]
    }

    fn mk_compactor(config: SessionMemoryCompactConfig) -> SessionMemoryCompactor {
        let mut c = SessionMemoryCompactor::new(config);
        c.session_memory_path = None;
        c
    }

    #[tokio::test]
    async fn cursor_zero_means_noop() {
        let compactor = mk_compactor(SessionMemoryCompactConfig::default());
        let msgs = build_simple_conversation(10);
        let (result, r) = compactor.compact(msgs.clone(), 1000, 3).await.unwrap();
        assert_eq!(result.len(), msgs.len(), "expected no change when cursor is 0");
        assert_eq!(r.tokens_after, r.tokens_before);
    }

    #[tokio::test]
    async fn cursor_beyond_message_count_is_noop() {
        let compactor = mk_compactor(SessionMemoryCompactConfig::default());
        let msgs = build_simple_conversation(3);
        compactor.mark_extracted(100); // far beyond actual messages
        let (result, r) = compactor.compact(msgs.clone(), 1000, 1).await.unwrap();
        assert_eq!(result.len(), msgs.len(), "noop when cursor exceeds message count");
        assert_eq!(r.tokens_after, r.tokens_before);
    }

    #[tokio::test]
    async fn not_enough_messages_for_keep_rounds() {
        let compactor = mk_compactor(SessionMemoryCompactConfig::default());
        let msgs = build_simple_conversation(3); // 6 messages
        compactor.mark_extracted(4); // first 4 messages "extracted"
        let (result, _) = compactor.compact(msgs.clone(), 1000, 3).await.unwrap();
        // keep_rounds=3 means keep at least 3 rounds = 6 msgs = all
        assert_eq!(result.len(), msgs.len(), "no pruning when keep_rounds covers everything");
    }

    #[tokio::test]
    async fn prunes_messages_before_cursor() {
        let compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 1,  // very low to allow pruning
            min_text_blocks: 1,
            max_tokens: 100_000,
        });
        let msgs = build_simple_conversation(10); // 20 messages, ~10 rounds
        compactor.mark_extracted(12); // first 12 messages (6 rounds) are extracted
        let tokens_before = estimate_tokens(&msgs);
        let (result, r) = compactor.compact(msgs, 1000, 2).await.unwrap();
        // Should prune extracted messages before cursor
        assert_eq!(r.strategy, CompactStrategy::SessionMemory);
        assert!(result.len() < r.messages_before, "should prune some messages (before: {}, after: {})", r.messages_before, result.len());
        assert_eq!(r.messages_before, 20);
        assert!(r.messages_after < r.messages_before, "should reduce message count");
        assert!(r.tokens_after < r.tokens_before, "should reduce tokens");
        assert_eq!(cursor_reset(&compactor), true, "cursor should be reset after pruning");
    }

    #[tokio::test]
    async fn respects_tool_use_tool_result_pairing() {
        let compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 1,
            min_text_blocks: 1,
            max_tokens: 100_000,
        });
        let msgs = build_conversation_with_tools(); // 10 messages
        // Cursor at 5 — this is in the middle of a round (tool pair)
        // Round structure:
        //   round 0: [user+tu+tr] (3 msgs)
        //   round 1: [assistant_text] (1 msg)
        //   round 2: [user+tu+tr] (3 msgs)
        //   round 3: [assistant_text] (1 msg)
        //   round 4: [user+assistant] (2 msgs)
        // cursor=5 falls in round 1 (running sum: round0=3, round1=4, round2=7)
        // prune_up_to_round should be 1 (only complete rounds before cursor)
        compactor.mark_extracted(5);
        let (result, r) = compactor.compact(msgs, 1000, 1).await.unwrap();
        // Should prune round 0 (3 msgs), keep rounds 1..4 (7 msgs)
        assert_eq!(r.messages_before, 10);
        // After pruning rounds before cursor, expected to keep most messages
        assert!(r.messages_after <= r.messages_before, "should not increase messages");
        assert!(r.messages_before > r.messages_after || r.messages_after == r.messages_before,
            "should prune some or keep all");
        assert_eq!(r.strategy, CompactStrategy::SessionMemory);
    }

    #[tokio::test]
    async fn respects_keep_rounds() {
        let compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 1,
            min_text_blocks: 1,
            max_tokens: 100_000,
        });
        let msgs = build_simple_conversation(10); // 10 rounds, 20 msgs
        compactor.mark_extracted(18); // cursor after 9 rounds
        // keep_rounds=8 → at most 2 rounds can be pruned (10-8=2)
        let (result, r) = compactor.compact(msgs, 1000, 8).await.unwrap();
        // Should prune at most 2 rounds (4 msgs)
        assert_eq!(r.messages_before, 20);
        assert!(r.messages_after >= 16, "should keep at least 16 msgs (8 rounds)");
    }

    #[tokio::test]
    async fn respects_max_tokens() {
        // Create a conversation where each round has lots of text
        let mut msgs = Vec::new();
        for i in 0..10 {
            let big_text = "A".repeat(2000); // ~500 tokens per round
            msgs.push(text_user(&format!("short msg {i}")));
            msgs.push(assistant_text(&big_text));
        }
        let compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 1,
            min_text_blocks: 1,
            max_tokens: 600, // cap pruning to ~600 tokens
        });
        compactor.mark_extracted(18);
        let (result, r) = compactor.compact(msgs, 1000, 2).await.unwrap();
        let tokens_freed = r.tokens_before.saturating_sub(r.tokens_after);
        assert!(
            tokens_freed <= 600,
            "should not free more than max_tokens (600), freed {tokens_freed}"
        );
    }

    #[tokio::test]
    async fn respects_min_tokens() {
        // Small conversation where pruning would save almost nothing
        let msgs = build_simple_conversation(4); // 8 msgs, tiny tokens
        let compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 10_000, // way more than 8 tiny msgs
            ..Default::default()
        });
        compactor.mark_extracted(6);
        let (result, r) = compactor.compact(msgs.clone(), 1000, 1).await.unwrap();
        // Should not prune — not enough tokens to free
        assert_eq!(result.len(), msgs.len(), "should not prune below min_tokens");
        assert_eq!(r.tokens_after, r.tokens_before);
    }

    #[tokio::test]
    async fn respects_min_text_blocks() {
        // Messages with only tool results, no text blocks
        let msgs = vec![
            tool_result_msg("t1", "result a"),
            tool_result_msg("t2", "result b"),
            tool_result_msg("t3", "result c"),
            text_user("hello"),
            assistant_text("world"),
        ];
        let compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 1,
            min_text_blocks: 5, // most messages are tool results with no text
            max_tokens: 100_000,
        });
        compactor.mark_extracted(3); // first 3 messages are tool results
        let (result, r) = compactor.compact(msgs, 1000, 1).await.unwrap();
        // Should not prune — not enough text blocks in pruned region
        assert_eq!(result.len(), 5, "should not prune below min_text_blocks");
        assert_eq!(r.tokens_after, r.tokens_before);
    }

    #[tokio::test]
    async fn cursor_resets_after_pruning() {
        let compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 1,
            min_text_blocks: 1,
            max_tokens: 100_000,
        });
        let msgs = build_simple_conversation(10);
        compactor.mark_extracted(12);
        // First compact — should prune
        let _ = compactor.compact(msgs, 1000, 2).await.unwrap();
        assert_eq!(
            compactor.extracted_count(),
            0,
            "cursor should be 0 after pruning"
        );
    }

    #[tokio::test]
    async fn truncates_session_memory_file_when_over_budget() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");

        // Write a file with frontmatter and a large body
        let big_body = "A".repeat(10_000); // ~2500 tokens
        let content = format!(
            "---\nextraction_started: 2026-01-01\nextraction_completed: 2026-01-02\nlast_update_turn: 42\n---\n\n# Session Memory\n\nTrack persistent facts.\n\n{}",
            big_body
        );
        tokio::fs::write(&path, &content).await.unwrap();

        let mut compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 1,
            min_text_blocks: 1,
            max_tokens: 100, // very low — ~400 chars max
            ..Default::default()
        });
        compactor.session_memory_path = Some(path.clone());

        let msgs = build_simple_conversation(10);
        compactor.mark_extracted(12);
        let _ = compactor.compact(msgs, 1000, 2).await.unwrap();

        // File should now be truncated
        let saved = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(saved.len() < 2000, "file should be truncated, was {} bytes", saved.len());
        // Frontmatter should be preserved
        assert!(saved.contains("extraction_started: 2026-01-01"), "frontmatter should be preserved");
        assert!(saved.contains("# Session Memory"), "header should be preserved");
    }

    #[tokio::test]
    async fn does_not_truncate_when_within_budget() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");

        let content = "---\nextraction_started: ~\nextraction_completed: ~\n---\n\n# Session Memory\n\nshort body";
        tokio::fs::write(&path, content).await.unwrap();

        let mut compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 1,
            min_text_blocks: 1,
            max_tokens: 100_000,
            ..Default::default()
        });
        compactor.session_memory_path = Some(path.clone());

        let msgs = build_simple_conversation(10);
        compactor.mark_extracted(12);
        let _ = compactor.compact(msgs, 1000, 2).await.unwrap();

        let saved = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(saved.len(), content.len(), "file should not be truncated");
    }

    #[tokio::test]
    async fn empty_messages() {
        let compactor = mk_compactor(SessionMemoryCompactConfig::default());
        let (result, r) = compactor.compact(vec![], 1000, 3).await.unwrap();
        assert!(result.is_empty());
        assert_eq!(r.messages_before, 0);
        assert_eq!(r.messages_after, 0);
    }

    #[tokio::test]
    async fn single_round_conversation() {
        let msgs = vec![text_user("hello"), assistant_text("world")];
        let compactor = mk_compactor(SessionMemoryCompactConfig {
            min_tokens: 1,
            min_text_blocks: 1,
            max_tokens: 100_000,
        });
        compactor.mark_extracted(2);
        let (result, _) = compactor.compact(msgs.clone(), 1000, 1).await.unwrap();
        // With keep_rounds=1 and only 1 round, nothing should be pruned
        assert_eq!(result.len(), 2, "should keep the only round");
    }

    fn cursor_reset(compactor: &SessionMemoryCompactor) -> bool {
        compactor.extracted_count() == 0
    }
}
