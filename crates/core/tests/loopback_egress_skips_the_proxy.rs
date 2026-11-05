//! An `HTTP_PROXY` in the environment means "reach the internet through this
//! egress", not "also proxy calls to my own machine". A deployment pointing at
//! a local Ollama or vLLM cannot reach it if the ambient proxy is applied, and
//! the proxy cannot reach back into a loopback port on the client's host.
//!
//! Its own test binary on purpose: the process-wide clients read the proxy
//! environment once, when they are first built, so a test that sets it has to
//! be the only thing in the process that could trigger that build.

#![cfg(feature = "http")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use base::interface::exec::local::LocalNetwork;
use base::interface::exec::{HttpRequest, Network, Origin};

/// A server answering every request with `body`, whatever was asked for.
/// Standing in for an HTTP proxy is exactly this: a plain-HTTP proxy request
/// differs only in the request line, which this never reads.
async fn a_server_saying(body: &'static str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    port
}

#[tokio::test]
async fn a_loopback_target_is_reached_directly_while_the_proxy_still_applies() {
    let proxy = a_server_saying("through-the-proxy").await;
    let origin = a_server_saying("straight-to-the-origin").await;

    std::env::set_var("HTTP_PROXY", format!("http://127.0.0.1:{proxy}"));
    let net = LocalNetwork::default();

    let remote = net
        .send(HttpRequest::get("http://example.com/"), Origin::Operator)
        .await
        .expect("the proxy answers everything");
    assert_eq!(
        remote.body_text(),
        "through-the-proxy",
        "without the ambient proxy actually in effect this test proves nothing \
         about bypassing it"
    );

    let local = net
        .send(
            HttpRequest::get(format!("http://127.0.0.1:{origin}/")),
            Origin::Operator,
        )
        .await
        .expect("a loopback endpoint on this machine");
    assert_eq!(
        local.body_text(),
        "straight-to-the-origin",
        "a loopback endpoint went through the ambient proxy — a local Ollama \
         or vLLM is unreachable from any host that sets HTTP_PROXY"
    );
}
