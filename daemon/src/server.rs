//! Daemon server — JSON-RPC over Unix socket / TCP.
//!
//! Accepts newline-delimited JSON-RPC 2.0 requests, dispatches them
//! to the agent engine, and streams events back as `StreamFrame` lines.
//!
//! The framing is the only part that belongs to a transport. `dispatch` takes
//! a request and a [`Sink`], so `crate::ws` carries the same protocol over
//! WebSocket without reimplementing any of it — and, more to the point,
//! without being able to drift from the auth handshake this module defines.
//!
//! **Trust boundary**: every RPC method here — including `config.setProvider`
//! (writes `providers.<id>.api_key`/`base_url` into settings.json) — trusts
//! whoever can reach this socket. There is no per-method authorization on
//! top of that: Unix socket
//! file permissions are the entire access-control story locally. For TCP,
//! each connection must open with a `daemon.auth` handshake carrying the
//! shared token (`--token`/`AGIX_DAEMON_TOKEN`, see
//! [`DaemonServer::authenticate_tcp`]) before any other method is dispatched
//! — once a connection passes the handshake it is trusted for its lifetime,
//! same as a Unix socket connection. In particular, `config.setProvider` lets
//! any authenticated caller point future LLM traffic at an
//! attacker-controlled `base_url`/`api_key` — treat socket/token exposure
//! with the same care as exposing the LLM credentials themselves.
//!
//! What that boundary does *not* cover any more is tool execution.
//! `session.run_turn` used to run every session under an unconditional
//! allow-all `Permission` ("IDE plugins manage their own sandbox"); it now
//! defaults to `ask` and raises `session.event{kind:"prompt"}` frames that
//! `session.respondToPrompt` answers. See `daemon/src/main.rs`'s
//! `AllowAllPermission` doc comment for why the boundary moved, and
//! `docs/daemon_rpc_protocol.md` for the opt-out.

use crate::rpc::{codes, RpcRequest, RpcResponse};
use crate::rpc::{send_frame, Client, FrameSink, Sink};
use crate::session_pool::SessionPool;
use base::id::Id;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

type Reader = Box<dyn AsyncRead + Send + Unpin + 'static>;

/// A resume that could not happen, as the wire sees it.
///
/// Shared by `session.resume` and by `session.subscribe {resume: true}`:
/// opening a session that is only on disk fails for the same reasons however
/// the caller asked, and a second copy of this mapping would be a second set
/// of error codes to keep in step.
fn resume_failure(id: serde_json::Value, e: crate::session_pool::ResumeError) -> RpcResponse {
    match e {
        crate::session_pool::ResumeError::SidechainTerminal {
            session_id,
            parent_session_id,
            final_state,
        } => {
            let final_state = match final_state {
                history::entry::SessionEndState::Completed => "completed",
                history::entry::SessionEndState::Failed => "failed",
            };
            RpcResponse::err_with_data(
                id,
                codes::SIDECHAIN_TERMINAL,
                "sidechain session already ran to completion",
                serde_json::json!({
                    "session_id": session_id,
                    "parent_session_id": parent_session_id,
                    "final_state": final_state,
                }),
            )
        }
        crate::session_pool::ResumeError::Rpc((code, message)) => {
            RpcResponse::err(id, code, message)
        }
    }
}

/// How many requests one connection may have in flight at once.
///
/// A bound, not a queue: at the limit further requests are refused with
/// [`codes::TOO_MANY_IN_FLIGHT`] rather than made to wait, because waiting
/// would have to happen in the read loop and a read loop that stops reading
/// is precisely what this whole arrangement exists to avoid.
///
/// Generous next to any real client — a browser tab has a handful of calls
/// outstanding, and sessions are capped well below this.
pub(crate) const MAX_IN_FLIGHT_PER_CONNECTION: usize = 64;
type ByteWriter = Arc<AsyncMutex<Box<dyn AsyncWrite + Send + Unpin + 'static>>>;

/// Largest request a client may send on one connection.
///
/// Generous — a turn's worth of context or a pasted file is legitimately
/// large — but finite, which is the point: without a cap a peer that never
/// sends a newline makes the daemon buffer until it dies, and on the
/// WebSocket transport that peer can be any page the user's browser has open.
pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Newline-delimited frames, with the cap enforced as the bytes arrive.
///
/// `tokio::io::Lines` would be the obvious choice, but it grows its buffer
/// until it finds a newline, so the limit has to live where the reading
/// happens rather than in a check after the fact.
struct FrameReader {
    inner: BufReader<Reader>,
    buf: Vec<u8>,
}

impl FrameReader {
    fn new(reader: Reader) -> Self {
        Self {
            inner: BufReader::new(reader),
            buf: Vec::new(),
        }
    }

    /// The next frame, or `None` at end of stream, on an I/O error, or when
    /// the peer runs past [`MAX_FRAME_BYTES`].
    ///
    /// Over-long frames end the connection rather than being skipped: the
    /// daemon has no idea where the frame boundary is anymore, so the honest
    /// options are close it or keep buffering, and the second one is the
    /// problem being fixed.
    async fn next_frame(&mut self) -> Option<String> {
        self.buf.clear();
        loop {
            let (complete, consumed) = {
                let available = match self.inner.fill_buf().await {
                    Ok(b) => b,
                    Err(_) => return None,
                };
                if available.is_empty() {
                    return (!self.buf.is_empty())
                        .then(|| String::from_utf8_lossy(&self.buf).into_owned());
                }
                match available.iter().position(|&b| b == b'\n') {
                    Some(i) => {
                        self.buf.extend_from_slice(&available[..i]);
                        (true, i + 1)
                    }
                    None => {
                        self.buf.extend_from_slice(available);
                        (false, available.len())
                    }
                }
            };
            self.inner.consume(consumed);

            if self.buf.len() > MAX_FRAME_BYTES {
                warn!(
                    limit = MAX_FRAME_BYTES,
                    "closing a connection that sent a frame over the size limit"
                );
                return None;
            }
            if complete {
                return Some(String::from_utf8_lossy(&self.buf).into_owned());
            }
        }
    }
}

/// Newline-delimited JSON over a byte stream — Unix socket and TCP.
struct LineSink(ByteWriter);

#[async_trait::async_trait]
impl FrameSink for LineSink {
    async fn send_json(&self, json: String) -> bool {
        let mut buf = json.into_bytes();
        buf.push(b'\n');
        let mut w = self.0.lock().await;
        if w.write_all(&buf).await.is_err() {
            return false;
        }
        // Every frame, on every path. A response the caller is blocked on and
        // a streamed token are equally useless sitting in a buffer.
        w.flush().await.is_ok()
    }
}

