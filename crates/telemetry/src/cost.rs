//! CostTracker — usage-aware cost estimation wrapping UsageAccumulator.
//!
//! Provides pricing-aware methods (`estimated_cost_usd`, `format_cost`,
//! `cost_summary`) and a structured `CostSummary` breakdown.
//!
//! Pricing table mirrors the one in `stats.rs` (Anthropic public pricing, 2025-05).
//! Keep the two tables in sync.

use crate::stats::UsageAccumulator;
use serde::Serialize;

// Pricing table: (input $/1M, output $/1M, cache_write $/1M, cache_read $/1M).
// Mirrors the table in stats.rs; keep in sync if prices are updated.
const PRICING: &[(&str, f64, f64, f64, f64)] = &[
    // Claude 4 series
    ("claude-sonnet-4", 3.00, 15.00, 3.75, 0.30),
    ("claude-haiku-4", 1.00, 5.00, 1.25, 0.10),
    // Claude 3.5 series
    ("claude-sonnet-3.5", 3.00, 15.00, 3.75, 0.30),
    ("claude-haiku-3.5", 1.00, 5.00, 1.25, 0.10),
    // Claude 3 series
    ("claude-opus-3", 15.00, 75.00, 18.75, 1.50),
    ("claude-sonnet-3", 3.00, 15.00, 3.75, 0.30),
    ("claude-haiku-3", 0.80, 4.00, 1.00, 0.08),
];

/// Structured cost breakdown for a single model.
#[derive(Debug, Clone, Serialize)]
pub struct ModelCostSummary {
    pub model: String,
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub api_calls: u32,
}

/// Structured full cost summary for a session.
#[derive(Debug, Clone, Serialize)]
pub struct CostSummary {
    /// Total cost across all models in USD
    pub total_cost_usd: f64,
    /// Cost attributable to input tokens
    pub input_cost_usd: f64,
    /// Cost attributable to output tokens
    pub output_cost_usd: f64,
    /// Cost attributable to cache writes (creation)
    pub cache_write_cost_usd: f64,
    /// Cost attributable to cache reads
    pub cache_read_cost_usd: f64,
    /// Total input tokens across all models
    pub total_tokens_input: u64,
    /// Total output tokens across all models
    pub total_tokens_output: u64,
    /// Total API calls
    pub api_calls: u32,
    /// Per-model breakdown, sorted by cost descending
    pub by_model: Vec<ModelCostSummary>,
}

/// Pricing-aware wrapper around `UsageAccumulator`.
///
/// Provides convenience methods for cost estimation and human-readable
/// formatting of token usage costs. Construct via `CostTracker::new(acc)` or
/// `CostTracker::from(acc)`.
#[derive(Debug, Clone)]
pub struct CostTracker {
    usage: UsageAccumulator,
}

impl CostTracker {
    /// Wrap an existing `UsageAccumulator`.
    pub fn new(usage: UsageAccumulator) -> Self {
        Self { usage }
    }

    /// Borrow the underlying accumulator.
    pub fn usage(&self) -> &UsageAccumulator {
        &self.usage
    }

    /// Consume self and return the inner accumulator.
    pub fn into_inner(self) -> UsageAccumulator {
        self.usage
    }

    /// Total estimated cost in USD across all models.
    ///
    /// Delegates to `UsageAccumulator::estimated_cost_total()` which uses the
    /// same pricing table internally.
    pub fn estimated_cost_usd(&self) -> f64 {
        self.usage.estimated_cost_total()
    }

    /// Human-readable cost string, e.g. `"$0.0423"`.
    ///
    /// Uses four decimal places for values >= $0.0001 and six for smaller values.
    pub fn format_cost(&self) -> String {
        format_cost_value(self.estimated_cost_usd())
    }

