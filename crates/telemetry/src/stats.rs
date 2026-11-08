//! `UsageAccumulator` — token usage tracking with cost estimation for `/cost`.
//!
//! Lives in agent so the engine can merge sub-agent usage into the parent session.
//! CLI / TUI / slash query it through the Engine's public stats interface.
//!
//! # Pricing
//!
//! Anthropic public pricing (2025-05). Model matched by prefix
//! (e.g. `"claude-sonnet-4-20250514"` → `"claude-sonnet-4"`).
//! Unknown models report $0 (no error, just "no pricing data" in report).
//!
//! # Sub-agent merging
//!
//! When a child agent (spawned via `AgentTool`) completes, the parent engine calls
//! `merge_child()` to roll the child's token consumption into the parent accumulator.
//! This ensures `/cost` reflects the true session total including all sub-agents.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Pricing table: (input $/1M, output $/1M, cache_write $/1M, cache_read $/1M)
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

/// Single API usage delta — fed by streaming executor after each API call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageDelta {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub model: String,
}

/// Accumulated usage across all API calls in a session.
#[derive(Debug, Clone, Default)]
pub struct UsageAccumulator {
    pub api_calls: u32,
    pub input_total: u64,
    pub output_total: u64,
    pub cache_creation_total: u64,
    pub cache_read_total: u64,
    pub by_model: HashMap<String, ModelUsage>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelUsage {
    pub api_calls: u32,
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub model: String,
}

/// Public stats snapshot — the "统计接口" for external consumers.
#[derive(Debug, Clone, Serialize)]
pub struct UsageStats {
    pub api_calls: u32,
    pub input_total: u64,
    pub output_total: u64,
    pub cache_creation_total: u64,
    pub cache_read_total: u64,
    pub estimated_cost: f64,
}

impl UsageAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest a single API call usage delta.
    pub fn ingest(&mut self, delta: &UsageDelta) {
        if delta.output_tokens == 0 && delta.input_tokens == 0 {
            return;
        }
        self.api_calls += 1;
        self.input_total += delta.input_tokens;
        self.output_total += delta.output_tokens;
        self.cache_creation_total += delta.cache_creation;
        self.cache_read_total += delta.cache_read;

        let entry = self.by_model.entry(delta.model.clone()).or_default();
        entry.model = delta.model.clone();
        entry.api_calls += 1;
        entry.input += delta.input_tokens;
        entry.output += delta.output_tokens;
        entry.cache_creation += delta.cache_creation;
        entry.cache_read += delta.cache_read;
    }

    /// Merge a child (sub-agent) accumulator into this parent.
    ///
    /// Called by the engine when a sub-agent spawned via `AgentTool` completes.
    /// Ensures the parent `/cost` reflects all child token consumption.
    pub fn merge_child(&mut self, child: &UsageAccumulator) {
        self.api_calls += child.api_calls;
        self.input_total += child.input_total;
        self.output_total += child.output_total;
        self.cache_creation_total += child.cache_creation_total;
        self.cache_read_total += child.cache_read_total;
        for (model, mu) in &child.by_model {
            let entry = self.by_model.entry(model.clone()).or_default();
            entry.model = model.clone();
            entry.api_calls += mu.api_calls;
            entry.input += mu.input;
            entry.output += mu.output;
            entry.cache_creation += mu.cache_creation;
            entry.cache_read += mu.cache_read;
        }
    }

    /// Public stats snapshot — the "统计接口".
    pub fn stats(&self) -> UsageStats {
        UsageStats {
            api_calls: self.api_calls,
            input_total: self.input_total,
            output_total: self.output_total,
            cache_creation_total: self.cache_creation_total,
            cache_read_total: self.cache_read_total,
            estimated_cost: self.estimated_cost_total(),
        }
    }

    /// Prefix-match pricing. E.g. "claude-sonnet-4-20250514" → "claude-sonnet-4".
    fn price_for(model: &str) -> (f64, f64, f64, f64) {
        for (prefix, inp, outp, cw, cr) in PRICING {
            if model.starts_with(prefix) {
                return (*inp, *outp, *cw, *cr);
            }
        }
        (0.0, 0.0, 0.0, 0.0)
    }

    /// Human-readable report for `/cost`.
    pub fn report(&self) -> String {
        if self.api_calls == 0 {
            return "no API calls yet this session".to_string();
        }

        let mut lines: Vec<String> = Vec::new();

        let total_cost: f64 = self
            .by_model
            .values()
            .map(|u| {
                let (inp, outp, cw, cr) = Self::price_for(&u.model);
                (u.input as f64 * inp
                    + u.output as f64 * outp
                    + u.cache_creation as f64 * cw
                    + u.cache_read as f64 * cr)
                    / 1_000_000.0
            })
            .sum();
        lines.push(format!(
            "Total: {} API call{}, {:>10} in, {:>10} out, {:>8} cacheW, {:>8} cacheR",
            self.api_calls,
            if self.api_calls == 1 { "" } else { "s" },
            fmt_num(self.input_total),
            fmt_num(self.output_total),
            fmt_num(self.cache_creation_total),
            fmt_num(self.cache_read_total),
        ));

        let mut models: Vec<_> = self.by_model.iter().collect();
        models.sort_by_key(|(k, _)| *k);
        for (model, u) in &models {
            let (cost, cost_cache) = self.cost_for(u);
            lines.push(format!(
                "  · {model}: {} call{}, in={} out={} cacheW={} cacheR={}",
                u.api_calls,
                if u.api_calls == 1 { "" } else { "s" },
                fmt_num(u.input),
                fmt_num(u.output),
                fmt_num(u.cache_creation),
                fmt_num(u.cache_read),
            ));
            if cost > 0.0 || cost_cache > 0.0 {
                lines.push(format!(
                    "    ≈ ${:.4} (${:.4} base + ${:.4} cache)",
                    cost + cost_cache,
                    cost,
                    cost_cache
                ));
            }
        }

        if total_cost > 0.0 {
            lines.push(format!("Estimated cost: ${:.4}", total_cost));
        } else {
            lines.push(
                "(cost estimate unavailable for current model — no pricing data)".to_string(),
            );
        }

        lines.join("\n")
    }

