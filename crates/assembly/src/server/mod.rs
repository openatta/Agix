//! Daemon server — JSON-RPC over Unix socket / TCP.
//!
//! Accepts newline-delimited JSON-RPC 2.0 requests, dispatches them
//! to the agent engine, and streams events back as `session.event` lines.
//!
//! The framing is the only part that belongs to a transport. `dispatch` takes
//! a request and a [`Sink`], so `crate::ws` carries the same protocol over
//! WebSocket without reimplementing any of it — and, more to the point,
//! without being able to drift from the auth handshake this module defines.
//!
//! **Trust boundary**: every RPC method here — including `config.setProvider`
//! (writes `base_url` into settings.json and `api_key` into the 0600
//! `settings.local.json` beside it) — trusts
//! whoever can reach this socket. There is no per-method authorization on
//! top of that: Unix socket
//! file permissions are the entire access-control story locally. For TCP,
//! each connection must open with a `daemon.auth` handshake carrying the
//! shared token (`--token`/`AGIX_DAEMON_TOKEN`, see
//! [`DaemonServer::authenticate_tcp`]) before any other method is dispatched
//! — once a connection passes the handshake it is trusted for its lifetime,
//! same as a Unix socket connection. With `--unix-socket-auth`
//! ([`DaemonServer::require_unix_socket_auth`]) the unix socket takes the same
//! handshake. In particular, `config.setProvider` lets
//! any authenticated caller point future LLM traffic at an
//! attacker-controlled `base_url`/`api_key` — treat socket/token exposure
//! with the same care as exposing the LLM credentials themselves.
//!
//! The processes the daemon starts itself run as the same user, so file
//! permissions alone would let a model's command in. They are kept out three
//! ways: the unix socket refuses a peer this daemon started
//! (`lineage::started_here`); the OS sandbox refuses a confined command's
//! connect to the socket, and on macOS to the TCP / WebSocket ports
//! (`sandbox::fence_socket` / `fence_port`); and no child inherits
//! `AGIX_DAEMON_TOKEN`.
//!
//! That boundary does not cover tool execution: `session.run_turn` defaults
//! to `ask` and raises `session.event{kind:"prompt"}` frames that
//! `session.respondToPrompt` answers. See `crate::assemble::AllowAllPermission`
//! and `docs/daemon_rpc_protocol.md` for the opt-out.

use crate::rpc::{codes, RpcRequest, RpcResponse};
use crate::rpc::{send_frame, Client, FrameSink, Sink};
use crate::session_pool::SessionPool;
use crate::session_pool::TaskOutputRead;
use base::context::RunningTask;
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

/// What `session.taskOutput` returns when the caller does not say.
const DEFAULT_TASK_OUTPUT_TAIL: usize = 8 * 1024;

#[macro_use]
mod table;

mod auth;
mod connection;
mod errors;
mod listen;
mod methods;
mod methods_config;
pub mod schema;
mod transport;
mod watch;

#[cfg(feature = "ws")]
pub(crate) use connection::refusal_frame;
pub use listen::UnixBinding;
pub use table::methods;
#[cfg(feature = "ws")]
pub(crate) use transport::MAX_FRAME_BYTES;
pub(crate) use transport::MAX_IN_FLIGHT_PER_CONNECTION;
use transport::{ByteWriter, FrameReader, LineSink, Reader};

pub struct DaemonServer {
    pool: Arc<SessionPool>,
    /// Source of connection ids. Monotonic for the process lifetime — an id
    /// is never reused, so a subscription left behind by a closed connection
    /// can never be inherited by a later one.
    next_connection: std::sync::atomic::AtomicU64,
    started_at: Instant,
    shutdown_token: CancellationToken,
    tcp_token: tokio::sync::RwLock<Option<String>>,
    unix_socket_auth: std::sync::atomic::AtomicBool,
}

impl DaemonServer {
    pub fn new(pool: Arc<SessionPool>, shutdown_token: CancellationToken) -> Self {
        Self {
            pool,
            next_connection: std::sync::atomic::AtomicU64::new(1),
            started_at: Instant::now(),
            shutdown_token,
            tcp_token: tokio::sync::RwLock::new(None),
            unix_socket_auth: std::sync::atomic::AtomicBool::new(false),
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

    /// From now on a unix socket connection, too, must open with
    /// `daemon.auth`, against the token [`Self::set_tcp_token`] set. With no
    /// token set, every such connection is refused.
    pub fn require_unix_socket_auth(&self) {
        self.unix_socket_auth
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }

    /// Stop accepting connections, then stop every session and wait for
    /// them. Connections already open stay open meanwhile, and a session they
    /// ask for is refused.
    pub async fn shutdown(&self) {
        self.shutdown_token.cancel();
        self.pool.shutdown_all().await;
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
        self.pool
            .drop_watcher(crate::session_pool::api::WatcherId(connection.0))
            .await;
    }

    /// The configured TCP/WebSocket token, if any.
    pub async fn token(&self) -> Option<String> {
        self.tcp_token.read().await.clone()
    }
}
