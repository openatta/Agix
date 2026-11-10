//! AnthropicClient trait + HttpAnthropicClient（走执行层 `Network` 的实现）。
//!
//! 重试 / beta header / 多 provider 都在 HttpAnthropicClient 内部；
//! Engine 只看 trait。

use crate::error::{is_transient_status, AnthropicError};
use crate::parser::parse_sse;
use crate::stream::StreamEvent;
use crate::types::MessagesRequest;
use base::interface::backoff::{
    Backoff, BackoffPolicy, FailedAttempt, LadderBackoff, RequestFailure,
};
use base::interface::exec::{HttpRequest, HttpStream, Network, Origin};
use futures::stream::{Stream, StreamExt};
use platform::LocalNetwork;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

/// Anthropic API base URL.
pub const ANTHROPIC_API_BASE_URL: &str = "https://api.anthropic.com/";

/// SSE 流两个事件之间允许的最大静默时间。响应头拿到之后（`send_with_retry` 已经
/// 成功返回），流本身**没有任何超时保护**——如果底层连接在中途静默卡死（代理/
/// 网络吞掉了后续字节但没有发 TCP RST/FIN，连接不报错也不结束），`events.next()`
/// 会永远 pending，调用方（`crates/runtime/src/turn/` 的主循环）没有任何手段
/// 感知到这一点，整个 turn 就此挂死——不报错、不超时、CPU 占用 0%，几乎无法从
/// 外部区分"模型在正常思考"和"连接已经死了"。这个常量把这种情况转成一个明确的
/// 、可重试的 `AnthropicError::StreamInterrupted`，而不是无限等待。
/// 90s 留了足够余量给正常的长 thinking 停顿（比如大 context/推理模型），比大多数
/// LLM 单个 SSE chunk 间隔要长得多。
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// 从已解析的 SSE 事件流里读下一项，最多等 `idle_timeout`——超时说明连接静默
/// 卡死了（见 `STREAM_IDLE_TIMEOUT` 文档），返回一个 `StreamInterrupted` 错误项
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
        Err(_elapsed) => Some(Err(AnthropicError::StreamInterrupted)),
    }
}

/// 把响应按状态码分流为 Ok(continue) / Err(分类后的 AnthropicError)。
/// 抽到独立 fn：try_stream! 宏内的 ? 不能让借用检查器看出"err 分支总返回"。
async fn classify_response(resp: HttpStream) -> Result<HttpStream, AnthropicError> {
    let code = resp.status;
    if (200..300).contains(&code) {
        return Ok(resp);
    }
    let retry_after = parse_retry_after(&resp.headers);
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
    // A relay in front of the API does not always keep the status, but it
    // passes the body on, and the body's `error.type` says the same thing.
    Err(match (code, error_type(&body).as_deref()) {
        (401 | 403, _) => AnthropicError::Auth(body),
        (429, _) | (_, Some("rate_limit_error")) => AnthropicError::RateLimited {
            retry_after,
            headers: anthropic_headers,
        },
        (503 | 529, _) | (_, Some("overloaded_error")) => {
            AnthropicError::Overloaded { status: code }
        }
        _ => AnthropicError::Server { status: code, body },
    })
}

