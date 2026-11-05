//! Perf — in-process lightweight performance metrics collector.
//! Always running. Non-blocking. No external dependencies.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct PerfCollector {
    metrics: Arc<RwLock<HashMap<String, Vec<f64>>>>,
    /// Instant at which this collector was created.
    start: Instant,
}

impl Default for PerfCollector {
    fn default() -> Self {
        Self {
            metrics: Arc::new(RwLock::new(HashMap::new())),
            start: Instant::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetricStats {
    pub last: f64,
    pub avg: f64,
    pub min: f64,
    pub max: f64,
    pub p95: f64,
    pub count: usize,
}

impl PerfCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a timed measurement.
    pub fn record(&self, category: &str, name: &str, value_ms: f64) {
        let key = format!("{category}.{name}");
        if let Ok(mut m) = self.metrics.write() {
            m.entry(key).or_default().push(value_ms);
        }
    }

    /// Start a timer and return a closure that stops it and records the value.
    pub fn start_timer(&self, category: &str, name: &str) -> impl FnOnce() -> f64 {
        let start = Instant::now();
        let cat = category.to_string();
        let n = name.to_string();
        let this = self.clone();
        move || {
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            this.record(&cat, &n, elapsed);
            elapsed
        }
    }

    /// Record the elapsed time since collector creation as a named checkpoint.
    ///
    /// Returns the elapsed milliseconds since `Self` was created.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use telemetry::perf::PerfCollector;
    /// let perf = PerfCollector::new();
    /// // ... phase 1 ...
    /// let t1 = perf.checkpoint("config_loaded");
    /// // ... phase 2 ...
    /// let t2 = perf.checkpoint("skills_scanned");
    /// ```
    pub fn checkpoint(&self, name: &str) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64() * 1000.0;
        self.record("startup", name, elapsed);
        elapsed
    }

    /// Return the time elapsed since collector creation in milliseconds.
    pub fn elapsed_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    /// Return a snapshot of all startup checkpoint timings.
    ///
    /// Returns a `Vec` of `(checkpoint_name, elapsed_ms)` pairs sorted by elapsed time.
    pub fn checkpoint_snapshot(&self) -> Vec<(String, f64)> {
        let m = self.metrics.read().ok();
        let m = match m {
            Some(ref m) => m,
            None => return Vec::new(),
        };

        let mut result: Vec<(String, f64)> = m
            .iter()
            .filter_map(|(key, values)| {
                if key.starts_with("startup.") {
                    let name = key.strip_prefix("startup.").unwrap_or(key);
                    values.first().map(|v| (name.to_string(), *v))
                } else {
                    None
                }
            })
            .collect();

        result.sort_unstable_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        result
    }

    /// Compute stats for a given metric key.
    pub fn stats(&self, category: &str, name: &str) -> Option<MetricStats> {
        let key = format!("{category}.{name}");
        let m = self.metrics.read().ok()?;
        let values = m.get(&key)?;
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.clone();
        sorted.sort_unstable_by(f64::total_cmp);
        let len = sorted.len() as f64;
        let max = sorted[sorted.len() - 1];
        Some(MetricStats {
            last: max,
            avg: sorted.iter().sum::<f64>() / len,
            min: sorted[0],
            max,
            p95: sorted[(len * 0.95) as usize],
            count: values.len(),
        })
    }

    pub fn p95(&self, category: &str, name: &str) -> Option<f64> {
        self.stats(category, name).map(|s| s.p95)
    }

    /// Reset all recorded metrics.
    pub fn reset(&self) {
        if let Ok(mut m) = self.metrics.write() {
            m.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn checkpoint_records_elapsed_time() {
        let perf = PerfCollector::new();
        let t1 = perf.checkpoint("first");
        std::thread::sleep(Duration::from_millis(10));
        let t2 = perf.checkpoint("second");

        assert!(t1 >= 0.0, "first checkpoint should be non-negative");
        assert!(t2 > t1, "second checkpoint ({t2}) should be > first ({t1})");

        let snapshot = perf.checkpoint_snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].0, "first");
        assert_eq!(snapshot[1].0, "second");
    }

    #[test]
    fn checkpoint_snapshot_is_sorted() {
        let perf = PerfCollector::new();
        std::thread::sleep(Duration::from_millis(5));
        let _ = perf.checkpoint("b");
        std::thread::sleep(Duration::from_millis(5));
        let _ = perf.checkpoint("a");

        let snapshot = perf.checkpoint_snapshot();
        // Should be sorted by time, not by name
        assert_eq!(snapshot[0].0, "b");
        assert_eq!(snapshot[1].0, "a");
    }

    #[test]
    fn elapsed_ms_returns_non_zero() {
        let perf = PerfCollector::new();
        let e = perf.elapsed_ms();
        assert!(e >= 0.0);
    }

    #[test]
    fn reset_clears_all_metrics() {
        let perf = PerfCollector::new();
        perf.record("test", "a", 1.0);
        assert!(perf.stats("test", "a").is_some());
        perf.reset();
        assert!(perf.stats("test", "a").is_none());
    }
}
