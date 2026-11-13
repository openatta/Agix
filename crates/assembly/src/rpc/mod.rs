//! JSON-RPC 2.0 的线格式：请求、响应、错误、通知帧，以及连接、错误码、`options`
//! 的宽松读法、`session.event` 投影和方法表元数据。
//!
//! One JSON object per line (newline-delimited) — trivially readable
//! with `tail -f` for debugging.

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod client;
pub mod codes;
pub mod events;
mod method;
pub mod methods;
mod options;

pub use client::{send_frame, Client, ConnectionId, FrameSink, Sink};
pub use codes::ErrorCode;
pub use method::{schema_of, MethodInfo, NoParams, StartAgentParams, Untyped};
pub use options::{read_options, IgnoredOption, RawOptions};

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RpcRequest {
    /// JSON-RPC version (must be "2.0")
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    /// Method name (e.g. "session.create")
    pub method: String,
    /// Method-specific params (object or null)
    #[serde(default)]
    pub params: Value,
    /// Request id; we echo back. Number or string. None = notification.
    pub id: Option<Value>,
}

fn default_jsonrpc() -> String {
    "2.0".to_string()
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Like [`err`](Self::err), but attaches a structured `data` payload —
    /// for error codes where `docs/daemon_rpc_protocol.md` §9.1 documents the shape
    /// as part of the contract (clients are meant to read fields off it, not
    /// just display `message`).
    pub fn err_with_data(id: Value, code: i32, message: impl Into<String>, data: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: Some(data),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A `daemon.event` notification: what the pool reported, or what the
/// server says about the subscription itself.
#[derive(Debug, Clone, Serialize)]
pub struct DaemonEventFrame<'a> {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: DaemonEventParams<'a>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum DaemonEventParams<'a> {
    Pool(&'a crate::session_pool::api::DaemonEvent),
    Server(ServerDaemonEvent),
}

/// The `daemon.event` kinds the server makes up itself rather than relays
/// from the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerDaemonEvent {
    /// This connection read too slowly and missed `missed` notifications.
    EventsLagged { missed: u64 },
}

impl<'a> DaemonEventFrame<'a> {
    pub fn pool(event: &'a crate::session_pool::api::DaemonEvent) -> Self {
        Self::of(DaemonEventParams::Pool(event))
    }

    pub fn server(event: ServerDaemonEvent) -> Self {
        Self::of(DaemonEventParams::Server(event))
    }

    fn of(params: DaemonEventParams<'a>) -> Self {
        Self {
            jsonrpc: "2.0",
            method: "daemon.event",
            params,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_minimal() {
        let s = r#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#;
        let r: RpcRequest = serde_json::from_str(s).unwrap();
        assert_eq!(r.method, "daemon.status");
        assert_eq!(r.id, Some(serde_json::json!(1)));
    }

    #[test]
    fn parse_request_no_id_is_notification() {
        let s = r#"{"jsonrpc":"2.0","method":"daemon.ping"}"#;
        let r: RpcRequest = serde_json::from_str(s).unwrap();
        assert!(r.id.is_none());
    }

    #[test]
    fn ok_response_round_trip() {
        let r = RpcResponse::ok(serde_json::json!(42), serde_json::json!({"ok":true}));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""result":{"ok":true}"#));
        assert!(!s.contains("error"));
    }

    #[test]
    fn err_response_skips_result() {
        let r = RpcResponse::err(serde_json::json!(1), codes::METHOD_NOT_FOUND, "no");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""error""#));
        assert!(!s.contains(r#""result""#));
    }

    #[test]
    fn daemon_event_frame_format() {
        let event = crate::session_pool::api::DaemonEvent::SessionDeleted {
            session_id: "s1".into(),
            parent_session_id: None,
        };
        let frame: Value = serde_json::to_value(DaemonEventFrame::pool(&event)).unwrap();
        assert_eq!(
            frame,
            serde_json::json!({"jsonrpc": "2.0", "method": "daemon.event",
                               "params": {"kind": "session_deleted", "session_id": "s1"}})
        );
        let lagged = DaemonEventFrame::server(ServerDaemonEvent::EventsLagged { missed: 3 });
        assert_eq!(
            serde_json::to_value(lagged).unwrap()["params"],
            serde_json::json!({"kind": "events_lagged", "missed": 3})
        );
    }
}
