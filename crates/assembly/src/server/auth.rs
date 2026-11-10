//! The `daemon.auth` handshake for network transports: first-frame check and
//! constant-time token comparison.

use super::*;

/// Fixed-time byte comparison — avoids leaking the TCP token's length-matched
/// prefix via response-time side channels. `a`/`b` differing in length is not
/// itself timing-sensitive (tokens are a fixed shared secret, not attacker-
/// influenced length), so a cheap length check up front is fine.
pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl DaemonServer {
    /// TCP-only handshake: the first line on the connection must be a
    /// `daemon.auth` request carrying `params.token` equal to the token
    /// configured via `--token`/`AGIX_DAEMON_TOKEN`. Writes back an ok/err
    /// response either way. Returns `true` if the connection may proceed to
    /// the normal dispatch loop, `false` if it was rejected (or closed before
    /// sending anything) and the caller should just drop it.
    pub(super) async fn authenticate_lines(
        &self,
        lines: &mut FrameReader,
        client: &Client,
    ) -> bool {
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
}
