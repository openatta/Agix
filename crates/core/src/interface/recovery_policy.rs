//! `RecoveryPolicy` — what to do when a model call goes wrong.
//!
//! The engine's own answers ([`DefaultRecovery`]): a provider reporting
//! overload is retried on the configured fallback model, or fails if there is
//! none. A request refused for size is compacted and retried once, or fails. A
//! response cut off at the output limit escalates the limit — to 64K the first
//! time, 8K after that, three attempts, with a fixed nudge appended. A
//! response the provider paused is sent back to be continued.
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
//! tool again. Each call is recovered at most once; a retry that fails is
//! final.
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

/// Why a model call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelFailure<'a> {
    /// The provider is busy. Retrying the same request elsewhere may work.
    Overloaded,
    /// The provider refused because the request was too large. Retrying the
    /// same request will fail the same way; something has to shrink first.
    ContextTooLong { message: &'a str },
    /// Anything else.
    Other { message: &'a str },
}

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
    /// The call failed.
    fn on_failure(&self, failure: &ModelFailure<'_>) -> Recovery;

    /// The call succeeded but stopped early — `reason` is the model's own
    /// stop reason.
    fn on_early_stop(&self, reason: &str, attempt: &RecoveryAttempt) -> StopRecovery;
}

/// The engine's own answers, unchanged.
pub struct DefaultRecovery {
    /// Where an overload goes. `None` means overload is fatal, which is what
    /// a session with no `fallback_model` configured has always done.
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

    pub fn new(fallback_model: Option<String>) -> Self {
        Self {
            fallback_model,
            max_output_escalations: 3,
        }
    }
}

impl RecoveryPolicy for DefaultRecovery {
    fn on_failure(&self, failure: &ModelFailure<'_>) -> Recovery {
        match failure {
            ModelFailure::Overloaded => match &self.fallback_model {
                Some(model) => Recovery::RetryWith {
                    model: model.clone(),
                },
                None => Recovery::Fail,
            },
            ModelFailure::ContextTooLong { .. } => Recovery::CompactAndRetry,
            ModelFailure::Other { .. } => Recovery::Fail,
        }
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
    fn on_failure(&self, _failure: &ModelFailure<'_>) -> Recovery {
        Recovery::Fail
    }

    fn on_early_stop(&self, _reason: &str, _attempt: &RecoveryAttempt) -> StopRecovery {
        StopRecovery::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            with.on_failure(&ModelFailure::Overloaded),
            Recovery::RetryWith {
                model: "smaller-model".into()
            }
        );
        let without = DefaultRecovery::new(None);
        assert_eq!(
            without.on_failure(&ModelFailure::Overloaded),
            Recovery::Fail
        );
    }

    #[test]
    fn a_request_refused_for_size_is_shortened_rather_than_resent() {
        assert_eq!(
            DefaultRecovery::new(None).on_failure(&ModelFailure::ContextTooLong {
                message: "prompt too long"
            }),
            Recovery::CompactAndRetry
        );
    }

    #[test]
    fn anything_else_fails() {
        assert_eq!(
            DefaultRecovery::new(Some("m".into())).on_failure(&ModelFailure::Other {
                message: "connection reset"
            }),
            Recovery::Fail,
            "a fallback exists for overload, not for every error"
        );
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
            NeverRecover.on_failure(&ModelFailure::Overloaded),
            Recovery::Fail
        );
        assert_eq!(
            NeverRecover.on_early_stop("max_tokens", &attempt(0, 2000)),
            StopRecovery::Accept
        );
    }
}
