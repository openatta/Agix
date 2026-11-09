//! Both wire protocols back off through the same policy object.
//!
//! The two clients each grew their own retry loop, and nothing but a comment
//! claimed they matched. A test that lives inside either module can only see
//! that module, which is how they drifted. So this one drives both against a
//! local server that refuses everything, hands both the *same*
//! [`BackoffPolicy`] instance, and compares what each one asked it.
//!
//! The turn-level replay net cannot reach here: it substitutes a scripted
//! `Model`, which sits above both of these.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use base::interface::backoff::{Backoff, BackoffPolicy, FailedAttempt, NoBackoff, RequestFailure};
use base::interface::model::{Model, ModelError, StreamParams};
use base::settings::ThinkingMode;
use futures::StreamExt;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use model::types::MessagesRequest;
use model::OpenAICompatibleModel;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Records every question it is asked, then allows `retries` of them.
struct Recorder {
    seen: Mutex<Vec<FailedAttempt>>,
    retries: u32,
}

impl Recorder {
    fn new(retries: u32) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            retries,
        })
    }

    fn recorded(&self) -> Vec<FailedAttempt> {
        self.seen.lock().unwrap().clone()
    }
}

impl BackoffPolicy for Recorder {
    fn after_failure(&self, attempt: &FailedAttempt) -> Backoff {
        self.seen.lock().unwrap().push(*attempt);
        if attempt.index < self.retries {
            Backoff::WaitThenRetry(Duration::from_millis(1))
        } else {
            Backoff::GiveUp
        }
    }
}

/// A server that answers every request with 429, naming a seven-second
/// `retry-after`. `connection: close` keeps one request to one accept, which
/// is what makes the counter meaningful.
async fn always_rate_limited() -> (u16, Arc<AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const RESPONSE: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\n\
retry-after: 7\r\n\
anthropic-ratelimit-requests-remaining: 0\r\n\
content-type: text/plain\r\n\
content-length: 5\r\n\
connection: close\r\n\
\r\n\
limit";

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    let counter = served.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(RESPONSE).await;
            let _ = sock.shutdown().await;
        }
    });
    (port, served)
}

/// Drive the Anthropic client until it gives up. Returns how many requests the
/// server saw.
async fn run_anthropic(policy: Arc<dyn BackoffPolicy>) -> usize {
    let (port, served) = always_rate_limited().await;
    let client = HttpAnthropicClient::with_base(
        AuthMode::ApiKey("k".into()),
        url::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
    )
    .unwrap()
    .with_backoff_policy(policy);

    let mut events = client.stream_messages(MessagesRequest::minimal("m", "hi"));
    let first = events.next().await;
    assert!(
        matches!(first, Some(Err(_))),
        "a server that only rate-limits must end in an error, got {first:?}"
    );
    served.load(Ordering::SeqCst)
}

/// The same, through the OpenAI-compatible protocol.
async fn run_openai(policy: Arc<dyn BackoffPolicy>) -> usize {
    let (port, served) = always_rate_limited().await;
    let m = OpenAICompatibleModel::new(&format!("http://127.0.0.1:{port}"), "k", "gpt-4o")
        .unwrap()
        .with_backoff_policy(policy);

    let params = StreamParams {
        model: String::new(),
        max_tokens: 256,
        thinking_mode: ThinkingMode::Off,
        effort: None,
        fallback_model: None,
        cache_edits: Vec::new(),
        origin: None,
        input_map: None,
        provider_search: None,
    };
    let answer = m
        .stream(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            params,
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(answer, Err(ModelError::RateLimited)),
        "a server that only rate-limits must end in an error, got {:?}",
        answer.err()
    );
    served.load(Ordering::SeqCst)
}

/// The parity claim, stated as one assertion each about the questions asked
/// and the answers obeyed.
#[tokio::test]
async fn both_protocols_ask_one_policy_the_same_questions() {
    let anthropic_policy = Recorder::new(2);
    let openai_policy = Recorder::new(2);

    let anthropic_requests = run_anthropic(anthropic_policy.clone()).await;
    let openai_requests = run_openai(openai_policy.clone()).await;

    let a = anthropic_policy.recorded();
    let o = openai_policy.recorded();

    assert_eq!(
        a.len(),
        o.len(),
        "the policy was consulted a different number of times per protocol"
    );
    assert_eq!(a.len(), 3, "two allowed retries means three failures");
    assert_eq!(
        anthropic_requests, openai_requests,
        "the same policy allowed a different number of attempts per protocol"
    );
    assert_eq!(anthropic_requests, 3);

    for (i, (a, o)) in a.iter().zip(o.iter()).enumerate() {
        assert_eq!(a.index, i as u32, "anthropic attempt {i} misnumbered");
        assert_eq!(o.index, i as u32, "openai attempt {i} misnumbered");
        assert_eq!(
            a.failure,
            RequestFailure::RateLimited,
            "a 429 is a rate limit on both sides"
        );
        assert_eq!(o.failure, RequestFailure::RateLimited);
    }
}

