//! WebSocket transport.
//!
//! The same JSON-RPC the Unix socket and TCP transports carry, framed by
//! WebSocket instead of by newlines. Everything above the framing —
//! dispatch, sessions, streaming, permission prompts — is shared, because a
//! transport's whole job is turning bytes into `RpcRequest`s.
//!
//! It exists so a browser can talk to the daemon directly. That keeps a web
//! front end a front end: it serves files and never learns what a session
//! is, while the CLI, an editor, and the browser all connect to one daemon
//! and see the same sessions.
//!
//! WebSocket rather than HTTP with server-sent events because the protocol
//! is genuinely bidirectional. `session.respondToPrompt` answers a question
//! the daemon asked; over SSE that needs a second channel and a correlation
//! scheme, and here it is just another message.
//!
//! ## Why an Origin check, on a loopback-only listener
//!
//! Binding `127.0.0.1` keeps other machines out. It does not keep *pages*
//! out: any website the user visits can open `ws://127.0.0.1:<port>` from
//! their browser, because WebSocket is not subject to the same-origin
//! policy. The token stops such a page from doing anything, and the Origin
//! check stops it from getting as far as trying — a page cannot forge the
//! header, since browsers set it themselves.
//!
//! Non-browser clients send no `Origin` at all, and are allowed through to
//! the token handshake unchanged: the header is evidence about browsers, and
//! its absence is not evidence about anything.

use crate::rpc::{send_frame, Client, FrameSink, RpcRequest, RpcResponse, Sink};
use crate::server::DaemonServer;
use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, warn};

/// One frame per WebSocket text message.
///
/// No delimiter and no flush decision: WebSocket is already message-framed,
/// and `SinkExt::send` completes the message. The newline transports have to
/// choose when to flush; this one does not have the choice to get wrong.
struct WsSink(AsyncMutex<futures::stream::SplitSink<WebSocketStream<TcpStream>, Message>>);

impl WsSink {
    /// Close frame, then drop.
    ///
    /// Dropping the stream on its own reaches the peer as a reset, which its
    /// WebSocket API reports as a connection error rather than a close — so a
    /// front end refused for a bad token would show "something went wrong"
    /// instead of the refusal it was actually sent.
    async fn close(&self) {
        let _ = self.0.lock().await.close().await;
    }
}

#[async_trait::async_trait]
impl FrameSink for WsSink {
    async fn send_json(&self, json: String) -> bool {
        self.0
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .is_ok()
    }
}

/// Origins a browser may connect from.
///
/// Only loopback: a web front end for a personal install is served from the
/// same machine, so anything else is either a mistake or a page trying its
/// luck. The port is deliberately not checked — the front end's port is a
/// deployment detail, and pinning it here would mean editing the daemon to
/// move the UI.
fn origin_is_local(origin: &str) -> bool {
    let rest = match origin.split_once("://") {
        Some(("http" | "https", rest)) => rest,
        _ => return false,
    };
    let authority = rest.split('/').next().unwrap_or("");
    // An IPv6 host is bracketed precisely because its own colons would
    // otherwise be read as the port separator.
    let host = match authority.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((host, _port)) => host,
            None => return false,
        },
        None => authority.split(':').next().unwrap_or(""),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Serve WebSocket connections on `addr`.
///
/// Refuses to start without a token: this transport is reachable by any
/// process on the machine and by any page the user's browser loads, so an
/// unauthenticated one would be an open door. The Unix socket needs no token
/// because file permissions are its handshake.
pub async fn serve_ws(server: Arc<DaemonServer>, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(addr = %listener.local_addr()?, "WebSocket transport listening");
    serve_ws_listener(server, listener).await
}

/// As [`serve_ws`], against a listener the caller bound — so a caller who
/// already has one (a test on port 0, socket activation) can drive the accept
/// loop directly.
///
/// Checks the token here rather than only in [`serve_ws`], because this is the
/// entry point such a caller reaches, and a missing token would otherwise
/// surface as every connection being refused forever.
pub async fn serve_ws_listener(
    server: Arc<DaemonServer>,
    listener: TcpListener,
) -> anyhow::Result<()> {
    if server.token().await.is_none() {
        anyhow::bail!("the WebSocket transport requires a token; pass --token");
    }
    let cancel = server.shutdown_token();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                let server = server.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(server, stream).await {
                        debug!(peer = %peer, error = %e, "WebSocket connection ended");
                    }
                });
            }
        }
    }
}

