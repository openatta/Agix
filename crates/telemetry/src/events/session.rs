//! Session lifecycle events.

use serde::Serialize;

/// Session 开始事件的载荷。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SessionStartPayload {
    pub permission_mode: String,
    pub model: String,
    pub max_tokens: u32,
    pub thinking_mode: bool,
    pub sandbox_enabled: bool,
    pub resume_from: Option<String>,
    pub auth_modes: Vec<String>,
    pub mcp_server_count: usize,
    pub plugin_count: usize,
    pub skill_count: usize,
    pub output_format: String,
    pub started_at_ms: i64,
}

/// Session 结束事件的载荷。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SessionEndPayload {
    pub duration_ms: i64,
    pub total_turns: u32,
    pub total_api_calls: u32,
    pub total_tool_calls: u32,
    pub total_permission_denials: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation: u64,
    pub total_cache_read: u64,
    pub stop_reason: String,
}

/// How long the daemon took to start, recorded once it is ready to serve.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct StartupTimingPayload {
    pub total_startup_ms: u64,
    /// Reading the command line, settings and scenes.
    pub config_load_ms: u64,
    /// Assembling the session pool: providers, memory, history, skills, MCP.
    pub pool_build_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TelemetryEvent;
    use serde_json;

    #[test]
    fn event_has_schema_version_and_timestamps() {
        let event = TelemetryEvent::session_start(
            "sess_01",
            1,
            None,
            SessionStartPayload {
                permission_mode: "default".into(),
                model: "claude".into(),
                max_tokens: 4096,
                thinking_mode: false,
                sandbox_enabled: false,
                resume_from: None,
                auth_modes: vec![],
                mcp_server_count: 0,
                plugin_count: 0,
                skill_count: 0,
                output_format: "text".into(),
                started_at_ms: 1000,
            },
        );
        let v =
            serde_json::to_value(&event).expect("serialization of telemetry event should not fail");
        assert!(v.get("event_id").is_some(), "missing event_id");
        assert!(v.get("timestamp_ms").is_some(), "missing timestamp_ms");
        assert_eq!(v["schema_version"], 2);
        assert_eq!(v["session_id"], "sess_01");
        assert_eq!(event.kind(), "session_start");
    }

    #[test]
    fn session_end_serializes() {
        let event = TelemetryEvent::session_end(
            "sess",
            10,
            None,
            SessionEndPayload {
                duration_ms: 60000,
                total_turns: 10,
                total_api_calls: 12,
                total_tool_calls: 8,
                total_permission_denials: 1,
                total_input_tokens: 5000,
                total_output_tokens: 2000,
                total_cache_creation: 0,
                total_cache_read: 0,
                stop_reason: "end_turn".into(),
            },
        );
        let v =
            serde_json::to_value(&event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "session_end");
        assert_eq!(v["total_turns"], 10);
        assert_eq!(event.kind(), "session_end");
    }

    #[test]
    fn startup_timing_serializes() {
        let event = TelemetryEvent::startup_timing(
            "sess",
            1,
            None,
            StartupTimingPayload {
                total_startup_ms: 1500,
                config_load_ms: 50,
                pool_build_ms: 800,
            },
        );
        let v =
            serde_json::to_value(&event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "startup_timing");
        assert_eq!(v["total_startup_ms"], 1500);
        assert_eq!(v["pool_build_ms"], 800);
        assert_eq!(event.kind(), "startup_timing");
    }
}
