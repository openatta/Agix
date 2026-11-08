//! 通用错误类型 —— 跨 crate 边界的稳定错误。
//! 各 crate 可以再定义自己的局部错误（thiserror）并通过 `#[from]` 转入这些。

pub mod code;
pub use code::{CodedError, ErrorCode};

use std::time::Duration;

impl ToolError {
    /// Convenience: `Execution("...".into())`. Use `Execution(anyhow!(...))` in new code.
    pub fn exec(msg: impl Into<String>) -> Self {
        Self::Execution(anyhow::anyhow!("{}", msg.into()))
    }
}

// Allow String to convert directly into ToolError::Execution.
impl From<String> for ToolError {
    fn from(s: String) -> Self {
        Self::exec(s)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ToolError {
    #[error("denied: {0}")]
    Denied(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("input validation failed: {0}")]
    Validation(String),

    #[error("execution: {0}")]
    Execution(#[source] anyhow::Error),

    #[error("cancelled")]
    Cancelled,

    #[error("timeout after {0:?}")]
    Timeout(Duration),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),

    #[error("transport: {0}")]
    Transport(#[source] anyhow::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum EffectError {
    #[error("not interactive")]
    NotInteractive,

    #[error("user cancelled")]
    Cancelled,

    #[error("transport: {0}")]
    Transport(#[source] anyhow::Error),
}

/// HTTP 响应状态码的语义枚举。
///
/// 覆盖常见的 4xx（客户端错误）和 5xx（服务端错误）。
/// 通过 [`HttpError::from_status`] 按状态码构造。
#[derive(thiserror::Error, Clone, Debug, PartialEq, Eq)]
pub enum HttpError {
    #[error("400 Bad Request")]
    BadRequest,
    #[error("401 Unauthorized")]
    Unauthorized,
    #[error("403 Forbidden")]
    Forbidden,
    #[error("404 Not Found")]
    NotFound,
    #[error("405 Method Not Allowed")]
    MethodNotAllowed,
    #[error("406 Not Acceptable")]
    NotAcceptable,
    #[error("408 Request Timeout")]
    RequestTimeout,
    #[error("409 Conflict")]
    Conflict,
    #[error("410 Gone")]
    Gone,
    #[error("413 Payload Too Large")]
    PayloadTooLarge,
    #[error("415 Unsupported Media Type")]
    UnsupportedMediaType,
    #[error("422 Unprocessable Entity")]
    UnprocessableEntity,
    #[error("429 Too Many Requests")]
    TooManyRequests,

    #[error("500 Internal Server Error")]
    InternalServerError,
    #[error("501 Not Implemented")]
    NotImplemented,
    #[error("502 Bad Gateway")]
    BadGateway,
    #[error("503 Service Unavailable")]
    ServiceUnavailable,
    #[error("504 Gateway Timeout")]
    GatewayTimeout,
    #[error("505 HTTP Version Not Supported")]
    HttpVersionNotSupported,

    /// 上述未列出的状态码（携带原始数值）。
    #[error("HTTP {0}")]
    Unknown(u16),
}

impl HttpError {
    /// 从 HTTP 状态码构造对应的 [`HttpError`]。
    ///
    /// # Examples
    ///
    /// ```
    /// # use base::error::HttpError;
    /// assert!(matches!(HttpError::from_status(404), HttpError::NotFound));
    /// assert!(matches!(HttpError::from_status(500), HttpError::InternalServerError));
    /// assert!(matches!(HttpError::from_status(999), HttpError::Unknown(999)));
    /// ```
    pub fn from_status(code: u16) -> Self {
        match code {
            400 => Self::BadRequest,
            401 => Self::Unauthorized,
            403 => Self::Forbidden,
            404 => Self::NotFound,
            405 => Self::MethodNotAllowed,
            406 => Self::NotAcceptable,
            408 => Self::RequestTimeout,
            409 => Self::Conflict,
            410 => Self::Gone,
            413 => Self::PayloadTooLarge,
            415 => Self::UnsupportedMediaType,
            422 => Self::UnprocessableEntity,
            429 => Self::TooManyRequests,
            500 => Self::InternalServerError,
            501 => Self::NotImplemented,
            502 => Self::BadGateway,
            503 => Self::ServiceUnavailable,
            504 => Self::GatewayTimeout,
            505 => Self::HttpVersionNotSupported,
            _ => Self::Unknown(code),
        }
    }
}

#[cfg(test)]
mod http_error_tests {
    use super::*;

    #[test]
    fn err() {
        let e = HttpError::from_status(404);
        assert!(matches!(e, HttpError::NotFound));
    }

    #[test]
    fn all_4xx() {
        let cases: &[(u16, HttpError)] = &[
            (400, HttpError::BadRequest),
            (401, HttpError::Unauthorized),
            (403, HttpError::Forbidden),
            (404, HttpError::NotFound),
            (405, HttpError::MethodNotAllowed),
            (406, HttpError::NotAcceptable),
            (408, HttpError::RequestTimeout),
            (409, HttpError::Conflict),
            (410, HttpError::Gone),
            (413, HttpError::PayloadTooLarge),
            (415, HttpError::UnsupportedMediaType),
            (422, HttpError::UnprocessableEntity),
            (429, HttpError::TooManyRequests),
        ];
        for &(code, ref expected) in cases {
            assert_eq!(
                HttpError::from_status(code),
                *expected,
                "code {} did not match expected variant",
                code,
            );
        }
    }

    #[test]
    fn all_5xx() {
        let cases: &[(u16, HttpError)] = &[
            (500, HttpError::InternalServerError),
            (501, HttpError::NotImplemented),
            (502, HttpError::BadGateway),
            (503, HttpError::ServiceUnavailable),
            (504, HttpError::GatewayTimeout),
            (505, HttpError::HttpVersionNotSupported),
        ];
        for &(code, ref expected) in cases {
            assert_eq!(
                HttpError::from_status(code),
                *expected,
                "code {} did not match expected variant",
                code,
            );
        }
    }

    #[test]
    fn unknown_code() {
        assert!(matches!(
            HttpError::from_status(418),
            HttpError::Unknown(418)
        ));
        assert!(matches!(
            HttpError::from_status(999),
            HttpError::Unknown(999)
        ));
        assert!(matches!(HttpError::from_status(0), HttpError::Unknown(0)));
    }

    #[test]
    fn display() {
        assert_eq!(HttpError::NotFound.to_string(), "404 Not Found");
        assert_eq!(
            HttpError::InternalServerError.to_string(),
            "500 Internal Server Error"
        );
        assert_eq!(HttpError::Unknown(418).to_string(), "HTTP 418");
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    #[error("{message}")]
    InvalidInput { message: String, code: i32 },

    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),
}