/// Fixed-time byte comparison — avoids leaking the TCP token's length-matched
/// prefix via response-time side channels. `a`/`b` differing in length is not
/// itself timing-sensitive (tokens are a fixed shared secret, not attacker-
/// influenced length), so a cheap length check up front is fine.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub struct DaemonServer {
    pool: Arc<SessionPool>,
    /// Source of connection ids. Monotonic for the process lifetime — an id
    /// is never reused, so a subscription left behind by a closed connection
    /// can never be inherited by a later one.
    next_connection: std::sync::atomic::AtomicU64,
    started_at: Instant,
    shutdown_token: CancellationToken,
    tcp_token: tokio::sync::RwLock<Option<String>>,
}

impl DaemonServer {
    pub fn new(pool: Arc<SessionPool>, shutdown_token: CancellationToken) -> Self {
        Self {
            pool,
            next_connection: std::sync::atomic::AtomicU64::new(1),
            started_at: Instant::now(),
            shutdown_token,
            tcp_token: tokio::sync::RwLock::new(None),
        }
    }

    /// Wrap a freshly accepted connection: a new id, and the queue in front
    /// of its sink. Public so a transport in another module can mint one.
    pub fn accept_connection(&self, sink: Sink) -> Arc<Client> {
        let id = crate::rpc::ConnectionId(
            self.next_connection
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        Arc::new(Client::new(id, sink))
    }

    pub async fn set_tcp_token(&self, token: String) {
        *self.tcp_token.write().await = Some(token);
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }

    pub async fn serve_tcp(self: Arc<Self>, addr: SocketAddr) -> anyhow::Result<()> {
        if self.tcp_token.read().await.is_none() {
            anyhow::bail!("TCP requires token");
        }
        let listener = TcpListener::bind(addr).await?;
        info!(addr=%addr, "TCP listener bound");
        self.serve_tcp_listener(listener).await
    }

    /// Accept loop over an already-bound TCP listener — split out from
    /// [`Self::serve_tcp`] so tests can bind an ephemeral port (`127.0.0.1:0`),
    /// read the OS-assigned port via `listener.local_addr()`, and drive this
    /// loop directly instead of guessing a fixed port.
    ///
    /// Re-checks (doesn't just trust `serve_tcp`'s check) that a token is
    /// configured — this is a `pub` method, so a caller could reach it
    /// without going through `serve_tcp` at all (e.g. binding the listener
    /// itself for socket-activation setups) and forgetting `set_tcp_token`
    /// first. Without this, that mistake wouldn't fail loudly — every
    /// connection would just silently get rejected by `authenticate_tcp`
    /// forever, which is safe but confusing to debug.
    pub async fn serve_tcp_listener(self: Arc<Self>, listener: TcpListener) -> anyhow::Result<()> {
        if self.tcp_token.read().await.is_none() {
            anyhow::bail!("TCP requires token");
        }
        loop {
            tokio::select! {
                _ = self.shutdown_token.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer)) => {
                            debug!(peer=%peer);
                            let this = self.clone();
                            tokio::spawn(async move {
                                let (r, w) = stream.into_split();
                                let writer: ByteWriter = Arc::new(AsyncMutex::new(Box::new(w)));
                                let sink: Sink = Arc::new(LineSink(writer));
                                if let Err(e) = this.handle_connection(Box::new(r), sink, true).await {
                                    warn!(error=%e);
                                }
                            });
                        }
                        Err(e) => warn!(error=%e),
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn serve_unix(self: Arc<Self>, socket_path: &Path) -> anyhow::Result<()> {
        let listener = Self::bind_unix(socket_path)?;
        self.serve_unix_listener(listener).await
    }

    /// Bind the unix socket, replacing a stale one. Split from
    /// [`Self::serve_unix_listener`] so a caller can bind every listener before
    /// announcing the daemon — a client that finds it must be able to connect.
    pub fn bind_unix(socket_path: &Path) -> anyhow::Result<UnixListener> {
        if socket_path.exists() {
            std::fs::remove_file(socket_path)?;
        }
        let listener = UnixListener::bind(socket_path)?;
        info!(path=%socket_path.display());
        Ok(listener)
    }

    /// Accept loop over an already-bound unix listener.
    pub async fn serve_unix_listener(
        self: Arc<Self>,
        listener: UnixListener,
    ) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                _ = self.shutdown_token.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let this = self.clone();
                            tokio::spawn(async move {
                                let (r, w) = stream.into_split();
                                let writer: ByteWriter = Arc::new(AsyncMutex::new(Box::new(w)));
                                let sink: Sink = Arc::new(LineSink(writer));
                                if let Err(e) = this.handle_connection(Box::new(r), sink, false).await {
                                    warn!(error=%e);
                                }
                            });
                        }
                        Err(e) => warn!(error=%e),
                    }
                }
            }
        }
        Ok(())
    }

    /// Read requests and serve them.
    ///
    /// **Each request is served in its own task, and the read loop never
    /// waits for one.** `session.run_turn` runs for as long as the turn does;
    /// serving it inline meant the connection read nothing else meanwhile, so
    /// a client could not answer a permission prompt or interrupt the turn on
    /// the connection that was running it — the prompt deadlocked until its
    /// timeout denied it, and `session.interrupt` was unreachable from the
    /// one place that most needs it. §5.2 of the protocol has always
    /// promised concurrent in-flight requests; this is what delivers it.
    ///
    /// Responses may therefore come back out of order, which is what §5.2
    /// says and why clients match on `id`.
    async fn handle_connection(
        self: Arc<Self>,
        reader: Reader,
        sink: Sink,
        tcp: bool,
    ) -> anyhow::Result<()> {
        let mut lines = FrameReader::new(reader);
        let client = self.accept_connection(sink);
        let in_flight = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_PER_CONNECTION));

        // Unix socket connections are trusted for their lifetime by file
        // permissions alone (see the module doc comment's trust-boundary
        // note) — no handshake needed. TCP connections must authenticate
        // once, up front, before any RPC method is dispatched. This one stays
        // serial on purpose: nothing may be dispatched before it passes.
        if tcp && !self.authenticate_lines(&mut lines, &client).await {
            return Ok(());
        }

        while let Some(line) = lines.next_frame().await {
            let req: RpcRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => continue,
            };
            self.clone().serve_request(req, &client, &in_flight);
        }
        // The connection is what subscriptions belong to; sessions and their
        // turns are not. Dropping it takes its subscriptions and nothing else.
        self.pool.drop_connection(client.id()).await;
        Ok(())
    }

    /// Dispatch one request on its own task, or refuse it if this connection
    /// is already at its in-flight bound.
    ///
    /// Shared by every transport, so none of them can accidentally go back to
    /// serving requests inline.
    pub(crate) fn serve_request(
        self: Arc<Self>,
        req: RpcRequest,
        client: &Arc<Client>,
        in_flight: &Arc<tokio::sync::Semaphore>,
    ) {
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);
        let client = client.clone();
        let Ok(permit) = in_flight.clone().try_acquire_owned() else {
            let refusal = RpcResponse::err(
                id,
                codes::TOO_MANY_IN_FLIGHT,
                format!(
                    "this connection already has {MAX_IN_FLIGHT_PER_CONNECTION} requests in flight"
                ),
            );
            tokio::spawn(async move {
                send_frame(&client, &refusal).await;
            });
            return;
        };
        tokio::spawn(async move {
            let resp = self.dispatch(req, client.clone()).await;
            send_frame(&client, &resp).await;
            drop(permit);
        });
    }

    /// TCP-only handshake: the first line on the connection must be a
    /// `daemon.auth` request carrying `params.token` equal to the token
    /// configured via `--token`/`AGIX_DAEMON_TOKEN`. Writes back an ok/err
    /// response either way. Returns `true` if the connection may proceed to
    /// the normal dispatch loop, `false` if it was rejected (or closed before
    /// sending anything) and the caller should just drop it.
    async fn authenticate_lines(&self, lines: &mut FrameReader, client: &Client) -> bool {
        let Some(line) = lines.next_frame().await else {
            return false;
        };
        let response = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(req) => self.check_auth(&req).await,
            Err(_) => Err(RpcResponse::err(
                serde_json::Value::Null,
                codes::UNAUTHORIZED,
                "first message must be a valid daemon.auth request",
            )),
        };
        let ok = response.is_ok();
        send_frame(client, &response.unwrap_or_else(|e| e)).await;
        ok
    }

    /// Verify one `daemon.auth` request.
    ///
    /// Shared by every transport that needs a handshake, so a new one cannot
    /// accidentally accept a token the others would reject. `Ok` carries the
    /// success response to send; `Err` carries the refusal — both go back to
    /// the client, which is why the caller sends either way.
    ///
    /// `result_large_err` is about an error type that costs every caller on the
    /// happy path. Here both variants are the same type — the value is a
    /// response either way — so boxing one of them would cost an allocation to
    /// save nothing.
    #[allow(clippy::result_large_err)]
    pub(crate) async fn check_auth(&self, req: &RpcRequest) -> Result<RpcResponse, RpcResponse> {
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);
        if req.method != "daemon.auth" {
            return Err(RpcResponse::err(
                id,
                codes::UNAUTHORIZED,
                "must authenticate via daemon.auth first",
            ));
        }
        let provided = req
            .params
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let expected = self.tcp_token.read().await;
        let authenticated = expected
            .as_deref()
            .is_some_and(|t| constant_time_eq(t.as_bytes(), provided.as_bytes()));
        drop(expected);

        if authenticated {
            let mut result = crate::discovery::identity();
            result["authenticated"] = serde_json::json!(true);
            Ok(RpcResponse::ok(id, result))
        } else {
            Err(RpcResponse::err(id, codes::UNAUTHORIZED, "invalid token"))
        }
    }

    /// Shared `SCENE_MISMATCH` guard for the session-scoped RPCs that only
    /// need a pass/fail answer (unlike `session.resume`, which also reports
    /// `scene_inferred` on success and so keeps its own inline check) —
    /// `session.close`/`.delete`/`.fork` all reject outright rather than
    /// operating across a scene boundary they weren't asked to cross.
    /// Returns the ready-made error response when the caller should stop,
    /// `None` when it's clear to proceed.
    async fn scene_mismatch_response(
        &self,
        id: &serde_json::Value,
        session_id: &str,
    ) -> Option<RpcResponse> {
        if let crate::session_pool::SceneCheck::Mismatch(recorded_scene) =
            self.pool.check_scene(session_id).await
        {
            return Some(RpcResponse::err_with_data(
                id.clone(),
                codes::SCENE_MISMATCH,
                "scene mismatch",
                serde_json::json!({
                    "session_id": session_id,
                    "recorded_scene": recorded_scene,
                    "requested_scene": self.pool.scene_id(),
                }),
            ));
        }
        None
    }

    /// Dispatch one request. Public so a transport in another module can
    /// reuse it — the whole point of a transport is framing, not semantics.
    pub async fn dispatch_public(&self, req: RpcRequest, client: Arc<Client>) -> RpcResponse {
        self.dispatch(req, client).await
    }

    /// [`Self::check_auth`], for the same reason — including the lint.
    #[allow(clippy::result_large_err)]
    pub async fn check_auth_public(&self, req: &RpcRequest) -> Result<RpcResponse, RpcResponse> {
        self.check_auth(req).await
    }

    /// Forget a connection's subscriptions. Public for the same reason as
    /// [`Self::accept_connection`].
    pub async fn drop_connection(&self, connection: crate::rpc::ConnectionId) {
        self.pool.drop_connection(connection).await;
    }

    /// The configured TCP/WebSocket token, if any.
    pub async fn token(&self) -> Option<String> {
        self.tcp_token.read().await.clone()
    }

    async fn dispatch(&self, req: RpcRequest, client: Arc<Client>) -> RpcResponse {
        let id = req.id.unwrap_or(serde_json::Value::Null);
        match req.method.as_str() {
            "daemon.status" => {
                let count = self.pool.active_count().await;
                let mut result = crate::discovery::identity();
                result["uptime_secs"] = serde_json::json!(self.started_at.elapsed().as_secs());
                result["sessions"] = serde_json::json!(count);
                RpcResponse::ok(id, result)
            }
            "daemon.ping" => {
                let mut result = crate::discovery::identity();
                result["pong"] = serde_json::json!(true);
                RpcResponse::ok(id, result)
            }
            "daemon.doctor" => RpcResponse::ok(id, self.pool.doctor_report().await),
            "daemon.subscribeEvents" => self.method_daemon_subscribe_events(id, client).await,
            "config.setProvider" => self.method_config_set_provider(id, req.params).await,
            "config.getProvider" => self.method_config_get_provider(id, req.params).await,
            // No params. Re-reads all three settings.json tiers from disk
            // as-is — for a human (or another process) who hand-edited a
            // file directly instead of going through `config.setProvider`.
            // Shares `SessionPool::apply_reloaded_settings()` with
            // `config.setProvider`, so the two converge on identical
            // semantics (routing re-resolved, task_router rebuilt,
            // already-running sessions recreate themselves lazily on their
            // next turn — see `SessionPool::run_turn`).
            "config.reload" => self.method_config_reload(id, req.params).await,
            "config.get" => self.method_config_get(id, req.params).await,
            "mcp.status" => RpcResponse::ok(
                id,
                serde_json::json!({"servers": self.pool.mcp_status().await}),
            ),
            "mcp.addServer" => self.method_mcp_put_server(id, req.params, false).await,
            "mcp.updateServer" => self.method_mcp_put_server(id, req.params, true).await,
            "commands.list" => RpcResponse::ok(
                id,
                serde_json::json!({"commands": self.pool.list_commands().await}),
            ),
            "plugin.list" if !self.plugins_enabled() => self.plugins_disabled(id),
            "plugin.install" | "plugin.uninstall" | "plugin.enable" | "plugin.disable"
            | "plugin.reload"
                if !self.plugins_enabled() =>
            {
                self.plugins_disabled(id)
            }
            "plugin.list" => RpcResponse::ok(
                id,
                serde_json::json!({"plugins": self.pool.list_plugins().await}),
            ),
            "plugin.reload" => RpcResponse::ok(id, self.pool.reload_plugins().await),
            "plugin.install" => self.method_plugin_install(id, req.params).await,
            "plugin.uninstall" => self.method_plugin_uninstall(id, req.params).await,
            "plugin.enable" => self.method_plugin_set_enabled(id, req.params, true).await,
            "plugin.disable" => self.method_plugin_set_enabled(id, req.params, false).await,
            "import.list" => self.method_import_list(id, req.params).await,
            "import.run" => self.method_import_run(id, req.params).await,
            "daemon.shutdown" => {
                self.pool.shutdown_all().await;
                self.shutdown_token.cancel();
                RpcResponse::ok(id, serde_json::json!({"shutting_down":true}))
            }
            "session.list" => {
                let include_children = req
                    .params
                    .get("include_children")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let parent_session_id =
                    req.params.get("parent_session_id").and_then(|v| v.as_str());
                let sessions = self
                    .pool
                    .list_all(include_children, parent_session_id)
                    .await;
                RpcResponse::ok(id, serde_json::json!({"sessions": sessions}))
            }
            "session.close" => {
                let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id")
                    }
                };
                if let Some(err) = self.scene_mismatch_response(&id, &session_id).await {
                    return err;
                }
                let sidechains_deleted = self.pool.shutdown_session(&session_id).await;
                RpcResponse::ok(
                    id,
                    serde_json::json!({
                        "closed": session_id,
                        "sidechains_deleted": sidechains_deleted,
                    }),
                )
            }
            "schedule.create" => self.method_schedule_create(id, req.params).await,
            "schedule.list" => {
                let session_id = req.params.get("session_id").and_then(|v| v.as_str());
                RpcResponse::ok(
                    id,
                    serde_json::json!({"jobs": self.pool.schedule_list(session_id)}),
                )
            }
            "schedule.delete" => match req.params.get("id").and_then(|v| v.as_str()) {
                Some(job) => RpcResponse::ok(
                    id,
                    serde_json::json!({"deleted": self.pool.schedule_remove(job)}),
                ),
                None => RpcResponse::err(id, codes::INVALID_PARAMS, "missing id"),
            },
            "session.delete" => {
                let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id")
                    }
                };
                if let Some(err) = self.scene_mismatch_response(&id, &session_id).await {
                    return err;
                }
                let dry_run = req
                    .params
                    .get("dry_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match self.pool.delete_session(&session_id, dry_run).await {
                    Ok(result) => RpcResponse::ok(id, result),
                    Err((code, message)) => RpcResponse::err(id, code, message),
                }
            }
            "session.create" => self.method_session_create(id, req.params).await,
            "session.run_turn" => self.method_session_run_turn(id, req.params, client).await,
            "session.respondToPrompt" => {
                self.method_session_respond_to_prompt(id, req.params).await
            }
            "session.get" => self.method_session_get(id, req.params).await,
            "session.subscribe" => self.method_session_subscribe(id, req.params, client).await,
            "session.unsubscribe" => {
                self.method_session_unsubscribe(id, req.params, client)
                    .await
            }
            "session.interrupt" => self.method_session_interrupt(id, req.params).await,
            "session.compact" => {
                self.method_session_command(id, req.params, client, "/compact")
                    .await
            }
            "session.clear" => {
                self.method_session_command(id, req.params, client, "/clear")
                    .await
            }
            "session.history" => self.method_session_history(id, req.params).await,
            "session.fork" => self.method_session_fork(id, req.params).await,
            "session.resume" => self.method_session_resume(id, req.params).await,
            "session.setModel" => self.method_session_set_model(id, req.params).await,
            "session.rename" => self.method_session_rename(id, req.params).await,
            "scene.list" => RpcResponse::ok(
                id,
                serde_json::json!({"scenes": self.pool.list_scenes().await}),
            ),
            "scene.describe" => self.method_scene_describe(id, req.params).await,
            "scene.activate" => {
                let scene_id = match req.params.get("scene").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing scene"),
                };
                match self.pool.activate_scene(&scene_id).await {
                    Ok(()) => {
                        RpcResponse::ok(id, serde_json::json!({"scene": scene_id, "active": true}))
                    }
                    Err((code, message)) => RpcResponse::err(id, code, message),
                }
            }
            "scene.deactivate" => {
                let scene_id = match req.params.get("scene").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing scene"),
                };
                match self.pool.deactivate_scene(&scene_id).await {
                    Ok(()) => RpcResponse::ok(id, serde_json::json!({"scene": scene_id})),
                    Err((code, message)) => RpcResponse::err(id, code, message),
                }
            }
            _ => RpcResponse::err(
                id,
                codes::METHOD_NOT_FOUND,
                format!("unknown: {}", req.method),
            ),
        }
    }

    /// `daemon.subscribeEvents` — no params. Returns an immediate ack
    /// (`{"subscribed": true}`) and, from then on, pushes every future
    /// `daemon.event` notification (see `SessionPool::emit_event`) to this
    /// same connection as a `StreamFrame` — indefinitely, until the
    /// connection closes. Does not replay past events; subscribe before the
    /// event you care about can happen (e.g. right after startup, before
    /// relying on `mcp_connected`/`import_detected`).
    async fn method_daemon_subscribe_events(
        &self,
        id: serde_json::Value,
        client: Arc<Client>,
    ) -> RpcResponse {
        let mut rx = self.pool.subscribe_events();
        let forward = client;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let frame = crate::rpc::StreamFrame::daemon_event(event);
                        if !send_frame(&forward, &frame).await {
                            break; // connection closed
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        let frame = crate::rpc::StreamFrame::daemon_event(
                            serde_json::json!({"kind": "events_lagged", "missed": missed}),
                        );
                        if !send_frame(&forward, &frame).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        RpcResponse::ok(id, serde_json::json!({"subscribed": true}))
    }

    /// `config.getProvider` params: `{"include_secrets": false}` (optional,
    /// default `false`). Read-side counterpart to `config.setProvider` — see
    /// `SessionPool::get_providers` doc comment for the redaction default.
    async fn method_config_get_provider(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let include_secrets = params
            .get("include_secrets")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let (project_root, scene) = match self.config_target(&params) {
            Ok(target) => target,
            Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
        };
        RpcResponse::ok(
            id,
            self.pool
                .get_providers(project_root.as_deref(), &scene, include_secrets)
                .await,
        )
    }

    /// `config.reload` params: `{"scene": "...", "project_root": "..."}`,
    /// both optional and both defaulting to this daemon's own — see
    /// `config_target`.
    async fn method_config_reload(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let (project_root, scene) = match self.config_target(&params) {
            Ok(target) => target,
            Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
        };
        RpcResponse::ok(
            id,
            self.pool
                .reload_settings(project_root.as_deref(), &scene)
                .await,
        )
    }

    /// The `(project_root, scene)` pair a configuration method acts on.
    ///
    /// Omitted means this daemon's own — the behaviour every one of these
    /// methods had when it could only ever act on that one pair. A named
    /// `project_root` reaches another project this daemon serves; `null` is
    /// refused, because these methods write (or reload) the *project* tier
    /// and a session with no project has no project tier — the same answer
    /// `config.get {tier: "project"}` already gives.
    fn config_target(
        &self,
        params: &serde_json::Value,
    ) -> Result<(Option<std::path::PathBuf>, String), &'static str> {
        let scene = params
            .get("scene")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| self.pool.scene_id().to_string());
        match params.get("project_root") {
            None => Ok((None, scene)),
            Some(serde_json::Value::String(p)) => Ok((Some(std::path::PathBuf::from(p)), scene)),
            Some(_) => {
                Err("project_root must be a path: a session with no project has no project tier")
            }
        }
    }

    /// Shared by `scene.describe` and `config.get`: the `(scene,
    /// project_root, include_secrets)` triple they both key on.
    ///
    /// `project_root` absent and `project_root: null` mean the same thing
    /// here — describe the scene with no project — because unlike
    /// `session.create` there is no session to bind and so no difference
    /// between "unspecified" and "explicitly none".
    fn describe_target(
        &self,
        params: &serde_json::Value,
    ) -> (Option<String>, Option<std::path::PathBuf>, bool) {
        let scene = params
            .get("scene")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let project_root = params
            .get("project_root")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);
        let include_secrets = params
            .get("include_secrets")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        (scene, project_root, include_secrets)
    }

    /// `scene.describe` params:
    /// `{"scene": "coding", "project_root": "...", "include_secrets": false}`
    async fn method_scene_describe(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let (scene, project_root, include_secrets) = self.describe_target(&params);
        let Some(scene) = scene else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing scene");
        };
        match self
            .pool
            .describe_scene(&scene, project_root.as_deref(), include_secrets)
            .await
        {
            Ok(v) => RpcResponse::ok(id, v),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    /// `config.get` params:
    /// `{"scene": "coding", "project_root": "...", "tier": "effective"}`
    async fn method_config_get(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let (scene, project_root, include_secrets) = self.describe_target(&params);
        let Some(scene) = scene else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing scene");
        };
        let tier = params
            .get("tier")
            .and_then(|v| v.as_str())
            .unwrap_or("effective");
        match self
            .pool
            .config_tier(&scene, project_root.as_deref(), tier, include_secrets)
            .await
        {
            Ok(v) => RpcResponse::ok(id, v),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    /// `mcp.addServer` params: `{"name": "...", "config": {"type": "stdio", ...}}`
    /// (`config` is an `mcp::config::McpServerConfig`, tagged by `type`).
    /// `plugin.install` params:
    /// ```json
    /// {
    ///   "name": "code-review-helper",
    ///   "version": "1.0.0",
    ///   "download_url": "file:///path/to/plugin.zip",   // or https://...
    ///   "checksum": "<sha256 hex>",                       // required for http(s), optional for file://
    ///   "scope": "global"                                 // optional, default "global"; or "scene"
    /// }
    /// ```
    /// No marketplace lookup — installs directly from the given source (see
    /// `plugin::cli::PluginCommands::install_source`). Verifies the
    /// checksum before extracting; a network (`http(s)://`) source without
    /// a `checksum` is rejected outright.
    /// Is the plugin subsystem present and switched on? When it isn't, the
    /// `plugin.*` methods answer `PLUGINS_DISABLED` rather than pretending
    /// (an empty `plugin.list` would read as "nothing installed", which is a
    /// different fact from "this build has no plugin support").
    ///
    /// A packages-only build counts as on. Managing packages is exactly what
    /// it can do; what it cannot do is run their components, and that is the
    /// disclosure's business, not this gate's.
    fn plugins_enabled(&self) -> bool {
        matches!(
            self.pool.plugin_status(),
            crate::plugins::PluginStatus::Enabled | crate::plugins::PluginStatus::PackagesOnly
        )
    }

    fn plugins_disabled(&self, id: serde_json::Value) -> RpcResponse {
        RpcResponse::err(
            id,
            codes::PLUGINS_DISABLED,
            format!(
                "plugin subsystem unavailable ({})",
                self.pool.plugin_status().as_str()
            ),
        )
    }

    async fn method_plugin_install(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let version = match params.get("version").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing version"),
        };
        let download_url = match params.get("download_url").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing download_url"),
        };
        let checksum = params
            .get("checksum")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        match self
            .pool
            .install_plugin(&name, &version, &download_url, checksum.as_deref(), &scope)
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// `plugin.uninstall` params: `{"name": "...", "version": "..." (optional, omit = all), "scope": "global"|"scene" (optional, default "global")}`
    async fn method_plugin_uninstall(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let version = params
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        match self
            .pool
            .uninstall_plugin(&name, version.as_deref(), &scope)
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// Shared handler for `plugin.enable`/`plugin.disable`. Params:
    /// `{"name": "...", "scope": "global"|"scene" (optional, default "global")}`
    async fn method_plugin_set_enabled(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        enabled: bool,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();

        match self.pool.set_plugin_enabled(&name, enabled, &scope).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    async fn method_mcp_put_server(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        update: bool,
    ) -> RpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing name"),
        };
        let config = match params.get("config").cloned() {
            Some(c) => c,
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing config"),
        };
        let (project_root, scene) = match self.config_target(&params) {
            Ok(target) => target,
            Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
        };
        let put = if update {
            self.pool
                .update_mcp_server(project_root.as_deref(), &scene, &name, config)
                .await
        } else {
            self.pool
                .add_mcp_server(project_root.as_deref(), &scene, &name, config)
                .await
        };
        match put {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// `import.list` params: `{"project_root": "..."}` (optional — see
    /// `config_target`; an import reads and writes one project's files).
    async fn method_import_list(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let (project_root, _scene) = match self.config_target(&params) {
            Ok(target) => target,
            Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
        };
        let sources = self.pool.list_import_sources(project_root.as_deref()).await;
        RpcResponse::ok(id, serde_json::json!({"sources": sources}))
    }

    /// `import.run` params: `{"source": "claude_code" | "codex" | "cursor",
    /// "project_root": "..."}` (one of the `source` values `import.list`
    /// returned; `project_root` optional, as for `import.list`).
    async fn method_import_run(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let source = match params.get("source").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing source"),
        };
        let (project_root, _scene) = match self.config_target(&params) {
            Ok(target) => target,
            Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
        };
        match self.pool.run_import(project_root.as_deref(), &source).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    /// `config.setProvider` params:
    /// ```json
    /// {
    ///   "provider_id": "deepseek",
    ///   "config": { "api_type": "openai_compatible", "base_url": "...", "api_key": "...", "default_model": "..." },
    ///   "default_provider": "deepseek",       // optional
    ///   "task_models": { "subagent": "deepseek" }, // optional, merged
    ///   "delete": false                        // optional, default false
    /// }
    /// ```
    /// `config`/`task_models` are partial patches (see `SessionPool::set_provider`
    /// doc comment for why they're raw JSON, not typed structs). Writes to the
    /// project-tier `settings.json` and returns the reloaded effective config
    /// plus routing validation.
    async fn method_config_set_provider(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let provider_id = match params.get("provider_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing provider_id"),
        };
        let delete = params
            .get("delete")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let config_patch = params.get("config").cloned();
        let default_provider = params
            .get("default_provider")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let task_models_patch = params.get("task_models").cloned();

        let (project_root, scene) = match self.config_target(&params) {
            Ok(target) => target,
            Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
        };
        match self
            .pool
            .set_provider(
                project_root.as_deref(),
                &scene,
                &provider_id,
                delete,
                config_patch,
                default_provider,
                task_models_patch,
            )
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => RpcResponse::err(id, codes::INVALID_PARAMS, e),
        }
    }

    async fn method_session_run_turn(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        client: Arc<Client>,
    ) -> RpcResponse {
        // session_id 可选：不传则自动新建 session
        let session_id = params
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Every other method that reattaches to a recorded session refuses to
        // cross a scene boundary; this one used to be the exception, and the
        // gap was not visible as an error — the session simply came back
        // under whichever scene this daemon defaults to. An unknown id still
        // creates a session, because `check_scene` reads a transcript that
        // isn't there as `Inferred`.
        if let Some(ref sid) = session_id {
            if let Some(err) = self.scene_mismatch_response(&id, sid).await {
                return err;
            }
        }

        let user_msg = match params.get("message").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing message"),
        };

        let turn_id = params
            .get("turn_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| Id::new().to_string());

        let options: Option<crate::rpc::SessionOptions> = params
            .get("options")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        // Optional and backwards-compatible: a client that never sends
        // `attachments` behaves exactly as before. A malformed entry is
        // dropped rather than failing the turn — the user still wants their
        // message sent, and a missing attachment is visible to them.
        let attachments: Vec<runtime::agent::Attachment> = params
            .get("attachments")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| match serde_json::from_value(v.clone()) {
                        Ok(a) => Some(a),
                        Err(e) => {
                            tracing::warn!(error = %e, "dropping malformed attachment");
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let on_busy = match params.get("on_busy") {
            None | Some(serde_json::Value::Null) => crate::session_pool::OnBusy::default(),
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(o) => o,
                Err(_) => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "on_busy must be \"reject\", \"queue\" or \"steer\"",
                    )
                }
            },
        };

        self.pool
            .run_turn(
                session_id,
                user_msg,
                attachments,
                turn_id,
                client,
                id,
                options,
                on_busy,
                "client",
            )
            .await
    }

    /// `schedule.create {session_id, cron, prompt, recurring?, durable?}`: run
    /// `prompt` as a turn of `session_id` whenever `cron` (5 fields, UTC)
    /// matches. Recurring and durable by default — a host schedules work that
    /// should outlive the session's time in memory and the daemon's restarts.
    async fn method_schedule_create(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let text = |key: &str| {
            params
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let flag = |key: &str| params.get(key).and_then(|v| v.as_bool()).unwrap_or(true);
        let (Some(session_id), Some(cron), Some(prompt)) =
            (text("session_id"), text("cron"), text("prompt"))
        else {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                "schedule.create needs session_id, cron and prompt",
            );
        };
        if !tools::cron::cron_expression_valid(&cron) {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                format!("invalid cron expression `{cron}`: expected 5 fields, M H DoM Mon DoW"),
            );
        }
        if !self.pool.session_exists(&session_id).await {
            return RpcResponse::err(
                id,
                codes::SESSION_NOT_FOUND,
                format!("no session `{session_id}`"),
            );
        }
        let job = self.pool.schedule_add(tools::cron::NewCronJob {
            cron,
            prompt,
            recurring: flag("recurring"),
            durable: flag("durable"),
            session_id: Some(session_id),
            expires_ms: None,
        });
        RpcResponse::ok(id, serde_json::json!({"job": job}))
    }

    /// Default page size for `session.history` when the caller doesn't say.
    /// Big enough to render a typical conversation in one round trip, small
    /// enough that a forgotten `limit` can't turn one RPC into a
    /// multi-megabyte line on the socket.
    const HISTORY_DEFAULT_LIMIT: usize = 100;
    /// Hard ceiling on `session.history`'s `limit`. A request for more is
    /// clamped, not rejected — the response echoes the `limit` actually
    /// applied plus `total`/`has_more`, so a caller can always tell it was
    /// capped and page for the rest.
    const HISTORY_MAX_LIMIT: usize = 500;

    /// `session.history` params:
    /// `{"session_id": "...", "offset": 0, "limit": 100}` (`offset`/`limit`
    /// optional). Returns a bounded page of the projected transcript — see
    /// `SessionPool::session_history` for what "projected" means and why the
    /// on-disk log is the source rather than the live engine's memory.
    async fn method_session_history(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id"),
        };
        // A negative/huge/non-integer value clamps to the default rather
        // than erroring: `limit` is a hint about response size, and failing
        // a transcript fetch over it helps nobody.
        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(usize::MAX as u64) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(Self::HISTORY_MAX_LIMIT))
            .unwrap_or(Self::HISTORY_DEFAULT_LIMIT);

        match self.pool.session_history(&session_id, offset, limit).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    /// `session.create` params:
    /// `{"scene": "...", "project_root": "...", "options": {...}}`.
    /// `scene` omitted means the pool's default scene (`SessionPool::
    /// resolve_scene`); given, must be currently active
    /// (`SCENE_NOT_FOUND` otherwise — see `scene.list`/`scene.activate`).
    /// `project_root` disambiguates three ways — the
    /// key omitted means "this pool's default project"; present as JSON
    /// `null` means a genuine no-project session; present as a string means
    /// that project (rejected with `PROJECT_NOT_FOUND` if it doesn't exist
    /// as a directory). See `SessionPool::create_session` /
    /// `session_pool::ProjectSelector`.
    async fn method_session_create(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let project_root = match params.get("project_root") {
            None => crate::session_pool::ProjectSelector::Default,
            Some(v) if v.is_null() => crate::session_pool::ProjectSelector::NoProject,
            Some(v) => match v.as_str() {
                Some(s) => crate::session_pool::ProjectSelector::Path(std::path::PathBuf::from(s)),
                None => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "project_root must be a string or null",
                    )
                }
            },
        };
        let scene = params.get("scene").and_then(|v| v.as_str());
        let options: Option<crate::rpc::SessionOptions> = params
            .get("options")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        match self
            .pool
            .create_session(scene, project_root, options.as_ref())
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    /// `session.get` params: `{"session_id": "..."}`.
    ///
    /// The same summary `session.list` reports for this one session, plus
    /// `turn_state` — which only the live pool knows, and which a client
    /// needs to decide whether its send button should be enabled.
    async fn method_session_get(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        match self.pool.session_detail(session_id).await {
            Ok(detail) => RpcResponse::ok(id, detail),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    /// `session.subscribe` params: `{"session_id": "..."}`.
    ///
    /// Starts sending this session's frames to the calling connection and
    /// returns what the caller needs to catch up: the transcript watermark
    /// and any unanswered permission asks.
    ///
    /// The order matters and is the reason `last_seq` is here at all:
    /// subscribe, *then* read `session.history` up to `last_seq`, then apply
    /// the frames that arrived in between. Reading first and subscribing
    /// second silently loses whatever the session produced in the gap.
    async fn method_session_subscribe(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        client: Arc<Client>,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        // Opt-in, and opt-in is the whole design. Resuming costs a transcript
        // read and a slot in a capped pool, and a client drawing a session
        // list subscribes to rows it is only looking at — doing this by
        // default would resume every one of them on its behalf.
        let resume = params
            .get("resume")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }

        let mut report = match self
            .pool
            .subscribe_session(session_id, client.clone())
            .await
        {
            Ok(v) => Some(v),
            Err((code, _)) if resume && code == codes::SESSION_NOT_FOUND => {
                // §3.4 still applies: the mismatch check above ran first, and
                // this records the pre-v2 case the same way `session.resume`
                // reports it.
                let inferred = matches!(
                    self.pool.check_scene(session_id).await,
                    crate::session_pool::SceneCheck::Inferred
                );
                match self.pool.resume_session(session_id, false, None).await {
                    Ok(_) => match self.pool.subscribe_session(session_id, client).await {
                        Ok(mut v) => {
                            if let Some(obj) = v.as_object_mut() {
                                obj.insert("resumed".into(), serde_json::json!(true));
                                obj.insert("scene_inferred".into(), serde_json::json!(inferred));
                            }
                            return RpcResponse::ok(id, v);
                        }
                        Err((code, message)) => return RpcResponse::err(id, code, message),
                    },
                    Err(e) => return resume_failure(id, e),
                }
            }
            Err((code, message)) => return RpcResponse::err(id, code, message),
        };

        if let Some(obj) = report.as_mut().and_then(|v| v.as_object_mut()) {
            obj.insert("resumed".into(), serde_json::json!(false));
        }
        RpcResponse::ok(id, report.unwrap_or_else(|| serde_json::json!({})))
    }

    /// `session.rename` params: `{"session_id": "...", "name": "..."|null}`.
    ///
    /// A missing `name` key and an explicit `null` mean the same thing here —
    /// clear it — because there is nothing else a rename with no name could
    /// be asking for.
    async fn method_session_rename(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        let name = params.get("name").and_then(|v| v.as_str());
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        match self.pool.rename_session(session_id, name).await {
            Ok(v) => RpcResponse::ok(id, v),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    /// `session.setModel` params: `{"session_id": "...", "model": "..."}`.
    ///
    /// The model a session talks to was fixed when the session was built, and
    /// the only way to change it was to change the configuration and start
    /// over — which costs the conversation. The engine never had that
    /// limitation (`EngineCommand::UpdateModel`); the method table did.
    async fn method_session_set_model(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        let named = |key: &str| {
            params
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
        };
        let (model, provider) = (named("model"), named("provider"));
        if model.is_none() && provider.is_none() {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing model");
        }
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        match self
            .pool
            .set_session_model(session_id, model, provider)
            .await
        {
            Ok(v) => RpcResponse::ok(id, v),
            Err(crate::session_pool::SetModelError::Busy { current_turn_id }) => {
                RpcResponse::err_with_data(
                    id,
                    codes::SESSION_BUSY,
                    "session is busy",
                    serde_json::json!({
                        "session_id": session_id,
                        "current_turn_id": current_turn_id,
                    }),
                )
            }
            Err(crate::session_pool::SetModelError::Rpc((code, message))) => {
                RpcResponse::err(id, code, message)
            }
        }
    }

    /// `session.unsubscribe` params: `{"session_id": "..."}`.
    ///
    /// Unsubscribing from a session this connection was not watching is
    /// success, not an error: the caller wanted to stop receiving it and it
    /// is not receiving it.
    async fn method_session_unsubscribe(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        client: Arc<Client>,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        self.pool.unsubscribe_session(session_id, client.id()).await;
        RpcResponse::ok(
            id,
            serde_json::json!({"session_id": session_id, "subscribed": false}),
        )
    }

    /// `session.interrupt` params: `{"session_id": "..."}`.
    ///
    /// Cancels the in-flight turn and keeps the session. No turn running is
    /// `{"interrupted": false}`, not an error: the caller wanted the session
    /// idle and it is.
    async fn method_session_interrupt(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        match self.pool.interrupt_session(session_id).await {
            Ok(interrupted) => RpcResponse::ok(
                id,
                serde_json::json!({"session_id": session_id, "interrupted": interrupted}),
            ),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    /// `session.compact` / `session.clear`: the slash command run as a turn
    /// of its own, so it streams and finishes like one and is refused like
    /// one while another turn holds the session.
    async fn method_session_command(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        client: Arc<Client>,
        command: &str,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        self.pool
            .run_turn(
                Some(session_id.to_string()),
                command.to_string(),
                vec![],
                Id::new().to_string(),
                client,
                id,
                None,
                crate::session_pool::OnBusy::Reject,
                "client",
            )
            .await
    }

    /// `session.fork` params:
    /// `{"session_id": "...", "at_message": 12}` (`at_message` optional —
    /// omit to copy the whole transcript). Produces a new session id whose
    /// history is an independent copy; see `SessionPool::fork_session`.
    async fn method_session_fork(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id"),
        };
        // Unlike `limit`, a malformed `at_message` *is* rejected — silently
        // forking somewhere other than where the caller asked would produce
        // a wrong-but-plausible branch, which is worse than an error.
        let at_message = match params.get("at_message") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n as usize),
                None => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "at_message must be a non-negative integer",
                    )
                }
            },
        };

        // Same §3.4 rejection `session.resume` applies — forking a session
        // recorded under a different scene would hand back a "primary"
        // session that starts life already scene-mismatched.
        if let Some(err) = self.scene_mismatch_response(&id, &session_id).await {
            return err;
        }

        match self.pool.fork_session(&session_id, at_message).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    /// `session.resume` params:
    /// `{"session_id": "...", "create_if_missing": false, "options": {...}}`.
    /// The explicit form of `session.run_turn`'s implicit resume — see
    /// `SessionPool::resume_session` for why an unknown id errors here by
    /// default while `run_turn` still creates one.
    async fn method_session_resume(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id"),
        };
        let create_if_missing = params
            .get("create_if_missing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // Same lenient parse `session.run_turn` uses for `options` — an
        // unrecognized shape degrades to "no options", it doesn't fail the
        // call.
        let options: Option<crate::rpc::SessionOptions> = params
            .get("options")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        // §3.4: a session recorded under a different scene than this
        // daemon's must not be silently resumed into it — that's exactly
        // the "conversation ends up hanging off the wrong scene" failure
        // mode the design calls out as the one worth actively catching.
        let scene_inferred = match self.pool.check_scene(&session_id).await {
            crate::session_pool::SceneCheck::Mismatch(recorded_scene) => {
                return RpcResponse::err_with_data(
                    id,
                    codes::SCENE_MISMATCH,
                    "scene mismatch",
                    serde_json::json!({
                        "session_id": session_id,
                        "recorded_scene": recorded_scene,
                        "requested_scene": self.pool.scene_id(),
                    }),
                );
            }
            crate::session_pool::SceneCheck::Inferred => true,
            crate::session_pool::SceneCheck::Matches => false,
        };

        // §5.6/§9: a sidechain that already reached a terminal state has no
        // continuation semantics. This check (and the actual resume) now
        // happen inside `resume_session` itself, under its own
        // per-`session_id` lock — see `SessionPool::with_session_lock` and
        // `ResumeError` for why: doing it out here, unconditionally and
        // before ever calling in, both raced a concurrent cascade delete of
        // the same id (TOCTOU) and defeated `resume_session`'s
        // already-active fast path (paying for a full transcript read even
        // when the session never left memory).
        match self
            .pool
            .resume_session(&session_id, create_if_missing, options.as_ref())
            .await
        {
            Ok(mut result) => {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "scene_inferred".to_string(),
                        serde_json::json!(scene_inferred),
                    );
                }
                RpcResponse::ok(id, result)
            }
            Err(e) => resume_failure(id, e),
        }
    }

    /// `session.respondToPrompt` — answers a pending `kind:"prompt"`
    /// `session.event` (currently only `prompt_type: "permission"`; the
    /// method name and this dispatch are deliberately generic so a future
    /// non-permission prompt type can reuse both without a new RPC method).
    async fn method_session_respond_to_prompt(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id"),
        };
        let prompt_id = match params.get("prompt_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing prompt_id"),
        };
        let prompt_type = params
            .get("prompt_type")
            .and_then(|v| v.as_str())
            .unwrap_or("permission");
        if prompt_type != "permission" {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                format!("unsupported prompt_type: {prompt_type}"),
            );
        }
        let decision: runtime::agent::PermissionDecision = match params
            .get("decision")
            .cloned()
            .map(serde_json::from_value)
        {
            Some(Ok(d)) => d,
            Some(Err(e)) => {
                return RpcResponse::err(id, codes::INVALID_PARAMS, format!("bad decision: {e}"))
            }
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing decision"),
        };

        match self
            .pool
            .respond_to_prompt(&session_id, prompt_id.clone(), decision)
            .await
        {
            Ok(()) => RpcResponse::ok(id, serde_json::json!({"prompt_id": prompt_id})),
            Err(e) => RpcResponse::err(id, codes::SESSION_NOT_FOUND, e),
        }
    }
}

