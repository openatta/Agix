//! JSON-RPC 2.0 wire types + error codes.
//!
//! One JSON object per line (newline-delimited) — trivially readable
//! with `tail -f` for debugging.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Somewhere to put one outgoing JSON-RPC frame.
///
/// The transports disagree about framing — a byte stream needs a delimiter
/// and an explicit flush, a WebSocket connection is already message-framed —
/// and nothing above this line should have to know which one it is talking
/// to.
///
/// Having a single implementation of "write one frame" per transport is also
/// what keeps flushing honest. It used to be written twice: the response
/// path flushed after every frame, the event-streaming path never flushed at
/// all, so a streamed token could sit in a buffer until the next one pushed
/// it out.
#[async_trait::async_trait]
pub trait FrameSink: Send + Sync {
    /// Write one complete frame. `false` means the peer is gone and the
    /// caller should stop writing to this connection.
    async fn send_json(&self, json: String) -> bool;
}

/// A sink shared by the dispatcher and by any streaming task it spawns.
pub type Sink = Arc<dyn FrameSink>;

/// Identifies one client connection for its lifetime.
///
/// A connection is the unit of subscription: a browser tab holds one, and
/// when it goes away its subscriptions go with it — and nothing else does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u64);

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn-{}", self.0)
    }
}

/// How many frames may sit queued for one connection before it is treated as
/// not reading.
///
/// Generous: a turn's worth of token deltas is well under this, so the limit
/// is only reached by a client that has stopped draining. Past it, the
/// session broadcast drops that connection's *subscription* rather than
/// waiting — see [`Client::try_send_json`].
const OUTBOUND_QUEUE_DEPTH: usize = 1024;

/// One client connection: an id, and a queue in front of its sink.
///
/// Everything written to a connection goes through the one queue, responses
/// and pushed frames alike, so a client sees them in the order the daemon
/// produced them. Two write paths into one socket would let a response
/// overtake the events that led to it.
///
/// The queue also decouples the agent loop from the client: a session's
/// broadcast enqueues and moves on, so one tab that has stopped reading
/// cannot hold up the turn — or the other tabs watching it.
pub struct Client {
    id: ConnectionId,
    tx: tokio::sync::mpsc::Sender<Outgoing>,
}

/// A queued frame, optionally carrying an acknowledgement the pump fires once
/// the frame has actually reached the sink.
struct Outgoing {
    json: String,
    written: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Client {
    /// Wrap `sink` and start the pump that drains the queue into it.
    ///
    /// The pump ends when the connection is dropped or the sink reports the
    /// peer gone; nothing else has to notice.
    pub fn new(id: ConnectionId, sink: Sink) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Outgoing>(OUTBOUND_QUEUE_DEPTH);
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let ok = sink.send_json(item.json).await;
                if let Some(ack) = item.written {
                    let _ = ack.send(());
                }
                if !ok {
                    break;
                }
            }
        });
        Self { id, tx }
    }

    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// Enqueue and wait until the frame has reached the sink.
    ///
    /// For replies, which must not be dropped — a caller is blocked on this
    /// one — and which callers may follow by closing the connection. Waiting
    /// for room alone would not be enough there: the queue would still hold
    /// the frame when the socket closed under it.
    pub async fn send_json(&self, json: String) -> bool {
        let (ack, written) = tokio::sync::oneshot::channel();
        let queued = self
            .tx
            .send(Outgoing {
                json,
                written: Some(ack),
            })
            .await
            .is_ok();
        queued && written.await.is_ok()
    }

    /// Enqueue without waiting. `false` means the connection is gone or has
    /// fallen [`OUTBOUND_QUEUE_DEPTH`] frames behind, and the caller should
    /// stop pushing to it. Used for broadcast, where waiting on one slow
    /// client would stall every other reader of the same session.
    pub fn try_send_json(&self, json: String) -> bool {
        self.tx
            .try_send(Outgoing {
                json,
                written: None,
            })
            .is_ok()
    }
}

/// Serialize `frame` and hand it to `sink`.
///
/// A frame that cannot be serialized is dropped with a warning rather than
/// killing the connection: it is a bug in whatever built it, and taking the
/// session down would hide that behind a disconnect.
pub async fn send_frame<T: serde::Serialize>(client: &Client, frame: &T) -> bool {
    match serde_json::to_string(frame) {
        Ok(json) => client.send_json(json).await,
        Err(e) => {
            tracing::warn!(error = %e, "could not serialize an outgoing frame; dropping it");
            true
        }
    }
}

