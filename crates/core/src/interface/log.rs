//! 统一日志接口。
//!
//! 提供全局可开关的日志抽象。所有 crate 通过此接口输出日志，
//! 而非直接依赖 `tracing`。应用程序层注入具体实现（tracing / log / noop）。

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Trace => write!(f, "TRACE"),
            Self::Debug => write!(f, "DEBUG"),
            Self::Info => write!(f, "INFO"),
            Self::Warn => write!(f, "WARN"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

/// 日志后端 trait。应用程序层实现，注入到 [`LogRouter`]。
///
/// 默认实现是 noop（零开销）。
pub trait Logger: Send + Sync + 'static {
    /// 写一条日志。`target` 是模块路径，`level` 是级别，
    /// `args` 是 `fmt::Arguments`（与 `tracing` 的 span 兼容）。
    fn log(&self, target: &str, level: LogLevel, args: fmt::Arguments<'_>);
}

/// 默认的 noop 日志后端。
#[derive(Debug, Clone, Copy)]
pub struct NoopLogger;

impl Logger for NoopLogger {
    fn log(&self, _target: &str, _level: LogLevel, _args: fmt::Arguments<'_>) {}
}

static LOG_ENABLED: AtomicBool = AtomicBool::new(true);
// Future: per-level filtering. For now, enabled/disabled is the only toggle.

/// 全局日志路由器。线程安全，无锁。
///
/// # 使用
///
/// ```ignore
/// use base::log::{log_debug, log_info, log_warn, log_error};
///
/// log_info!("engine", "starting turn {}", turn_no);
/// log_error!("mcp", "connection to {} failed: {}", server, err);
/// ```
pub struct LogRouter;

impl LogRouter {
    /// 全局开关：禁用后所有日志调用变为 noop。
    pub fn set_enabled(enabled: bool) {
        LOG_ENABLED.store(enabled, Ordering::Relaxed);
    }

    /// 是否启用。
    pub fn is_enabled() -> bool {
        LOG_ENABLED.load(Ordering::Relaxed)
    }
}

/// 宏：在启用时走 tracing，禁用时完全跳过（零开销）。
#[macro_export]
macro_rules! log_event {
    ($target:expr, $level:expr, $($arg:tt)*) => {{
        if $crate::log::LogRouter::is_enabled() {
            tracing::event!(
                match $level {
                    $crate::log::LogLevel::Trace => tracing::Level::TRACE,
                    $crate::log::LogLevel::Debug => tracing::Level::DEBUG,
                    $crate::log::LogLevel::Info => tracing::Level::INFO,
                    $crate::log::LogLevel::Warn => tracing::Level::WARN,
                    $crate::log::LogLevel::Error => tracing::Level::ERROR,
                },
                target: $target,
                $($arg)*
            );
        }
    }};
}

/// 便捷宏。
#[macro_export]
macro_rules! log_trace {
    ($target:expr, $($arg:tt)*) => {
        $crate::log_event!($target, $crate::log::LogLevel::Trace, $($arg)*)
    };
}
#[macro_export]
macro_rules! log_debug {
    ($target:expr, $($arg:tt)*) => {
        $crate::log_event!($target, $crate::log::LogLevel::Debug, $($arg)*)
    };
}
#[macro_export]
macro_rules! log_info {
    ($target:expr, $($arg:tt)*) => {
        $crate::log_event!($target, $crate::log::LogLevel::Info, $($arg)*)
    };
}
#[macro_export]
macro_rules! log_warn {
    ($target:expr, $($arg:tt)*) => {
        $crate::log_event!($target, $crate::log::LogLevel::Warn, $($arg)*)
    };
}
#[macro_export]
macro_rules! log_error {
    ($target:expr, $($arg:tt)*) => {
        $crate::log_event!($target, $crate::log::LogLevel::Error, $($arg)*)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_level_ordering() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn log_level_display() {
        assert_eq!(LogLevel::Trace.to_string(), "TRACE");
        assert_eq!(LogLevel::Error.to_string(), "ERROR");
    }

    #[test]
    fn noop_logger_does_nothing() {
        let logger = NoopLogger;
        logger.log("test", LogLevel::Info, format_args!("hello"));
    }

    #[test]
    fn router_toggle() {
        LogRouter::set_enabled(false);
        assert!(!LogRouter::is_enabled());
        LogRouter::set_enabled(true);
        assert!(LogRouter::is_enabled());
    }
}
