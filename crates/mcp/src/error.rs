//! agix-mcp 错误。

#[derive(thiserror::Error, Debug)]
pub enum McpError {
    #[error("transport: {0}")]
    Transport(#[source] anyhow::Error),

    #[error("server '{name}' connection failed: {source}")]
    ConnectFailed {
        name: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("server '{name}' is not connected")]
    NotConnected { name: String },

    /// The connection dropped with the request on it. A tool call is not
    /// sent again, since it may already have run.
    #[error("server '{name}' connection was lost during {op}; it was not sent again, and the next request reconnects")]
    ConnectionLost { name: String, op: &'static str },

    #[error("server '{name}' did not answer {op} within {}s", .after.as_secs_f64())]
    Timeout {
        name: String,
        op: &'static str,
        after: std::time::Duration,
    },

    /// The server answered 401: it needs a person to sign in (again).
    #[error("server '{name}' needs the user to sign in before it can be used")]
    AuthRequired {
        name: String,
        challenge: Option<String>,
    },

    /// The server answered 403 `insufficient_scope`.
    #[error("server '{name}' needs the user to grant more access (scope: {})", .scope.as_deref().unwrap_or("unspecified"))]
    InsufficientScope {
        name: String,
        challenge: String,
        scope: Option<String>,
    },

    #[error("tool '{tool}' is not provided by server '{name}'")]
    UnknownTool { name: String, tool: String },

    #[error("rmcp service error: {0}")]
    RmcpService(String),

    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),
}

impl McpError {
    pub fn is_auth(&self) -> bool {
        matches!(
            self,
            McpError::AuthRequired { .. } | McpError::InsufficientScope { .. }
        )
    }

    /// A connect that fails the same way however often it is tried: the
    /// command is not there or may not be run, or the URL does not parse.
    pub fn is_permanent(&self) -> bool {
        let McpError::ConnectFailed { source, .. } = self else {
            return false;
        };
        source.downcast_ref::<url::ParseError>().is_some()
            || source.downcast_ref::<std::io::Error>().is_some_and(|e| {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                )
            })
    }
}
