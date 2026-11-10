//! Anthropic API 错误，以及两个协议共用的错误分类。
//!
//! 一次失败是哪一类（`ModelErrorKind`）只在这里判定一次；client 的退避、回合的恢复
//! 策略都读这个结果，不再各自从状态码或措辞里推。

use base::interface::model::{ModelError, ModelErrorKind};
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum AnthropicError {
    /// 网络层失败 / 连接重置 / DNS / TLS 等
    #[error("transport: {0}")]
    Transport(#[source] anyhow::Error),

    /// 响应头前或流中途，空闲窗口内什么都没等到
    #[error("timeout: {0}")]
    Timeout(String),

    /// HTTP 429 / 服务端 retry-after
    #[error("rate limited; retry-after = {retry_after:?}")]
    RateLimited {
        retry_after: Option<Duration>,
        /// `anthropic-ratelimit-*` headers，原样保留供上层决策
        headers: std::collections::HashMap<String, String>,
    },

    /// HTTP 503 / 529 —— 后端过载，可重试
    #[error("overloaded (HTTP {status})")]
    Overloaded {
        status: u16,
        retry_after: Option<Duration>,
    },

    /// 其他 4xx / 5xx，类别由 [`classify`] 定
    #[error("server error {status}: {body}")]
    Server {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },

    /// 认证失败（401 / 403）
    #[error("auth: {0}")]
    Auth(String),

    /// SSE 流中途断连
    #[error("stream interrupted")]
    StreamInterrupted,

    /// SSE 解析错（行格式 / data 非 JSON / unknown event）
    #[error("sse: {0}")]
    Sse(String),

    /// JSON schema 错（请求 / 响应反序列化）
    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),

    /// 模型主动拒绝
    #[error("refused by model")]
    Refused,

    /// 取消（CancellationToken 触发）
    #[error("cancelled")]
    Cancelled,

    /// 解析错误（意外类型 / 格式）
    #[error("parse error: {0}")]
    ParseError(String),

    /// 出口策略拒绝了这次连接（离线运行、宿主自己的限制）。
    ///
    /// 独立成一类，是因为它**不该重试**——再发一次撞的是同一条策略——
    /// 而 `Transport` 会被退避策略当成值得再试一次的那一类。
    #[error("refused by egress policy: {0}")]
    Denied(String),
}

