//! Telemetry configuration — deserialized from settings.json `telemetry` block.
//!
//! This crate never talks to the network itself: `enabled`/`mode` gate the
//! whole pipeline, `event_toggles`/`default_event_enabled` gate individual
//! event kinds (see `EventPayload::kind`). Error messages are scrubbed before
//! events reach the caller-supplied `TelemetryRecorder` callbacks (see
//! `spawn`). Where those events end up (file, HTTP, OTLP, …) is entirely the
//! caller's decision.

use serde::Deserialize;
use std::collections::HashMap;

/// Telemetry configuration.
///
/// `Default` is implemented by hand rather than derived: a derived `Default`
/// would give every `bool` field `false` (including `default_event_enabled`), silently diverging from what `serde` produces
/// for an absent/empty `telemetry` block in settings.json (`default_true`).
/// Callers building a config in Rust via `..Default::default()` need the two
/// to agree, or a struct-update literal quietly disables redaction and every
/// event kind.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub mode: TelemetryMode,
    #[serde(skip)]
    pub queue_size: usize,
    /// Per-event-kind enable switch, keyed by `EventPayload::kind()` (e.g.
    /// `"tool_execution"`, `"api_request"`). Kinds absent from this map
    /// fall back to `default_event_enabled`.
    #[serde(default)]
    pub event_toggles: HashMap<String, bool>,
    /// Fallback for event kinds not listed in `event_toggles`.
    #[serde(default = "default_true")]
    pub default_event_enabled: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: TelemetryMode::default(),
            queue_size: 0,
            event_toggles: HashMap::new(),
            default_event_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TelemetryMode {
    #[default]
    Enabled,
    Disabled,
}

fn default_true() -> bool {
    true
}
fn default_queue_size() -> usize {
    500
}

impl TelemetryConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            mode: TelemetryMode::Disabled,
            ..Default::default()
        }
    }

    pub fn queue_size(&self) -> usize {
        if self.queue_size > 0 {
            self.queue_size
        } else {
            default_queue_size()
        }
    }

    /// Whether events of the given kind (`EventPayload::kind()`) should be recorded.
    pub fn is_event_enabled(&self, kind: &str) -> bool {
        self.event_toggles
            .get(kind)
            .copied()
            .unwrap_or(self.default_event_enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlisted_kind_falls_back_to_default_event_enabled() {
        let config = TelemetryConfig {
            default_event_enabled: true,
            ..Default::default()
        };
        assert!(config.is_event_enabled("tool_execution"));

        let config = TelemetryConfig {
            default_event_enabled: false,
            ..Default::default()
        };
        assert!(!config.is_event_enabled("tool_execution"));
    }

    #[test]
    fn explicit_toggle_overrides_default() {
        let mut toggles = HashMap::new();
        toggles.insert("mcp_tool_call".to_string(), false);
        let config = TelemetryConfig {
            default_event_enabled: true,
            event_toggles: toggles,
            ..Default::default()
        };
        assert!(!config.is_event_enabled("mcp_tool_call"));
        assert!(config.is_event_enabled("tool_execution"));
    }

    #[test]
    fn queue_size_falls_back_to_default() {
        let config = TelemetryConfig::default();
        assert_eq!(config.queue_size(), 500);
    }

    #[test]
    fn disabled_sets_mode_and_enabled() {
        let config = TelemetryConfig::disabled();
        assert!(!config.enabled);
        assert_eq!(config.mode, TelemetryMode::Disabled);
    }
}
