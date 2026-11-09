//! Runtime feature flags — the `feature_flags` section of settings, switching
//! on behaviour that is off by default.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeatureFlags {
    /// Enable microcompact caching (time-based tool result clearing).
    #[serde(default)]
    pub cached_microcompact: bool,

    /// Enable reactive compaction — proactive context compression triggered
    /// before the budget is fully exhausted, based on token usage velocity.
    #[serde(default)]
    pub reactive_compact: bool,
}
