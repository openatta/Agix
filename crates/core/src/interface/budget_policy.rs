//! `BudgetPolicy` — what a turn is allowed to spend, and how much context it
//! may carry.
//!
//! Three judgements, all of them constants before this: a cumulative token
//! ceiling read straight off settings, an output-volume target with three
//! magic numbers deciding when to stop asking for more, and a compaction
//! threshold the scene returned as a pair of numbers with no ceiling above
//! them. The last one is the gap that matters: `TokenBudget` said when to
//! start compacting and never said how large a context was too large, so
//! there was no configuration that could answer "never send a request bigger
//! than this".
//!
//! # Two ceilings that look alike and are not
//!
//! `Spending` is about the bill, accumulated across the turn and checked
//! against what the provider actually reported. `OutputTarget` is about the
//! opposite problem — the model stopping short of the volume of work that was
//! asked for. One ends a turn that has spent too much, the other prolongs a
//! turn that has produced too little. They stay separate methods because
//! merging them would let "write more" and "you have spent enough" argue with
//! each other inside one verdict.
//!
//! # What counts as spent is the policy's call, not the engine's
//!
//! [`Spend`] carries the provider's four figures apart rather than one sum.
//! There is no reading of them that is right for every deployment: a cache
//! read is billed at a fraction of ordinary input, so counting it whole
//! overstates the bill, and dropping it understates a cache-heavy turn by
//! most of what it read. Both answers are defensible and they are not the
//! engine's to choose, which is the reason this is a policy at all. The
//! engine reports; [`Spend::total_tokens`] and [`Spend::all_tokens`] name the
//! two obvious readings, and a policy that wants a third has the parts.
//!
//! `ContextBudget` is neither: it is about the size of a single request, not
//! the cost of the turn.

use crate::interface::scene::TokenBudget;

/// What the turn has spent so far, as the provider reported it.
///
/// Accumulated across every call in this turn, kept apart by kind because
/// each kind is priced differently and no single sum answers for all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Spend {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens written into the prompt cache, and tokens read back from it.
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

impl Spend {
    /// Input plus output, leaving the cache out.
    ///
    /// What the engine has always measured a budget against, and what
    /// [`EngineBudget`] still measures. Neither reading is the correct one —
    /// see the module docs — but this one is the older, so it keeps the
    /// shorter name and the existing behaviour.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Every token the provider reported, cache included.
    ///
    /// Counts a cache read the same as an ordinary input token, which is not
    /// what it costs. A policy that wants the bill rather than the volume has
    /// to weigh the parts itself.
    pub fn all_tokens(&self) -> u64 {
        self.total_tokens()
            .saturating_add(self.cache_creation_tokens)
            .saturating_add(self.cache_read_tokens)
    }
}

impl From<crate::interface::model::Usage> for Spend {
    fn from(u: crate::interface::model::Usage) -> Self {
        Self {
            input_tokens: u.input_tokens as u64,
            output_tokens: u.output_tokens as u64,
            cache_creation_tokens: u.cache_creation_input_tokens as u64,
            cache_read_tokens: u.cache_read_input_tokens as u64,
        }
    }
}

/// Whether the turn may keep spending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spending {
    WithinBudget,
    /// Close enough to say so. The turn continues.
    Warn {
        reminder: String,
        /// The ceiling this was measured against, for the record the engine
        /// keeps of budget enforcement.
        limit: u64,
    },
    /// The turn ends, reporting `budget_exceeded`.
    Exhausted {
        limit: u64,
    },
}

/// Progress toward an output-volume target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputProgress {
    /// Output tokens accumulated across the turn.
    pub accumulated: u64,
    pub target: u64,
    /// How many times the model has already been asked to keep going.
    pub continuations: u32,
    /// Output tokens from the call that just returned.
    pub this_delta: u64,
    /// Output tokens from the call before it.
    pub last_delta: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// Enough, or not worth asking again. The turn ends normally.
    Reached,
    /// Say this and go around again.
    KeepGoing { nudge: String },
}

