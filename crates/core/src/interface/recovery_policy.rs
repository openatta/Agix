//! `RecoveryPolicy` — what to do when a model call goes wrong.
//!
//! The engine's own answers ([`DefaultRecovery`]): a request refused for size
//! is compacted and retried once. A provider reporting overload or a rate
//! limit is retried on the configured fallback model, once. A transient
//! failure after the stream had started — a stream that broke off, went
//! silent, or ended in an overload event — is sent again as it was, twice at
//! most, after 2s and then 8s, or after the provider's own `retry-after` —
//! unless it asks for longer than the client's ladder would wait
//! ([`LadderBackoff::MAX_SERVER_HINT`]), which fails instead. Anything else
//! fails. A response cut off at the
//! output limit escalates the limit — to 64K the first time, 8K after that,
//! three attempts, with a fixed nudge appended. A response the provider paused
//! is sent back to be continued.
//!
//! Every one of those is a reasonable answer and none of them is the only
//! one. A deployment on a single model has no fallback to switch to and would
//! rather wait. One paying per token would rather fail than escalate to 64K.
//! One with a cheap second provider would rather switch on *any* error, not
//! only overload.
//!
//! # Which failures reach it
//!
//! A failure is put to the policy when nothing of the call has been kept: the
//! provider refused the request, or its stream broke off — an error event, or
//! an end before the message's — before the reply asked for a tool. The
//! partial reply is dropped, so what a recovery retries is the request that
//! failed. Once a tool has run, the reply stays in the transcript with its
//! results and the turn ends with the error: sending it again would run the
//! tool again.
//!
//! A failure before the first byte has already been through the client's own
//! backoff ([`BackoffPolicy`](crate::interface::backoff::BackoffPolicy)), so
//! sending the same request again is not this policy's to decide — that
//! would multiply one ladder by another. [`ModelFailure::after_first_byte`]
//! says which side of that line a failure is on.
//!
//! What one call has already spent is handed in as [`CallRecovery`]. However
//! the policy answers, the loop recovers one call at most
//! [`MAX_RECOVERIES_PER_CALL`] times.
//!
//! # The policy decides; the loop acts
//!
//! Recovery *mechanisms* — compacting, rebuilding a request, sending it —
//! belong to the turn: they touch the session, the compactor and the model in
//! ways that only the loop can sequence correctly. What was missing is the
//! choice between them. So this contract answers "what should happen", in a
//! vocabulary the loop already implements, and nothing else. That is what
//! keeps the skeleton closed while the decision opens.
//!
//! # Attempt counts are the policy's memory
//!
//! Both recovery paths bound themselves with a counter the loop kept. The
//! counters are still the loop's — they belong to one turn, and a policy is
//! shared across turns — but they are *given* to the policy, so a policy that
//! wants to allow four escalations rather than three needs no state of its
//! own.

use crate::interface::backoff::LadderBackoff;
use crate::interface::model::ModelErrorKind;
use std::time::Duration;

/// Why a model call failed: the protocol client's classification, carried
/// over as it stands.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFailure<'a> {
    pub kind: ModelErrorKind,
    pub message: &'a str,
    /// How long the provider asked to be left alone, when it said.
    pub retry_after: Option<Duration>,
    /// The provider had started answering: the client's backoff no longer
    /// covered this call.
    pub after_first_byte: bool,
}

impl<'a> ModelFailure<'a> {
    pub fn new(kind: ModelErrorKind, message: &'a str) -> Self {
        Self {
            kind,
            message,
            retry_after: None,
            after_first_byte: false,
        }
    }

    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    pub fn with_after_first_byte(mut self, after_first_byte: bool) -> Self {
        self.after_first_byte = after_first_byte;
        self
    }
}

/// What one model call has already spent on recovering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallRecovery {
    pub switched_model: bool,
    pub compacted: bool,
    /// [`Recovery::RetrySame`]s so far.
    pub same_retries: u32,
}

/// The most recoveries the loop gives one call, whatever the policy answers.
pub const MAX_RECOVERIES_PER_CALL: u32 = 4;

/// What the turn should do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Give up. The turn fails and the message reaches the host.
    Fail,
    /// Send the same conversation to a different model.
    ///
    /// The retry carries no fallback of its own — a fallback that could fall
    /// back would be a loop with no bound, and the bound is the point.
    RetryWith { model: String },
    /// Shorten the conversation, then retry against the same model. Failing to
    /// shorten it is a failure: retrying an unshortened request that was
    /// refused for size just spends another call to be told the same thing.
    CompactAndRetry,
    /// Wait, then send the same request again. The host is told with a
    /// `model_retry` event, and the step starts over.
    RetrySame { after: Duration },
}

