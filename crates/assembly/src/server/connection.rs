//! The per-connection read loop and per-request task spawning with the
//! in-flight bound.

use super::*;

impl DaemonServer {
    /// Read requests and serve them.
    ///
    /// **Each request is served in its own task, and the read loop never
    /// waits for one.** `session.run_turn` runs for as long as the turn does;
    /// served inline, the connection would read nothing else meanwhile, so a
    /// client could not answer a permission prompt or interrupt the turn on
    /// the connection that is running it. `docs/daemon_rpc_protocol.md` §5.2
    /// promises concurrent in-flight requests.
    ///
    /// Responses may therefore come back out of order, which is what §5.2
    /// says and why clients match on `id`.
    pub(super) async fn handle_connection(
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
        let notification = req.id.is_none();
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);
        let client = client.clone();
        let Ok(permit) = in_flight.clone().try_acquire_owned() else {
            if notification {
                return;
            }
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
            if !notification {
                send_frame(&client, &resp).await;
            }
            drop(permit);
        });
    }
}
