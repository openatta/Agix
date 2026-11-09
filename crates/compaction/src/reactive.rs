//! Reactive compaction — proactive context compression before the budget is exhausted.
//!
//! Instead of waiting until the token budget is critically low, reactive
//! compaction predicts when compaction will be needed and fires early
//! (a MicroCompact) to avoid stalling the user with a last-minute big
//! compaction. A circuit breaker stops it after
//! [`MAX_CONSECUTIVE_FAILURES`] passes in a row that bought nothing.

use crate::grouping::estimate_tokens;
use base::interface::model::ModelMessage;

/// Compact once fewer than this many tokens are left below the limit.
const TRIGGER_AT_REMAINING_TOKENS: usize = 50_000;
/// Maximum consecutive compaction failures before opening the circuit breaker.
const MAX_CONSECUTIVE_FAILURES: usize = 3;

/// State tracking for compaction across turns.
#[derive(Debug, Clone, Default)]
pub struct CompactionState {
    /// Number of consecutive compaction failures.
    pub consecutive_failures: usize,
    /// Whether the circuit breaker is open (skip further compaction attempts).
    pub circuit_open: bool,
}

impl CompactionState {
    /// Record a successful compaction. Resets the failure counter and closes the circuit.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.circuit_open = false;
    }

    /// Record a failed compaction. Increments the failure counter; opens the circuit
    /// after MAX_CONSECUTIVE_FAILURES consecutive failures.
    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            self.circuit_open = true;
        }
    }
}

/// Whether to compact before the next API call: fewer than
/// [`TRIGGER_AT_REMAINING_TOKENS`] are left below `context_limit`, or at
/// `token_usage_velocity` (tokens per call) the rest runs out within three
/// calls.
pub fn should_compact_reactively(
    current_tokens: usize,
    context_limit: usize,
    token_usage_velocity: Option<f64>,
) -> bool {
    let remaining = context_limit.saturating_sub(current_tokens);
    if remaining <= TRIGGER_AT_REMAINING_TOKENS {
        return true;
    }
    match token_usage_velocity {
        Some(velocity) if velocity > 0.0 => remaining as f64 / velocity <= 3.0,
        _ => false,
    }
}

/// Pre-compute the token usage velocity from messages.
/// Rough estimate: rate of token growth per API round.
pub fn estimate_token_velocity(messages: &[ModelMessage]) -> Option<f64> {
    if messages.len() < 4 {
        return None;
    }
    let total = estimate_tokens(messages);
    let per_message = total as f64 / messages.len() as f64;
    Some(per_message * 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_when_remaining_below_threshold() {
        assert!(should_compact_reactively(180_000, 200_000, None));
        assert!(!should_compact_reactively(100_000, 200_000, None));
    }

    #[test]
    fn triggers_early_with_high_velocity() {
        assert!(should_compact_reactively(100_000, 200_000, Some(50_000.0)));
    }

    #[test]
    fn does_not_trigger_with_low_velocity() {
        assert!(!should_compact_reactively(100_000, 200_000, Some(10_000.0)));
    }

    #[test]
    fn circuit_breaker_opens_after_3_failures() {
        let mut state = CompactionState::default();
        assert!(!state.circuit_open);
        state.record_failure();
        state.record_failure();
        assert!(!state.circuit_open);
        state.record_failure();
        assert!(state.circuit_open);
    }

    #[test]
    fn success_resets_circuit() {
        let mut state = CompactionState::default();
        state.record_failure();
        state.record_failure();
        state.record_success();
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.circuit_open);
    }
}
