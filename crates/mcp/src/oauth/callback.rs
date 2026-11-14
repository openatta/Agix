//! Where the authorization server sends the browser back to.
//!
//! A loopback flow listens on `127.0.0.1` itself ([`serve_loopback`]): each
//! `GET /callback` is handed to whoever completes the flow, and the browser is
//! answered with the outcome once it is known. An external flow uses a
//! redirect URI the host owns and passes the callback URL back in;
//! [`check_redirect_uri`] decides which of those are acceptable.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

pub const CALLBACK_PATH: &str = "/callback";
const MAX_REQUEST: usize = 16 * 1024;
const REPLY_WAIT: Duration = Duration::from_secs(60);

/// One browser arrival at the loopback listener. `reply` takes the outcome
/// shown on the page; `Ok` carries the server name.
pub struct LoopbackCallback {
    pub url: String,
    pub reply: oneshot::Sender<Result<String, String>>,
}

/// A redirect URI a host may supply: `https`, `http` on a loopback address,
/// or a private-use scheme (RFC 8252 §7.1, which needs a `.` in it). Never a
/// fragment or credentials.
pub fn check_redirect_uri(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("redirect_uri `{raw}` is not a URL: {e}"))?;
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "redirect_uri `{raw}` must not carry a fragment or credentials"
        ));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if super::http::is_loopback(&url) => Ok(url),
        "http" => Err(format!(
            "redirect_uri `{raw}`: plain http is only accepted on a loopback address"
        )),
        scheme if scheme.contains('.') => Ok(url),
        _ => Err(format!(
            "redirect_uri `{raw}`: use https, http on loopback, or a reverse-domain private scheme"
        )),
    }
}

/// Whether `callback` is a redirect to `redirect_uri`: same scheme, host,
/// port and path. Loopback ports must match too — the listener that receives
/// it is bound to one.
pub fn is_callback_for(callback: &Url, redirect_uri: &Url) -> bool {
    callback.scheme() == redirect_uri.scheme()
        && callback.host_str() == redirect_uri.host_str()
        && callback.port_or_known_default() == redirect_uri.port_or_known_default()
        && callback.path() == redirect_uri.path()
}

const REBIND_ATTEMPTS: u32 = 100;
const REBIND_PAUSE: Duration = Duration::from_millis(10);

/// A fixed port a sign-in has just closed can stay unbindable for a few
/// milliseconds under load on macOS, so `AddrInUse` is retried for about a
/// second before it is reported.
pub async fn bind_loopback(port: Option<u16>) -> Result<(TcpListener, String), String> {
    let mut attempts = 0;
    let listener = loop {
        match TcpListener::bind(("127.0.0.1", port.unwrap_or(0))).await {
            Ok(listener) => break listener,
            Err(e)
                if port.is_some()
                    && e.kind() == std::io::ErrorKind::AddrInUse
                    && attempts < REBIND_ATTEMPTS =>
            {
                attempts += 1;
                tokio::time::sleep(REBIND_PAUSE).await;
            }
            Err(e) => {
                return Err(format!(
                    "cannot listen for the OAuth callback on 127.0.0.1: {e}"
                ))
            }
        }
    };
    let port = listener
        .local_addr()
        .map_err(|e| format!("cannot listen for the OAuth callback: {e}"))?
        .port();
    Ok((listener, format!("http://127.0.0.1:{port}{CALLBACK_PATH}")))
}

/// Serve the callback until `stop` fires or a callback was accepted. The
/// listener is closed when the returned task ends.
pub fn serve_loopback(
    listener: TcpListener,
    redirect_uri: String,
    server: String,
    stop: CancellationToken,
) -> (mpsc::Receiver<LoopbackCallback>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(4);
    let serving = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = stop.cancelled() => return,
                accepted = listener.accept() => accepted,
            };
            let Ok((stream, _)) = accepted else {
                continue;
            };
            if handle(stream, &redirect_uri, &server, &tx).await {
                return;
            }
        }
    });
    (rx, serving)
}