    fn cost_for(&self, mu: &ModelUsage) -> (f64, f64) {
        let (inp, outp, cw, cr) = Self::price_for(&mu.model);
        let base = (mu.input as f64 * inp + mu.output as f64 * outp) / 1_000_000.0;
        let cache = (mu.cache_creation as f64 * cw + mu.cache_read as f64 * cr) / 1_000_000.0;
        (base, cache)
    }

    /// Accumulated USD cost for status-line footer.
    pub fn estimated_cost_total(&self) -> f64 {
        self.by_model
            .values()
            .map(|u| {
                let (b, c) = self.cost_for(u);
                b + c
            })
            .sum()
    }
}

/// Human-friendly number formatting: 1024 → "1,024", 1000000 → "1,000,000".
fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(model: &str, input: u64, output: u64) -> UsageDelta {
        UsageDelta {
            input_tokens: input,
            output_tokens: output,
            cache_creation: 0,
            cache_read: 0,
            model: model.into(),
        }
    }

    fn ingest_with_cache(
        u: &mut UsageAccumulator,
        model: &str,
        input: u64,
        output: u64,
        cache_creation: u64,
        cache_read: u64,
    ) {
        u.ingest(&UsageDelta {
            input_tokens: input,
            output_tokens: output,
            cache_creation,
            cache_read,
            model: model.into(),
        });
    }

    #[test]
    fn accumulates_across_calls() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("claude-x", 100, 50));
        u.ingest(&delta("claude-x", 200, 30));
        assert_eq!(u.api_calls, 2);
        assert_eq!(u.input_total, 300);
        assert_eq!(u.output_total, 80);
        assert_eq!(u.by_model.len(), 1);
    }

    #[test]
    fn empty_delta_ignored() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("claude-x", 0, 0));
        assert_eq!(u.api_calls, 0);
    }

    #[test]
    fn by_model_split_works() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("haiku", 10, 5));
        u.ingest(&delta("sonnet", 100, 50));
        u.ingest(&delta("haiku", 20, 10));
        assert_eq!(u.by_model["haiku"].api_calls, 2);
        assert_eq!(u.by_model["sonnet"].api_calls, 1);
        assert_eq!(u.by_model["haiku"].input, 30);
    }

    #[test]
    fn report_zero_calls_message() {
        let u = UsageAccumulator::new();
        assert!(u.report().contains("no API calls"));
    }

    #[test]
    fn report_known_model_shows_cost() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("claude-sonnet-4-20250514", 1_000_000, 100_000));
        let r = u.report();
        assert!(r.contains("Estimated cost: $"));
        assert!(r.contains("$4.50")); // 1M in @$3 + 100K out @$15 = $3 + $1.50
    }

    #[test]
    fn report_with_cache() {
        let mut u = UsageAccumulator::new();
        ingest_with_cache(&mut u, "claude-haiku-4", 500_000, 20_000, 200_000, 300_000);
        let r = u.report();
        assert!(r.contains("$0.88"));
    }

    #[test]
    fn fmt_num_commas() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(999), "999");
        assert_eq!(fmt_num(1000), "1,000");
        assert_eq!(fmt_num(1000000), "1,000,000");
    }

    #[test]
    fn merge_child_accumulates() {
        let mut parent = UsageAccumulator::new();
        parent.ingest(&delta("sonnet", 1000, 500));

        let mut child = UsageAccumulator::new();
        child.ingest(&delta("haiku", 100, 50));
        child.ingest(&delta("haiku", 200, 30));

        parent.merge_child(&child);

        assert_eq!(parent.api_calls, 3);
        assert_eq!(parent.input_total, 1300);
        assert_eq!(parent.output_total, 580);
        assert_eq!(parent.by_model.len(), 2);
        assert_eq!(parent.by_model["sonnet"].api_calls, 1);
        assert_eq!(parent.by_model["haiku"].api_calls, 2);
    }

    #[test]
    fn stats_snapshot_matches() {
        let mut u = UsageAccumulator::new();
        u.ingest(&delta("sonnet", 1_000_000, 100_000));
        let s = u.stats();
        assert_eq!(s.api_calls, 1);
        assert_eq!(s.input_total, 1_000_000);
        assert_eq!(s.output_total, 100_000);
        assert!(s.estimated_cost >= 0.0);
    }

    #[test]
    fn merge_child_preserves_stats() {
        let mut parent = UsageAccumulator::new();
        parent.ingest(&delta("sonnet", 1000, 500));
        let mut child = UsageAccumulator::new();
        child.ingest(&delta("haiku", 100, 50));
        parent.merge_child(&child);
        let s = parent.stats();
        assert_eq!(s.api_calls, 2);
        assert_eq!(s.input_total, 1100);
        assert_eq!(s.output_total, 550);
    }
}
