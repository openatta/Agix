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
        needs_auth: bool,
    ) -> anyhow::Result<()> {
        let mut lines = FrameReader::new(reader);
        let client = self.accept_connection(sink);
        let in_flight = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_PER_CONNECTION));

        // The handshake stays serial on purpose: nothing may be dispatched
        // before it passes.
        if needs_auth && !self.authenticate_lines(&mut lines, &client).await {
            return Ok(());
        }

        while let Some(line) = lines.next_frame().await {
            let req: RpcRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    if let Some(frame) = parse_error_frame(&e) {
                        send_frame(&client, &frame).await;
                    }
                    continue;
                }
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

/// The reply to a line that is not JSON at all. Valid JSON of the wrong shape
/// (no `method`) is still dropped without an answer.
pub(crate) fn parse_error_frame(e: &serde_json::Error) -> Option<RpcResponse> {
    (e.classify() != serde_json::error::Category::Data)
        .then(|| RpcResponse::err(serde_json::Value::Null, codes::PARSE_ERROR, "Parse error"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_gets_a_parse_error_and_a_request_without_method_does_not() {
        let garbage = serde_json::from_str::<RpcRequest>("{not json").unwrap_err();
        let frame = parse_error_frame(&garbage).unwrap();
        let v = serde_json::to_value(frame).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], serde_json::Value::Null);
        assert_eq!(v["error"]["code"], -32700);
        assert_eq!(v["error"]["message"], "Parse error");

        let truncated = serde_json::from_str::<RpcRequest>("{\"id\":1").unwrap_err();
        assert!(parse_error_frame(&truncated).is_some());
        let no_method = serde_json::from_str::<RpcRequest>("{\"id\":1}").unwrap_err();
        assert!(parse_error_frame(&no_method).is_none());
    }
}
