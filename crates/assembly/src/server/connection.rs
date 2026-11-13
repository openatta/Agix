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
                    if let Some(frame) = refusal_frame(&line, &e) {
                        send_frame(&client, &frame).await;
                    }
                    continue;
                }
            };
            self.clone().serve_request(req, &client, &in_flight);
        }
        // The connection is what subscriptions belong to; sessions and their
        // turns are not. Dropping it takes its subscriptions and nothing else.
        self.pool
            .drop_watcher(crate::session_pool::api::WatcherId(client.id().0))
            .await;
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
            let method = req.method.clone();
            let dispatched = std::panic::AssertUnwindSafe(self.dispatch(req, client.clone()));
            let resp = match futures::FutureExt::catch_unwind(dispatched).await {
                Ok(resp) => resp,
                Err(panic) => {
                    let what = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_default();
                    tracing::error!(%method, panic = %what, "a request handler panicked");
                    RpcResponse::err(
                        id,
                        codes::INTERNAL_ERROR,
                        format!("internal error while serving `{method}`"),
                    )
                }
            };
            if !notification {
                send_frame(&client, &resp).await;
            }
            drop(permit);
        });
    }
}

/// The reply to a frame that is not a request: `PARSE_ERROR` with a null id
/// for text that is not JSON, `INVALID_REQUEST` echoing the id for JSON of the
/// wrong shape (no `method`, a field of the wrong type). Frames with no usable
/// id get nothing — no one is waiting on them — and neither do frames shaped
/// like responses: answering those could ping-pong errors with a peer that
/// answers ours.
pub(crate) fn refusal_frame(text: &str, e: &serde_json::Error) -> Option<RpcResponse> {
    if e.classify() != serde_json::error::Category::Data {
        return Some(RpcResponse::err(
            serde_json::Value::Null,
            codes::PARSE_ERROR,
            "Parse error",
        ));
    }
    let frame: serde_json::Value = serde_json::from_str(text).ok()?;
    let object = frame.as_object()?;
    if object.contains_key("result") || object.contains_key("error") {
        return None;
    }
    let id = object
        .get("id")
        .filter(|id| id.is_string() || id.is_number())?;
    Some(RpcResponse::err(
        id.clone(),
        codes::INVALID_REQUEST,
        format!("Invalid Request: {e}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_gets_a_parse_error() {
        let garbage = serde_json::from_str::<RpcRequest>("{not json").unwrap_err();
        let frame = refusal_frame("{not json", &garbage).unwrap();
        let v = serde_json::to_value(frame).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], serde_json::Value::Null);
        assert_eq!(v["error"]["code"], -32700);
        assert_eq!(v["error"]["message"], "Parse error");

        let truncated = serde_json::from_str::<RpcRequest>("{\"id\":1").unwrap_err();
        assert_eq!(
            refusal_frame("{\"id\":1", &truncated)
                .unwrap()
                .error
                .unwrap()
                .code,
            codes::PARSE_ERROR
        );
    }

    fn refusal(text: &str) -> Option<serde_json::Value> {
        let e = serde_json::from_str::<RpcRequest>(text).unwrap_err();
        refusal_frame(text, &e).map(|f| serde_json::to_value(f).unwrap())
    }

    #[test]
    fn a_misshapen_request_with_an_id_is_answered_with_that_id() {
        for text in [
            r#"{"jsonrpc":2.0,"method":"daemon.ping","id":7}"#,
            r#"{"jsonrpc":"2.0","method":42,"id":7}"#,
            r#"{"jsonrpc":"2.0","id":7}"#,
        ] {
            let v = refusal(text).unwrap_or_else(|| panic!("{text} got no answer"));
            assert_eq!(v["id"], 7, "{text}");
            assert_eq!(v["error"]["code"], -32600, "{text}");
        }
        assert_eq!(refusal(r#"{"method":1,"id":"a"}"#).unwrap()["id"], "a");
    }

    #[test]
    fn a_misshapen_frame_without_a_usable_id_or_shaped_like_a_response_is_dropped() {
        for text in [
            r#"{"jsonrpc":2.0,"method":"daemon.ping"}"#,
            r#"{"method":1,"id":{"x":1}}"#,
            r#"{"method":1,"id":null}"#,
            r#"[{"method":"daemon.ping","id":1}]"#,
            r#"{"jsonrpc":"2.0","id":7,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32600,"message":"x"}}"#,
        ] {
            assert!(refusal(text).is_none(), "{text} was answered");
        }
    }
}
