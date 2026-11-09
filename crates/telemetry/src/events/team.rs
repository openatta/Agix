//! Team / multi-agent events.

use serde::Serialize;

/// A sub-agent started: one the `Agent` tool runs, or a team member.
#[derive(Debug, Clone, Serialize)]
pub struct AgentSpawnedPayload {
    /// The sub-agent's label (`<type>#<id>`) or the team member's label.
    pub agent_id: String,
    /// Its agent type; `default` when none was asked for.
    pub role: String,
    /// The model it runs on, when the spawn site knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The team stage it belongs to; `None` outside a team.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_name: Option<String>,
    /// Whether it runs in the background (`run_in_background`).
    pub background: bool,
}

/// A sub-agent finished.
#[derive(Debug, Clone, Serialize)]
pub struct AgentCompletedPayload {
    pub agent_id: String,
    pub role: String,
    pub had_error: bool,
    pub duration_ms: u64,
}

/// Team 流水线阶段完成事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct TeamStageCompletePayload {
    pub stage_name: String,
    pub agent_count: usize,
    pub duration_ms: u64,
    pub success: bool,
    pub agents_with_errors: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TelemetryEvent;

    #[test]
    fn agent_spawned_serializes() {
        let event = TelemetryEvent::agent_spawned(
            "sess",
            5,
            None,
            AgentSpawnedPayload {
                agent_id: "agent_02".into(),
                role: "researcher".into(),
                model: Some("claude-sonnet-4-6".into()),
                stage_name: Some("research".into()),
                background: false,
            },
        );
        assert_eq!(event.kind(), "agent_spawned");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "agent_spawned");
        assert_eq!(v["role"], "researcher");
    }

    #[test]
    fn agent_completed_serializes() {
        let event = TelemetryEvent::agent_completed(
            "sess",
            8,
            None,
            AgentCompletedPayload {
                agent_id: "agent_02".into(),
                role: "researcher".into(),
                had_error: false,
                duration_ms: 45000,
            },
        );
        assert_eq!(event.kind(), "agent_completed");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "agent_completed");
        assert_eq!(v["duration_ms"], 45000);
    }

    #[test]
    fn team_stage_complete_serializes() {
        let event = TelemetryEvent::team_stage_complete(
            "sess",
            10,
            None,
            TeamStageCompletePayload {
                stage_name: "implementation".into(),
                agent_count: 3,
                duration_ms: 120000,
                success: true,
                agents_with_errors: 0,
            },
        );
        assert_eq!(event.kind(), "team_stage_complete");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "team_stage_complete");
        assert_eq!(v["agent_count"], 3);
    }
}
