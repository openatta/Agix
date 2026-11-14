//! Time-based micro-compact configuration.
//!
//! Instead of only compacting when the token budget is exceeded, TimeBasedMC
//! clears old tool results based on wall-clock age. This is useful for
//! long-running sessions where old tool outputs become stale.
//!
//! Default: clear tool results older than 15 minutes, unless the session
//! has fewer than 100 messages (skip to preserve context in short sessions).

use base::interface::model::{ModelContentBlock, ModelMessage};
use std::time::{Duration, Instant};

/// Configuration for time-based micro-compaction.
#[derive(Debug, Clone)]
pub struct TimeBasedMcConfig {
    /// Clear tool results older than this duration. None = disabled.
    pub max_age: Option<Duration>,
    /// Only run time-based MC when the session has at least this many messages.
    /// Prevents clearing context in very short sessions.
    pub min_messages: usize,
}

impl Default for TimeBasedMcConfig {
    fn default() -> Self {
        Self {
            max_age: Some(Duration::from_secs(15 * 60)), // 15 minutes
            min_messages: 100,
        }
    }
}

impl TimeBasedMcConfig {
    /// Disable time-based MC entirely.
    pub fn disabled() -> Self {
        Self {
            max_age: None,
            min_messages: usize::MAX,
        }
    }
}

/// Result of a time-based micro-compaction pass.
#[derive(Debug, Clone, Default)]
pub struct TimeBasedMcResult {
    /// Number of tool results that were cleared.
    pub cleared: usize,
    /// Number of messages skipped (not old enough or not compactable).
    pub skipped: usize,
}

/// Apply time-based micro-compaction: replace old tool results with a placeholder.
/// Only clears results from COMPACTABLE_TOOLS.
pub fn apply_time_based_mc(
    messages: &mut [ModelMessage],
    config: &TimeBasedMcConfig,
    message_ages: &[(usize, Instant)], // (message_index, created_at)
) -> TimeBasedMcResult {
    let Some(max_age) = config.max_age else {
        return TimeBasedMcResult::default();
    };
    if messages.len() < config.min_messages {
        return TimeBasedMcResult::default();
    }

    let compactable: &[&str] = &[
        "Read",
        "Bash",
        "Grep",
        "Glob",
        "WebSearch",
        "WebFetch",
        "Edit",
        "Write",
    ];

    let mut result = TimeBasedMcResult::default();
    let ages: std::collections::HashMap<usize, Instant> = message_ages.iter().cloned().collect();

    // Track the last tool use name to determine compactability
    let mut last_tool_name: Option<String> = None;

    for (idx, msg) in messages.iter_mut().enumerate() {
        // Track tool use names
        for block in &msg.content {
            if let ModelContentBlock::ToolUse { name, .. } = block {
                last_tool_name = Some(name.clone());
            }
        }

        // Check if this message is old enough
        let age = ages
            .get(&idx)
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO);
        if age < max_age {
            result.skipped += 1;
            continue;
        }

        // Clear eligible tool results
        for block in &mut msg.content {
            if let ModelContentBlock::ToolResult { content, .. } = block {
                let is_compactable = last_tool_name
                    .as_deref()
                    .map(|n| compactable.contains(&n))
                    .unwrap_or(false);
                if is_compactable
                    && content != "[Old tool result content cleared]"
                    && content != "[Old tool result content cleared by time-based MC]"
                {
                    *content = "[Old tool result content cleared by time-based MC]".to_string();
                    result.cleared += 1;
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::MessageRole;

    fn make_tool_result(content: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: content.to_string(),
                is_error: Some(false),
            }],
        }
    }

    fn make_tool_use(name: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: "t1".into(),
                name: name.to_string(),
                input: serde_json::json!({}),
            }],
        }
    }

    #[test]
    fn disabled_config_skips_all() {
        let config = TimeBasedMcConfig::disabled();
        let mut messages = vec![make_tool_use("Read"), make_tool_result("old data")];
        let ages = vec![
            (0, Instant::now()),
            (1, Instant::now() - Duration::from_secs(3600)),
        ];
        let result = apply_time_based_mc(&mut messages, &config, &ages);
        assert_eq!(result.cleared, 0);
    }

    #[test]
    fn clears_old_compactable_results() {
        let config = TimeBasedMcConfig {
            min_messages: 0, // Override for test
            ..Default::default()
        };
        let now = Instant::now();
        let mut messages = vec![
            make_tool_use("Read"),
            make_tool_result("old read result"),
            make_tool_use("Bash"),
            make_tool_result("old bash result"),
        ];
        let ages = vec![
            (0, now - Duration::from_secs(1200)), // old
            (1, now - Duration::from_secs(1200)), // old
            (2, now - Duration::from_secs(1200)), // old
            (3, now - Duration::from_secs(1200)), // old
        ];
        let result = apply_time_based_mc(&mut messages, &config, &ages);
        assert_eq!(result.cleared, 2);
    }

    #[test]
    fn skips_fresh_messages() {
        let config = TimeBasedMcConfig {
            min_messages: 0,
            ..Default::default()
        };
        let mut messages = vec![make_tool_use("Read"), make_tool_result("fresh result")];
        let ages = vec![(0, Instant::now()), (1, Instant::now())];
        let result = apply_time_based_mc(&mut messages, &config, &ages);
        assert_eq!(result.cleared, 0);
        assert_eq!(result.skipped, 2);
    }
}
