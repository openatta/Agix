//! 统一错误码体系。
//!
//! 每个可观测的错误都分配一个唯一错误码。错误码跨 crate 稳定，
//! 上层可以据此做 i18n / 监控告警 / 降级策略。

use std::fmt;

/// 全局错误码。
///
/// 编码规则：
/// - `1xxx` — 通用 / 输入验证
/// - `2xxx` — 工具执行
/// - `3xxx` — 权限 / 安全
/// - `4xxx` — 网络 / API
/// - `5xxx` — 文件 / IO
/// - `6xxx` — 会话 / 状态
/// - `7xxx` — 模型 / LLM
/// - `8xxx` — MCP 协议
/// - `9xxx` — 内部 / 未分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// 输入格式不符合预期
    InvalidInput = 1001,
    /// 缺少必填参数
    MissingParameter = 1002,
    /// 操作被用户取消
    Cancelled = 1003,
    /// 操作超时
    Timeout = 1004,
    /// 功能未实现
    NotImplemented = 1005,
    /// 不支持的配置
    UnsupportedConfig = 1006,

    /// 工具未找到
    ToolNotFound = 2001,
    /// 工具输入校验失败
    ToolValidation = 2002,
    /// 工具执行失败
    ToolExecution = 2003,
    /// 工具被沙箱拒绝
    ToolSandboxed = 2004,

    /// 权限拒绝
    PermissionDenied = 3001,
    /// 路径不在允许范围内
    PathNotAllowed = 3002,
    /// 网络访问被策略阻止
    NetworkBlocked = 3003,

    /// HTTP 客户端错误 (4xx)
    HttpClientError = 4001,
    /// HTTP 服务端错误 (5xx)
    HttpServerError = 4002,
    /// API 认证失败
    ApiUnauthorized = 4003,
    /// API 限流
    ApiRateLimited = 4004,
    /// 连接超时
    ConnectionTimeout = 4005,
    /// DNS / TCP 连接失败
    ConnectionFailed = 4006,

    /// 文件不存在
    FileNotFound = 5001,
    /// 文件读取失败
    FileReadError = 5002,
    /// 文件写入失败
    FileWriteError = 5003,
    /// 文件权限不足
    FilePermission = 5004,
    /// 路径格式不合法
    InvalidPath = 5005,
    /// 磁盘空间不足
    DiskFull = 5006,

    /// 会话不存在
    SessionNotFound = 6001,
    /// 会话已过期
    SessionExpired = 6002,
    /// 会话达到上限
    SessionCapReached = 6003,
    /// JSONL 格式错误
    TranscriptCorrupt = 6004,

    /// 模型不支持的参数
    ModelUnsupportedParam = 7001,
    /// token 超出上下文窗口
    ContextWindowExceeded = 7002,
    /// 模型响应解析失败
    ModelParseError = 7003,
    /// 模型返回空响应
    ModelEmptyResponse = 7004,
    /// 流意外终止
    StreamAborted = 7005,

    /// MCP 服务连接失败
    McpConnectFailed = 8001,
    /// MCP 工具调用失败
    McpToolCallFailed = 8002,
    /// MCP OAuth 流程失败
    McpOAuthFailed = 8003,

    /// 内部状态不一致
    InternalInconsistency = 9001,
    /// channel 发送端断开
    ChannelClosed = 9002,
    /// 未分类的内部错误
    Internal = 9999,
}

impl ErrorCode {
    /// 人类可读的简短标签（英文，一行）。
    pub fn label(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid input",
            Self::MissingParameter => "missing parameter",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::NotImplemented => "not implemented",
            Self::UnsupportedConfig => "unsupported config",
            Self::ToolNotFound => "tool not found",
            Self::ToolValidation => "tool validation failed",
            Self::ToolExecution => "tool execution failed",
            Self::ToolSandboxed => "tool blocked by sandbox",
            Self::PermissionDenied => "permission denied",
            Self::PathNotAllowed => "path not allowed",
            Self::NetworkBlocked => "network blocked by policy",
            Self::HttpClientError => "HTTP client error",
            Self::HttpServerError => "HTTP server error",
            Self::ApiUnauthorized => "API unauthorized",
            Self::ApiRateLimited => "API rate limited",
            Self::ConnectionTimeout => "connection timeout",
            Self::ConnectionFailed => "connection failed",
            Self::FileNotFound => "file not found",
            Self::FileReadError => "file read error",
            Self::FileWriteError => "file write error",
            Self::FilePermission => "file permission denied",
            Self::InvalidPath => "invalid path",
            Self::DiskFull => "disk full",
            Self::SessionNotFound => "session not found",
            Self::SessionExpired => "session expired",
            Self::SessionCapReached => "session capacity reached",
            Self::TranscriptCorrupt => "transcript corrupt",
            Self::ModelUnsupportedParam => "model unsupported parameter",
            Self::ContextWindowExceeded => "context window exceeded",
            Self::ModelParseError => "model response parse error",
            Self::ModelEmptyResponse => "model returned empty response",
            Self::StreamAborted => "stream aborted",
            Self::McpConnectFailed => "MCP connect failed",
            Self::McpToolCallFailed => "MCP tool call failed",
            Self::McpOAuthFailed => "MCP OAuth failed",
            Self::InternalInconsistency => "internal state inconsistency",
            Self::ChannelClosed => "channel closed",
            Self::Internal => "internal error",
        }
    }

    /// 错误码数值。
    pub fn code(self) -> u16 {
        self as u16
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[E{:04}] {}", self.code(), self.label())
    }
}

