//! 自动压缩连续落空后的退避：下一次要跳过几次到期检查。

/// Automatic compactions in a row that left the conversation as it was. Each
/// one makes the next wait twice as many due checks as the last, up to
/// [`MAX_WAIT`](Self::MAX_WAIT), unless the context grows by a tenth of the
/// threshold first: more conversation may be something to drop.
#[derive(Debug, Default)]
pub(super) struct AutoCompactBackoff {
    misses: u32,
    checks_left: u32,
    resume_above: usize,
}

impl AutoCompactBackoff {
    const FIRST_WAIT: u32 = 2;
    const MAX_WAIT: u32 = 32;

    pub(super) fn misses(&self) -> u32 {
        self.misses
    }

    /// Whether this due check is skipped.
    pub(super) fn holds(&mut self, context_tokens: usize) -> bool {
        if self.checks_left == 0 || context_tokens > self.resume_above {
            self.checks_left = 0;
            return false;
        }
        self.checks_left -= 1;
        true
    }

    pub(super) fn missed(&mut self, context_tokens: usize, threshold: usize) {
        self.misses = self.misses.saturating_add(1);
        self.checks_left = (Self::FIRST_WAIT << (self.misses - 1).min(16)).min(Self::MAX_WAIT);
        self.resume_above = context_tokens.saturating_add(threshold / 10);
    }
}
