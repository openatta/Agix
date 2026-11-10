//! Anthropic API 错误分类。

use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum AnthropicError {
    /// 网络层失败 / 连接重置 / DNS / TLS 等
    #[error("transport: {0}")]
    Transport(#[source] anyhow::Error),

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

    /// 其他 4xx / 5xx。只有 [`is_transient_status`] 认的那几个在首字节前重试
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

/// 服务端那一侧临时出了错、请求本身没问题的状态码：内部错误、网关拿不到或等不到
/// 上游、请求超时。两个协议都用它决定首字节前要不要再发一次。
pub(crate) fn is_transient_status(status: u16) -> bool {
    matches!(status, 408 | 500 | 502 | 504)
}

impl From<base::interface::exec::ExecError> for AnthropicError {
    fn from(e: base::interface::exec::ExecError) -> Self {
        match e {
            base::interface::exec::ExecError::Denied(m) => Self::Denied(m),
            other => Self::Transport(anyhow::Error::new(other)),
        }
    }
}
