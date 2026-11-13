//! `HealthCheck` — whether the engine, and what a host wired into it, is
//! working.
//!
//! Something with state, asked "are you all right", answering with a verdict
//! and enough detail to act on. `daemon`'s `doctor` is one such answer; this
//! is the shape as a contract, so an embedder linking the engine directly can
//! read the engine's answers and add its own checks beside them.
//!
//! # A check answers from what it already knows
//!
//! [`HealthCheck::check`] is synchronous and is expected to return promptly. A
//! probe that blocks on the subsystem it is probing turns "is the database
//! reachable" into a request that hangs exactly when the answer matters most,
//! and a health endpoint that hangs during an outage is worse than no health
//! endpoint. Both implementations the engine ships answer from state they
//! already hold — a fault counter, a file that is either there or not. A host
//! whose check needs to go over a network keeps a cached verdict, updated out
//! of band, and reports that.
//!
//! # A check reports, it does not repair
//!
//! Nothing here can change what the engine does. A check returns a verdict and
//! detail; there is no expression that reopens a circuit breaker, reloads
//! configuration or restarts anything. Diagnosis and action are different
//! decisions with different consequences, and a diagnostic that silently
//! repaired things would make every report a description of what it just did
//! rather than of what it found.
//!
//! # There is no `Err`
//!
//! A check that cannot determine the answer has determined something: it says
//! [`HealthStatus::Degraded`] and puts the reason in its summary. Returning a
//! `Result` would leave every caller deciding whether a failed check means
//! unhealthy or means nothing, and they would not all decide the same way.

use std::sync::Arc;

/// How well one thing is.
///
/// Ordered worst-last, so aggregating a report is a maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum HealthStatus {
    /// Working.
    Ok,
    /// Working, but something is wrong that someone should know about — a
    /// misconfigured tier, a plugin set aside, a fallback in use.
    Degraded,
    /// Not working. Whatever this check covers cannot be relied on.
    Failing,
}

impl HealthStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Failing => "failing",
        }
    }
}

/// One check's answer.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub status: HealthStatus,
    /// One line, for a person reading a status page.
    pub summary: String,
    /// Whatever a caller needs to act on it. `Null` when the summary is the
    /// whole story.
    pub details: serde_json::Value,
}

impl CheckResult {
    pub fn ok(summary: impl Into<String>) -> Self {
        Self::new(HealthStatus::Ok, summary)
    }

    pub fn degraded(summary: impl Into<String>) -> Self {
        Self::new(HealthStatus::Degraded, summary)
    }

    pub fn failing(summary: impl Into<String>) -> Self {
        Self::new(HealthStatus::Failing, summary)
    }

    pub fn new(status: HealthStatus, summary: impl Into<String>) -> Self {
        Self {
            status,
            summary: summary.into(),
            details: serde_json::Value::Null,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}

/// Something that can say whether it is working.
pub trait HealthCheck: Send + Sync {
    /// Stable, dotted, and unique among the checks registered beside it —
    /// this is what a status page keys on and what an alert names.
    fn name(&self) -> &str;

    fn check(&self) -> CheckResult;
}

/// One entry in a report.
#[derive(Debug, Clone)]
pub struct NamedResult {
    pub name: String,
    pub result: CheckResult,
}

/// What every registered check answered, and the worst of it.
#[derive(Debug, Clone)]
pub struct HealthReport {
    pub status: HealthStatus,
    pub checks: Vec<NamedResult>,
}

impl HealthReport {
    pub fn is_ok(&self) -> bool {
        self.status == HealthStatus::Ok
    }

    /// What one named check answered.
    pub fn find(&self, name: &str) -> Option<&NamedResult> {
        self.checks.iter().find(|c| c.name == name)
    }

    /// The report as JSON, for a status endpoint or an RPC result.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status.label(),
            "checks": self
                .checks
                .iter()
                .map(|c| serde_json::json!({
                    "name": c.name,
                    "status": c.result.status.label(),
                    "summary": c.result.summary,
                    "details": c.result.details,
                }))
                .collect::<Vec<_>>(),
        })
    }
}

/// The checks in force at one scope, and the report they produce together.
///
/// One of these per scope rather than one per process: a session's health and
/// the whole daemon's are different questions with different lifetimes, and a
/// single shared registry would either grow a check per session forever or
/// answer the wrong one of the two.
#[derive(Default)]
pub struct HealthChecks(Vec<Arc<dyn HealthCheck>>);

impl HealthChecks {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn with(mut self, check: Arc<dyn HealthCheck>) -> Self {
        self.0.push(check);
        self
    }

    pub fn from_vec(checks: Vec<Arc<dyn HealthCheck>>) -> Self {
        Self(checks)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn checks(&self) -> &[Arc<dyn HealthCheck>] {
        &self.0
    }

    /// Run every check. An empty set is [`HealthStatus::Ok`] — nothing has
    /// said otherwise, which is all any report ever claims.
    pub fn report(&self) -> HealthReport {
        let checks: Vec<NamedResult> = self
            .0
            .iter()
            .map(|c| NamedResult {
                name: c.name().to_string(),
                result: c.check(),
            })
            .collect();
        let status = checks
            .iter()
            .map(|c| c.result.status)
            .max()
            .unwrap_or(HealthStatus::Ok);
        HealthReport { status, checks }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(&'static str, HealthStatus);

    impl HealthCheck for Fixed {
        fn name(&self) -> &str {
            self.0
        }
        fn check(&self) -> CheckResult {
            CheckResult::new(self.1, "fixed")
        }
    }

    #[test]
    fn an_empty_set_is_healthy() {
        let report = HealthChecks::new().report();
        assert!(report.is_ok());
        assert!(report.checks.is_empty());
    }

    #[test]
    fn the_worst_check_decides_the_report() {
        let report = HealthChecks::new()
            .with(Arc::new(Fixed("a", HealthStatus::Ok)))
            .with(Arc::new(Fixed("b", HealthStatus::Degraded)))
            .with(Arc::new(Fixed("c", HealthStatus::Ok)))
            .report();
        assert_eq!(report.status, HealthStatus::Degraded);

        let worse = HealthChecks::new()
            .with(Arc::new(Fixed("b", HealthStatus::Degraded)))
            .with(Arc::new(Fixed("d", HealthStatus::Failing)))
            .report();
        assert_eq!(worse.status, HealthStatus::Failing);
    }

    #[test]
    fn every_check_is_in_the_report_in_the_order_it_was_registered() {
        let report = HealthChecks::new()
            .with(Arc::new(Fixed("a", HealthStatus::Ok)))
            .with(Arc::new(Fixed("b", HealthStatus::Failing)))
            .report();
        let names: Vec<_> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(report.to_json()["checks"][1]["status"], "failing");
    }
}
