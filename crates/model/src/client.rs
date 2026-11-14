//! AnthropicClient trait + HttpAnthropicClient（走执行层 `Network` 的实现）。
//!
//! 重试 / beta header / 多 provider 都在 HttpAnthropicClient 内部；
//! Engine 只看 trait。

use crate::error::{classify, error_type, AnthropicError};
use crate::parser::parse_sse;
use crate::retry_after::parse_retry_after;
use crate::stream::StreamEvent;
use crate::types::MessagesRequest;
use base::interface::backoff::{
    Backoff, BackoffPolicy, FailedAttempt, LadderBackoff, RequestFailure,
};
use base::interface::exec::{HttpRequest, HttpStream, Network, Origin};
use futures::stream::{Stream, StreamExt};
use platform::LocalNetwork;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use url::Url;

/// Anthropic API base URL.
pub const ANTHROPIC_API_BASE_URL: &str = "https://api.anthropic.com/";

/// SSE 流两个事件之间允许的最大静默时间。响应头拿到之后（`send_with_retry` 已经
/// 成功返回），流本身**没有任何超时保护**——如果底层连接在中途静默卡死（代理/
/// 网络吞掉了后续字节但没有发 TCP RST/FIN，连接不报错也不结束），`events.next()`
/// 会永远 pending，调用方（`crates/runtime/src/agent/turn/` 的主循环）没有任何手段
/// 感知到这一点，整个 turn 就此挂死——不报错、不超时、CPU 占用 0%，几乎无法从
/// 外部区分"模型在正常思考"和"连接已经死了"。这个常量把这种情况转成一个明确的
/// 、可重试的 `AnthropicError::Timeout`，而不是无限等待。
/// 90s 留了足够余量给正常的长 thinking 停顿（比如大 context/推理模型），比大多数
/// LLM 单个 SSE chunk 间隔要长得多。
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// 从已解析的 SSE 事件流里读下一项，最多等 `idle_timeout`——超时说明连接静默
/// 卡死了（见 `STREAM_IDLE_TIMEOUT` 文档），返回一个 `Timeout` 错误项
/// 而不是让调用方永远 pending。抽成独立函数方便直接单测，不用起真实 HTTP。
async fn next_with_idle_timeout<S>(
    events: &mut S,
    idle_timeout: Duration,
) -> Option<Result<StreamEvent, AnthropicError>>
where
    S: Stream<Item = Result<StreamEvent, AnthropicError>> + Unpin,
{
    match tokio::time::timeout(idle_timeout, events.next()).await {
        Ok(next) => next,
        Err(_elapsed) => Some(Err(AnthropicError::Timeout(format!(
            "stream idle for {}s",
            idle_timeout.as_secs()
        )))),
    }
}

/// 把响应按状态码分流为 Ok(continue) / Err(分类后的 AnthropicError)。
/// 抽到独立 fn：try_stream! 宏内的 ? 不能让借用检查器看出"err 分支总返回"。
async fn classify_response(resp: HttpStream) -> Result<HttpStream, AnthropicError> {
    let code = resp.status;
    if (200..300).contains(&code) {
        return Ok(resp);
    }
    let retry_after = parse_retry_after(&resp.headers, SystemTime::now());
    let mut anthropic_headers = HashMap::new();
    for (name, value) in &resp.headers {
        let n = name.to_ascii_lowercase();
        if n.starts_with("anthropic-ratelimit-") || n == "retry-after" {
            anthropic_headers.insert(n, value.clone());
        }
    }
    // 截掉超长 body 防止日志爆炸
    let body = drain_to_string(resp.body).await;
    let body = if body.len() > 4096 {
        body.chars().take(4096).collect()
    } else {
        body
    };
    use base::interface::model::ModelErrorKind;
    Err(
        match classify(Some(code), error_type(&body).as_deref(), &body) {
            ModelErrorKind::Auth => AnthropicError::Auth(body),
            ModelErrorKind::RateLimited => AnthropicError::RateLimited {
                retry_after,
                headers: anthropic_headers,
            },
            ModelErrorKind::Overloaded => AnthropicError::Overloaded {
                status: code,
                retry_after,
            },
            _ => AnthropicError::Server {
                status: code,
                body,
                retry_after,
            },
        },
    )
}

