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
    /// Aim at `target` from nothing, for the model to be told of it.
    pub fn aim(&mut self, target: u64) {
        self.target = Some(target);
        self.unannounced = true;
        self.accumulated = 0;
        self.continuations = 0;
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