/// The upgrade callback, as a named function so the size of tungstenite's
/// `ErrorResponse` — which this code does not choose — can be waved through
/// in one place.
#[allow(clippy::result_large_err)]
fn check_origin(req: &Request, res: Response) -> Result<Response, ErrorResponse> {
    let Some(origin) = req.headers().get("origin") else {
        return Ok(res);
    };
    let origin = origin.to_str().unwrap_or("");
    if origin_is_local(origin) {
        return Ok(res);
    }
    warn!(origin = %origin, "refused a WebSocket upgrade from a foreign origin");
    // The status has to be set explicitly: `ErrorResponse::new` defaults to
    // 200, and a 200 that is not an upgrade leaves the client waiting for a
    // handshake that never arrives instead of reporting a refusal.
    let mut refusal = ErrorResponse::new(Some(
        "this daemon only accepts WebSocket connections from localhost".to_string(),
    ));
    *refusal.status_mut() = StatusCode::FORBIDDEN;
    Err(refusal)
}

async fn handle(server: Arc<DaemonServer>, stream: TcpStream) -> anyhow::Result<()> {
    // The same ceiling the newline transports enforce. tungstenite defaults to
    // 64 MiB, which is not a limit chosen for this protocol.
    let config = WebSocketConfig::default()
        .max_message_size(Some(crate::server::MAX_FRAME_BYTES))
        .max_frame_size(Some(crate::server::MAX_FRAME_BYTES));

    // The Origin check belongs in the handshake, not after it: a page that is
    // not allowed to talk to us should be told during the upgrade, when its own
    // WebSocket API reports the failure, rather than being connected and then
    // ignored.
    let ws =
        tokio_tungstenite::accept_hdr_async_with_config(stream, check_origin, Some(config)).await?;

    let (write, mut read) = ws.split();
    let ws_sink = Arc::new(WsSink(AsyncMutex::new(write)));
    let sink: Sink = ws_sink.clone();
    let client = server.accept_connection(sink);
    let in_flight = Arc::new(tokio::sync::Semaphore::new(
        crate::server::MAX_IN_FLIGHT_PER_CONNECTION,
    ));

    // Same handshake as TCP: the first message must be `daemon.auth`. Reusing
    // the check rather than writing a second one is what keeps a new
    // transport from accidentally accepting a token the others reject.
    if !authenticate(&server, &mut read, &client).await {
        ws_sink.close().await;
        return Ok(());
    }

    while let Some(Ok(message)) = read.next().await {
        let text = match message {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            // Ping/pong are handled by tungstenite; a close ends the loop.
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(req) = serde_json::from_str::<RpcRequest>(&text) else {
            continue;
        };
        // Served on its own task — a browser tab holds one socket for
        // everything, so a turn that occupied the read loop would make its
        // own permission prompt unanswerable. See `serve_request`.
        server.clone().serve_request(req, &client, &in_flight);
    }
    // A tab closing takes its subscriptions and nothing else — see
    // `SessionPool::drop_connection`.
    server.drop_connection(client.id()).await;
    ws_sink.close().await;
    Ok(())
}

async fn authenticate(
    server: &Arc<DaemonServer>,
    read: &mut futures::stream::SplitStream<WebSocketStream<TcpStream>>,
    client: &Client,
) -> bool {
    let Some(Ok(Message::Text(first))) = read.next().await else {
        return false;
    };
    let response = match serde_json::from_str::<RpcRequest>(&first) {
        Ok(req) => server.check_auth_public(&req).await,
        Err(_) => Err(RpcResponse::err(
            serde_json::Value::Null,
            crate::rpc::codes::UNAUTHORIZED,
            "first message must be a valid daemon.auth request",
        )),
    };
    let ok = response.is_ok();
    send_frame(client, &response.unwrap_or_else(|e| e)).await;
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page the user happens to visit can open a WebSocket to loopback —
    /// the same-origin policy does not apply — so the header is what
    /// distinguishes the front end from someone else's site.
    #[test]
    fn only_loopback_origins_are_accepted() {
        for allowed in [
            "http://localhost",
            "http://localhost:5173",
            "http://127.0.0.1:8080",
            "https://localhost:443",
            "http://[::1]:3000",
            "http://[::1]",
        ] {
            assert!(origin_is_local(allowed), "{allowed} should be allowed");
        }

        for refused in [
            "https://evil.example",
            "http://localhost.evil.example",
            "https://127.0.0.1.evil.example",
            "file://",
            "null",
            "",
            // An unterminated bracket is not a host we can identify.
            "http://[::1",
        ] {
            assert!(!origin_is_local(refused), "{refused} should be refused");
        }
    }

    /// The front end's port is a deployment detail; pinning it would mean
    /// editing the daemon to move the UI.
    #[test]
    fn the_port_is_not_part_of_the_decision() {
        assert!(origin_is_local("http://localhost:1"));
        assert!(origin_is_local("http://localhost:65535"));
    }
}