impl AnthropicError {
    pub fn kind(&self) -> ModelErrorKind {
        match self {
            Self::Transport(_) => ModelErrorKind::Transport,
            Self::Timeout(_) => ModelErrorKind::Timeout,
            Self::RateLimited { .. } => ModelErrorKind::RateLimited,
            Self::Overloaded { .. } => ModelErrorKind::Overloaded,
            Self::Server { status, body, .. } => {
                classify(Some(*status), error_type(body).as_deref(), body)
            }
            Self::Auth(_) => ModelErrorKind::Auth,
            Self::StreamInterrupted => ModelErrorKind::StreamInterrupted,
            Self::Sse(_) | Self::Schema(_) | Self::ParseError(_) => ModelErrorKind::Protocol,
            Self::Refused => ModelErrorKind::InvalidRequest,
            Self::Cancelled => ModelErrorKind::Cancelled,
            Self::Denied(_) => ModelErrorKind::Denied,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. }
            | Self::Overloaded { retry_after, .. }
            | Self::Server { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl From<AnthropicError> for ModelError {
    fn from(e: AnthropicError) -> Self {
        let kind = e.kind();
        let retry_after = e.retry_after();
        let (status, message) = match e {
            AnthropicError::Overloaded { status, .. } => (Some(status), String::new()),
            AnthropicError::RateLimited { .. } | AnthropicError::Cancelled => (None, String::new()),
            AnthropicError::Server { status, body, .. } => (Some(status), body),
            AnthropicError::Auth(m)
            | AnthropicError::Timeout(m)
            | AnthropicError::Sse(m)
            | AnthropicError::ParseError(m)
            | AnthropicError::Denied(m) => (None, m),
            AnthropicError::Transport(e) => (None, e.to_string()),
            other => (None, other.to_string()),
        };
        let error = ModelError::new(kind, message).with_retry_after(retry_after);
        match status {
            Some(status) => error.with_status(status),
            None => error,
        }
    }
}

/// What kind of failure a provider's refusal is, from whatever it said: an
/// HTTP status, an error `type` in the body (Anthropic's, which relays that
/// lose the status still pass on), and the message.
pub(crate) fn classify(
    status: Option<u16>,
    error_type: Option<&str>,
    message: &str,
) -> ModelErrorKind {
    use ModelErrorKind as K;
    let refused_request =
        matches!(status, Some(400..=499)) || error_type == Some("invalid_request_error");
    match (status, error_type) {
        (Some(401 | 403), _) | (_, Some("authentication_error" | "permission_error")) => K::Auth,
        (Some(429), _) | (_, Some("rate_limit_error")) => K::RateLimited,
        (Some(503 | 529), _) | (_, Some("overloaded_error")) => K::Overloaded,
        (Some(402), _) | (_, Some("billing_error")) => K::Billing,
        (Some(413), _) | (_, Some("request_too_large")) => K::ContextTooLong,
        _ if refused_request && says_context_too_long(message) => K::ContextTooLong,
        (Some(404), _) | (_, Some("not_found_error")) => K::NotFound,
        (Some(408 | 500 | 502 | 504), _) | (_, Some("timeout_error" | "api_error")) => {
            K::Unavailable
        }
        _ if refused_request => K::InvalidRequest,
        _ => K::ServerError,
    }
}

/// Whether a refusal's text says the request was too large for the model.
///
/// No provider gives this a status of its own — Anthropic, OpenAI and the
/// relays in front of them all answer 400 with a type that covers every bad
/// request — so the wording is the signal.
pub(crate) fn says_context_too_long(message: &str) -> bool {
    const WORDINGS: &[&str] = &[
        "prompt is too long",
        "prompt too long",
        "input is too long",
        "context_length_exceeded",
        "maximum context length",
        "context length exceeded",
        "exceeds the context window",
        "exceeds the maximum number of tokens",
        "too many tokens",
        "request_too_large",
    ];
    let message = message.to_ascii_lowercase();
    WORDINGS.iter().any(|w| message.contains(w))
}

/// `error.type` of an error body shaped `{"type":"error","error":{"type":…}}`.
pub(crate) fn error_type(body: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(body.get("error")?.get("type")?.as_str()?.to_string())
}

impl From<base::interface::exec::ExecError> for AnthropicError {
    fn from(e: base::interface::exec::ExecError) -> Self {
        match e {
            base::interface::exec::ExecError::Denied(m) => Self::Denied(m),
            other => Self::Transport(anyhow::Error::new(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::backoff::RequestFailure;
    use std::collections::HashMap;

    #[test]
    fn the_providers_size_refusals_are_context_too_long() {
        for (status, body) in [
            (400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 212034 tokens > 200000 maximum"}}"#),
            (400, "This model's maximum context length is 65536 tokens. However, you requested 70000 tokens"),
            (413, "request entity too large"),
        ] {
            assert_eq!(
                classify(Some(status), error_type(body).as_deref(), body),
                ModelErrorKind::ContextTooLong,
                "{status} {body}"
            );
        }
        assert_eq!(
            classify(None, Some("invalid_request_error"), "prompt is too long"),
            ModelErrorKind::ContextTooLong,
            "an error event carries no status, only its type"
        );
    }

    #[test]
    fn other_bad_requests_and_server_errors_are_not() {
        assert_eq!(
            classify(Some(400), None, "max_tokens: must be a positive integer"),
            ModelErrorKind::InvalidRequest
        );
        assert_eq!(
            classify(Some(500), None, "prompt is too long"),
            ModelErrorKind::Unavailable,
            "the wording counts only on a refused request"
        );
        assert_eq!(classify(Some(501), None, ""), ModelErrorKind::ServerError);
    }

    #[test]
    fn a_relay_that_loses_the_status_keeps_the_type() {
        for (error_type, kind) in [
            ("overloaded_error", ModelErrorKind::Overloaded),
            ("rate_limit_error", ModelErrorKind::RateLimited),
            ("billing_error", ModelErrorKind::Billing),
            ("not_found_error", ModelErrorKind::NotFound),
            ("authentication_error", ModelErrorKind::Auth),
        ] {
            assert_eq!(
                classify(Some(500), Some(error_type), ""),
                kind,
                "{error_type}"
            );
        }
    }

    /// Which failures are worth another attempt before the first byte, in the
    /// vocabulary the shared backoff policy reads.
    #[test]
    fn only_pre_stream_failures_classify_as_retryable_kinds() {
        let server = |status| AnthropicError::Server {
            status,
            body: String::new(),
            retry_after: None,
        };
        let kind = |e: &AnthropicError| e.kind().request_failure();
        assert_eq!(
            kind(&AnthropicError::Transport(anyhow::anyhow!("reset"))),
            RequestFailure::Transport
        );
        assert_eq!(
            kind(&AnthropicError::Timeout("no response headers".into())),
            RequestFailure::Transport
        );
        assert_eq!(
            kind(&AnthropicError::RateLimited {
                retry_after: None,
                headers: HashMap::new(),
            }),
            RequestFailure::RateLimited
        );
        assert_eq!(
            kind(&AnthropicError::Overloaded {
                status: 529,
                retry_after: None
            }),
            RequestFailure::Overloaded
        );
        for status in [408, 500, 502, 504] {
            assert_eq!(
                kind(&server(status)),
                RequestFailure::Overloaded,
                "{status}"
            );
        }
        for fatal in [
            server(400),
            server(501),
            AnthropicError::Auth("bad key".into()),
            AnthropicError::StreamInterrupted,
            AnthropicError::Refused,
            AnthropicError::Cancelled,
            AnthropicError::Denied("offline".into()),
        ] {
            assert_eq!(kind(&fatal), RequestFailure::Other, "{fatal}");
        }
    }

    #[test]
    fn the_hint_and_status_survive_into_the_model_error() {
        let e = ModelError::from(AnthropicError::Overloaded {
            status: 529,
            retry_after: Some(Duration::from_secs(7)),
        });
        assert_eq!(e.kind, ModelErrorKind::Overloaded);
        assert_eq!(e.status, Some(529));
        assert_eq!(e.retry_after, Some(Duration::from_secs(7)));
    }
}
