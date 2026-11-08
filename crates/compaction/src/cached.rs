//! Time-driven cached micro-compact — clears old tool results without LLM calls.
//!
//! Rather than waiting for token budget to trigger compaction, this proactively
//! clears old tool result content on a time interval. This keeps the context
//! window manageable between full compactions without burning API calls.

use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use std::time::{Duration, Instant};

/// Configuration for time-based micro-compact.
#[derive(Debug, Clone)]
pub struct CachedMcConfig {
    /// Enable time-based micro-compact.
    pub enabled: bool,
    /// Interval between micro-compact passes.
    pub interval: Duration,
    /// Keep the N most recent tool results intact.
    pub keep_recent: usize,
}

impl Default for CachedMcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(3600), // every 60 minutes (matches the server-side cache TTL)
            keep_recent: 20,
        }
    }
}

/// Tracks time-based micro-compact state and pending cache edits.
#[derive(Debug)]
pub struct CachedMicroCompact {
    config: CachedMcConfig,
    last_run: Instant,
    run_count: u64,
    /// Tool_use_ids of results cleared in the most recent `run()` pass,
    /// pending consumption by the next API request.
    pending_edits: Vec<String>,
}

impl CachedMicroCompact {
    pub fn new(config: CachedMcConfig) -> Self {
        Self {
            config,
            last_run: Instant::now(),
            run_count: 0,
            pending_edits: Vec::new(),
        }
    }

    pub fn disabled() -> Self {
        Self::new(CachedMcConfig {
            enabled: false,
            ..Default::default()
        })
    }

    /// Check if it's time to run another micro-compact pass.
    pub fn should_run(&self) -> bool {
        self.config.enabled && self.last_run.elapsed() >= self.config.interval
    }

    /// Run a micro-compact pass: clear old tool results, keeping the N most recent.
    /// Records the tool_use_ids of cleared results in `pending_edits` so they
    /// can be sent as `cache_edits` in the next API request.
    ///
    /// Returns how many results were cleared.
    pub fn run(&mut self, messages: &mut [ModelMessage]) -> usize {
        if !self.config.enabled {
            return 0;
        }
        let (cleared, ids) = clear_stale_tool_results(messages, self.config.keep_recent);
        self.record_pass(ids);
        cleared
    }

    /// Record that a pass just happened, whoever ran it: the interval restarts
    /// and `ids` becomes what the next request sends as `cache_edits`.
    pub fn record_pass(&mut self, ids: Vec<String>) {
        self.pending_edits = ids;
        self.last_run = Instant::now();
        self.run_count += 1;
    }

    pub fn keep_recent(&self) -> usize {
        self.config.keep_recent
    }

    /// Consume and clear the pending cache edits (tool_use_ids).
    /// Returns the list of IDs that should be deleted via cache_edits in the next API call.
    pub fn consume_pending_edits(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_edits)
    }

    /// Alias: consume pending edits after they've been sent to the API.
    pub fn mark_tools_sent_to_api(&mut self) -> Vec<String> {
        self.consume_pending_edits()
    }

    /// Get a clone of the current pending edits without consuming them.
    /// Used for the edit-pinning pattern (re-inject edits across model calls).
    pub fn get_pinned_cache_edits(&self) -> Vec<String> {
        self.pending_edits.clone()
    }

    /// Track tool_use IDs for cache_edits. Records which tool results were
    /// generated for each tool_use, so the Anthropic API can delete them from
    /// the server-side cached prefix without busting the cache.
    ///
    /// Returns a list of tool_use_ids whose results should be deleted via cache_edits.
    pub fn build_cache_edits(&self, messages: &[ModelMessage], keep_recent: usize) -> Vec<String> {
        let mut tool_use_ids: Vec<String> = Vec::new();
        let mut result_count = 0usize;

        // Scan from newest to oldest, collecting tool_use IDs with results
        for msg in messages.iter().rev() {
            if msg.role == MessageRole::User {
                for block in &msg.content {
                    if let ModelContentBlock::ToolResult { tool_use_id, .. } = block {
                        result_count += 1;
                        if result_count > keep_recent
                            && !tool_use_id.is_empty()
                            && !tool_use_ids.contains(tool_use_id)
                        {
                            tool_use_ids.push(tool_use_id.clone());
                        }
                    }
                }
            }
        }
        tool_use_ids
    }
}