/// What to do about a response that stopped early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopRecovery {
    /// Accept the response as it is.
    Accept,
    /// Send the conversation again as it stands, the response included, so
    /// the model picks up where it stopped — what a provider asks for with
    /// `pause_turn`. Each continuation is another call of the turn, bounded
    /// by the turn policy's ceiling on calls.
    Continue,
    /// Raise the output limit and ask again.
    RaiseOutputLimit {
        max_tokens: u32,
        /// Text to append as a user message before retrying, if any. The
        /// engine's default appends a nudge telling the model to resume
        /// mid-thought rather than apologize and recap — which is worth
        /// keeping and worth being able to reword.
        nudge: Option<String>,
    },
}

/// How many times this turn has already tried to recover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryAttempt {
    /// Output-limit escalations so far in this turn.
    pub output_limit_escalations: u32,
    /// The limit currently in force.
    pub current_max_tokens: u32,
}

/// What to do when a model call goes wrong.
pub trait RecoveryPolicy: Send + Sync {
    /// The call failed, after `so_far` was already spent on it.
    fn on_failure(&self, failure: &ModelFailure<'_>, so_far: &CallRecovery) -> Recovery;

    /// The call succeeded but stopped early — `reason` is the model's own
    /// stop reason.
    ///
    /// Asked only for a reply that called no tools, after
    /// [`TurnPolicy::after_model_call`] let the turn go on. A retry decided
    /// here comes before the `Stop` hook and before
    /// [`BudgetPolicy::on_output_target`], which see the reply only when this
    /// gives up.
    ///
    /// [`TurnPolicy::after_model_call`]: crate::interface::turn_policy::TurnPolicy::after_model_call
    /// [`BudgetPolicy::on_output_target`]: crate::interface::budget_policy::BudgetPolicy::on_output_target
    fn on_early_stop(&self, reason: &str, attempt: &RecoveryAttempt) -> StopRecovery;
}

/// The engine's own answers.
pub struct DefaultRecovery {
    /// Where an overload or a rate limit goes. `None` leaves it to the
    /// same-request retry, which only covers a failure after the first byte.
    pub fallback_model: Option<String>,
    /// How many output-limit escalations a turn may spend.
    pub max_output_escalations: u32,
}

impl DefaultRecovery {
    /// First escalation goes straight to 64K on the theory that a response cut
    /// off once will be cut off again at a smaller bump; later ones settle at
    /// 8K and lean on the nudge instead.
    pub const FIRST_ESCALATION: u32 = 64_000;
    pub const LATER_ESCALATION: u32 = 8_000;

    pub const NUDGE: &'static str = "Output token limit hit. Resume directly — no apology, no \
                                     recap of what you were doing. Pick up mid-thought if that \
                                     is where the cut happened. Break remaining work into \
                                     smaller pieces.";

    /// How long to wait before each [`Recovery::RetrySame`] when the provider
    /// named no delay. Its length is the budget.
    pub const SAME_RETRY_DELAYS: &'static [Duration] =
        &[Duration::from_secs(2), Duration::from_secs(8)];

    pub fn new(fallback_model: Option<String>) -> Self {
        Self {
            fallback_model,
            max_output_escalations: 3,
        }
    }
}

impl RecoveryPolicy for DefaultRecovery {
    fn on_failure(&self, failure: &ModelFailure<'_>, so_far: &CallRecovery) -> Recovery {
        if failure.kind == ModelErrorKind::ContextTooLong && !so_far.compacted {
            return Recovery::CompactAndRetry;
        }
        if matches!(
            failure.kind,
            ModelErrorKind::Overloaded | ModelErrorKind::RateLimited
        ) && !so_far.switched_model
        {
            if let Some(model) = &self.fallback_model {
                return Recovery::RetryWith {
                    model: model.clone(),
                };
            }
        }
        if failure.after_first_byte && failure.kind.transient() {
            if let Some(delay) = Self::SAME_RETRY_DELAYS.get(so_far.same_retries as usize) {
                return match failure.retry_after {
                    Some(hint) if hint > LadderBackoff::MAX_SERVER_HINT => Recovery::Fail,
                    hint => Recovery::RetrySame {
                        after: hint.unwrap_or(*delay),
                    },
                };
            }
        }
        Recovery::Fail
    }