/// Standard JSON-RPC error codes + daemon extensions.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const SESSION_NOT_FOUND: i32 = -32000;
    pub const SESSION_CAP_REACHED: i32 = -32001;
    pub const ENGINE_ERROR: i32 = -32002;
    /// TCP handshake (`daemon.auth`) missing, malformed, or wrong token.
    pub const UNAUTHORIZED: i32 = -32003;
    /// `session.resume`/`session.fork`: the session's recorded `Meta.scene`
    /// does not match the scene being resumed into. See
    /// `docs/daemon_rpc_protocol.md` §9.1 for the `data` shape this carries.
    pub const SCENE_MISMATCH: i32 = -32006;
    /// `session.create`: the given `project_root` doesn't exist or isn't a
    /// readable directory. See `docs/daemon_rpc_protocol.md` §9.1 for the `data`
    /// shape this carries.
    pub const PROJECT_NOT_FOUND: i32 = -32010;
    /// `session.create`/`scene.activate`: the given scene isn't one this
    /// daemon binary registers at all.
    pub const SCENE_NOT_FOUND: i32 = -32004;
    /// `scene.deactivate`: refused because sessions are still recorded
    /// under this scene. See `docs/daemon_rpc_protocol.md` §9.1 for the `data` shape
    /// this carries.
    pub const SCENE_HAS_ACTIVE_SESSIONS: i32 = -32009;
    /// `session.resume`: the target is a sidechain that already ran its
    /// one-shot task to conclusion — no continuation semantics, re-derive a
    /// new sub-agent instead. See `docs/daemon_rpc_protocol.md` §9.1 for the `data`
    /// shape this carries.
    pub const SIDECHAIN_TERMINAL: i32 = -32014;
    /// Any `plugin.*` management call on a build where the plugin subsystem
    /// is unavailable — compiled out (`--no-default-features`) or switched
    /// off by policy. Distinct from `METHOD_NOT_FOUND`: the method exists and
    /// the request was well-formed; the capability is absent by deployment
    /// choice, which is what a client needs to be able to tell apart.
    pub const PLUGINS_DISABLED: i32 = -32016;

    /// This connection already has [`MAX_IN_FLIGHT_PER_CONNECTION`] requests
    /// being served and will not start another.
    ///
    /// Refused rather than queued, because queuing would mean the connection
    /// stops reading — the exact failure this bound exists to prevent. A client
    /// that sees this has more outstanding calls than it needs and should wait
    /// for some to answer.
    pub const TOO_MANY_IN_FLIGHT: i32 = -32017;

    /// A second `run_turn` arrived while one was already running on that
    /// session. Not queued and not preempting: the caller decides whether to
    /// wait or to `session.interrupt` first. `data.current_turn_id` names the
    /// turn holding the session.
    pub const SESSION_BUSY: i32 = -32015;

    /// The scene requires a project (`AgentScene::requires_project`) but the
    /// caller asked for a session without one. Reported at `session.create`
    /// rather than at the first tool call, so a host can put the user in front
    /// of a project picker instead of a failed turn.
    pub const PROJECT_REQUIRED: i32 = -32012;
}

// ── Session options (extensible, passed via session.run_turn params) ──

/// Optional per-session configuration passed via `session.create` or
/// `session.run_turn` params.
///
/// Taken when the session is created and kept with it: every rebuild of the
/// session's engine — a settings change, an idle eviction, a daemon restart —
/// applies the same options again. Options sent with a later call to an
/// existing session are ignored.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct SessionOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorder: Option<RecorderOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryOptions>,
    /// This session's permission mode. `None` takes the settings'
    /// `permission_mode`. A session may always ask for a stricter mode than
    /// the settings'; a looser one is clamped unless the settings set
    /// `allow_client_permission_override`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<base::interface::settings::PermissionMode>,
    /// Allow/deny/ask rules for the `RuleSetPermission` this session's
    /// `permission_mode` (if set) constructs. Ignored when `permission_mode`
    /// is `None`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<base::permission::PermissionRule>,
    /// JSON Schema the session's structured output must satisfy — see
    /// `runtime::agent::Builder::output_schema`.
    pub output_schema: Option<serde_json::Value>,
}

/// Record or replay this session's LLM calls.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RecorderOptions {
    pub mode: String, // "record" | "replay" | "rerun"
    /// Directory name for the recording under `dir`. Omitted, the session id
    /// is used — which is what production recording wants, and what a replay
    /// must therefore override with the name of the recording it wants back.
    #[serde(default)]
    pub name: Option<String>,
    /// Absolute path to the recordings root. Omitted, the daemon's own root is
    /// used — `settings.recorder.root` if configured, else
    /// `ConfigPaths::global_recordings_dir()`. A caller that just wants
    /// recording on no longer has to know where recordings live.
    #[serde(default)]
    pub dir: Option<String>,
    /// On replay, whether a live request that differs from the recorded one
    /// fails the call (`true`) or only warns (`false`).
    #[serde(default)]
    pub strict: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TelemetryOptions {
    pub output: String, // absolute path to telemetry output file
}

/// Streaming frame sent during long-running operations (e.g. session.run_turn).
/// Matches stream-json format.
#[derive(Debug, Clone, Serialize)]
pub struct StreamFrame {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: Value,
}

impl StreamFrame {
    pub fn event(session_id: &str, turn_id: &str, event: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method: "session.event",
            params: serde_json::json!({"session_id": session_id, "turn_id": turn_id, "event": event}),
        }
    }

    /// A daemon-level async notification (MCP connect outcome, import
    /// auto-detection, ...) pushed to any connection subscribed via
    /// `daemon.subscribeEvents` — not tied to any particular session/turn.
    pub fn daemon_event(event: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method: "daemon.event",
            params: event,
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
    fn stream_frame_format() {
        let f = StreamFrame::event(
            "sess-1",
            "turn-1",
            serde_json::json!({"kind": "text", "data": "hi"}),
        );
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains(r#""method":"session.event""#));
        assert!(s.contains(r#""session_id":"sess-1""#));
        assert!(s.contains(r#""turn_id":"turn-1""#));
    }

    #[test]
    fn rpc_error_code_constants_match_jsonrpc_spec() {
        assert_eq!(codes::PARSE_ERROR, -32700);
        assert_eq!(codes::INVALID_REQUEST, -32600);
        assert_eq!(codes::METHOD_NOT_FOUND, -32601);
        assert_eq!(codes::INVALID_PARAMS, -32602);
        assert_eq!(codes::INTERNAL_ERROR, -32603);
    }
}
