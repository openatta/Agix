//! MCP server 配置。`type` 字段做 enum tag。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServerConfig {
    /// 通过 stdin/stdout 与 server 子进程通信（典型）
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        /// Optional scope: limit which tools/resources/prompts this server exposes.
        /// An empty vec or None means "all". Each entry is a tool/resource/prompt name prefix.
        #[serde(default)]
        scope: Option<Vec<String>>,
    },
    /// HTTP streamable transport（rmcp 1.6 起的现代远端 transport）
    StreamableHttp {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        /// **v2-4 **: when set, name of an `oauth_providers.<name>`
        /// entry in settings.json.
        #[serde(default)]
        oauth_provider: Option<String>,
        #[serde(default)]
        scope: Option<Vec<String>>,
    },
    /// SSE (Server-Sent Events) transport — URL-based connection with
    /// optional headers. Forward-compatible: SSE server connection is
    /// implemented via rmcp when available, with fallback to streamable
    /// HTTP if SSE fails.
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        oauth_provider: Option<String>,
        #[serde(default)]
        scope: Option<Vec<String>>,
    },
    /// In-process transport: look up a pre-registered McpClient by name
    /// in the process-local registry. No subprocess or network is spawned;
    /// `name` must match a prior call to `register_in_process_service`.
    InProcess {
        name: String,
        #[serde(default)]
        scope: Option<Vec<String>>,
    },
    /// WebSocket transport: connect to an MCP server over WebSocket.
    WebSocket {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        scope: Option<Vec<String>>,
    },
}

impl McpServerConfig {
    /// Extract the OAuth provider name from any config variant that supports it.
    /// Returns `None` for Stdio or variants without an `oauth_provider` field.
    pub fn oauth_provider(&self) -> Option<&str> {
        match self {
            McpServerConfig::Stdio { .. } => None,
            McpServerConfig::StreamableHttp { oauth_provider, .. } => oauth_provider.as_deref(),
            McpServerConfig::Sse { oauth_provider, .. } => oauth_provider.as_deref(),
            McpServerConfig::InProcess { .. } => None,
            McpServerConfig::WebSocket { .. } => None,
        }
    }

    /// Extract the URL from any config variant that has one.
    /// Returns `None` for Stdio and InProcess.
    pub fn url(&self) -> Option<&str> {
        match self {
            McpServerConfig::Stdio { .. } | McpServerConfig::InProcess { .. } => None,
            McpServerConfig::StreamableHttp { url, .. }
            | McpServerConfig::Sse { url, .. }
            | McpServerConfig::WebSocket { url, .. } => Some(url.as_str()),
        }
    }

    pub fn transport_kind(&self) -> &'static str {
        match self {
            McpServerConfig::Stdio { .. } => "stdio",
            McpServerConfig::StreamableHttp { .. } => "streamable_http",
            McpServerConfig::Sse { .. } => "sse",
            McpServerConfig::InProcess { .. } => "in_process",
            McpServerConfig::WebSocket { .. } => "web_socket",
        }
    }
}