    fn on_early_stop(&self, reason: &str, attempt: &RecoveryAttempt) -> StopRecovery {
        if reason == "pause_turn" {
            return StopRecovery::Continue;
        }
        if reason != "max_tokens" || attempt.output_limit_escalations >= self.max_output_escalations
        {
            return StopRecovery::Accept;
        }
        // The first escalation is a limit change and nothing else; later ones
        // also nudge, because by then the model has been cut off twice and the
        // limit is evidently not the whole problem.
        if attempt.output_limit_escalations == 0
            && attempt.current_max_tokens < Self::FIRST_ESCALATION
        {
            return StopRecovery::RaiseOutputLimit {
                max_tokens: Self::FIRST_ESCALATION,
                nudge: None,
            };
        }
        StopRecovery::RaiseOutputLimit {
            max_tokens: attempt.current_max_tokens.max(Self::LATER_ESCALATION),
            nudge: Some(Self::NUDGE.to_string()),
        }
    }
}

/// Fails on everything and escalates nothing.
///
/// The second implementation, and a real configuration rather than a stub: a
/// deployment billed per token, or one running a batch where a surprise 64K
/// call is worse than a truncated answer, wants exactly this.
pub struct NeverRecover;

impl RecoveryPolicy for NeverRecover {
    fn on_failure(&self, _failure: &ModelFailure<'_>, _so_far: &CallRecovery) -> Recovery {
        Recovery::Fail
    }

    fn on_early_stop(&self, _reason: &str, _attempt: &RecoveryAttempt) -> StopRecovery {
        StopRecovery::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(kind: ModelErrorKind) -> ModelFailure<'static> {
        ModelFailure::new(kind, "")
    }

    fn streamed(kind: ModelErrorKind) -> ModelFailure<'static> {
        ModelFailure::new(kind, "").with_after_first_byte(true)
    }

    fn fresh() -> CallRecovery {
        CallRecovery::default()
    }

    fn attempt(escalations: u32, max_tokens: u32) -> RecoveryAttempt {
        RecoveryAttempt {
            output_limit_escalations: escalations,
            current_max_tokens: max_tokens,
        }
    }

    #[test]
    fn an_overload_switches_to_the_fallback_when_there_is_one() {
        let with = DefaultRecovery::new(Some("smaller-model".into()));
        assert_eq!(
            with.on_failure(&failure(ModelErrorKind::Overloaded), &fresh()),
            Recovery::RetryWith {
                model: "smaller-model".into()
            }
        );
        let without = DefaultRecovery::new(None);
        assert_eq!(
            without.on_failure(&failure(ModelErrorKind::Overloaded), &fresh()),
            Recovery::Fail
        );
    }

    #[test]
    fn a_request_refused_for_size_is_shortened_rather_than_resent() {
        assert_eq!(
            DefaultRecovery::new(None)
                .on_failure(&failure(ModelErrorKind::ContextTooLong), &fresh()),
            Recovery::CompactAndRetry
        );
    }

    #[test]
    fn anything_else_fails() {
        assert_eq!(
            DefaultRecovery::new(Some("m".into()))
                .on_failure(&failure(ModelErrorKind::Transport), &fresh()),
            Recovery::Fail,
            "a fallback exists for overload, not for every error"
        );
    }

    #[test]
    fn a_rate_limit_also_goes_to_the_fallback_once() {
        let p = DefaultRecovery::new(Some("smaller-model".into()));
        assert_eq!(
            p.on_failure(&failure(ModelErrorKind::RateLimited), &fresh()),
            Recovery::RetryWith {
                model: "smaller-model".into()
            }
        );
        let switched = CallRecovery {
            switched_model: true,
            ..fresh()
        };
        assert_eq!(
            p.on_failure(&failure(ModelErrorKind::RateLimited), &switched),
            Recovery::Fail,
            "before the first byte the client's ladder already waited"
        );
    }

    #[test]
    fn a_request_is_compacted_once() {
        let compacted = CallRecovery {
            compacted: true,
            ..fresh()
        };
        assert_eq!(
            DefaultRecovery::new(None)
                .on_failure(&failure(ModelErrorKind::ContextTooLong), &compacted),
            Recovery::Fail
        );
    }

    #[test]
    fn a_stream_that_broke_off_is_sent_again_twice() {
        let p = DefaultRecovery::new(None);
        let after = |same_retries| {
            p.on_failure(
                &streamed(ModelErrorKind::StreamInterrupted),
                &CallRecovery {
                    same_retries,
                    ..fresh()
                },
            )
        };
        assert_eq!(
            after(0),
            Recovery::RetrySame {
                after: Duration::from_secs(2)
            }
        );
        assert_eq!(
            after(1),
            Recovery::RetrySame {
                after: Duration::from_secs(8)
            }
        );
        assert_eq!(after(2), Recovery::Fail);
    }

