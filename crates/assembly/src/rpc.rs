//! JSON-RPC 2.0 wire types + error codes.
//!
//! One JSON object per line (newline-delimited) — trivially readable
//! with `tail -f` for debugging.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub mod events;

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

/// Somewhere to put one outgoing JSON-RPC frame.
///
/// The transports disagree about framing — a byte stream needs a delimiter
/// and an explicit flush, a WebSocket connection is already message-framed —
/// and nothing above this line should have to know which one it is talking
/// to.
///
/// Responses and streamed events both go through this one implementation, so
/// both flush after every frame; a streamed token never waits in a buffer.
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
/// waiting, and the connection is told so — see [`Client::try_send_for`].
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
    enqueued: std::sync::Arc<std::sync::atomic::AtomicU64>,
    dropped: std::sync::Arc<std::sync::Mutex<Vec<DroppedSubscription>>>,
    daemon_events: std::sync::atomic::AtomicBool,
}

/// A session this connection was unsubscribed from because its queue was
/// full, and how many frames had been queued when that happened: the notice
/// goes out once those are written, so it arrives after everything the old
/// subscription delivered and before anything a new one will.
struct DroppedSubscription {
    session_id: String,
    after: u64,
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
        let dropped: std::sync::Arc<std::sync::Mutex<Vec<DroppedSubscription>>> =
            std::sync::Arc::default();
        let pending = dropped.clone();
        tokio::spawn(async move {
            let mut written: u64 = 0;
            while let Some(item) = rx.recv().await {
                let ok = sink.send_json(item.json).await;
                if let Some(ack) = item.written {
                    let _ = ack.send(());
                }
                if !ok {
                    break;
                }
                written += 1;
                let due: Vec<String> = {
                    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
                    let (due, later): (Vec<_>, Vec<_>) =
                        pending.drain(..).partition(|d| d.after <= written);
                    *pending = later;
                    due.into_iter().map(|d| d.session_id).collect()
                };
                for session_id in due {
                    let notice =
                        events::SessionEventNotification::subscription_dropped(&session_id);
                    let Ok(json) = serde_json::to_string(&notice) else {
                        continue;
                    };
                    if !sink.send_json(json).await {
                        return;
                    }
                }
            }
        });
        Self {
            id,
            tx,
            enqueued: std::sync::Arc::default(),
            dropped,
            daemon_events: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// Mark this connection as receiving `daemon.event`; `false` if it
    /// already was.
    pub fn start_daemon_events(&self) -> bool {
        !self
            .daemon_events
            .swap(true, std::sync::atomic::Ordering::AcqRel)
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
        if queued {
            self.enqueued
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        queued && written.await.is_ok()
    }

    /// Enqueue without waiting. `false` means the connection is gone or has
    /// fallen [`OUTBOUND_QUEUE_DEPTH`] frames behind, and the caller should
    /// stop pushing to it. Used for broadcast, where waiting on one slow
    /// client would stall every other reader of the same session.
    pub fn try_send_json(&self, json: String) -> bool {
        let queued = self
            .tx
            .try_send(Outgoing {
                json,
                written: None,
            })
            .is_ok();
        if queued {
            self.enqueued
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        queued
    }

    /// [`Self::try_send_json`] for a frame of `session_id`'s broadcast. When
    /// the queue is full the caller drops this connection's subscription, and
    /// this arranges for the connection to be told — a `subscription_dropped`
    /// event, written once everything already queued has gone out — so it can
    /// subscribe again and catch up rather than miss frames without knowing.
    pub fn try_send_for(&self, session_id: &str, json: String) -> bool {
        match self.tx.try_send(Outgoing {
            json,
            written: None,
        }) {
            Ok(()) => {
                self.enqueued
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let after = self.enqueued.load(std::sync::atomic::Ordering::SeqCst);
                self.dropped
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(DroppedSubscription {
                        session_id: session_id.to_string(),
                        after,
                    });
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
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

    /// The scene requires a project (`Scene::requires_project`) but the
    /// caller asked for a session without one. Reported at `session.create`
    /// rather than at the first tool call, so a host can put the user in front
    /// of a project picker instead of a failed turn.
    pub const PROJECT_REQUIRED: i32 = -32012;

    /// `session.taskOutput`: the session's engine knows no such task — it
    /// never ran, ended long enough ago to be forgotten, or the engine is not
    /// in memory.
    pub const TASK_NOT_FOUND: i32 = -32018;

    /// `session.startAgent`: the session already runs as many sub-agents as
    /// `execution.max_concurrent_subagents` allows, or is as deep as
    /// delegation goes. Refused rather than queued, as the model's own
    /// `Agent` call is.
    pub const TASK_LIMIT: i32 = -32019;

    /// `mcp.addServer`/`mcp.updateServer`: the MCP policy denies this server,
    /// so nothing was written. `message` names the policy and the rule.
    pub const MCP_SERVER_DENIED: i32 = -32020;

    /// `mcp.auth.start` / `mcp.auth.complete` / `mcp.auth.logout`: the
    /// authorization server, or the sign-in itself, failed — discovery,
    /// registration, a refused or mismatched callback, the code exchange.
    /// `message` is meant for the user.
    pub const MCP_AUTH_FAILED: i32 = -32021;

    /// `mcp.addServer`/`mcp.updateServer` into a project that is not trusted:
    /// the server would never start, so nothing was written.
    pub const PROJECT_UNTRUSTED: i32 = -32022;
}

/// `session.startAgent`'s params.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StartAgentParams {
    pub session_id: String,
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub subagent_type: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Refused when given: a sub-agent runs on its parent's provider.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub permission_rules: Vec<base::permission::PermissionRule>,
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default = "notify_parent_by_default")]
    pub notify_parent: bool,
    #[serde(default)]
    pub tag: Option<String>,
}

fn notify_parent_by_default() -> bool {
    true
}

impl StartAgentParams {
    pub fn into_request(self) -> base::interface::agent_spawner::AgentRequest {
        base::interface::agent_spawner::AgentRequest {
            prompt: self.prompt,
            description: self.description,
            subagent_type: self.subagent_type,
            model: self.model,
            allowed_tools: self.allowed_tools,
            disallowed_tools: self.disallowed_tools,
            permission_rules: self.permission_rules,
            output_schema: self.output_schema,
            notify_parent: self.notify_parent,
            tag: self.tag,
        }
    }
}

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
    /// Where this session's kept time starts (the date in its prompt, its log's timestamps).
    /// Omitted: the machine's clock — except in replay, where it starts when the recording did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryOptions>,
    /// This session's permission mode. `None` takes the settings'
    /// `permission_mode`. A session may always ask for a stricter mode than
    /// the settings'; a looser one is clamped unless the settings set
    /// `allow_client_permission_override`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<base::settings::PermissionMode>,
    /// Allow/deny/ask rules for the `RuleSetPermission` this session's
    /// `permission_mode` (if set) constructs. Ignored when `permission_mode`
    /// is `None`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<base::permission::PermissionRule>,
    /// JSON Schema the session's structured output must satisfy — see
    /// `runtime::agent::Builder::output_schema`.
    pub output_schema: Option<serde_json::Value>,
    /// The provider this session's conversation goes to, by its id in
    /// `providers`. `None` follows the settings' routing. Sub-agents,
    /// compaction and the other routed tasks keep the settings' routing
    /// either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The conversation's model. `None` takes the provider's `default_model`
    /// when `provider` is given, the settings' model otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A system-prompt append / override for this session's own agent, over
    /// the settings files'. Its delegates do not get it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<runtime::agent::SessionPrompt>,
    /// The kinds of `prompt` frame this session's host answers: `permission`
    /// always, `question` when listed. Names this daemon does not know are
    /// ignored rather than voiding the options.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_types: Vec<String>,
}

impl SessionOptions {
    /// Every field `options` may carry.
    pub const FIELDS: &'static [&'static str] = &[
        "recorder",
        "clock",
        "telemetry",
        "permission_mode",
        "permission_rules",
        "output_schema",
        "provider",
        "model",
        "prompt",
        "prompt_types",
    ];

    /// The fields whose value is a list. Each entry is read on its own, so
    /// one bad entry is left out without the rest — a bad allow rule must not
    /// take the deny rules beside it along.
    pub const LIST_FIELDS: &'static [&'static str] = &["permission_rules", "prompt_types"];