/// How large a single request's context may get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Compact once the context passes this. `0` disables compaction.
    pub compact_threshold: usize,
    /// Rounds compaction keeps.
    pub compact_keep_recent: usize,
    /// A ceiling compaction is not allowed to leave the context above. `None`
    /// is what the engine has always done — the threshold said when to start
    /// compacting and nothing said when to stop trying.
    pub hard_cap: Option<usize>,
}

/// What a turn may spend and how much it may carry.
pub trait BudgetPolicy: Send + Sync {
    /// After a model call, against the usage the provider reported.
    fn on_usage(&self, _spend: &Spend) -> Spending {
        Spending::WithinBudget
    }

    /// When the model has stopped and a volume of output was asked for.
    fn on_output_target(&self, _progress: &OutputProgress) -> OutputTarget {
        OutputTarget::Reached
    }

    /// What the scene asked for, and what the deployment will actually allow.
    fn context_budget(&self, scene: &TokenBudget) -> ContextBudget {
        ContextBudget {
            compact_threshold: scene.compact_threshold,
            compact_keep_recent: scene.compact_keep_recent,
            hard_cap: None,
        }
    }
}

/// The engine's own numbers.
#[derive(Default)]
pub struct EngineBudget {
    /// `settings.execution.max_budget_tokens`. `None` is unlimited, which is
    /// the default.
    pub max_total_tokens: Option<u64>,
    /// `settings.compaction`: over the scene's threshold and kept rounds when
    /// set.
    pub compact_threshold: Option<usize>,
    pub compact_keep_recent: Option<usize>,
}

impl EngineBudget {
    pub fn new(max_total_tokens: Option<u64>) -> Self {
        Self {
            max_total_tokens,
            ..Self::default()
        }
    }

    /// The numbers a session's settings give.
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        Self {
            max_total_tokens: settings.execution.max_budget_tokens,
            compact_threshold: settings.compaction.threshold_tokens,
            compact_keep_recent: settings.compaction.keep_recent,
        }
    }
}

impl BudgetPolicy for EngineBudget {
    fn on_usage(&self, spend: &Spend) -> Spending {
        let Some(budget) = self.max_total_tokens else {
            return Spending::WithinBudget;
        };
        // `total_tokens()`, not `all_tokens()`: deployments set this ceiling
        // against that reading, and counting cache reads too would silently
        // lower every one of them by roughly the cache hit rate.
        let spent = spend.total_tokens();
        if spent >= budget {
            return Spending::Exhausted { limit: budget };
        }
        if spent >= budget * 9 / 10 {
            return Spending::Warn {
                reminder: "<system-reminder>\nOutput token budget nearly exhausted. \
                           Keep your response concise and wrap up.\n</system-reminder>"
                    .into(),
                limit: budget,
            };
        }
        Spending::WithinBudget
    }

    fn on_output_target(&self, p: &OutputProgress) -> OutputTarget {
        // 90% of the target counts as met, and three consecutive
        // sub-500-token calls count as the model having run out of things to
        // say rather than needing another nudge.
        let met = (p.target as f64 * 0.9) as u64;
        let diminishing = p.continuations >= 3 && p.this_delta < 500 && p.last_delta < 500;
        if diminishing || p.accumulated >= met {
            return OutputTarget::Reached;
        }
        let remaining = p.target.saturating_sub(p.accumulated);
        OutputTarget::KeepGoing {
            nudge: format!(
                "\
<system-reminder>
Continue working. Used {}/{} output tokens ({} remaining).
</system-reminder>",
                p.accumulated, p.target, remaining
            ),
        }
    }

    fn context_budget(&self, scene: &TokenBudget) -> ContextBudget {
        ContextBudget {
            compact_threshold: self.compact_threshold.unwrap_or(scene.compact_threshold),
            compact_keep_recent: self
                .compact_keep_recent
                .unwrap_or(scene.compact_keep_recent),
            hard_cap: None,
        }
    }
}

