//! What a session has spent and asked for over its turns — kept on its
//! [`SessionState`](super::SessionState), so it outlives the engine that ran
//! them.

use crate::interface::budget_policy::OutputProgress;

#[derive(Debug, Clone, Default)]
pub struct SessionLedger {
    pub spent: SessionSpend,
    pub token_target: TokenTarget,
    /// Skills invoked or announced so far, re-injected after compaction.
    pub invoked_skills: Vec<String>,
    /// Repositories nested in the project the session has already reported
    /// or been told to keep, with a digest of their configuration then.
    pub nested_repos: std::collections::BTreeMap<std::path::PathBuf, u64>,
    /// The repositories nested in the project before the session's first
    /// tool call ran, with a digest of their configuration then; `None`
    /// until that call.
    pub nested_repos_at_start: Option<std::collections::BTreeMap<std::path::PathBuf, u64>>,
}

impl SessionLedger {
    pub fn note_skills<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
        for name in names {
            if !self.invoked_skills.iter().any(|s| s == name) {
                self.invoked_skills.push(name.to_string());
            }
        }
    }
}

/// A session's model calls and tool calls, summed over its turns.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionSpend {
    pub usage: crate::interface::model::Usage,
    pub api_calls: u32,
    pub tool_calls: u32,
}

/// An output token target, kept until it is met, given up on, or cleared —
/// a turn that ends otherwise leaves it running into the next.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenTarget {
    pub target: Option<u64>,
    /// Set by the message that set the target; the model has not been told yet.
    pub unannounced: bool,
    pub accumulated: u64,
    pub continuations: u32,
    pub last_delta: u64,
}

impl TokenTarget {
    /// No target, nothing counted toward one.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Aim at `target` from nothing, for the model to be told of it.
    pub fn aim(&mut self, target: u64) {
        *self = Self {
            target: Some(target),
            unannounced: true,
            ..Self::default()
        };
    }

    /// The target the model has not been told of yet, now told.
    pub fn take_unannounced(&mut self) -> Option<u64> {
        std::mem::take(&mut self.unannounced)
            .then_some(self.target)
            .flatten()
    }

    /// Count `this_delta` output tokens toward `target`.
    pub fn add_output(&mut self, target: u64, this_delta: u64) -> OutputProgress {
        self.accumulated = self.accumulated.saturating_add(this_delta);
        OutputProgress {
            accumulated: self.accumulated,
            target,
            continuations: self.continuations,
            this_delta,
            last_delta: self.last_delta,
        }
    }

    /// Record that the model was asked to keep going after `this_delta`
    /// output tokens; how many times it has been, now.
    pub fn continued(&mut self, this_delta: u64) -> u32 {
        self.continuations += 1;
        self.last_delta = this_delta;
        self.continuations
    }
}

#[cfg(test)]
mod tests {
    use super::TokenTarget;

    #[test]
    fn a_new_aim_carries_nothing_over_from_the_last_target() {
        let mut target = TokenTarget::default();
        target.aim(5_000);
        target.add_output(5_000, 700);
        target.continued(700);
        target.take_unannounced();

        target.aim(9_000);
        let progress = target.add_output(9_000, 10);
        assert_eq!(
            (
                target.unannounced,
                progress.accumulated,
                progress.continuations,
                progress.last_delta
            ),
            (true, 10, 0, 0)
        );
    }
}