/// 带错误码的通用错误载体。
///
/// 所有可以通过 crate 边界传播的错误都应包装为 `CodedError`，
/// 确保接收方可以拿到稳定的错误码而不是解析字符串。
#[derive(Debug)]
pub struct CodedError {
    pub code: ErrorCode,
    pub message: String,
    /// 可选的底层错误（用于日志 / 调试，不进入用户可见消息）。
    pub source: Option<anyhow::Error>,
}

impl CodedError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(code: ErrorCode, message: impl Into<String>, source: anyhow::Error) -> Self {
        Self {
            code,
            message: message.into(),
            source: Some(source),
        }
    }

    /// 仅用于日志的诊断字符串（含 source chain）。
    pub fn diagnostic(&self) -> String {
        let mut s = format!("[E{:04}] {}", self.code.code(), self.message);
        if let Some(ref src) = self.source {
            s.push_str(&format!("\n  caused by: {src}"));
        }
        s
    }
}

impl fmt::Display for CodedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[E{:04}] {}", self.code.code(), self.message)
    }
}

impl std::error::Error for CodedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &dyn std::error::Error)
    }
}

// CodedError 实现了 std::error::Error，anyhow 的 blanket From<E> 自动覆盖，
// 无需手动实现 From<CodedError> for anyhow::Error。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_label() {
        // 覆盖所有变体，确保 match 无遗漏
        let codes = &[
            ErrorCode::InvalidInput,
            ErrorCode::MissingParameter,
            ErrorCode::Cancelled,
            ErrorCode::Timeout,
            ErrorCode::NotImplemented,
            ErrorCode::UnsupportedConfig,
            ErrorCode::ToolNotFound,
            ErrorCode::ToolValidation,
            ErrorCode::ToolExecution,
            ErrorCode::ToolSandboxed,
            ErrorCode::PermissionDenied,
            ErrorCode::PathNotAllowed,
            ErrorCode::NetworkBlocked,
            ErrorCode::HttpClientError,
            ErrorCode::HttpServerError,
            ErrorCode::ApiUnauthorized,
            ErrorCode::ApiRateLimited,
            ErrorCode::ConnectionTimeout,
            ErrorCode::ConnectionFailed,
            ErrorCode::FileNotFound,
            ErrorCode::FileReadError,
            ErrorCode::FileWriteError,
            ErrorCode::FilePermission,
            ErrorCode::InvalidPath,
            ErrorCode::DiskFull,
            ErrorCode::SessionNotFound,
            ErrorCode::SessionExpired,
            ErrorCode::SessionCapReached,
            ErrorCode::TranscriptCorrupt,
            ErrorCode::ModelUnsupportedParam,
            ErrorCode::ContextWindowExceeded,
            ErrorCode::ModelParseError,
            ErrorCode::ModelEmptyResponse,
            ErrorCode::StreamAborted,
            ErrorCode::McpConnectFailed,
            ErrorCode::McpToolCallFailed,
            ErrorCode::McpOAuthFailed,
            ErrorCode::InternalInconsistency,
            ErrorCode::ChannelClosed,
            ErrorCode::Internal,
        ];
        for &c in codes {
            assert!(!c.label().is_empty(), "{c:?} missing label");
            assert!(c.code() >= 1001, "{c:?} code out of range");
            assert!(
                format!("{c}").contains(&format!("E{:04}", c.code())),
                "{c:?} Display format wrong"
            );
        }
        // ensure every variant is covered (count may grow as codes are added)
        assert!(
            codes.len() >= 39,
            "expected at least 39 codes, got {}",
            codes.len()
        );
    }

    #[test]
    fn coded_error_display() {
        let e = CodedError::new(ErrorCode::FileNotFound, "no such file: /tmp/x");
        assert_eq!(e.to_string(), "[E5001] no such file: /tmp/x");
    }

    #[test]
    fn coded_error_diagnostic_includes_source() {
        let inner = anyhow::anyhow!("permission denied");
        let e = CodedError::with_source(ErrorCode::FileReadError, "read failed", inner);
        let diag = e.diagnostic();
        assert!(diag.contains("E5002"));
        assert!(diag.contains("read failed"));
        assert!(diag.contains("permission denied"));
    }

    #[test]
    fn coded_error_into_anyhow() {
        let e = CodedError::new(ErrorCode::Timeout, "timed out");
        let a: anyhow::Error = e.into();
        assert!(a.to_string().contains("E1004"));
    }

    #[test]
    fn error_code_prefixes_cover_all_categories() {
        // Verify that all 9 major categories (1xxx–9xxx) are used by at least one code
        let all_codes = &[
            ErrorCode::InvalidInput,
            ErrorCode::ToolNotFound,
            ErrorCode::PermissionDenied,
            ErrorCode::HttpClientError,
            ErrorCode::FileNotFound,
            ErrorCode::SessionNotFound,
            ErrorCode::ModelUnsupportedParam,
            ErrorCode::McpConnectFailed,
            ErrorCode::Internal,
        ];
        let categories: std::collections::HashSet<u16> =
            all_codes.iter().map(|c| c.code() / 1000).collect();
        for cat in 1..=9 {
            assert!(categories.contains(&cat), "missing error category {cat}xxx");
        }
    }
}
