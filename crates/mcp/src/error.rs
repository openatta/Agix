//! agix-mcp 错误，以及它们被显示时对其中 URL 的处理：userinfo、query、fragment
//! 常装着令牌或密码，凡是会进 `mcp.status`、事件与日志的错误文本都先经
//! [`without_url_secrets`]。

#[derive(thiserror::Error, Debug)]
pub enum McpError {
    #[error("transport: {}", without_url_secrets(&.0.to_string()))]
    Transport(#[source] anyhow::Error),

    #[error("server '{name}' connection failed: {}", without_url_secrets(&source.to_string()))]
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

    #[error("rmcp service error: {}", without_url_secrets(.0))]
    RmcpService(String),

    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),
}

/// `text` with every URL in it reduced to scheme, host, port and path.
pub fn without_url_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = next_url(rest) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | ')'))
            .unwrap_or(tail.len());
        out.push_str(&url_shown(&tail[..end]));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn next_url(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    ["https://", "http://", "wss://", "ws://"]
        .iter()
        .filter_map(|scheme| lower.find(scheme))
        .min()
}

fn url_shown(raw: &str) -> String {
    if !raw.contains(['@', '?', '#']) {
        return raw.to_string();
    }
    if let Ok(mut url) = url::Url::parse(raw) {
        if url.host_str().is_some() {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            return url.to_string();
        }
    }
    let cut = raw.find(['?', '#']).unwrap_or(raw.len());
    let (scheme, after) = raw[..cut].split_once("://").unwrap_or(("", &raw[..cut]));
    let authority_end = after.find('/').unwrap_or(after.len());
    let from_host = match after[..authority_end].rfind('@') {
        Some(at) => &after[at + 1..],
        None => after,
    };
    format!("{scheme}://{from_host}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_in_an_error_shows_neither_its_credentials_nor_its_query() {
        for (source, kept) in [
            (
                "error sending request for url (https://alice:hunter2@mcp.example.com/mcp?token=SECRET&x=1#frag)",
                "(https://mcp.example.com/mcp)",
            ),
            (
                "invalid url \"http://alice:hunter2@h.example:99999/mcp?token=SECRET\"",
                "\"http://h.example:99999/mcp\"",
            ),
            (
                "WebSocket connection to 'wss://alice@ws.example/socket?token=SECRET' failed",
                "'wss://ws.example/socket'",
            ),
        ] {
            let shown = McpError::ConnectFailed {
                name: "docs".into(),
                source: anyhow::anyhow!(source),
            }
            .to_string();
            for secret in ["alice", "hunter2", "SECRET", "frag"] {
                assert!(!shown.contains(secret), "{shown}");
            }
            assert!(shown.contains(kept), "{shown}");
        }
        assert_eq!(
            McpError::Transport(anyhow::anyhow!("see https://example.com/docs, then retry"))
                .to_string(),
            "transport: see https://example.com/docs, then retry"
        );
    }
}
