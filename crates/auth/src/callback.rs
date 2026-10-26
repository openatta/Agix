//! Local HTTP listener for OAuth redirect_uri. Binds to 127.0.0.1:<ephemeral>,
//! accepts a single GET to `/callback?code=…&state=…`, writes a tiny HTML
//! "you can close this tab" response, and returns the captured params.
//!
//! No external HTTP framework — we parse the request line + query string
//! directly. Only handles a single connection then exits.

use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Error)]
pub enum CallbackError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("listener timed out after {secs}s waiting for callback")]
    Timeout { secs: u64 },
    #[error("malformed request: {0}")]
    Malformed(String),
    #[error("upstream OAuth error: {0}")]
    Upstream(String),
}

#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: String,
    pub state: Option<String>,
}

pub struct CallbackListener {
    listener: TcpListener,
    redirect_uri: String,
}

impl CallbackListener {
    /// Bind to an ephemeral 127.0.0.1 port. Returns the listener and the
    /// `http://127.0.0.1:<port>/callback` URL to use as `redirect_uri` in
    /// the auth URL.
    pub async fn start() -> Result<Self, CallbackError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        Ok(Self {
            listener,
            redirect_uri,
        })
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait for a single inbound request, return its `code` + `state`.
    /// Times out after `timeout`.
    pub async fn await_callback(self, timeout: Duration) -> Result<CallbackResult, CallbackError> {
        let secs = timeout.as_secs();
        let accept = self.listener.accept();
        let (stream, _addr) = tokio::time::timeout(timeout, accept)
            .await
            .map_err(|_| CallbackError::Timeout { secs })??;
        handle_one(stream).await
    }
}

async fn handle_one(mut stream: TcpStream) -> Result<CallbackResult, CallbackError> {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Err(CallbackError::Malformed("empty request".into()));
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let first_line = head.lines().next().unwrap_or("");
    let target = parse_request_line(first_line)
        .ok_or_else(|| CallbackError::Malformed(format!("bad request line: {first_line:?}")))?;
    let query = target.split('?').nth(1).unwrap_or("");
    let params = parse_query(query);

    if let Some(err) = params.get("error") {
        let msg = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| err.clone());
        // best-effort failure page
        let _ = write_response(
            &mut stream,
            200,
            "text/html",
            r#"<!doctype html><body><h1>Authorization failed</h1>
            <p>You can close this window.</p>"#,
        )
        .await;
        return Err(CallbackError::Upstream(msg));
    }

    let code = params
        .get("code")
        .cloned()
        .ok_or_else(|| CallbackError::Malformed("missing `code` param".into()))?;
    let state = params.get("state").cloned();

    write_response(
        &mut stream,
        200,
        "text/html",
        r#"<!doctype html><body style="font-family:system-ui;padding:32px">
        <h1>✓ agix signed in</h1>
        <p>You can close this window and return to your terminal.</p>
        </body>"#,
    )
    .await
    .ok();

    Ok(CallbackResult { code, state })
}

fn parse_request_line(line: &str) -> Option<&str> {
    // "GET /callback?code=… HTTP/1.1"
    let mut parts = line.split(' ');
    let _method = parts.next()?;
    let target = parts.next()?;
    Some(target)
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if q.is_empty() {
        return out;
    }
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        out.insert(url_decode(k), url_decode(v));
    }
    out
}

/// Minimal `application/x-www-form-urlencoded` decoder. Handles `+ → space`
/// and `%XX → byte`.
fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h1 = (bytes[i + 1] as char).to_digit(16);
                let h2 = (bytes[i + 2] as char).to_digit(16);
                match (h1, h2) {
                    (Some(a), Some(b)) => {
                        out.push((a * 16 + b) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status} OK\r\n\
         Content-Type: {content_type}; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_basic_pairs() {
        let p = parse_query("code=abc&state=xyz");
        assert_eq!(p.get("code").map(|s| s.as_str()), Some("abc"));
        assert_eq!(p.get("state").map(|s| s.as_str()), Some("xyz"));
    }

    #[test]
    fn parse_query_empty_returns_empty() {
        assert!(parse_query("").is_empty());
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("hello+world"), "hello world");
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("%7Bfoo%7D"), "{foo}");
    }

    #[test]
    fn parse_request_line_extracts_target() {
        assert_eq!(
            parse_request_line("GET /callback?code=x HTTP/1.1"),
            Some("/callback?code=x")
        );
    }

    #[test]
    fn parse_request_line_returns_none_on_garbage() {
        assert_eq!(parse_request_line(""), None);
    }

    #[tokio::test]
    async fn start_binds_to_loopback_with_callback_path() {
        let l = CallbackListener::start().await.unwrap();
        assert!(l.redirect_uri().starts_with("http://127.0.0.1:"));
        assert!(l.redirect_uri().ends_with("/callback"));
    }

    #[tokio::test]
    async fn await_callback_times_out_when_no_request() {
        let l = CallbackListener::start().await.unwrap();
        let r = l.await_callback(Duration::from_millis(100)).await;
        assert!(matches!(r, Err(CallbackError::Timeout { .. })));
    }

    #[tokio::test]
    async fn await_callback_returns_code_and_state() {
        let l = CallbackListener::start().await.unwrap();
        let uri = l.redirect_uri().to_string();
        // Spawn a fake browser request
        tokio::spawn(async move {
            let url = url::Url::parse(&uri).unwrap();
            let host = url.host_str().unwrap();
            let port = url.port().unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut s = tokio::net::TcpStream::connect(format!("{host}:{port}"))
                .await
                .unwrap();
            s.write_all(b"GET /callback?code=A1&state=S1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .unwrap();
            // drain a bit of response so the server's write_all completes
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).await;
        });
        let r = l.await_callback(Duration::from_secs(2)).await.unwrap();
        assert_eq!(r.code, "A1");
        assert_eq!(r.state.as_deref(), Some("S1"));
    }

    #[tokio::test]
    async fn upstream_error_propagates() {
        let l = CallbackListener::start().await.unwrap();
        let uri = l.redirect_uri().to_string();
        tokio::spawn(async move {
            let url = url::Url::parse(&uri).unwrap();
            let host = url.host_str().unwrap();
            let port = url.port().unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut s = tokio::net::TcpStream::connect(format!("{host}:{port}"))
                .await
                .unwrap();
            s.write_all(
                b"GET /callback?error=access_denied&error_description=user%20refused HTTP/1.1\r\n\r\n",
            )
            .await
            .unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).await;
        });
        let r = l.await_callback(Duration::from_secs(2)).await;
        match r {
            Err(CallbackError::Upstream(msg)) => assert!(msg.contains("user refused")),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }
}