/// The one thing the two protocols legitimately differ on. `retry-after` is
/// part of the Messages API's wire format and has no counterpart in Chat
/// Completions, so parsing it stays in the Anthropic client — but it arrives
/// as an input to the shared policy rather than as a second retry rule.
#[tokio::test]
async fn only_the_protocol_that_has_a_retry_after_header_supplies_one() {
    let anthropic_policy = Recorder::new(1);
    let openai_policy = Recorder::new(1);

    run_anthropic(anthropic_policy.clone()).await;
    run_openai(openai_policy.clone()).await;

    let a = anthropic_policy.recorded();
    let o = openai_policy.recorded();
    // `all` over an empty recording is vacuously true, which would let a
    // protocol that never consults the policy pass this.
    assert_eq!(
        (a.len(), o.len()),
        (2, 2),
        "one allowed retry means two failures on each side"
    );

    assert!(
        a.iter()
            .all(|a| a.server_hint == Some(Duration::from_secs(7))),
        "the retry-after header the server sent never reached the policy"
    );
    assert!(
        o.iter().all(|o| o.server_hint.is_none()),
        "chat completions has no retry-after to report"
    );
}

/// The policy decides whether to try again at all, not only how long to wait —
/// on both sides.
#[tokio::test]
async fn a_policy_that_refuses_to_wait_stops_both_protocols_after_one_attempt() {
    assert_eq!(run_anthropic(Arc::new(NoBackoff)).await, 1);
    assert_eq!(run_openai(Arc::new(NoBackoff)).await, 1);
}

/// A server that answers its first request with a bare 502, as a gateway does
/// when it cannot reach the provider, and every later one with `ok`.
async fn bad_gateway_once(ok: &'static [u8]) -> (u16, Arc<AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const BAD_GATEWAY: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\n\
content-type: text/html\r\n\
content-length: 12\r\n\
connection: close\r\n\
\r\n\
<h1>502</h1>";

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    let counter = served.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(if n == 0 { BAD_GATEWAY } else { ok }).await;
            let _ = sock.shutdown().await;
        }
    });
    (port, served)
}

#[tokio::test]
async fn a_bad_gateway_before_the_stream_is_tried_again_on_both_protocols() {
    const ANTHROPIC_OK: &[u8] = b"HTTP/1.1 200 OK\r\n\
content-type: text/event-stream\r\n\
connection: close\r\n\
\r\n\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";
    const OPENAI_OK: &[u8] = b"HTTP/1.1 200 OK\r\n\
content-type: text/event-stream\r\n\
connection: close\r\n\
\r\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"recovered\"},\"finish_reason\":\"stop\"}]}\n\
\n\
data: [DONE]\n\
\n";

    let (port, served) = bad_gateway_once(ANTHROPIC_OK).await;
    let policy = Recorder::new(2);
    let client = HttpAnthropicClient::with_base(
        AuthMode::ApiKey("k".into()),
        url::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
    )
    .unwrap()
    .with_backoff_policy(policy.clone());
    let events: Vec<_> = client
        .stream_messages(MessagesRequest::minimal("m", "hi"))
        .collect()
        .await;
    assert!(
        matches!(
            events.last(),
            Some(Ok(model::stream::StreamEvent::MessageStop))
        ) && events.iter().all(Result::is_ok),
        "the retry after the 502 should have streamed cleanly: {events:?}"
    );
    assert_eq!(served.load(Ordering::SeqCst), 2);
    assert_eq!(policy.recorded()[0].failure, RequestFailure::Overloaded);

    let (port, served) = bad_gateway_once(OPENAI_OK).await;
    let policy = Recorder::new(2);
    let m = OpenAICompatibleModel::new(&format!("http://127.0.0.1:{port}"), "k", "gpt-4o")
        .unwrap()
        .with_backoff_policy(policy.clone());
    let params = StreamParams {
        model: String::new(),
        max_tokens: 256,
        thinking_mode: ThinkingMode::Off,
        effort: None,
        fallback_model: None,
        cache_edits: Vec::new(),
        origin: None,
        input_map: None,
        provider_search: None,
    };
    let stream = m
        .stream(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            params,
            CancellationToken::new(),
        )
        .await
        .expect("the retry after the 502 should have been accepted");
    let events: Vec<_> = stream.collect().await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Ok(base::interface::model::ModelEvent::TextDelta { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "recovered", "{events:?}");
    assert_eq!(served.load(Ordering::SeqCst), 2);
    assert_eq!(policy.recorded()[0].failure, RequestFailure::Overloaded);
}