#[cfg(test)]
mod frame_reader_tests {
    use super::*;

    fn reader(bytes: &'static [u8]) -> FrameReader {
        FrameReader::new(Box::new(std::io::Cursor::new(bytes)))
    }

    #[tokio::test]
    async fn frames_are_split_on_newlines() {
        let mut r = reader(b"{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(r.next_frame().await.as_deref(), Some("{\"a\":1}"));
        assert_eq!(r.next_frame().await.as_deref(), Some("{\"b\":2}"));
        assert_eq!(r.next_frame().await, None);
    }

    /// A client that closes without a trailing newline still said something.
    #[tokio::test]
    async fn a_final_frame_without_a_newline_is_still_delivered() {
        let mut r = reader(b"{\"a\":1}");
        assert_eq!(r.next_frame().await.as_deref(), Some("{\"a\":1}"));
        assert_eq!(r.next_frame().await, None);
    }

    /// The reason this type exists: without a cap, a peer that never sends a
    /// newline makes the daemon buffer until it dies.
    #[tokio::test]
    async fn a_frame_over_the_limit_ends_the_connection() {
        let oversized: &'static [u8] = Box::leak(
            std::iter::repeat_n(b'x', MAX_FRAME_BYTES + 1)
                .collect::<Vec<u8>>()
                .into_boxed_slice(),
        );
        assert_eq!(reader(oversized).next_frame().await, None);
    }

    #[tokio::test]
    async fn a_frame_at_the_limit_is_delivered() {
        let at_limit: &'static [u8] = Box::leak(
            std::iter::repeat_n(b'x', MAX_FRAME_BYTES)
                .chain(std::iter::once(b'\n'))
                .collect::<Vec<u8>>()
                .into_boxed_slice(),
        );
        assert_eq!(
            reader(at_limit).next_frame().await.map(|s| s.len()),
            Some(MAX_FRAME_BYTES)
        );
    }
}
