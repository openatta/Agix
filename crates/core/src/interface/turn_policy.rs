//! `TurnPolicy` — when a turn has gone on long enough.
//!
//! A turn is a loop: call the model, run what it asked for, call it again. Two
//! judgements decide when that loop ends, and both were constants compiled
//! into the middle of a twelve-hundred-line function — a ceiling on model
//! calls, and a ceiling on structured-output retries. Neither is a fact about
//! how an agent works; both are opinions about how long is too long, and
//! different deployments hold different ones.
//!
//! # What is a judgement here, and what is not
//!
//! Only judgements about *progress* belong behind this contract. Three other
//! things end a turn and none of them is one:
//!
//! * **Cancellation** — the session was stopped. That is an instruction, and
//!   interruption is kernel-only for a reason: a policy that could decline to
//!   stop is a policy that could refuse to be stopped.
//! * **A hook said stop** — `PostToolUse` discontinuing, or a `Stop` hook.
//!   Those are already an extension point's output. Letting a policy overrule
//!   one would invert the trust order: the operator configured the hook, and
//!   the policy might have arrived with a plugin.
//! * **The model asked for tools, so there is more to do.** That is the loop's
//!   definition rather than an opinion about it — the skeleton, which stays
//!   closed.
//!
//! # Asked where the judgements already happen
//!
//! Two methods rather than one, because the two ceilings are checked at
//! different points today and consolidating them would reorder the loop. The
//! call ceiling is checked before spending a call; the retry ceiling after one
//! comes back. Moving either would be a behavior change wearing a refactor's
//! clothes.

use std::sync::Arc;

/// How far into a turn we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnProgress<'a> {
    /// Model calls made in this turn so far.
    pub api_calls: u32,
    /// Tool calls dispatched in this turn so far.
    pub tool_calls: u32,
    /// How many structured-output retries the model has spent.
    pub structured_output_calls: u32,
    /// The stop reason the model itself gave for the last call. Empty before
    /// the first call.
    pub stop_reason: &'a str,
}

/// What to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStep {
    /// Keep going.
    Continue,
    /// End the turn, reporting `reason`.
    ///
    /// The reason reaches the host as `TurnComplete.stop_reason` and is part
    /// of the observable contract — a policy inventing a new one is telling
    /// every consumer something they have no case for, so prefer the existing
    /// vocabulary (`max_turns`, `max_structured_output_retries`) unless the
    /// situation genuinely is new.
    Stop { reason: String },
}

impl TurnStep {
    pub fn stop(reason: impl Into<String>) -> Self {
        Self::Stop {
            reason: reason.into(),
        }
    }

    pub fn is_stop(&self) -> bool {
        matches!(self, Self::Stop { .. })
    }
}

/// When a turn has gone on long enough.
pub trait TurnPolicy: Send + Sync {
    /// Before spending another model call.
    fn before_model_call(&self, _progress: &TurnProgress<'_>) -> TurnStep {
        TurnStep::Continue
    }

    /// After a model call came back, before acting on what it asked for.
    fn after_model_call(&self, _progress: &TurnProgress<'_>) -> TurnStep {
        TurnStep::Continue
    }
}

/// The engine's own ceilings.
///
/// `max_api_calls` in particular is the *lower* of what settings allow and
/// what the scene allows, so a scene declaring a tighter ceiling is honoured.
/// Constructing the policy is the one place that rule lives.
pub struct LimitsPolicy {
    pub max_api_calls: u32,
    pub max_structured_output_retries: u32,
}

impl LimitsPolicy {
    /// `max_api_calls` is the lower of the two ceilings.
    pub fn new(settings_max_api_calls: u32, scene_max_api_calls: u32, max_retries: u32) -> Self {
        Self {
            max_api_calls: settings_max_api_calls.min(scene_max_api_calls),
            max_structured_output_retries: max_retries,
        }
    }
}

impl TurnPolicy for LimitsPolicy {
    fn before_model_call(&self, progress: &TurnProgress<'_>) -> TurnStep {
        if progress.api_calls >= self.max_api_calls {
            return TurnStep::stop("max_turns");
        }
        TurnStep::Continue
    }