impl CachedMicroCompact {
    /// Helper: build a `cache_edit` specification suitable for the Anthropic API.
    /// The caller appends this to the next API request to delete cached tool result
    /// content without invalidating the global cache prefix.
    pub fn format_cache_edit(tool_use_ids: &[String]) -> Option<serde_json::Value> {
        if tool_use_ids.is_empty() {
            return None;
        }
        Some(serde_json::json!({
            "type": "cache_edits",
            "cache_edits": tool_use_ids.iter().map(|id| {
                serde_json::json!({
                    "type": "delete_tool_result",
                    "tool_use_id": id
                })
            }).collect::<Vec<_>>()
        }))
    }
}

/// Blank every tool result but the `keep_recent` newest, and report the
/// `tool_use_id`s that were blanked.
///
/// The pass itself, with no state: the interval that decides *when* it runs
/// and the buffer holding the ids until the next request are the caller's.
pub fn clear_stale_tool_results(
    messages: &mut [ModelMessage],
    keep_recent: usize,
) -> (usize, Vec<String>) {
    let mut result_positions: Vec<(usize, usize)> = Vec::new();
    let mut cleared_ids: Vec<String> = Vec::new();

    for (i, msg) in messages.iter().enumerate().rev() {
        if msg.role == MessageRole::User {
            for (j, block) in msg.content.iter().enumerate() {
                if matches!(block, ModelContentBlock::ToolResult { .. }) {
                    result_positions.push((i, j));
                }
            }
        }
    }

    let to_keep = keep_recent.min(result_positions.len());
    let to_clear = result_positions.len().saturating_sub(to_keep);

    for &(msg_idx, block_idx) in &result_positions[to_keep..] {
        if let Some(msg) = messages.get_mut(msg_idx) {
            if let Some(block) = msg.content.get_mut(block_idx) {
                if let ModelContentBlock::ToolResult { tool_use_id, .. } = &block {
                    if !tool_use_id.is_empty() {
                        cleared_ids.push(tool_use_id.clone());
                    }
                }
                *block = ModelContentBlock::ToolResult {
                    tool_use_id: String::new(),
                    content: "[Old tool result content cleared]".to_string(),
                    is_error: Some(false),
                };
            }
        }
    }

    (to_clear, cleared_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_does_nothing() {
        let mut mc = CachedMicroCompact::disabled();
        let mut msgs = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "hello".into(),
                is_error: Some(false),
            }],
        }];
        let cleared = mc.run(&mut msgs);
        assert_eq!(cleared, 0);
        // Content unchanged
        assert_eq!(
            msgs[0].content[0],
            ModelContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "hello".into(),
                is_error: Some(false),
            }
        );
    }

    #[test]
    fn clears_old_results_beyond_keep_recent() {
        let mut mc = CachedMicroCompact::new(CachedMcConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            keep_recent: 1,
        });
        let mut msgs = vec![
            ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "old result".into(),
                    is_error: Some(false),
                }],
            },
            ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "recent result".into(),
                    is_error: Some(false),
                }],
            },
        ];
        let cleared = mc.run(&mut msgs);
        assert_eq!(cleared, 1);
        // First (old) result should be cleared
        assert_eq!(
            msgs[0].content[0],
            ModelContentBlock::ToolResult {
                tool_use_id: String::new(),
                content: "[Old tool result content cleared]".into(),
                is_error: Some(false),
            }
        );
        // Second (recent) result should be intact
        assert_eq!(
            msgs[1].content[0],
            ModelContentBlock::ToolResult {
                tool_use_id: "t2".into(),
                content: "recent result".into(),
                is_error: Some(false),
            }
        );
    }
}
