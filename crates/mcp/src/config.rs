//! MCP server 配置。`type` 字段做 enum tag；远端 server 的 `oauth` 块描述它的访问令牌
//! 从哪来，语义见 `docs/mcp_auth.md`。

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self")]
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
    StreamableHttp {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<McpOAuthConfig>,
        #[serde(default)]
        scope: Option<Vec<String>>,
    },
    /// Served by the streamable HTTP transport, which accepts the same servers.
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<McpOAuthConfig>,
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

const OAUTH_PROVIDER_REMOVED: &str = "`oauth_provider` is no longer supported. Describe how the \
     server is authorized with an `oauth` block instead: \
     \"oauth\": {\"client_id\": \"…\", \"client_secret_env\": \"ENV_VAR_NAME\", \
     \"scopes\": [\"…\"], \"callback_port\": 8765} — every key is optional, and a server with \
     no `oauth` block that asks for authorization is still offered to the user to sign in to \
     (docs/mcp_auth.md)";

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("oauth_provider").is_some() {
            return Err(D::Error::custom(OAUTH_PROVIDER_REMOVED));
        }
        let config = McpServerConfig::deserialize(value).map_err(D::Error::custom)?;
        if let Some(oauth) = config.oauth() {
            oauth.validate().map_err(D::Error::custom)?;
        }
        Ok(config)
    }
}

impl Serialize for McpServerConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        McpServerConfig::serialize(self, serializer)
    }
}

/// How a remote server's access token is obtained. Secrets are never written
/// here: `client_secret_env` names an environment variable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpOAuthConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_env: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
}

impl McpOAuthConfig {
    pub fn validate(&self) -> Result<(), String> {
        let blank = |v: &Option<String>| v.as_deref().is_some_and(|s| s.trim().is_empty());
        if blank(&self.client_id) || blank(&self.client_secret_env) {
            return Err("oauth: `client_id` and `client_secret_env` must not be empty".into());
        }
        if self.client_secret_env.is_some() && self.client_id.is_none() {
            return Err("oauth: `client_secret_env` needs the `client_id` it belongs to".into());
        }
        Ok(())
    }

    /// The configured client secret, read from the environment variable
    /// `client_secret_env` names.
    pub fn client_secret(&self) -> Result<Option<String>, String> {
        let Some(var) = &self.client_secret_env else {
            return Ok(None);
        };
        match std::env::var(var) {
            Ok(v) if !v.is_empty() => Ok(Some(v)),
            _ => Err(format!(
                "oauth.client_secret_env names `{var}`, which is not set"
            )),
        }
    }
}

impl McpServerConfig {
    /// The `oauth` block of a remote server.
    pub fn oauth(&self) -> Option<&McpOAuthConfig> {
        match self {
            McpServerConfig::StreamableHttp { oauth, .. } | McpServerConfig::Sse { oauth, .. } => {
                oauth.as_ref()
            }
            _ => None,
        }
    }

    /// Whether this server's token can come from OAuth: a streamable HTTP or
    /// SSE server that sends no fixed `Authorization` header.
    pub fn oauth_capable(&self) -> bool {
        match self {
            McpServerConfig::StreamableHttp { headers, .. }
            | McpServerConfig::Sse { headers, .. } => !headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization")),
            _ => false,
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
    fn an_oauth_block_is_parsed() {
        let v = json!({
            "type": "streamable_http",
            "url": "https://api.example/mcp",
            "oauth": {"client_id": "agix", "client_secret_env": "X", "scopes": ["a"], "callback_port": 8765},
        });
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        let oauth = cfg.oauth().expect("oauth block");
        assert_eq!(oauth.client_id.as_deref(), Some("agix"));
        assert_eq!(oauth.client_secret_env.as_deref(), Some("X"));
        assert_eq!(oauth.scopes, vec!["a".to_string()]);
        assert_eq!(oauth.callback_port, Some(8765));
        assert!(cfg.oauth_capable());
    }

    #[test]
    fn oauth_provider_is_refused_with_the_new_shape() {
        let v = json!({
            "type": "streamable_http",
            "url": "https://api.example/mcp",
            "oauth_provider": "github",
        });
        let err = serde_json::from_value::<McpServerConfig>(v)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`oauth_provider` is no longer supported"),
            "{err}"
        );
        assert!(err.contains("\"oauth\": {\"client_id\""), "{err}");
    }

    #[test]
    fn a_misspelled_oauth_key_is_refused() {
        let v =
            json!({"type": "sse", "url": "https://h/mcp", "oauth": {"client_secret": "literal"}});
        let err = serde_json::from_value::<McpServerConfig>(v)
            .unwrap_err()
            .to_string();
        assert!(err.contains("client_secret"), "{err}");
    }

    #[test]
    fn a_client_secret_needs_its_client_id() {
        let v = json!({"type": "streamable_http", "url": "https://h/mcp", "oauth": {"client_secret_env": "S"}});
        assert!(serde_json::from_value::<McpServerConfig>(v).is_err());
    }

    #[test]
    fn a_fixed_authorization_header_turns_oauth_off() {
        let v = json!({
            "type": "streamable_http", "url": "https://h/mcp",
            "headers": {"authorization": "Bearer t"}
        });
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        assert!(!cfg.oauth_capable());
    }

    #[test]
    fn a_config_round_trips_through_serialization() {
        let v = json!({
            "type": "streamable_http", "url": "https://h/mcp",
            "oauth": {"scopes": ["read"]}
        });
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        let back: McpServerConfig =
            serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(back.oauth(), cfg.oauth());
        assert_eq!(back.transport_kind(), "streamable_http");
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
    fn only_remote_http_servers_take_oauth() {
        let cfg = McpServerConfig::InProcess {
            name: "x".into(),
            scope: None,
        };
        assert!(cfg.oauth().is_none() && !cfg.oauth_capable());
        let cfg = McpServerConfig::WebSocket {
            url: "ws://localhost/mcp".into(),
            headers: HashMap::new(),
            scope: None,
        };
        assert!(cfg.oauth().is_none() && !cfg.oauth_capable());
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
            oauth: None,
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