    fn after_model_call(&self, progress: &TurnProgress<'_>) -> TurnStep {
        if progress.structured_output_calls >= self.max_structured_output_retries {
            return TurnStep::stop("max_structured_output_retries");
        }
        TurnStep::Continue
    }
}

/// Every policy in turn; the first one that says stop wins.
///
/// The second implementation, and the shape a host actually wants: adding a
/// ceiling of your own should not mean reimplementing the engine's. A stop is
/// never overridden by a later policy — a policy that could veto another's
/// stop would make "add a limit" mean "hope nobody added a looser one".
pub struct FirstOf(pub Vec<Arc<dyn TurnPolicy>>);

impl TurnPolicy for FirstOf {
    fn before_model_call(&self, progress: &TurnProgress<'_>) -> TurnStep {
        for p in &self.0 {
            let step = p.before_model_call(progress);
            if step.is_stop() {
                return step;
            }
        }
        TurnStep::Continue
    }

    fn after_model_call(&self, progress: &TurnProgress<'_>) -> TurnStep {
        for p in &self.0 {
            let step = p.after_model_call(progress);
            if step.is_stop() {
                return step;
            }
        }
        TurnStep::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(api_calls: u32, structured: u32) -> TurnProgress<'static> {
        TurnProgress {
            api_calls,
            tool_calls: 0,
            structured_output_calls: structured,
            stop_reason: "",
        }
    }

    fn limits() -> LimitsPolicy {
        LimitsPolicy::new(25, u32::MAX, 5)
    }

    #[test]
    fn the_call_ceiling_stops_at_it_not_after_it() {
        assert_eq!(limits().before_model_call(&at(24, 0)), TurnStep::Continue);
        assert_eq!(
            limits().before_model_call(&at(25, 0)),
            TurnStep::stop("max_turns"),
            "the check is `>=`; an off-by-one here is 26 calls billed instead of 25"
        );
    }

    #[test]
    fn the_tighter_of_the_two_ceilings_wins() {
        let scene_is_tighter = LimitsPolicy::new(25, 3, 5);
        assert_eq!(scene_is_tighter.max_api_calls, 3);
        let settings_are_tighter = LimitsPolicy::new(2, 10, 5);
        assert_eq!(settings_are_tighter.max_api_calls, 2);
    }

    #[test]
    fn the_retry_ceiling_is_checked_after_the_call_not_before() {
        assert_eq!(
            limits().before_model_call(&at(0, 99)),
            TurnStep::Continue,
            "a retry count must not end a turn before the call that would raise it"
        );
        assert_eq!(
            limits().after_model_call(&at(0, 5)),
            TurnStep::stop("max_structured_output_retries")
        );
    }

    #[test]
    fn a_host_ceiling_composes_with_the_engines_rather_than_replacing_it() {
        struct StopAtThree;
        impl TurnPolicy for StopAtThree {
            fn before_model_call(&self, progress: &TurnProgress<'_>) -> TurnStep {
                if progress.api_calls >= 3 {
                    return TurnStep::stop("host_ceiling");
                }
                TurnStep::Continue
            }
        }

        let composed = FirstOf(vec![Arc::new(limits()), Arc::new(StopAtThree)]);
        assert_eq!(composed.before_model_call(&at(2, 0)), TurnStep::Continue);
        assert_eq!(
            composed.before_model_call(&at(3, 0)),
            TurnStep::stop("host_ceiling"),
            "the tighter one wins even though the engine's would still allow it"
        );
        assert_eq!(
            composed.after_model_call(&at(0, 5)),
            TurnStep::stop("max_structured_output_retries"),
            "and the engine's still applies where the host's says nothing"
        );
    }

    /// A later policy cannot un-stop an earlier one. Otherwise "add a limit"
    /// would mean "hope nobody registered a looser one after you".
    #[test]
    fn a_stop_is_never_overridden() {
        struct NeverStops;
        impl TurnPolicy for NeverStops {}

        let composed = FirstOf(vec![Arc::new(limits()), Arc::new(NeverStops)]);
        assert_eq!(
            composed.before_model_call(&at(25, 0)),
            TurnStep::stop("max_turns")
        );
    }
}
