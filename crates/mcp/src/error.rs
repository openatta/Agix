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
}