    /// Not a session option but an instruction about reading them: with it,
    /// what [`Self::from_params`] would leave out makes the call fail.
    pub const STRICT: &'static str = "strict";

    /// `params.options` as far as it can be read, and what in it could not
    /// be: a field this daemon does not know, one whose value does not
    /// parse, or one entry of a list field. Each field is read on its own, so
    /// one bad field leaves the rest in force.
    pub fn from_params(params: &Value) -> (Option<Self>, Vec<IgnoredOption>) {
        let fields = match params.get("options") {
            None | Some(Value::Null) => return (None, Vec::new()),
            Some(Value::Object(fields)) => fields,
            Some(_) => {
                return (
                    None,
                    vec![IgnoredOption {
                        field: None,
                        error: "options must be an object".to_string(),
                    }],
                )
            }
        };
        let mut ignored = Vec::new();
        let mut readable = serde_json::Map::new();
        for (name, value) in fields {
            if name == Self::STRICT {
                if !value.is_boolean() {
                    ignored.push(IgnoredOption {
                        field: Some(name.clone()),
                        error: "must be a boolean".to_string(),
                    });
                }
                continue;
            }
            if !Self::FIELDS.contains(&name.as_str()) {
                ignored.push(IgnoredOption {
                    field: Some(name.clone()),
                    error: "unknown field".to_string(),
                });
                continue;
            }
            let value = match value {
                Value::Array(entries) if Self::LIST_FIELDS.contains(&name.as_str()) => {
                    Value::Array(Self::readable_entries(name, entries, &mut ignored))
                }
                _ => value.clone(),
            };
            match Self::read_alone(name, &value) {
                Ok(()) => {
                    readable.insert(name.clone(), value);
                }
                Err(error) => ignored.push(IgnoredOption {
                    field: Some(name.clone()),
                    error,
                }),
            }
        }
        if readable.is_empty() {
            return (None, ignored);
        }
        match serde_json::from_value(Value::Object(readable)) {
            Ok(options) => (Some(options), ignored),
            Err(e) => {
                ignored.push(IgnoredOption {
                    field: None,
                    error: e.to_string(),
                });
                (None, ignored)
            }
        }
    }