/// 把一个还没读的响应体读成字符串。错误当作提前结束——这里只用于错误 body，
/// 读到多少算多少比再报一个覆盖掉状态码的错更有用。
async fn drain_to_string(
    mut body: futures::stream::BoxStream<
        'static,
        Result<Vec<u8>, base::interface::exec::ExecError>,
    >,
) -> String {
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(c) => buf.extend_from_slice(&c),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// 一次 HTTP send + classify。失败时按 AnthropicError 分类返回。
async fn send_one(inner: &HttpInner, req: &MessagesRequest) -> Result<HttpStream, AnthropicError> {
    let body = serde_json::to_vec(req)?;

    let mut builder = HttpRequest::post(inner.endpoint.as_str())
        .header("anthropic-version", inner.anthropic_version)
        .header("content-type", "application/json");

    // Collect beta tags from BOTH the request (per-call
    // explicit) AND the auth mode (e.g. OAuth flows need
    // `anthropic-beta: oauth-2025-04-20` set unconditionally). De-dup before
    // joining.
    let mut betas: Vec<String> = req.betas.clone();
    match &inner.auth {
        AuthMode::ApiKey(k) => builder = builder.header("x-api-key", k),
        AuthMode::OauthToken(t) => {
            builder = builder.header("authorization", format!("Bearer {t}"));
            push_unique(&mut betas, "oauth-2025-04-20");
        }
        AuthMode::OauthRefreshing(provider) => {
            let token = provider.current_bearer_token().await?;
            builder = builder.header("authorization", format!("Bearer {token}"));
            push_unique(&mut betas, "oauth-2025-04-20");
        }
    }

    if !betas.is_empty() {
        builder = builder.header("anthropic-beta", betas.join(","));
    }

    // `Origin::Operator`: this endpoint is the deployment's own configuration.
    // Binding it to `allowed_domains` would take the agent's model away from
    // it the moment anyone wrote a domain list.
    let resp = inner.net.open(builder.body(body), Origin::Operator).await?;
    classify_response(resp).await
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

/// 一次响应读到的第一个事件，以及它之后的事件流。
type Opened = (
    Option<Result<StreamEvent, AnthropicError>>,
    crate::parser::ParsedStream,
);

/// 200 响应的开头就是失败：第一个事件是 `error`，或者 body 是空的。这时还什么都
/// 没交给调用方，按首字节前的失败一样交给退避策略。
fn failed_opening(
    first: &Option<Result<StreamEvent, AnthropicError>>,
) -> Option<(RequestFailure, String)> {
    match first {
        None => Some((RequestFailure::Transport, "empty response body".into())),
        Some(Ok(StreamEvent::Error { error })) => Some((
            classify(None, Some(&error.kind), &error.message).request_failure(),
            format!("error event {}: {}", error.kind, error.message),
        )),
        Some(_) => None,
    }
}

/// 在首字节前重试：等多久、还试不试由 `BackoffPolicy` 决定；
/// `retry-after` 头的解析留在本协议这一侧，作为提示交给策略。
/// 放弃时原样交回最后一次的结果，开头的 `error` 事件仍由调用方按事件处理。
async fn send_with_retry(
    inner: &HttpInner,
    req: &MessagesRequest,
) -> Result<Opened, AnthropicError> {
    let mut attempt = 0u32;
    loop {
        // `send_one` 的 `.send().await` 只有 TCP connect_timeout（30s）——连上之后
        // 服务端/代理如果吃掉请求、永远不回任何字节，这一步会无限期挂着，跟
        // `STREAM_IDLE_TIMEOUT` 保护的"连上了但流中途卡死"是同一类问题的另一半
        // （见该常量的文档）。用同样的超时把它转成一个明确的错误。
        let attempt_result =
            match tokio::time::timeout(STREAM_IDLE_TIMEOUT, send_one(inner, req)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(AnthropicError::Timeout(
                    "no response headers within the idle window".into(),
                )),
            };
        let (failure, server_hint, error, outcome) = match attempt_result {
            Ok(resp) => {
                let byte_stream = resp
                    .body
                    .map(|c| c.map(bytes::Bytes::from).map_err(AnthropicError::from));
                let mut events = parse_sse(byte_stream);
                let first = next_with_idle_timeout(&mut events, STREAM_IDLE_TIMEOUT).await;
                let Some((failure, error)) = failed_opening(&first) else {
                    return Ok((first, events));
                };
                (failure, None, error, Ok((first, events)))
            }
            Err(e) => (
                e.kind().request_failure(),
                e.retry_after(),
                e.to_string(),
                Err(e),
            ),
        };
        let verdict = inner.backoff.after_failure(&FailedAttempt {
            index: attempt,
            failure,
            server_hint,
        });
        match verdict {
            Backoff::GiveUp => return outcome,
            Backoff::WaitThenRetry(delay) => {
                tracing::warn!(
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %error,
                    "anthropic request retryable; backing off"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

pub type EventStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, AnthropicError>> + Send + 'static>>;

pub type CountFuture<'a> = Pin<Box<dyn Future<Output = Result<usize, AnthropicError>> + Send + 'a>>;

/// 不上 `#[async_trait]` —— 流式返回 `Box<dyn Stream>` 比 async_trait 改写
/// 出的 future-of-stream 更直观，也省一层 box。
pub trait AnthropicClient: Send + Sync {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream;

    fn count_tokens<'a>(&'a self, req: &'a MessagesRequest) -> CountFuture<'a>;
}

#[derive(Clone)]
pub enum AuthMode {
    /// `x-api-key: <token>`（标准）
    ApiKey(String),
    /// `authorization: Bearer <token>`（OAuth / Anthropic session token，静态）
    OauthToken(String),
    /// Dynamic OAuth token. Each request consults the provider,
    /// which may pre-emptively refresh if the token is about to expire and
    /// write the new token back to its underlying store.
    OauthRefreshing(Arc<dyn BearerTokenProvider>),
}

impl std::fmt::Debug for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("AuthMode::ApiKey(<redacted>)"),
            Self::OauthToken(_) => f.write_str("AuthMode::OauthToken(<redacted>)"),
            Self::OauthRefreshing(_) => f.write_str("AuthMode::OauthRefreshing(<provider>)"),
        }
    }
}

/// Token-source abstraction. Implementors handle refresh +
/// store mutation behind the scenes; the client only ever asks for "the
/// current valid bearer token".
#[async_trait::async_trait]
pub trait BearerTokenProvider: Send + Sync {
    async fn current_bearer_token(&self) -> Result<String, AnthropicError>;
}

#[derive(Clone)]
pub struct HttpAnthropicClient {
    inner: Arc<HttpInner>,
}

struct HttpInner {
    net: Arc<dyn Network>,
    endpoint: Url,
    auth: AuthMode,
    anthropic_version: &'static str,
    backoff: Arc<dyn BackoffPolicy>,
}

/// Messages API 的完整地址。配置里有人写到主机为止，有人照着文档写到 `/v1`，
/// 也有人写全了端点，三种都落到同一个 `…/v1/messages` 上；主机后面的前缀（中转
/// 站常见的 `/anthropic`）原样保留。
pub fn messages_url(base: &Url) -> Result<Url, AnthropicError> {
    if base.cannot_be_a_base() {
        return Err(AnthropicError::ParseError(format!(
            "base url '{base}' cannot carry a path"
        )));
    }
    let path = base.path().trim_end_matches('/');
    let path = if path.ends_with("/v1/messages") {
        path.to_string()
    } else if path.ends_with("/v1") {
        format!("{path}/messages")
    } else {
        format!("{path}/v1/messages")
    };
    let mut url = base.clone();
    url.set_path(&path);
    Ok(url)
}

impl HttpAnthropicClient {
    /// 创建一个默认指向 `https://api.anthropic.com` 的 client。
    pub fn new(auth: AuthMode) -> Result<Self, AnthropicError> {
        #[allow(clippy::unwrap_used)]
        let base = Url::parse(ANTHROPIC_API_BASE_URL).unwrap();
        Self::with_base(auth, base)
    }

    /// 自定义 base URL（用于本地 mock server / Bedrock relay 等）。写成
    /// `https://host`、`https://host/v1` 还是 `https://host/v1/messages` 都行，
    /// 见 [`messages_url`]。
    pub fn with_base(auth: AuthMode, base: Url) -> Result<Self, AnthropicError> {
        Ok(Self {
            inner: Arc::new(HttpInner {
                net: Arc::new(LocalNetwork::default()),
                endpoint: messages_url(&base)?,
                auth,
                anthropic_version: "2023-06-01",
                backoff: Arc::new(LadderBackoff::new()),
            }),
        })
    }

    /// 换掉出口。宿主要审计、限速或整体离线时，模型 API 也要走它那一条。
    pub fn with_network(mut self, net: Arc<dyn Network>) -> Self {
        let mut inner = (*self.inner).clone_with_backoff(self.inner.backoff.clone());
        inner.net = net;
        self.inner = Arc::new(inner);
        self
    }

    /// 自定义退避序列（毫秒）。测试用——缩短重试间隔避免拖长 CI。
    pub fn with_backoff(self, backoff_ms: Vec<u64>) -> Self {
        self.with_backoff_policy(Arc::new(LadderBackoff::from_millis(backoff_ms)))
    }

    /// 换掉整条退避策略。宿主自己实现 `ModelFactory` 时的接入点。
    pub fn with_backoff_policy(mut self, backoff: Arc<dyn BackoffPolicy>) -> Self {
        let inner = (*self.inner).clone_with_backoff(backoff);
        self.inner = Arc::new(inner);
        self
    }
}

impl HttpInner {
    fn clone_with_backoff(&self, backoff: Arc<dyn BackoffPolicy>) -> Self {
        Self {
            net: self.net.clone(),
            endpoint: self.endpoint.clone(),
            auth: self.auth.clone(),
            anthropic_version: self.anthropic_version,
            backoff,
        }
    }
}

impl AnthropicClient for HttpAnthropicClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        let inner = self.inner.clone();

        Box::pin(async_stream::try_stream! {
            // 重试只在第一个事件之前做；流中错（或中途的 error 事件）不重试 —— 避免重复计 token。
            let (first, mut events) = send_with_retry(&inner, &req).await?;
            if let Some(first) = first {
                yield first?;
                while let Some(ev) = next_with_idle_timeout(&mut events, STREAM_IDLE_TIMEOUT).await {
                    yield ev?;
                }
            }
        })
    }

    fn count_tokens<'a>(&'a self, req: &'a MessagesRequest) -> CountFuture<'a> {
        // 本地字符类估算（Anthropic 不公开精确 tokenizer）。
        // 用于 autoCompact 的阈值检查，足够。
        // 实际精确计 token 走 Anthropic /v1/messages/count_tokens 端点 —— 推迟到 。
        Box::pin(async move { Ok(crate::tokens::estimate_input_tokens(req)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// trait object 可以装 —— 这是给 Engine 用的方式。
    #[test]
    fn boxes_into_trait_object() {
        let c = HttpAnthropicClient::new(AuthMode::ApiKey("dummy".into())).unwrap();
        let _: Arc<dyn AnthropicClient> = Arc::new(c);
    }

    struct SilentNetwork(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl Network for SilentNetwork {
        async fn send(
            &self,
            _req: HttpRequest,
            _origin: Origin,
        ) -> Result<base::interface::exec::HttpResponse, base::interface::exec::ExecError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            futures::future::pending().await
        }

        fn permits(&self, _host: &str, _origin: Origin) -> bool {
            true
        }
    }

    /// Answers every request with 200 and the next scripted body.
    struct ScriptedOk {
        bodies: std::sync::Mutex<Vec<&'static str>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Network for ScriptedOk {
        async fn send(
            &self,
            _req: HttpRequest,
            _origin: Origin,
        ) -> Result<base::interface::exec::HttpResponse, base::interface::exec::ExecError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body = self.bodies.lock().unwrap().remove(0);
            Ok(base::interface::exec::HttpResponse {
                status: 200,
                headers: Default::default(),
                body: body.as_bytes().to_vec(),
            })
        }

        fn permits(&self, _host: &str, _origin: Origin) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn a_200_that_opens_with_a_retryable_error_event_or_nothing_is_retried() {
        const OK: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";
        const OVERLOADED: &str = "event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n";
        const INVALID: &str = "event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad\"}}\n\n";

        async fn run(
            bodies: Vec<&'static str>,
        ) -> (Vec<Result<StreamEvent, AnthropicError>>, usize) {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let client = HttpAnthropicClient::new(AuthMode::ApiKey("k".into()))
                .unwrap()
                .with_network(Arc::new(ScriptedOk {
                    bodies: std::sync::Mutex::new(bodies),
                    calls: calls.clone(),
                }))
                .with_backoff(vec![1, 1]);
            let events = client
                .stream_messages(MessagesRequest::minimal("m", "hi"))
                .collect()
                .await;
            (events, calls.load(std::sync::atomic::Ordering::SeqCst))
        }

        for opening in [OVERLOADED, ""] {
            let (events, calls) = run(vec![opening, OK]).await;
            assert_eq!(calls, 2, "{opening:?} was not retried");
            assert!(
                matches!(events.last(), Some(Ok(StreamEvent::MessageStop))),
                "{events:?}"
            );
        }

        let (events, calls) = run(vec![INVALID, OK]).await;
        assert_eq!(calls, 1, "an invalid request is not sent again");
        assert!(matches!(
            events.as_slice(),
            [Ok(StreamEvent::Error { error })] if error.kind == "invalid_request_error"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn no_response_headers_within_the_window_is_retried() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let client = HttpAnthropicClient::new(AuthMode::ApiKey("k".into()))
            .unwrap()
            .with_network(Arc::new(SilentNetwork(calls.clone())))
            .with_backoff(vec![1, 1]);
        let mut events = client.stream_messages(MessagesRequest::minimal("m", "hi"));
        assert!(matches!(events.next().await, Some(Err(_))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// 复现导致 000.c_project 真实录制卡死 150s+ 的场景：底层连接静默卡死（不报
    /// 错、不结束流），没有这层超时的话 `events.next()` 会永远 pending。用
    /// `start_paused` 让 90s 的等待在测试里瞬间"流逝"，不用真的等。
    #[tokio::test(start_paused = true)]
    async fn idle_timeout_fires_when_stream_goes_silent_after_first_event() {
        let mut events = futures::stream::iter(vec![Ok(StreamEvent::MessageStop)])
            .chain(futures::stream::pending());

        let first = next_with_idle_timeout(&mut events, Duration::from_secs(90)).await;
        assert!(matches!(first, Some(Ok(StreamEvent::MessageStop))));

        // 底层流之后再也不产出任何东西（模拟连接静默卡死）——第二次读应该在
        // idle_timeout 后返回 Timeout，而不是永远 pending。
        let second = next_with_idle_timeout(&mut events, Duration::from_secs(90)).await;
        assert!(
            matches!(second, Some(Err(AnthropicError::Timeout(_)))),
            "expected Timeout after idle timeout, got: {second:?}"
        );
    }

    /// 正常情况：流在超时窗口内就正常结束（`None`），不应该被误判为超时。
    #[tokio::test]
    async fn idle_timeout_does_not_fire_when_stream_ends_normally() {
        let mut events = futures::stream::iter(vec![Ok(StreamEvent::MessageStop)]);

        let first = next_with_idle_timeout(&mut events, Duration::from_secs(90)).await;
        assert!(matches!(first, Some(Ok(StreamEvent::MessageStop))));

        let second = next_with_idle_timeout(&mut events, Duration::from_secs(90)).await;
        assert!(
            second.is_none(),
            "stream ended normally, expected None, got: {second:?}"
        );
    }

    #[test]
    fn every_way_of_writing_the_base_reaches_the_same_endpoint() {
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/",
            "https://api.anthropic.com/v1/messages",
            "https://api.anthropic.com/v1/messages/",
        ] {
            let url = messages_url(&Url::parse(base).unwrap()).unwrap();
            assert_eq!(
                url.as_str(),
                "https://api.anthropic.com/v1/messages",
                "{base}"
            );
        }
        for base in [
            "https://relay.test/anthropic",
            "https://relay.test/anthropic/v1",
        ] {
            let url = messages_url(&Url::parse(base).unwrap()).unwrap();
            assert_eq!(
                url.as_str(),
                "https://relay.test/anthropic/v1/messages",
                "{base}"
            );
        }
        assert!(messages_url(&Url::parse("data:text/plain,x").unwrap()).is_err());
    }

    /// 没有 ANTHROPIC_API_KEY / 没有网络也不能 panic。仅检查 URL 拼写 +
    /// header 装载逻辑能编过。实际 HTTP 调用在 integration test 里走 mock server。
    #[test]
    fn build_http_client_does_not_panic() {
        let c = HttpAnthropicClient::new(AuthMode::ApiKey("x".into()));
        assert!(c.is_ok());
        let c2 = HttpAnthropicClient::with_base(
            AuthMode::OauthToken("y".into()),
            Url::parse("http://127.0.0.1:1/").unwrap(),
        );
        assert!(c2.is_ok());
    }
}
