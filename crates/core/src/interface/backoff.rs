//! `BackoffPolicy` — whether a failed request is tried again, and how long the
//! caller waits first.
//!
//! Every wire protocol the engine speaks needs this decision, and answers kept
//! per client drift apart — most damagingly on jitter: without it, every
//! client that hits the same rate limit retries in lockstep.
//!
//! # What is protocol-specific and what is not
//!
//! Reading a `retry-after` header is protocol work: the header exists on one
//! side and not the other, and its spelling is part of the wire format. So
//! parsing stays in the client. What the number *means* — whether to honour
//! it, whether the attempt budget is spent, how long to actually sleep — is
//! not protocol work, and that is what moves here. A client's whole
//! contribution is to describe the failure it just had, in the four kinds
//! below, plus whatever the provider itself asked for.
//!
//! # The attempt budget is the policy's, not the caller's
//!
//! Bounding a retry loop by comparing an attempt counter against the ladder's
//! length would put "how many times" in the caller and "how long" in the
//! table. They are one decision. The policy is asked after every
//! failure and answers [`Backoff::GiveUp`] when it is done, so a deployment
//! that wants four retries, or none, changes one thing rather than two.

use std::time::Duration;

/// Why an attempt failed, reduced to what a backoff decision can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestFailure {
    /// The provider said "too many requests".
    RateLimited,
    /// The provider said it is busy and to come back.
    Overloaded,
    /// Nothing usable came back: connection reset, DNS, TLS.
    Transport,
    /// Anything else — a rejected key, a malformed request, a stream that died
    /// after the provider had already started billing it.
    Other,
}

/// One failed attempt, as the policy sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailedAttempt {
    /// How many attempts already failed before this one. The first failure is
    /// `0`, which is also the index into a ladder of delays.
    pub index: u32,
    pub failure: RequestFailure,
    /// What the provider asked for, when it said. Anthropic's `retry-after`
    /// arrives here; the OpenAI-compatible side sends nothing, and a policy
    /// that only ever sees `None` still works.
    pub server_hint: Option<Duration>,
}

/// What the caller should do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// Stop. The failure reaches the caller as it stands.
    GiveUp,
    /// Sleep this long, then send the same request again.
    WaitThenRetry(Duration),
}

/// Whether a failed request is tried again, and after how long.
pub trait BackoffPolicy: Send + Sync {
    fn after_failure(&self, attempt: &FailedAttempt) -> Backoff;
}

/// The engine's own answer: a ladder of delays, jittered, deferring to the
/// provider when it names a delay itself.
pub struct LadderBackoff {
    /// One delay per retry, in milliseconds. Its length is the retry budget.
    pub steps: Vec<u64>,
    /// How far either side of a step the actual delay may land, as a fraction.
    pub jitter_ratio: f32,
}

impl LadderBackoff {
    /// Six steps covering roughly a minute — what both clients shipped with.
    pub const DEFAULT_STEPS: &'static [u64] = &[1_000, 2_000, 4_000, 8_000, 16_000, 32_000];
    pub const DEFAULT_JITTER: f32 = 0.25;

    pub fn new() -> Self {
        Self::from_millis(Self::DEFAULT_STEPS.to_vec())
    }

    /// A ladder of the caller's own. Tests use it to collapse a minute of
    /// waiting into a few milliseconds.
    pub fn from_millis(steps: Vec<u64>) -> Self {
        Self {
            steps,
            jitter_ratio: Self::DEFAULT_JITTER,
        }
    }
}

impl Default for LadderBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl BackoffPolicy for LadderBackoff {
    fn after_failure(&self, attempt: &FailedAttempt) -> Backoff {
        if attempt.failure == RequestFailure::Other {
            return Backoff::GiveUp;
        }
        let Some(step) = self.steps.get(attempt.index as usize) else {
            return Backoff::GiveUp;
        };
        // A provider that names its own delay knows something the ladder does
        // not, but it does not get to extend the budget — the step it replaces
        // is still spent.
        let base = match attempt.server_hint {
            Some(hint) => hint.as_millis() as u64,
            None => *step,
        };
        Backoff::WaitThenRetry(jittered(base, self.jitter_ratio))
    }
}