    /// Whether `params.options` asks for strict reading. A `strict` that is
    /// not a boolean counts as asking: it was meant as something.
    pub fn strict_requested(params: &Value) -> bool {
        params
            .get("options")
            .and_then(|o| o.get(Self::STRICT))
            .is_some_and(|v| v != &Value::Bool(false))
    }

    fn read_alone(name: &str, value: &Value) -> Result<(), String> {
        let alone = Value::Object([(name.to_string(), value.clone())].into_iter().collect());
        serde_json::from_value::<Self>(alone)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn readable_entries(
        name: &str,
        entries: &[Value],
        ignored: &mut Vec<IgnoredOption>,
    ) -> Vec<Value> {
        let mut kept = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            match Self::read_alone(name, &Value::Array(vec![entry.clone()])) {
                Ok(()) => kept.push(entry.clone()),
                Err(error) => ignored.push(IgnoredOption {
                    field: Some(format!("{name}[{index}]")),
                    error,
                }),
            }
        }
        kept
    }

    /// Whether the host answers the model's questions (`AskUserQuestion`).
    pub fn answers_questions(&self) -> bool {
        self.prompt_types.iter().any(|t| t == PROMPT_TYPE_QUESTION)
    }
}

/// A field of `options` the daemon left out, as the response's
/// `ignored_options` reports it. `field` is `None` when `options` as a whole
/// could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IgnoredOption {
    pub field: Option<String>,
    pub error: String,
}

