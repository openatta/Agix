//! The allowlist that binds the model does not bind the model's own endpoint.
//!
//! `sandbox.allowed_domains` answers "where may the model reach". A model API
//! is where the model *is*, chosen by whoever deployed this engine — bind that
//! to the same list and the first person who writes one takes the agent's
//! brain away. Both wire protocols are driven here because the mistake is one
//! constant per protocol and each would have to make it separately.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use base::context::config::NetworkModeConfig;
use base::interface::exec::local::LocalNetwork;
use base::interface::exec::{Network, Origin};
use base::interface::model::{Model, ModelError, StreamParams};
use base::settings::ThinkingMode;
use futures::StreamExt;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use model::error::AnthropicError;
use model::types::MessagesRequest;
use model::OpenAICompatibleModel;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A server that rejects every request, so reaching it is provable from the
/// error alone: only a request that actually arrived can come back a 401.
async fn a_server_that_answers_401() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const RESPONSE: &[u8] = b"HTTP/1.1 401 Unauthorized\r\n\
content-type: text/plain\r\n\
content-length: 17\r\n\
connection: close\r\n\
\r\n\
served-and-denied";

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(RESPONSE).await;
            let _ = sock.shutdown().await;
        }
    });
    port
}

/// An egress whose allowlist deliberately excludes the host the test server
/// runs on.
fn allowlist_excluding_loopback() -> Arc<dyn Network> {
    let net = LocalNetwork::for_agent_policy(
        NetworkModeConfig::Allowlist,
        vec!["nothing-here.test".into()],
    );
    assert!(
        !net.permits("127.0.0.1", Origin::Agent),
        "this whole file is vacuous unless the test server's host really is \
         off the model-directed allowlist"
    );
    Arc::new(net)
}

#[tokio::test]
async fn the_messages_api_is_reachable_from_behind_an_allowlist() {
    let port = a_server_that_answers_401().await;
    let client = HttpAnthropicClient::with_base(
        AuthMode::ApiKey("k".into()),
        url::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
    )
    .unwrap()
    .with_network(allowlist_excluding_loopback());

    let mut events = client.stream_messages(MessagesRequest::minimal("m", "hi"));
    match events.next().await {
        Some(Err(AnthropicError::Auth(body))) => assert!(
            body.contains("served-and-denied"),
            "the server's own answer came back: {body}"
        ),
        other => panic!(
            "the request never reached the endpoint — an operator-configured \
             model API must not be subject to the model's allowlist: {other:?}"
        ),
    }
}

#[tokio::test]
async fn chat_completions_is_reachable_from_behind_an_allowlist() {
    let port = a_server_that_answers_401().await;
    let m = OpenAICompatibleModel::new(&format!("http://127.0.0.1:{port}"), "k", "gpt-4o")
        .unwrap()
        .with_network(allowlist_excluding_loopback());

    let params = StreamParams {
        model: String::new(),
        max_tokens: 256,
        thinking_mode: ThinkingMode::Off,
        fallback_model: None,
        cache_edits: Vec::new(),
        origin: None,
        input_map: None,
        provider_search: None,
    };
    let mut stream = m
        .stream(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            params,
            CancellationToken::new(),
        )
        .await
        .expect("the request is built before it is sent");

    match stream.next().await {
        Some(Err(ModelError::Auth(body))) => assert!(
            body.contains("served-and-denied"),
            "the server's own answer came back: {body}"
        ),
        other => panic!(
            "the request never reached the endpoint — an operator-configured \
             model API must not be subject to the model's allowlist: {other:?}"
        ),
    }
}

/// The other half of the same claim: the egress this endpoint goes out through
/// is a real one, and it does refuse what the model picked.
#[tokio::test]
async fn the_same_egress_still_refuses_a_model_directed_request() {
    let net = allowlist_excluding_loopback();
    let err = net
        .send(
            base::interface::exec::HttpRequest::get("http://127.0.0.1:1/"),
            Origin::Agent,
        )
        .await
        .expect_err("the host is off the list");
    assert!(
        matches!(err, base::interface::exec::ExecError::Denied(_)),
        "got {err:?}"
    );
}
