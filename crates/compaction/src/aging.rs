//! 老化（按时间的 MC、cached MC）：两道老化看什么、做了什么。

use crate::time_based_mc::{TimeBasedMcConfig, TimeBasedMcResult};
use std::time::Instant;

/// What the aging passes may look at.
///
/// Neither pass is about the token budget: one clears tool results that have
/// simply grown old, the other keeps the server-side cache from carrying
/// results nobody will read again. They run before anything consults the
/// budget, which is why they get their own input.
pub struct AgingInput<'a> {
    /// `(message index, when it was pushed)` for every message.
    pub message_ages: &'a [(usize, Instant)],
    pub time_based: &'a TimeBasedMcConfig,
    /// Whether the cached pass's interval has elapsed. The interval and the
    /// clock stay with the caller, which owns the pass's pending cache edits.
    pub cached_due: bool,
    pub cached_keep_recent: usize,
}

/// What one aging pass did.
#[derive(Debug, Default)]
pub struct Aging {
    pub time_based: TimeBasedMcResult,
    pub cached_cleared: usize,
    /// `Some` when the cached pass ran: the `tool_use_id`s whose results it
    /// blanked, which the next request sends as `cache_edits`.
    pub cache_edits: Option<Vec<String>>,
}

impl Aging {
    pub fn cleared(&self) -> usize {
        self.time_based.cleared + self.cached_cleared
    }
}