    /// Structured breakdown of costs per model and in aggregate.
    ///
    /// Returns a `CostSummary` with the total cost, category breakdowns, and
    /// per-model details sorted by cost (most expensive first). Unknown models
    /// contribute $0 to each metric (no error).
    pub fn cost_summary(&self) -> CostSummary {
        let mut by_model: Vec<ModelCostSummary> = self
            .usage
            .by_model
            .values()
            .map(|mu| {
                let (inp, outp, cw, cr) = price_for(&mu.model);
                let cost_usd = (mu.input as f64 * inp
                    + mu.output as f64 * outp
                    + mu.cache_creation as f64 * cw
                    + mu.cache_read as f64 * cr)
                    / 1_000_000.0;
                ModelCostSummary {
                    model: mu.model.clone(),
                    cost_usd,
                    input_tokens: mu.input,
                    output_tokens: mu.output,
                    cache_write_tokens: mu.cache_creation,
                    cache_read_tokens: mu.cache_read,
                    api_calls: mu.api_calls,
                }
            })
            .collect();

        // Sort by cost descending
        by_model.sort_by(|a, b| {
            b.cost_usd
                .partial_cmp(&a.cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let total_cost_usd: f64 = by_model.iter().map(|m| m.cost_usd).sum();
        let (input_cost_usd, output_cost_usd, cache_write_cost_usd, cache_read_cost_usd) =
            self.compute_category_costs();

        CostSummary {
            total_cost_usd,
            input_cost_usd,
            output_cost_usd,
            cache_write_cost_usd,
            cache_read_cost_usd,
            total_tokens_input: self.usage.input_total,
            total_tokens_output: self.usage.output_total,
            api_calls: self.usage.api_calls,
            by_model,
        }
    }

    /// Compute cost broken down by category (input, output, cache write, cache read).
    fn compute_category_costs(&self) -> (f64, f64, f64, f64) {
        let mut input = 0.0f64;
        let mut output = 0.0f64;
        let mut cw = 0.0f64;
        let mut cr = 0.0f64;
        for mu in self.usage.by_model.values() {
            let (inp, outp, cwp, crp) = price_for(&mu.model);
            input += mu.input as f64 * inp / 1_000_000.0;
            output += mu.output as f64 * outp / 1_000_000.0;
            cw += mu.cache_creation as f64 * cwp / 1_000_000.0;
            cr += mu.cache_read as f64 * crp / 1_000_000.0;
        }
        (input, output, cw, cr)
    }
}

impl From<UsageAccumulator> for CostTracker {
    fn from(usage: UsageAccumulator) -> Self {
        Self::new(usage)
    }
}

/// Prefix-match pricing. E.g. `"claude-sonnet-4-20250514"` matches `"claude-sonnet-4"`.
fn price_for(model: &str) -> (f64, f64, f64, f64) {
    for (prefix, inp, outp, cw, cr) in PRICING {
        if model.starts_with(prefix) {
            return (*inp, *outp, *cw, *cr);
        }
    }
    (0.0, 0.0, 0.0, 0.0)
}

/// Format a cost value as a human-readable string.
///
/// - `>= 0.0001`: four decimal places (e.g. `"$0.0423"`, `"$1.5000"`)
/// - `< 0.0001`: six decimal places (e.g. `"$0.000012"`)
/// - `0.0`: `"$0.0000"`
fn format_cost_value(cost: f64) -> String {
    if cost == 0.0 {
        "$0.0000".to_string()
    } else if cost < 0.0001 {
        format!("${:.6}", cost)
    } else {
        format!("${:.4}", cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{UsageAccumulator, UsageDelta};

    fn delta(model: &str, input: u64, output: u64) -> UsageDelta {
        UsageDelta {
            input_tokens: input,
            output_tokens: output,
            cache_creation: 0,
            cache_read: 0,
            model: model.into(),
        }
    }

    fn delta_full(
        model: &str,
        input: u64,
        output: u64,
        cache_creation: u64,
        cache_read: u64,
    ) -> UsageDelta {
        UsageDelta {
            input_tokens: input,
            output_tokens: output,
            cache_creation,
            cache_read,
            model: model.into(),
        }
    }

    #[test]
    fn empty_tracker_zero_cost() {
        let tracker = CostTracker::new(UsageAccumulator::default());
        assert_eq!(tracker.estimated_cost_usd(), 0.0);
    }

    #[test]
    fn format_cost_zero() {
        assert_eq!(format_cost_value(0.0), "$0.0000");
    }

    #[test]
    fn format_cost_typical() {
        // User's example: "$0.0423"
        assert_eq!(format_cost_value(0.0423), "$0.0423");
    }

    #[test]
    fn format_cost_larger() {
        assert_eq!(format_cost_value(1.5), "$1.5000");
    }

    #[test]
    fn format_cost_very_small() {
        // Below $0.0001 threshold → six decimal places
        let s = format_cost_value(0.000012);
        assert_eq!(s, "$0.000012");
    }

    #[test]
    fn estimated_cost_sonnet() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("claude-sonnet-4-20250514", 1_000_000, 100_000));
        let tracker = CostTracker::new(u);
        // 1M input @ $3/M + 100K output @ $15/M = $3.00 + $1.50 = $4.50
        let cost = tracker.estimated_cost_usd();
        assert!((cost - 4.50).abs() < 0.001, "expected ~4.50, got {cost}");
    }

    #[test]
    fn estimated_cost_haiku() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("claude-haiku-4", 1_000_000, 100_000));
        let tracker = CostTracker::new(u);
        // 1M input @ $1/M + 100K output @ $5/M = $1.00 + $0.50 = $1.50
        let cost = tracker.estimated_cost_usd();
        assert!((cost - 1.50).abs() < 0.001, "expected ~1.50, got {cost}");
    }

    #[test]
    fn estimated_cost_unknown_model_is_zero() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("gpt-4", 1_000_000, 100_000));
        let tracker = CostTracker::new(u);
        assert_eq!(tracker.estimated_cost_usd(), 0.0);
    }

    #[test]
    fn format_cost_returns_formatted_string() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("claude-sonnet-4", 50_000, 10_000));
        let tracker = CostTracker::new(u);
        let s = tracker.format_cost();
        assert!(s.starts_with('$'));
        assert!(s.len() > 1);
    }

    #[test]
    fn cost_summary_aggregates_correctly() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("claude-haiku-4", 500_000, 20_000));
        let tracker = CostTracker::new(u);
        let summary = tracker.cost_summary();
        assert_eq!(summary.api_calls, 1);
        assert_eq!(summary.total_tokens_input, 500_000);
        assert_eq!(summary.total_tokens_output, 20_000);
        assert_eq!(summary.by_model.len(), 1);
        assert_eq!(summary.by_model[0].model, "claude-haiku-4");
    }

    #[test]
    fn cost_summary_multi_model() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("claude-sonnet-4", 1_000_000, 100_000));
        u.ingest(&delta("claude-haiku-4", 500_000, 20_000));
        let tracker = CostTracker::new(u);
        let summary = tracker.cost_summary();
        // Sonnet should be first (more expensive)
        assert!(summary.by_model[0].cost_usd > summary.by_model[1].cost_usd);
    }

    #[test]
    fn cost_summary_with_cache() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta_full(
            "claude-haiku-4",
            500_000,
            20_000,
            200_000,
            300_000,
        ));
        let tracker = CostTracker::new(u);
        let summary = tracker.cost_summary();
        // input: 500K @ $1/M = $0.50
        // output: 20K @ $5/M = $0.10
        // cacheW: 200K @ $1.25/M = $0.25
        // cacheR: 300K @ $0.10/M = $0.03
        // total = $0.88
        assert!(
            (summary.total_cost_usd - 0.88).abs() < 0.001,
            "expected ~0.88, got {}",
            summary.total_cost_usd
        );
        assert!(
            (summary.cache_write_cost_usd - 0.25).abs() < 0.001,
            "expected cache_write ~0.25, got {}",
            summary.cache_write_cost_usd
        );
        assert!(
            (summary.cache_read_cost_usd - 0.03).abs() < 0.001,
            "expected cache_read ~0.03, got {}",
            summary.cache_read_cost_usd
        );
    }

    #[test]
    fn from_usage_accumulator() {
        let u = UsageAccumulator::new();
        let tracker: CostTracker = u.into();
        assert_eq!(tracker.estimated_cost_usd(), 0.0);
    }

    #[test]
    fn into_inner_recovers_accumulator() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("sonnet", 100, 50));
        let tracker = CostTracker::new(u);
        let u_back = tracker.into_inner();
        assert_eq!(u_back.api_calls, 1);
    }

    #[test]
    fn usage_reflects_accumulator() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("sonnet", 100, 50));
        let tracker = CostTracker::new(u);
        assert_eq!(tracker.usage().api_calls, 1);
    }

    #[test]
    fn format_cost_edge_cases() {
        assert_eq!(format_cost_value(0.0001), "$0.0001");
        assert_eq!(format_cost_value(0.000099), "$0.000099");
        assert_eq!(format_cost_value(99.9999), "$99.9999");
        assert_eq!(format_cost_value(0.0), "$0.0000");
    }
}