/// `error.type` of an error body shaped `{"type":"error","error":{"type":…}}`.
fn error_type(body: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(body.get("error")?.get("type")?.as_str()?.to_string())
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

fn parse_retry_after(headers: &BTreeMap<String, String>) -> Option<Duration> {
    let v = headers.get("retry-after")?;
    // RFC 7231 retry-after：要么数字秒数，要么 HTTP 时间。我们只支持数字秒数。
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
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

/// 把 `AnthropicError` 翻译成退避策略的词汇。这里就是"哪些错误还值得再发一次"
/// 的唯一答案——落进 `Other` 的策略一律不重试。
fn failure_kind(e: &AnthropicError) -> RequestFailure {
    match e {
        AnthropicError::Transport(_) => RequestFailure::Transport,
        AnthropicError::RateLimited { .. } => RequestFailure::RateLimited,
        AnthropicError::Overloaded { .. } => RequestFailure::Overloaded,
        AnthropicError::Server { status, .. } if is_transient_status(*status) => {
            RequestFailure::Overloaded
        }
        _ => RequestFailure::Other,
    }
}

/// 在首字节前重试：等多久、还试不试由 `BackoffPolicy` 决定；
/// `retry-after` 头的解析留在本协议这一侧，作为提示交给策略。
async fn send_with_retry(
    inner: &HttpInner,
    req: &MessagesRequest,
) -> Result<HttpStream, AnthropicError> {
    let mut attempt = 0u32;
    loop {
        // `send_one` 的 `.send().await` 只有 TCP connect_timeout（30s）——连上之后
        // 服务端/代理如果吃掉请求、永远不回任何字节，这一步会无限期挂着，跟
        // `STREAM_IDLE_TIMEOUT` 保护的"连上了但流中途卡死"是同一类问题的另一半
        // （见该常量的文档）。用同样的超时把它转成一个明确的错误。
        let attempt_result =
            match tokio::time::timeout(STREAM_IDLE_TIMEOUT, send_one(inner, req)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(AnthropicError::StreamInterrupted),
            };
        let e = match attempt_result {
            Ok(resp) => return Ok(resp),
            Err(e) => e,
        };
        let server_hint = match &e {
            AnthropicError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        };
        let verdict = inner.backoff.after_failure(&FailedAttempt {
            index: attempt,
            failure: failure_kind(&e),
            server_hint,
        });
        match verdict {
            Backoff::GiveUp => return Err(e),
            Backoff::WaitThenRetry(delay) => {
                tracing::warn!(
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
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
            // 重试只在**首字节前**做：classify_response 拿到 Ok 之后开始流式，
            // 流中错（或服务端 error 事件）不重试 —— 避免重复计 token。
            let resp = send_with_retry(&inner, &req).await?;

            let byte_stream = resp
                .body
                .map(|c| c.map(bytes::Bytes::from).map_err(AnthropicError::from));

            let mut events = parse_sse(byte_stream);
            while let Some(ev) = next_with_idle_timeout(&mut events, STREAM_IDLE_TIMEOUT).await {
                yield ev?;
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
        // idle_timeout 后返回 StreamInterrupted，而不是永远 pending。
        let second = next_with_idle_timeout(&mut events, Duration::from_secs(90)).await;
        assert!(
            matches!(second, Some(Err(AnthropicError::StreamInterrupted))),
            "expected StreamInterrupted after idle timeout, got: {second:?}"
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

    /// 哪些错误还值得再发一次，用共享策略读得懂的词汇表达。`Other` 是默认策略
    /// 一律不重试的那一类——注意 `StreamInterrupted` 在这一类里：首字节前的静默
    /// 超时不重试，因为分不清服务端是没收到请求还是已经在算了。
    #[test]
    fn only_pre_stream_failures_classify_as_retryable_kinds() {
        assert_eq!(
            failure_kind(&AnthropicError::Transport(anyhow::anyhow!("reset"))),
            RequestFailure::Transport
        );
        assert_eq!(
            failure_kind(&AnthropicError::RateLimited {
                retry_after: None,
                headers: HashMap::new(),
            }),
            RequestFailure::RateLimited
        );
        assert_eq!(
            failure_kind(&AnthropicError::Overloaded { status: 529 }),
            RequestFailure::Overloaded
        );
        for status in [408, 500, 502, 504] {
            assert_eq!(
                failure_kind(&AnthropicError::Server {
                    status,
                    body: String::new(),
                }),
                RequestFailure::Overloaded,
                "{status}"
            );
        }
        for fatal in [
            AnthropicError::Server {
                status: 400,
                body: String::new(),
            },
            AnthropicError::Server {
                status: 501,
                body: String::new(),
            },
            AnthropicError::Auth("bad key".into()),
            AnthropicError::StreamInterrupted,
            AnthropicError::Refused,
            AnthropicError::Cancelled,
        ] {
            assert_eq!(failure_kind(&fatal), RequestFailure::Other, "{fatal}");
        }
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
