//! MCP (Model Context Protocol) server events.

use serde::Serialize;

use crate::RedactionPolicy;

/// An MCP server connected: at startup, on a reload, or when one is added.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerConnectedPayload {
    pub server_name: String,
    pub transport: String,
    pub tool_count: usize,
}

/// An MCP server that was to be connected was not: it would not start, the
/// handshake failed or timed out, its configuration was rejected.
#[derive(Debug, Clone, Serialize)]
pub struct McpConnectionErrorPayload {
    pub server_name: String,
    pub error_message: String,
}

impl McpConnectionErrorPayload {
    pub(crate) fn redact(mut self, policy: &RedactionPolicy) -> Self {
        self.error_message = policy.scrub_message(&self.error_message);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TelemetryEvent;
    use crate::redact::RedactionPolicy;

    #[test]
    fn mcp_server_connected_serializes() {
        let event = TelemetryEvent::mcp_server_connected(
            "sess",
            1,
            None,
            McpServerConnectedPayload {
                server_name: "filesystem".into(),
                transport: "stdio".into(),
                tool_count: 8,
            },
        );
        assert_eq!(event.kind(), "mcp_server_connected");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "mcp_server_connected");
        assert_eq!(v["transport"], "stdio");
        assert_eq!(v["tool_count"], 8);
    }

    #[test]
    fn mcp_connection_error_serializes_and_redacts() {
        let event = TelemetryEvent::mcp_connection_error(
            "sess",
            1,
            None,
            McpConnectionErrorPayload {
                server_name: "db".into(),
                error_message: "connection to localhost:5432 refused".into(),
            },
        );
        assert_eq!(event.kind(), "mcp_connection_error");
        let v = serde_json::to_value(event.clone())
            .expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "mcp_connection_error");
        assert_eq!(v["server_name"], "db");

        let redacted = event.redact(&RedactionPolicy::all());
        let rv = serde_json::to_value(redacted)
            .expect("serialization of telemetry event should not fail");
        assert_eq!(rv["error_message"], "[REDACTED]");
    }
}
