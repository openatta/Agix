//! Feature flag system — compile-time (Cargo features) + runtime (JSON-configurable),
//! using `cfg` for compile-time and a `FeatureFlags` struct for runtime.
//!
//! Usage:
//! ```ignore
//! if features::is_enabled("team_mode") {
//!     // team-specific logic
//! }
//! ```

use serde::{Deserialize, Serialize};

/// Runtime-configurable feature flags (loaded from settings JSON or CLI args).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeatureFlags {
    /// Enable multi-agent team coordination mode.
    #[serde(default)]
    pub team_mode: bool,

    /// Enable plugin marketplace (remote registry + dependency resolution).
    #[serde(default)]
    pub plugin_marketplace: bool,

    /// Enable extended memory features (LLM-based relevance selection, team memory sync).
    #[serde(default)]
    pub extended_memory: bool,

    /// Enable experimental agent routing (model selection by task type).
    #[serde(default)]
    pub experimental_agent: bool,

    /// Enable recorder auto-detection in test environments.
    #[serde(default)]
    pub recorder_auto_detect: bool,

    /// Enable microcompact caching (time-based tool result clearing).
    #[serde(default)]
    pub cached_microcompact: bool,

    /// Enable reactive compaction — proactive context compression triggered
    /// before the budget is fully exhausted, based on token usage velocity.
    #[serde(default)]
    pub reactive_compact: bool,

    /// Enable dream task — background thinking sub-agent that runs without
    /// blocking the user, writing thoughts to disk for later inspection.
    #[serde(default)]
    pub dream_task: bool,
}

impl FeatureFlags {
    /// Check if a named feature flag is enabled.
    /// Combines compile-time `cfg` gates with runtime configuration.
    pub fn is_enabled(&self, flag: &str) -> bool {
        // Compile-time gates take precedence (Cargo features disable at build time)
        match flag {
            "team_mode" => cfg!(feature = "team-mode") && self.team_mode,
            "plugin_marketplace" => cfg!(feature = "plugin-marketplace") && self.plugin_marketplace,
            "extended_memory" => self.extended_memory,
            "experimental_agent" => cfg!(feature = "experimental-agent") && self.experimental_agent,
            "recorder_auto_detect" => self.recorder_auto_detect,
            "cached_microcompact" => self.cached_microcompact,
            "reactive_compact" => self.reactive_compact,
            "dream_task" => self.dream_task,
            _ => false,
        }
    }

    /// Enable flags from environment variables (AGIX_FEATURE_*).
    pub fn apply_env_overrides(&mut self) {
        if std::env::var("AGIX_FEATURE_TEAM_MODE").is_ok() {
            self.team_mode = true;
        }
        if std::env::var("AGIX_FEATURE_PLUGIN_MARKETPLACE").is_ok() {
            self.plugin_marketplace = true;
        }
        if std::env::var("AGIX_FEATURE_EXTENDED_MEMORY").is_ok() {
            self.extended_memory = true;
        }
        if std::env::var("AGIX_FEATURE_EXPERIMENTAL_AGENT").is_ok() {
            self.experimental_agent = true;
        }
        if std::env::var("AGIX_FEATURE_REACTIVE_COMPACT").is_ok() {
            self.reactive_compact = true;
        }
        if std::env::var("AGIX_FEATURE_DREAM_TASK").is_ok() {
            self.dream_task = true;
        }
    }

    /// List all enabled flags.
    pub fn enabled_flags(&self) -> Vec<&'static str> {
        let all = [
            "team_mode",
            "plugin_marketplace",
            "extended_memory",
            "experimental_agent",
            "recorder_auto_detect",
            "cached_microcompact",
            "reactive_compact",
            "dream_task",
        ];
        all.into_iter().filter(|&f| self.is_enabled(f)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_flags_are_disabled() {
        let flags = FeatureFlags::default();
        assert!(!flags.is_enabled("team_mode"));
        assert!(!flags.is_enabled("plugin_marketplace"));
        assert!(!flags.is_enabled("extended_memory"));
    }

    #[test]
    fn enabled_flag_is_detected() {
        let flags = FeatureFlags {
            extended_memory: true,
            ..Default::default()
        };
        assert!(flags.is_enabled("extended_memory"));
        assert!(!flags.is_enabled("team_mode"));
    }

    #[test]
    fn env_override_enables_flags() {
        std::env::set_var("AGIX_FEATURE_EXTENDED_MEMORY", "1");
        let mut flags = FeatureFlags::default();
        flags.apply_env_overrides();
        assert!(flags.extended_memory);
        std::env::remove_var("AGIX_FEATURE_EXTENDED_MEMORY");
    }

    #[test]
    fn enabled_flags_lists_all() {
        let flags = FeatureFlags {
            team_mode: true,
            extended_memory: true,
            recorder_auto_detect: true,
            ..Default::default()
        };
        let list = flags.enabled_flags();
        // team_mode is gated by cfg(feature = "team-mode") — skip if not enabled
        if cfg!(feature = "team-mode") {
            assert!(list.contains(&"team_mode"));
        }
        assert!(list.contains(&"extended_memory"));
        assert!(list.contains(&"recorder_auto_detect"));
        assert!(!list.contains(&"plugin_marketplace"));
    }
}
