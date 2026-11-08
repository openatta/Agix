//! MCP (Model Context Protocol) server events.

use serde::Serialize;

use crate::RedactionPolicy;

/// MCP 服务器连接事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct McpServerConnectedPayload {
    pub server_name: String,
    pub transport: String,
    pub server_version: Option<String>,
    pub tool_count: usize,
    pub connect_duration_ms: u64,
}

/// MCP 服务器断开事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct McpServerDisconnectedPayload {
    pub server_name: String,
    pub reason: String,
    pub session_duration_ms: u64,
    pub was_connected: bool,
}

/// MCP 工具调用事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct McpToolCallPayload {
    pub server_name: String,
    pub tool_name: String,
    pub success: bool,
    pub latency_ms: u64,
    pub error_message: Option<String>,
}

/// MCP 连接错误事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct McpConnectionErrorPayload {
    pub server_name: String,
    pub error_kind: String,
    pub error_message: String,
    pub retry_count: u32,
    pub transport: String,
}

impl McpToolCallPayload {
    pub(crate) fn redact(mut self, _policy: &RedactionPolicy) -> Self {
        self.error_message = self.error_message.map(|_| "[REDACTED]".to_string());
        self
    }
}

impl McpConnectionErrorPayload {
    pub(crate) fn redact(mut self, _policy: &RedactionPolicy) -> Self {
        self.error_message = crate::events::redact_string(&self.error_message);
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
                server_version: Some("1.0.0".into()),
                tool_count: 8,
                connect_duration_ms: 150,
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
    fn mcp_server_disconnected_serializes() {
        let event = TelemetryEvent::mcp_server_disconnected(
            "sess",
            15,
            None,
            McpServerDisconnectedPayload {
                server_name: "filesystem".into(),
                reason: "server_exit".into(),
                session_duration_ms: 600000,
                was_connected: true,
            },
        );
        assert_eq!(event.kind(), "mcp_server_disconnected");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "mcp_server_disconnected");
        assert_eq!(v["reason"], "server_exit");
    }

    #[test]
    fn mcp_tool_call_serializes() {
        let event = TelemetryEvent::mcp_tool_call(
            "sess",
            5,
            None,
            McpToolCallPayload {
                server_name: "filesystem".into(),
                tool_name: "read".into(),
                success: true,
                latency_ms: 200,
                error_message: None,
            },
        );
        assert_eq!(event.kind(), "mcp_tool_call");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "mcp_tool_call");
        assert_eq!(v["server_name"], "filesystem");
        assert_eq!(v["success"], true);
    }

    #[test]
    fn mcp_connection_error_serializes_and_redacts() {
        let event = TelemetryEvent::mcp_connection_error(
            "sess",
            1,
            None,
            McpConnectionErrorPayload {
                server_name: "db".into(),
                error_kind: "connection_refused".into(),
                error_message: "connection to localhost:5432 refused".into(),
                retry_count: 3,
                transport: "stdio".into(),
            },
        );
        assert_eq!(event.kind(), "mcp_connection_error");
        let v = serde_json::to_value(event.clone())
            .expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "mcp_connection_error");
        assert_eq!(v["error_kind"], "connection_refused");

        let redacted = event.redact(&RedactionPolicy::all());
        let rv = serde_json::to_value(redacted)
            .expect("serialization of telemetry event should not fail");
        assert_eq!(rv["error_message"], "[REDACTED]");
    }
}
