//! Team / multi-agent events.

use serde::Serialize;

use crate::RedactionPolicy;

/// Agent 创建事件的载荷（Team 模式）。
#[derive(Debug, Clone, Serialize)]
pub struct AgentSpawnedPayload {
    pub agent_id: String,
    pub role: String,
    pub model: String,
    pub parent_agent_id: Option<String>,
    pub stage_name: String,
}

/// Agent 完成事件的载荷（Team 模式）。
#[derive(Debug, Clone, Serialize)]
pub struct AgentCompletedPayload {
    pub agent_id: String,
    pub role: String,
    pub turn_count: u32,
    pub tool_call_count: u32,
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

/// Agent 间消息事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct AgentMessagePayload {
    pub sender_id: String,
    pub receiver_id: String,
    pub content_size: u64,
    pub is_command: bool,
    pub error_message: Option<String>,
}

impl AgentMessagePayload {
    pub(crate) fn redact(mut self, _policy: &RedactionPolicy) -> Self {
        self.error_message = self.error_message.map(|_| "[REDACTED]".to_string());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TelemetryEvent;
    use crate::redact::RedactionPolicy;

    #[test]
    fn agent_spawned_serializes() {
        let event = TelemetryEvent::agent_spawned(
            "sess",
            5,
            None,
            AgentSpawnedPayload {
                agent_id: "agent_02".into(),
                role: "researcher".into(),
                model: "claude-sonnet-4-6".into(),
                parent_agent_id: Some("agent_01".into()),
                stage_name: "research".into(),
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
                turn_count: 5,
                tool_call_count: 12,
                had_error: false,
                duration_ms: 45000,
            },
        );
        assert_eq!(event.kind(), "agent_completed");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "agent_completed");
        assert_eq!(v["turn_count"], 5);
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

    #[test]
    fn agent_message_serializes_and_redacts() {
        let policy = RedactionPolicy::all();
        let event = TelemetryEvent::agent_message(
            "sess",
            6,
            None,
            AgentMessagePayload {
                sender_id: "agent_01".into(),
                receiver_id: "agent_02".into(),
                content_size: 2048,
                is_command: true,
                error_message: Some("timeout".into()),
            },
        )
        .redact(&policy);
        assert_eq!(event.kind(), "agent_message");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "agent_message");
        assert_eq!(v["sender_id"], "agent_01");
        assert_eq!(v["error_message"], "[REDACTED]");
    }
}