/// Answer one connection; `true` once a callback completed the flow.
async fn handle(
    mut stream: TcpStream,
    redirect_uri: &str,
    server: &str,
    tx: &mpsc::Sender<LoopbackCallback>,
) -> bool {
    let Some(target) = read_target(&mut stream).await else {
        let _ = respond(&mut stream, 400, "Bad request", "").await;
        return false;
    };
    let Some(path) = target.split('?').next() else {
        return false;
    };
    if path != CALLBACK_PATH {
        let _ = respond(&mut stream, 404, "Not found", "").await;
        return false;
    }
    let url = format!("{}{}", redirect_uri.trim_end_matches(CALLBACK_PATH), target);
    let (reply, outcome) = oneshot::channel();
    if tx.send(LoopbackCallback { url, reply }).await.is_err() {
        let _ = respond(
            &mut stream,
            410,
            "This sign-in is no longer in progress",
            "",
        )
        .await;
        return true;
    }
    match tokio::time::timeout(REPLY_WAIT, outcome).await {
        Ok(Ok(Ok(name))) => {
            let _ = respond(
                &mut stream,
                200,
                &format!("Signed in to {}", escape(&name)),
                "You can close this window.",
            )
            .await;
            true
        }
        Ok(Ok(Err(error))) => {
            let _ = respond(
                &mut stream,
                400,
                &format!("Sign-in to {} did not complete", escape(server)),
                &escape(&error),
            )
            .await;
            false
        }
        _ => {
            let _ = respond(&mut stream, 500, "Sign-in did not finish", "").await;
            true
        }
    }
}

async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if n == 0 || buf.len() + n > MAX_REQUEST {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next()?.split(' ');
    match (parts.next()?, parts.next()?) {
        ("GET", target) if target.starts_with('/') => Some(target.to_string()),
        _ => None,
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    title: &str,
    detail: &str,
) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:system-ui;padding:32px\"><h1>{title}</h1><p>{detail}</p>\
         </body></html>"
    );
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[tokio::test]
    async fn a_fixed_port_still_being_released_is_bound_once_it_is() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let mut binding = Box::pin(bind_loopback(Some(port)));
        std::future::poll_fn(|cx| {
            assert!(binding.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(held);
        let (_listener, uri) = binding.await.unwrap();
        assert_eq!(uri, format!("http://127.0.0.1:{port}{CALLBACK_PATH}"));
    }

    #[test]
    fn redirect_uris_are_https_loopback_or_a_private_scheme() {
        for ok in [
            "https://app.example/oauth/done",
            "http://127.0.0.1:43123/callback",
            "http://[::1]:5000/cb",
            "http://localhost:8080/cb",
            "com.example.app:/oauth2redirect",
        ] {
            assert!(check_redirect_uri(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://app.example/cb",
            "https://app.example/cb#frag",
            "https://user:pw@app.example/cb",
            "javascript:alert(1)",
            "not a url",
        ] {
            assert!(check_redirect_uri(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_callback_must_come_back_to_the_redirect_uri() {
        let redirect = Url::parse("http://127.0.0.1:4000/callback").unwrap();
        let back = |s: &str| is_callback_for(&Url::parse(s).unwrap(), &redirect);
        assert!(back("http://127.0.0.1:4000/callback?code=c&state=s"));
        assert!(!back("http://127.0.0.1:4001/callback?code=c&state=s"));
        assert!(!back("http://127.0.0.1:4000/other?code=c"));
        assert!(!back("https://127.0.0.1:4000/callback?code=c"));
    }

    #[tokio::test]
    async fn the_listener_hands_over_the_callback_and_shows_the_outcome() {
        use tokio::io::AsyncReadExt;
        let (listener, redirect) = bind_loopback(None).await.unwrap();
        let stop = CancellationToken::new();
        let (mut rx, _serving) =
            serve_loopback(listener, redirect.clone(), "docs".into(), stop.clone());
        let addr = redirect
            .trim_start_matches("http://")
            .trim_end_matches(CALLBACK_PATH)
            .to_string();

        let get = |path: &'static str| {
            let addr = addr.clone();
            async move {
                let mut s = TcpStream::connect(addr).await.unwrap();
                s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).await.unwrap();
                out
            }
        };

        assert!(get("/favicon.ico").await.starts_with("HTTP/1.1 404"));

        let browser = tokio::spawn(get("/callback?code=c1&state=s1"));
        let callback = rx.recv().await.unwrap();
        assert_eq!(callback.url, format!("{redirect}?code=c1&state=s1"));
        callback.reply.send(Err("state mismatch".into())).unwrap();
        let page = browser.await.unwrap();
        assert!(page.starts_with("HTTP/1.1 400"), "{page}");
        assert!(page.contains("state mismatch"), "{page}");

        let browser = tokio::spawn(get("/callback?code=c2&state=s2"));
        let callback = rx.recv().await.unwrap();
        callback.reply.send(Ok("docs".into())).unwrap();
        let page = browser.await.unwrap();
        assert!(page.contains("Signed in to docs"), "{page}");
        assert!(
            rx.recv().await.is_none(),
            "the listener stops after a sign-in"
        );
        stop.cancel();
    }
}
