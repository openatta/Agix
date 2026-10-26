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

    #[error("tool '{tool}' is not provided by server '{name}'")]
    UnknownTool { name: String, tool: String },

    #[error("rmcp service error: {0}")]
    RmcpService(String),

    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),
}