/// Spread a delay by `±ratio` so that clients which failed together do not
/// return together.
///
/// The randomness is the sub-second part of the wall clock rather than a real
/// generator: this is jitter, not a nonce, and the alternative is a `rand`
/// dependency in the crate at the bottom of the graph.
fn jittered(base_ms: u64, ratio: f32) -> Duration {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let unit = (nanos % 1_000_000) as f32 / 1_000_000.0;
    let signed = (unit - 0.5) * 2.0;
    let delta_ms = (base_ms as f32 * ratio * signed) as i64;
    let final_ms = (base_ms as i64 + delta_ms).max(0) as u64;
    Duration::from_millis(final_ms)
}

/// Never retries anything.
///
/// The second implementation, and a configuration a host really runs: put a
/// retrying LLM gateway in front of the engine and the engine's own ladder
/// stops being resilience and starts being a minute of invisible latency on
/// top of the gateway's. An interactive deployment that would rather show the
/// error than sit silently for a minute wants the same thing.
pub struct NoBackoff;

impl BackoffPolicy for NoBackoff {
    fn after_failure(&self, _attempt: &FailedAttempt) -> Backoff {
        Backoff::GiveUp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(index: u32, failure: RequestFailure) -> FailedAttempt {
        FailedAttempt {
            index,
            failure,
            server_hint: None,
        }
    }

    fn wait_ms(b: Backoff) -> u64 {
        match b {
            Backoff::WaitThenRetry(d) => d.as_millis() as u64,
            Backoff::GiveUp => panic!("expected a retry, got GiveUp"),
        }
    }

    /// The ladder is six numbers a reader has no way to check against intent,
    /// and the sixth-to-seventh boundary is where retrying stops. Both are
    /// pinned here because both clients depend on them being what they were.
    #[test]
    fn the_ladder_is_one_to_thirty_two_seconds_and_then_stops() {
        let p = LadderBackoff::new();
        let expected = [1_000u64, 2_000, 4_000, 8_000, 16_000, 32_000];
        for (i, base) in expected.iter().enumerate() {
            let ms = wait_ms(p.after_failure(&at(i as u32, RequestFailure::Overloaded)));
            let slack = (*base as f64 * LadderBackoff::DEFAULT_JITTER as f64) as u64;
            assert!(
                ms >= base - slack && ms <= base + slack,
                "step {i}: {ms}ms is not within ±25% of {base}ms"
            );
        }
        assert_eq!(
            p.after_failure(&at(6, RequestFailure::Overloaded)),
            Backoff::GiveUp,
            "six delays is six retries"
        );
    }

    #[test]
    fn a_failure_that_will_not_get_better_is_not_retried() {
        assert_eq!(
            LadderBackoff::new().after_failure(&at(0, RequestFailure::Other)),
            Backoff::GiveUp
        );
    }

    #[test]
    fn rate_limits_overload_and_transport_failures_all_retry() {
        let p = LadderBackoff::new();
        for failure in [
            RequestFailure::RateLimited,
            RequestFailure::Overloaded,
            RequestFailure::Transport,
        ] {
            assert!(
                matches!(p.after_failure(&at(0, failure)), Backoff::WaitThenRetry(_)),
                "{failure:?} should retry"
            );
        }
    }

    /// A provider that names a delay overrides the ladder for that step —
    /// but does not buy an extra one.
    #[test]
    fn a_server_hint_replaces_the_step_without_extending_the_budget() {
        let p = LadderBackoff::new();
        let hinted = |index| FailedAttempt {
            index,
            failure: RequestFailure::RateLimited,
            server_hint: Some(Duration::from_secs(7)),
        };
        let ms = wait_ms(p.after_failure(&hinted(0)));
        assert!(
            (5_250..=8_750).contains(&ms),
            "{ms}ms is not the hinted 7s ±25%"
        );
        assert_eq!(p.after_failure(&hinted(6)), Backoff::GiveUp);
    }

    /// Jitter has to actually spread, or it is a constant with extra steps.
    #[test]
    fn jitter_stays_inside_the_band_it_promises() {
        for _ in 0..200 {
            let ms = jittered(1_000, 0.25).as_millis() as u64;
            assert!((750..=1_250).contains(&ms), "{ms}ms escaped the ±25% band");
        }
        assert_eq!(jittered(1_000, 0.0), Duration::from_millis(1_000));
    }

    #[test]
    fn a_deployment_behind_its_own_gateway_can_refuse_to_wait_at_all() {
        for failure in [
            RequestFailure::RateLimited,
            RequestFailure::Overloaded,
            RequestFailure::Transport,
            RequestFailure::Other,
        ] {
            assert_eq!(NoBackoff.after_failure(&at(0, failure)), Backoff::GiveUp);
        }
    }
}