/// `prompt_type` of a `prompt` frame that asks whether a tool call may run.
pub const PROMPT_TYPE_PERMISSION: &str = "permission";
/// `prompt_type` of a `prompt` frame that carries the model's question.
pub const PROMPT_TYPE_QUESTION: &str = "question";

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
    /// `ConfigPaths::global_recordings_dir()`.
    #[serde(default)]
    pub dir: Option<String>,
    /// On replay, whether a live request that differs from the recorded one
    /// fails the call (`true`) or only warns (`false`).
    #[serde(default)]
    pub strict: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ClockOptions {
    /// RFC 3339, e.g. `2026-10-28T09:00:00Z`. The clock reads this when the session is built
    /// and runs on at wall-clock speed.
    pub start: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TelemetryOptions {
    pub output: String, // absolute path to telemetry output file
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

    /// A sink that writes nothing until the test opens the gate, so the
    /// queue in front of it can be filled.
    struct GatedSink {
        open: tokio::sync::Semaphore,
        written: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl FrameSink for GatedSink {
        async fn send_json(&self, json: String) -> bool {
            self.open.acquire().await.unwrap().forget();
            self.written.lock().unwrap().push(json);
            true
        }
    }

    #[tokio::test]
    async fn a_dropped_subscription_is_told_after_what_was_already_queued() {
        let sink = Arc::new(GatedSink {
            open: tokio::sync::Semaphore::new(0),
            written: std::sync::Mutex::new(Vec::new()),
        });
        let client = Client::new(ConnectionId(1), sink.clone());

        let mut queued = 0;
        while client.try_send_for("s1", format!("{{\"n\":{queued}}}")) {
            queued += 1;
        }
        assert!(queued >= OUTBOUND_QUEUE_DEPTH, "the queue filled first");

        sink.open.add_permits(queued + 1);
        for _ in 0..200 {
            if sink.written.lock().unwrap().len() == queued + 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let written = sink.written.lock().unwrap().clone();
        assert_eq!(
            written.len(),
            queued + 1,
            "every queued frame, then the notice"
        );
        let notice: Value = serde_json::from_str(written.last().unwrap()).unwrap();
        assert_eq!(notice["method"], "session.event");
        assert_eq!(notice["params"]["session_id"], "s1");
        assert_eq!(notice["params"]["event"]["kind"], "subscription_dropped");
    }

    #[test]
    fn the_listed_option_fields_are_exactly_the_struct_s() {
        let every_field = SessionOptions {
            recorder: Some(serde_json::from_value(serde_json::json!({"mode": "record"})).unwrap()),
            clock: Some(ClockOptions {
                start: "2026-10-28T09:00:00Z".into(),
            }),
            telemetry: Some(TelemetryOptions {
                output: "/tmp/t.jsonl".into(),
            }),
            permission_mode: Some(base::settings::PermissionMode::Plan),
            permission_rules: vec![serde_json::from_value(serde_json::json!({
                "source": "session", "behavior": "allow", "tool_name": "Read"
            }))
            .unwrap()],
            output_schema: Some(serde_json::json!({})),
            provider: Some("p".into()),
            model: Some("m".into()),
            prompt: Some(Default::default()),
            prompt_types: vec![PROMPT_TYPE_QUESTION.into()],
        };
        let serialized = serde_json::to_value(&every_field).unwrap();
        let mut names: Vec<&str> = serialized
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut listed = SessionOptions::FIELDS.to_vec();
        names.sort_unstable();
        listed.sort_unstable();
        assert_eq!(names, listed);

        let mut lists: Vec<&str> = serialized
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| v.is_array())
            .map(|(k, _)| k.as_str())
            .collect();
        let mut listed = SessionOptions::LIST_FIELDS.to_vec();
        lists.sort_unstable();
        listed.sort_unstable();
        assert_eq!(lists, listed, "a list field is not read entry by entry");
    }

    #[test]
    fn a_bad_entry_of_a_list_field_is_left_out_alone() {
        let params = serde_json::json!({"options": {
            "permission_rules": [
                {"source": "session", "behavior": "allwo", "tool_name": "Read"},
                {"source": "session", "behavior": "deny", "tool_name": "Bash"},
            ],
            "prompt_types": [7, "question"],
        }});
        let (options, ignored) = SessionOptions::from_params(&params);
        let options = options.unwrap();
        assert_eq!(options.permission_rules.len(), 1);
        assert_eq!(options.permission_rules[0].tool_name, "Bash");
        assert!(options.answers_questions());
        let mut fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        fields.sort();
        assert_eq!(
            fields,
            [Some("permission_rules[0]"), Some("prompt_types[0]")]
        );

        let (_, ignored) =
            SessionOptions::from_params(&serde_json::json!({"options": {"permission_rules": {}}}));
        assert_eq!(ignored[0].field.as_deref(), Some("permission_rules"));
    }

    #[test]
    fn strict_is_read_but_not_kept_as_an_option() {
        let on = serde_json::json!({"options": {"strict": true, "model": "m"}});
        let (options, ignored) = SessionOptions::from_params(&on);
        assert!(ignored.is_empty(), "{ignored:?}");
        assert_eq!(options.unwrap().model.as_deref(), Some("m"));
        assert!(SessionOptions::strict_requested(&on));

        let off = serde_json::json!({"options": {"strict": false}});
        assert!(!SessionOptions::strict_requested(&off));
        assert!(!SessionOptions::strict_requested(&serde_json::json!({})));

        let odd = serde_json::json!({"options": {"strict": "yes"}});
        assert!(SessionOptions::strict_requested(&odd));
        let (_, ignored) = SessionOptions::from_params(&odd);
        assert_eq!(ignored[0].field.as_deref(), Some("strict"));
    }

    #[test]
    fn a_bad_option_field_is_reported_and_the_rest_still_apply() {
        let params = serde_json::json!({"options": {
            "permission_mode": "plann",
            "model": "m",
            "permision_rules": [],
        }});
        let (options, mut ignored) = SessionOptions::from_params(&params);
        assert_eq!(options.unwrap().model.as_deref(), Some("m"));
        ignored.sort_by(|a, b| a.field.cmp(&b.field));
        let fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        assert_eq!(fields, [Some("permision_rules"), Some("permission_mode")]);
        assert_eq!(ignored[0].error, "unknown field");
        assert!(ignored[1].error.contains("plann"), "{:?}", ignored[1]);

        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({}));
        assert!(options.is_none() && ignored.is_empty());
        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({"options": 3}));
        assert!(options.is_none());
        assert_eq!(ignored[0].field, None);
    }

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

    #[test]
    fn rpc_error_code_constants_match_jsonrpc_spec() {
        assert_eq!(codes::PARSE_ERROR, -32700);
        assert_eq!(codes::INVALID_REQUEST, -32600);
        assert_eq!(codes::METHOD_NOT_FOUND, -32601);
        assert_eq!(codes::INVALID_PARAMS, -32602);
        assert_eq!(codes::INTERNAL_ERROR, -32603);
    }
}