/// A ceiling on request size, on top of whatever else a policy decides.
///
/// The second implementation, and the one the missing capability calls for: a
/// deployment that has to guarantee it never sends a request past some size —
/// a provider hard limit, a cost ceiling, a compliance rule — wraps its policy
/// in this. A turn whose context is still above the cap after compaction ends
/// rather than sending the request anyway.
pub struct Capped<P> {
    pub inner: P,
    pub context_hard_cap: usize,
}

impl<P: BudgetPolicy> BudgetPolicy for Capped<P> {
    fn on_usage(&self, spend: &Spend) -> Spending {
        self.inner.on_usage(spend)
    }

    fn on_output_target(&self, progress: &OutputProgress) -> OutputTarget {
        self.inner.on_output_target(progress)
    }

    fn context_budget(&self, scene: &TokenBudget) -> ContextBudget {
        ContextBudget {
            hard_cap: Some(self.context_hard_cap),
            ..self.inner.context_budget(scene)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(budget: u64) -> EngineBudget {
        EngineBudget::new(Some(budget))
    }

    /// Input and output only, the way a caller with no cache reports.
    fn spent(input: u64, output: u64) -> Spend {
        Spend {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    #[test]
    fn the_bill_warns_at_ninety_percent_and_stops_at_the_cap() {
        let p = engine(1_000);
        assert_eq!(p.on_usage(&spent(800, 99)), Spending::WithinBudget);
        assert!(matches!(
            p.on_usage(&spent(800, 100)),
            Spending::Warn { .. }
        ));
        assert_eq!(
            p.on_usage(&spent(800, 200)),
            Spending::Exhausted { limit: 1_000 },
            "the check is `>=`; at the cap the turn is over, not one call later"
        );
    }

    #[test]
    fn no_cap_configured_means_no_ceiling() {
        assert_eq!(
            EngineBudget::new(None).on_usage(&spent(u64::MAX, 0)),
            Spending::WithinBudget
        );
    }

    /// The two readings, and the gap between them that made this a choice.
    ///
    /// A cache-heavy turn reads far more than `input_tokens` says: the figures
    /// here are one call's worth from a provider with the cache warm, where
    /// what was read is an order of magnitude past what a budget counting
    /// input and output would see.
    #[test]
    fn the_cache_is_most_of_what_a_warm_turn_read() {
        let spend = Spend {
            input_tokens: 400,
            output_tokens: 600,
            cache_creation_tokens: 2_000,
            cache_read_tokens: 30_000,
        };
        assert_eq!(spend.total_tokens(), 1_000);
        assert_eq!(spend.all_tokens(), 33_000);
    }

    /// A ceiling an operator set stays where they set it.
    ///
    /// `EngineBudget` reads `total_tokens()`, so a turn whose cache traffic
    /// dwarfs its budget is not stopped by it. That is a decision and not an
    /// oversight — a policy that wants the cache counted overrides `on_usage`
    /// and reads `all_tokens()`, and the engine does not make that choice for
    /// deployments that configured the ceiling against the older reading.
    #[test]
    fn the_engines_ceiling_does_not_move_when_the_cache_fills() {
        let spend = Spend {
            input_tokens: 400,
            output_tokens: 300,
            cache_creation_tokens: 5_000,
            cache_read_tokens: 90_000,
        };
        assert_eq!(
            engine(1_000).on_usage(&spend),
            Spending::WithinBudget,
            "the cache moved a 1000-token ceiling that nobody asked to move"
        );

        struct CountsTheCache;
        impl BudgetPolicy for CountsTheCache {
            fn on_usage(&self, spend: &Spend) -> Spending {
                if spend.all_tokens() >= 1_000 {
                    Spending::Exhausted { limit: 1_000 }
                } else {
                    Spending::WithinBudget
                }
            }
        }
        assert_eq!(
            CountsTheCache.on_usage(&spend),
            Spending::Exhausted { limit: 1_000 },
            "the parts a policy needs to reach the other answer are not reachable"
        );
    }

    #[test]
    fn a_usage_becomes_a_spend_without_losing_the_cache() {
        let spend = Spend::from(crate::interface::model::Usage {
            input_tokens: 11,
            output_tokens: 7,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 23,
        });
        assert_eq!(
            spend,
            Spend {
                input_tokens: 11,
                output_tokens: 7,
                cache_creation_tokens: 5,
                cache_read_tokens: 23,
            }
        );
    }

    fn progress(accumulated: u64, continuations: u32, this: u64, last: u64) -> OutputProgress {
        OutputProgress {
            accumulated,
            target: 100_000,
            continuations,
            this_delta: this,
            last_delta: last,
        }
    }

    #[test]
    fn ninety_percent_of_the_output_target_counts_as_reached() {
        let p = engine(u64::MAX);
        assert!(matches!(
            p.on_output_target(&progress(89_000, 0, 1_000, 0)),
            OutputTarget::KeepGoing { .. }
        ));
        assert_eq!(
            p.on_output_target(&progress(90_000, 0, 1_000, 0)),
            OutputTarget::Reached
        );
    }

    /// The rule that stops the engine nagging a model that has nothing left
    /// to add: three continuations *and* two consecutive small answers.
    #[test]
    fn diminishing_returns_take_all_three_conditions() {
        let p = engine(u64::MAX);
        assert_eq!(
            p.on_output_target(&progress(10_000, 3, 400, 400)),
            OutputTarget::Reached
        );
        assert!(matches!(
            p.on_output_target(&progress(10_000, 2, 400, 400)),
            OutputTarget::KeepGoing { .. },
        ),);
        assert!(matches!(
            p.on_output_target(&progress(10_000, 3, 600, 400)),
            OutputTarget::KeepGoing { .. }
        ));
    }

    /// There is no cap on how many times the model can be asked to continue.
    /// A count alone never ends it — only the count together with two small
    /// answers does.
    #[test]
    fn a_long_run_of_substantial_answers_keeps_going() {
        assert!(matches!(
            engine(u64::MAX).on_output_target(&progress(10_000, 20, 1_000, 1_000)),
            OutputTarget::KeepGoing { .. }
        ));
    }

    #[test]
    fn the_scene_decides_the_threshold_until_a_deployment_caps_it() {
        let scene = TokenBudget {
            compact_threshold: 150_000,
            compact_keep_recent: 4,
        };
        let engine_only = EngineBudget::new(None).context_budget(&scene);
        assert_eq!(engine_only.compact_threshold, 150_000);
        assert_eq!(
            engine_only.hard_cap, None,
            "the shape of the gap this fills"
        );

        let capped = Capped {
            inner: EngineBudget::new(None),
            context_hard_cap: 120_000,
        }
        .context_budget(&scene);
        assert_eq!(capped.hard_cap, Some(120_000));
        assert_eq!(
            capped.compact_threshold, 150_000,
            "a ceiling is not a trigger — the scene still says when to compact"
        );
    }

    #[test]
    fn the_settings_compaction_numbers_override_the_scenes_when_set() {
        let scene = TokenBudget {
            compact_threshold: 150_000,
            compact_keep_recent: 20,
        };
        let configured = EngineBudget {
            compact_threshold: Some(60_000),
            compact_keep_recent: Some(4),
            ..EngineBudget::default()
        }
        .context_budget(&scene);
        assert_eq!(
            (configured.compact_threshold, configured.compact_keep_recent),
            (60_000, 4)
        );

        let half = EngineBudget {
            compact_keep_recent: Some(4),
            ..EngineBudget::default()
        }
        .context_budget(&scene);
        assert_eq!(
            (half.compact_threshold, half.compact_keep_recent),
            (150_000, 4)
        );
    }
}
