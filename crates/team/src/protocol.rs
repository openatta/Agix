//! Protocol message types for agent-to-agent communication within a team.
//!
//! The [`ProtocolMessage`] enum provides structured, typed messages that bridge
//! agents can send via the team mailbox.  Currently, permission_request /
//! permission_response messages were sent as raw JSON; the explicit enum here
//! adds type safety and documents the full protocol surface.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

/// Protocol-level permission decision (independent of `base::permission` so
/// the protocol crate does not need to depend on the full permission system).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolDecision {
    /// Grant permission.
    Allow,
    /// Deny with a human-readable reason.
    Deny {
        reason: String,
        permissiveness: Option<Permissiveness>,
    },
    /// Escalate to user for interactive decision.
    Ask { message: String },
}

/// How permissive a decision is (used by the coordinator to reason about
/// aggregate agent behaviour).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Permissiveness {
    Strict,
    Normal,
    Lenient,
}

/// Structured messages that agents exchange through the team mailbox.
///
/// Each variant is serialised as a JSON object with a `"type"` tag so that
/// the receiver can dispatch on the type field without needing a statically
/// typed deserialiser.
///
/// # Examples
///
/// ```rust
/// use team::protocol::{ProtocolMessage, ProtocolDecision};
/// use time::OffsetDateTime;
///
/// let msg = ProtocolMessage::PermissionRequest {
///     tool_name: "Bash".into(),
///     tool_input: serde_json::json!({"command": "ls"}),
/// };
/// let json = msg.serialize_to_json();
/// let back = ProtocolMessage::deserialize_from_json(&json).unwrap();
/// assert_eq!(msg, back);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProtocolMessage {
    /// A sub-agent requests permission to call a tool.
    PermissionRequest {
        /// Name of the tool the sub-agent wants to call.
        tool_name: String,
        /// Input arguments for the tool.
        tool_input: Value,
        /// Optional correlation ID for matching responses to requests.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        /// Optional human-readable explanation.
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },

    /// A parent agent responds to a permission request.
    PermissionResponse {
        /// The decision (allow / deny / ask).
        decision: ProtocolDecision,
        /// Correlation ID matching the original `PermissionRequest`.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
    },

    /// A sub-agent reports that it has been idle for a while, allowing the
    /// coordinator to make scheduling decisions.
    IdleNotification {
        /// ID of the agent that became idle.
        agent_id: String,
        /// Instant when the agent entered the idle state.
        idle_since: OffsetDateTime,
        /// Optional human-readable reason.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// A coordinator or agent requests that a sub-agent be shut down.
    ShutdownRequest {
        /// Why the shutdown was requested.
        reason: String,
        /// If true, terminate immediately without graceful cleanup.
        #[serde(default)]
        force: bool,
        /// Target agent ID (when `None` the message is a broadcast).
        #[serde(skip_serializing_if = "Option::is_none")]
        target_agent_id: Option<String>,
    },

    /// A sub-agent presents a plan for the coordinator to review before
    /// executing it.
    PlanApprovalRequest {
        /// The plan text (e.g. markdown).
        plan: String,
        /// Ordered stages within the plan that need approval.
        #[serde(default)]
        stages: Vec<String>,
    },

    /// The coordinator (or another agent) responds to a plan approval request.
    PlanApprovalResponse {
        /// Whether the plan (or each stage) was approved.
        approved: bool,
        /// Optional feedback / requested changes.
        #[serde(skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
        /// Correlation ID matching the original `PlanApprovalRequest`.
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Periodic liveness signal sent by an agent to the coordinator.
    Heartbeat {
        /// ID of the agent sending the heartbeat.
        agent_id: String,
        /// Timestamp of the heartbeat.
        timestamp: OffsetDateTime,
        /// Optional status indicator (e.g. "busy", "idle", "processing").
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
}

impl ProtocolMessage {
    /// Serialise this message to a JSON string.
    pub fn serialize_to_json(&self) -> String {
        serde_json::to_string(self).expect("ProtocolMessage is always serialisable")
    }

    /// Deserialise a message from a JSON string.
    pub fn deserialize_from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Extract the `"type"` tag without fully deserialising.
    ///
    /// This is useful for quick routing decisions without paying the cost of
    /// a full parse.
    pub fn message_type(json: &str) -> Option<String> {
        let obj: Value = serde_json::from_str(json).ok()?;
        obj.get("type")?.as_str().map(static_from_type)
    }

    /// Return the `"type"` string for this message.
    pub fn type_str(&self) -> &'static str {
        match self {
            Self::PermissionRequest { .. } => "permission_request",
            Self::PermissionResponse { .. } => "permission_response",
            Self::IdleNotification { .. } => "idle_notification",
            Self::ShutdownRequest { .. } => "shutdown_request",
            Self::PlanApprovalRequest { .. } => "plan_approval_request",
            Self::PlanApprovalResponse { .. } => "plan_approval_response",
            Self::Heartbeat { .. } => "heartbeat",
        }
    }
}

/// Map a raw `"type"` string to a `String` so we can pattern-match on
/// known types without allocating.
fn static_from_type(s: &str) -> String {
    s.to_string()
}

/// Convert a `base::permission::PermissionDecision` to a protocol-level
/// [`ProtocolDecision`] so the mailbox layer does not leak the base crate's
/// internal permission model into the wire format.
impl From<&base::permission::PermissionDecision> for ProtocolDecision {
    fn from(d: &base::permission::PermissionDecision) -> Self {
        match d {
            base::permission::PermissionDecision::Allow { .. } => Self::Allow,
            base::permission::PermissionDecision::Deny { message, .. } => Self::Deny {
                reason: message.clone(),
                permissiveness: None,
            },
            base::permission::PermissionDecision::Ask { message, .. } => Self::Ask {
                message: message.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_permission_request() {
        let msg = ProtocolMessage::PermissionRequest {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls /tmp"}),
            tool_use_id: Some("req-001".into()),
            message: Some("need to inspect /tmp".into()),
        };
        let json = msg.serialize_to_json();
        let back = ProtocolMessage::deserialize_from_json(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn roundtrip_permission_response() {
        let msg = ProtocolMessage::PermissionResponse {
            decision: ProtocolDecision::Deny {
                reason: "not now".into(),
                permissiveness: Some(Permissiveness::Strict),
            },
            tool_use_id: Some("req-001".into()),
        };
        let json = msg.serialize_to_json();
        let back = ProtocolMessage::deserialize_from_json(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn roundtrip_idle_notification() {
        let msg = ProtocolMessage::IdleNotification {
            agent_id: "worker-1".into(),
            idle_since: OffsetDateTime::now_utc(),
            reason: Some("waiting for input".into()),
        };
        let json = msg.serialize_to_json();
        let back = ProtocolMessage::deserialize_from_json(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn roundtrip_shutdown_request() {
        let msg = ProtocolMessage::ShutdownRequest {
            reason: "task complete".into(),
            force: true,
            target_agent_id: Some("worker-1".into()),
        };
        let json = msg.serialize_to_json();
        let back = ProtocolMessage::deserialize_from_json(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn roundtrip_plan_approval_request() {
        let msg = ProtocolMessage::PlanApprovalRequest {
            plan: "1. Research\n2. Implement\n3. Test".into(),
            stages: vec!["Research".into(), "Implement".into(), "Test".into()],
        };
        let json = msg.serialize_to_json();
        let back = ProtocolMessage::deserialize_from_json(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn roundtrip_plan_approval_response() {
        let msg = ProtocolMessage::PlanApprovalResponse {
            approved: true,
            feedback: Some("Looks good, proceed".into()),
            request_id: Some("plan-001".into()),
        };
        let json = msg.serialize_to_json();
        let back = ProtocolMessage::deserialize_from_json(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn roundtrip_heartbeat() {
        let msg = ProtocolMessage::Heartbeat {
            agent_id: "coordinator-1".into(),
            timestamp: OffsetDateTime::now_utc(),
            status: Some("processing".into()),
        };
        let json = msg.serialize_to_json();
        let back = ProtocolMessage::deserialize_from_json(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn message_type_returns_correct_tag() {
        let msg = ProtocolMessage::PermissionRequest {
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"path": "/x"}),
            tool_use_id: None,
            message: None,
        };
        assert_eq!(msg.type_str(), "permission_request");
        let json = msg.serialize_to_json();
        assert_eq!(
            ProtocolMessage::message_type(&json),
            Some("permission_request".to_string())
        );
    }

    #[test]
    fn message_type_heartbeat() {
        let msg = ProtocolMessage::Heartbeat {
            agent_id: "a1".into(),
            timestamp: OffsetDateTime::now_utc(),
            status: None,
        };
        assert_eq!(msg.type_str(), "heartbeat");
        let json = msg.serialize_to_json();
        assert_eq!(
            ProtocolMessage::message_type(&json),
            Some("heartbeat".to_string())
        );
    }

    #[test]
    fn message_type_preserves_unknown() {
        let s = r#"{"type":"future_message","data":"x"}"#;
        assert_eq!(
            ProtocolMessage::message_type(s),
            Some("future_message".to_string())
        );
    }

    #[test]
    fn message_type_bad_json_none() {
        assert_eq!(ProtocolMessage::message_type("not-json"), None);
    }

    #[test]
    fn from_base_allow() {
        use base::permission::PermissionDecision;
        let base_d = PermissionDecision::allow();
        let proto: ProtocolDecision = (&base_d).into();
        assert_eq!(proto, ProtocolDecision::Allow);
    }

    #[test]
    fn from_base_deny() {
        use base::permission::{DecisionReason, PermissionDecision};
        let base_d = PermissionDecision::deny("blocked", DecisionReason::Other("test".into()));
        let proto: ProtocolDecision = (&base_d).into();
        match proto {
            ProtocolDecision::Deny {
                reason,
                permissiveness,
            } => {
                assert_eq!(reason, "blocked");
                assert_eq!(permissiveness, None);
            }
            _ => panic!("expected Deny"),
        }
    }

    #[test]
    fn from_base_ask() {
        use base::permission::PermissionDecision;
        let base_d = PermissionDecision::Ask {
            message: "confirm?".into(),
            decision_reason: None,
        };
        let proto: ProtocolDecision = (&base_d).into();
        match proto {
            ProtocolDecision::Ask { message } => assert_eq!(message, "confirm?"),
            _ => panic!("expected Ask"),
        }
    }
}