    #[test]
    fn the_providers_delay_wins_over_the_default() {
        let hinted =
            streamed(ModelErrorKind::Overloaded).with_retry_after(Some(Duration::from_secs(5)));
        assert_eq!(
            DefaultRecovery::new(None).on_failure(&hinted, &fresh()),
            Recovery::RetrySame {
                after: Duration::from_secs(5)
            }
        );
    }

    #[test]
    fn a_provider_delay_beyond_the_ladders_cap_is_not_waited_for() {
        let p = DefaultRecovery::new(None);
        let asking = |delay| streamed(ModelErrorKind::Overloaded).with_retry_after(Some(delay));
        assert_eq!(
            p.on_failure(&asking(LadderBackoff::MAX_SERVER_HINT), &fresh()),
            Recovery::RetrySame {
                after: LadderBackoff::MAX_SERVER_HINT
            }
        );
        assert_eq!(
            p.on_failure(
                &asking(LadderBackoff::MAX_SERVER_HINT + Duration::from_secs(1)),
                &fresh()
            ),
            Recovery::Fail
        );
        assert_eq!(
            p.on_failure(&asking(Duration::from_secs(u64::MAX)), &fresh()),
            Recovery::Fail
        );
    }

    #[test]
    fn only_a_transient_failure_after_the_first_byte_is_sent_again() {
        let p = DefaultRecovery::new(None);
        for kind in [ModelErrorKind::Timeout, ModelErrorKind::Transport] {
            assert_eq!(
                p.on_failure(&failure(kind), &fresh()),
                Recovery::Fail,
                "{kind}: before the first byte"
            );
            assert!(matches!(
                p.on_failure(&streamed(kind), &fresh()),
                Recovery::RetrySame { .. }
            ));
        }
        for kind in [
            ModelErrorKind::Auth,
            ModelErrorKind::InvalidRequest,
            ModelErrorKind::Protocol,
            ModelErrorKind::Cancelled,
        ] {
            assert_eq!(
                p.on_failure(&streamed(kind), &fresh()),
                Recovery::Fail,
                "{kind}"
            );
        }
    }

    /// The escalation ladder, which is three numbers and an ordering that a
    /// reader has no way to check against intent — so it is checked here.
    #[test]
    fn the_escalation_ladder_is_64k_then_8k_with_a_nudge() {
        let p = DefaultRecovery::new(None);
        assert_eq!(
            p.on_early_stop("max_tokens", &attempt(0, 2000)),
            StopRecovery::RaiseOutputLimit {
                max_tokens: 64_000,
                nudge: None
            }
        );
        assert_eq!(
            p.on_early_stop("max_tokens", &attempt(1, 64_000)),
            StopRecovery::RaiseOutputLimit {
                max_tokens: 64_000,
                nudge: Some(DefaultRecovery::NUDGE.to_string())
            },
            "a limit already above 8K is not lowered to it"
        );
        assert_eq!(
            p.on_early_stop("max_tokens", &attempt(3, 64_000)),
            StopRecovery::Accept,
            "three escalations is the bound"
        );
    }

    #[test]
    fn a_paused_response_is_continued_whatever_the_escalations() {
        let p = DefaultRecovery::new(None);
        for escalations in [0, 3] {
            assert_eq!(
                p.on_early_stop("pause_turn", &attempt(escalations, 2000)),
                StopRecovery::Continue
            );
        }
        assert_eq!(
            NeverRecover.on_early_stop("pause_turn", &attempt(0, 2000)),
            StopRecovery::Accept
        );
    }

    #[test]
    fn an_ordinary_stop_reason_is_not_a_recovery() {
        assert_eq!(
            DefaultRecovery::new(None).on_early_stop("end_turn", &attempt(0, 2000)),
            StopRecovery::Accept
        );
    }

    #[test]
    fn a_deployment_can_refuse_to_spend_anything_on_recovery() {
        assert_eq!(
            NeverRecover.on_failure(&streamed(ModelErrorKind::Overloaded), &fresh()),
            Recovery::Fail
        );
        assert_eq!(
            NeverRecover.on_early_stop("max_tokens", &attempt(0, 2000)),
            StopRecovery::Accept
        );
    }
}
