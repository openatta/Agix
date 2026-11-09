//! Tool execution and permission events.

use serde::Serialize;

use crate::RedactionPolicy;

/// 工具开始执行事件的载荷（在 ToolExecution 之前发出）。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ToolStartPayload {
    pub tool_name: String,
    pub tool_use_id: String,
    pub input_json_size: u64,
    pub destructive: bool,
    pub deferred: bool,
}

/// 工具执行事件的载荷。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ToolExecutionPayload {
    pub tool_name: String,
    pub tool_use_id: String,
    /// Unified outcome taxonomy: Succeeded / Failed / Timeout / etc.
    pub outcome: ToolOutcome,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub latency_ms: u64,
    pub input_json_size: u64,
    pub result_content_size: u64,
    pub user_approved: bool,
}

/// 工具被取消事件的载荷。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ToolCancelledPayload {
    pub tool_name: String,
    pub reason: String,
    pub elapsed_ms: u64,
}

/// What the session's permission check said about one tool call, before
/// anyone was asked. Whether an asked call then ran is `tool_execution`'s
/// `user_approved`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PermissionDecisionPayload {
    pub tool_name: String,
    pub outcome: PermissionDecisionOutcome,
    pub permission_mode: String,
    pub destructive: bool,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Approved,
    Denied,
    Succeeded,
    Failed,
    Timeout,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionOutcome {
    AutoAllow,
    Ask,
    Deny,
}

impl ToolExecutionPayload {
    pub(crate) fn redact(mut self, policy: &RedactionPolicy) -> Self {
        self.error_message = self.error_message.map(|m| policy.scrub_message(&m));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TelemetryEvent;
    use crate::redact::RedactionPolicy;
    use serde_json::json;

    #[test]
    fn permission_decision_redacts_inputs_by_shape() {
        let event = TelemetryEvent::permission_decision(
            "sess",
            1,
            None,
            PermissionDecisionPayload {
                tool_name: "bash".into(),
                outcome: PermissionDecisionOutcome::Ask,
                permission_mode: "default".into(),
                destructive: true,
                latency_ms: 4,
            },
        );

        assert_eq!(event.kind(), "permission_decision");
        let value =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(value["type"], "permission_decision");
        assert_eq!(value["outcome"], "ask");
        assert_eq!(value["tool_name"], "bash");
        assert_eq!(value.get("input").unwrap_or(&json!(null)), &json!(null));
        assert_eq!(value.get("prompt").unwrap_or(&json!(null)), &json!(null));
    }

    #[test]
    fn tool_outcome_all_variants() {
        for (outcome, expected) in [
            (ToolOutcome::Approved, "approved"),
            (ToolOutcome::Denied, "denied"),
            (ToolOutcome::Succeeded, "succeeded"),
            (ToolOutcome::Failed, "failed"),
            (ToolOutcome::Timeout, "timeout"),
            (ToolOutcome::Cancelled, "cancelled"),
        ] {
            let payload = ToolExecutionPayload {
                tool_name: "Bash".into(),
                tool_use_id: "tu_01".into(),
                outcome,
                is_error: matches!(outcome, ToolOutcome::Failed | ToolOutcome::Timeout),
                error_message: None,
                latency_ms: 100,
                input_json_size: 42,
                result_content_size: 128,
                user_approved: true,
            };
            let event = TelemetryEvent::tool_execution("sess", 1, None, payload);
            assert_eq!(event.kind(), "tool_execution");
            let v = serde_json::to_value(event)
                .expect("serialization of telemetry event should not fail");
            assert_eq!(v["type"], "tool_execution");
            assert_eq!(v["outcome"], expected);
            assert_eq!(v["tool_name"], "Bash");
        }
    }

    #[test]
    fn permission_decision_all_outcomes() {
        for (outcome, expected) in [
            (PermissionDecisionOutcome::AutoAllow, "auto_allow"),
            (PermissionDecisionOutcome::Ask, "ask"),
            (PermissionDecisionOutcome::Deny, "deny"),
        ] {
            let payload = PermissionDecisionPayload {
                tool_name: "Read".into(),
                outcome,
                permission_mode: "default".into(),
                destructive: false,
                latency_ms: 3,
            };
            let event = TelemetryEvent::permission_decision("sess", 2, None, payload);
            let v = serde_json::to_value(event)
                .expect("serialization of telemetry event should not fail");
            assert_eq!(v["type"], "permission_decision");
            assert_eq!(v["outcome"], expected);
        }
    }

    #[test]
    fn tool_execution_redacts_error_on_failure() {
        let policy = RedactionPolicy::all();
        let event = TelemetryEvent::tool_execution(
            "sess",
            3,
            None,
            ToolExecutionPayload {
                tool_name: "Bash".into(),
                tool_use_id: "tu_01".into(),
                outcome: ToolOutcome::Failed,
                is_error: true,
                error_message: Some("sensitive stderr output".into()),
                latency_ms: 200,
                input_json_size: 10,
                result_content_size: 500,
                user_approved: true,
            },
        )
        .redact(&policy);
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["error_message"], "[REDACTED]");
        assert_eq!(v["tool_name"], "Bash");
        assert_eq!(v["outcome"], "failed");
    }

    #[test]
    fn tool_start_serializes() {
        let event = TelemetryEvent::tool_start(
            "sess",
            5,
            None,
            ToolStartPayload {
                tool_name: "Bash".into(),
                tool_use_id: "tu_01".into(),
                input_json_size: 128,
                destructive: true,
                deferred: false,
            },
        );
        assert_eq!(event.kind(), "tool_start");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "tool_start");
        assert_eq!(v["tool_name"], "Bash");
        assert_eq!(v["destructive"], true);
    }

    #[test]
    fn tool_cancelled_serializes() {
        let event = TelemetryEvent::tool_cancelled(
            "sess",
            3,
            None,
            ToolCancelledPayload {
                tool_name: "Read".into(),
                reason: "user_interrupt".into(),
                elapsed_ms: 500,
            },
        );
        assert_eq!(event.kind(), "tool_cancelled");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "tool_cancelled");
        assert_eq!(v["reason"], "user_interrupt");
    }
}