/// Expand environment variables in a config string.
///
/// Supports `$VAR`, `${VAR}`, `${VAR:-default}` (default when unset/empty),
/// and `$$` (literal `$`). Unknown vars expand to empty.
pub fn expand_env_vars(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        // c == '$'
        if i + 1 >= chars.len() {
            out.push('$');
            i += 1;
            continue;
        }
        let next = chars[i + 1];
        if next == '$' {
            out.push('$');
            i += 2;
            continue;
        }
        if next == '{' {
            // find closing '}'
            let mut end = None;
            let mut j = i + 2;
            while j < chars.len() {
                if chars[j] == '}' {
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(j) = end {
                let body: String = chars[i + 2..j].iter().collect();
                let (name, default) = match body.split_once(":-") {
                    Some((n, d)) => (n.to_string(), Some(d.to_string())),
                    None => (body, None),
                };
                match std::env::var(&name) {
                    Ok(v) if !v.is_empty() => out.push_str(&v),
                    _ => {
                        if let Some(d) = default {
                            out.push_str(&d);
                        }
                    }
                }
                i = j + 1;
                continue;
            }
            // no closing brace — emit literally
            out.push('$');
            i += 1;
            continue;
        }
        // `$VAR` — name is [A-Za-z_][A-Za-z0-9_]*
        let start = i + 1;
        let mut j = start;
        while j < chars.len() {
            let ch = chars[j];
            let valid =
                ch.is_ascii_alphabetic() || ch == '_' || (j > start && ch.is_ascii_alphanumeric());
            if !valid {
                break;
            }
            j += 1;
        }
        if j > start {
            let name: String = chars[start..j].iter().collect();
            if let Ok(v) = std::env::var(&name) {
                out.push_str(&v);
            }
            i = j;
        } else {
            out.push('$');
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_stdio_config() {
        let v = json!({
            "type": "stdio",
            "command": "uvx",
            "args": ["mcp-server-filesystem", "--root", "/tmp"],
            "env": {"FOO": "bar"}
        });
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        match cfg {
            McpServerConfig::Stdio {
                command, args, env, ..
            } => {
                assert_eq!(command, "uvx");
                assert_eq!(args.len(), 3);
                assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
            }
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn parses_streamable_http_config() {
        let v = json!({
            "type": "streamable_http",
            "url": "https://example.com/mcp",
            "headers": {"Authorization": "Bearer x"}
        });
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        assert!(matches!(cfg, McpServerConfig::StreamableHttp { .. }));
    }

    #[test]
    fn streamable_http_with_oauth_provider() {
        let v = json!({
            "type": "streamable_http",
            "url": "https://api.example/mcp",
            "oauth_provider": "github",
        });
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        match cfg {
            McpServerConfig::StreamableHttp {
                url,
                oauth_provider,
                ..
            } => {
                assert_eq!(url, "https://api.example/mcp");
                assert_eq!(oauth_provider.as_deref(), Some("github"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_in_process_config() {
        let v = json!({
            "type": "in_process",
            "name": "my-local-server"
        });
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        match cfg {
            McpServerConfig::InProcess { name, .. } => {
                assert_eq!(name, "my-local-server");
            }
            _ => panic!("expected in_process"),
        }
    }

    #[test]
    fn parses_websocket_config() {
        let v = json!({
            "type": "web_socket",
            "url": "ws://localhost:8080/mcp"
        });
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        match cfg {
            McpServerConfig::WebSocket { url, .. } => {
                assert_eq!(url, "ws://localhost:8080/mcp");
            }
            _ => panic!("expected web_socket"),
        }
    }

    #[test]
    fn in_process_oauth_provider_is_none() {
        let cfg = McpServerConfig::InProcess {
            name: "x".into(),
            scope: None,
        };
        assert!(cfg.oauth_provider().is_none());
    }

    #[test]
    fn websocket_oauth_provider_is_none() {
        let cfg = McpServerConfig::WebSocket {
            url: "ws://localhost/mcp".into(),
            headers: HashMap::new(),
            scope: None,
        };
        assert!(cfg.oauth_provider().is_none());
    }

    #[test]
    fn empty_args_and_env_default_to_empty() {
        let v = json!({"type": "stdio", "command": "echo"});
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        match cfg {
            McpServerConfig::Stdio { args, env, .. } => {
                assert!(args.is_empty());
                assert!(env.is_empty());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn url_method_returns_correct_variants() {
        let stdio = McpServerConfig::Stdio {
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            scope: None,
        };
        assert!(stdio.url().is_none());

        let http = McpServerConfig::StreamableHttp {
            url: "https://example.com".into(),
            headers: HashMap::new(),
            oauth_provider: None,
            scope: None,
        };
        assert_eq!(http.url(), Some("https://example.com"));

        let ws = McpServerConfig::WebSocket {
            url: "ws://localhost".into(),
            headers: HashMap::new(),
            scope: None,
        };
        assert_eq!(ws.url(), Some("ws://localhost"));

        let inproc = McpServerConfig::InProcess {
            name: "test".into(),
            scope: None,
        };
        assert!(inproc.url().is_none());
    }

    #[test]
    fn expand_env_vars_supports_patterns() {
        std::env::set_var("AGIX_TEST_EXPAND", "/opt/x");
        assert_eq!(expand_env_vars("$AGIX_TEST_EXPAND/bin"), "/opt/x/bin");
        assert_eq!(expand_env_vars("${AGIX_TEST_EXPAND}/bin"), "/opt/x/bin");
        assert_eq!(expand_env_vars("${AGIX_TEST_EXPAND:-/fallback}"), "/opt/x");
        assert_eq!(expand_env_vars("${AGIX_NOPE_VAR:-/fallback}"), "/fallback");
        assert_eq!(expand_env_vars("price $$5"), "price $5");
        assert_eq!(expand_env_vars("$AGIX_NOPE_VAR/end"), "/end");
        std::env::remove_var("AGIX_TEST_EXPAND");
    }
}
