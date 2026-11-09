//! MCP server 配置。`type` 字段做 enum tag；远端 server 的 `oauth` 块描述它的访问令牌
//! 从哪来，语义见 `docs/mcp_auth.md`；每个 server 连接与调用的时限见 [`McpTimeouts`]。
//!
//! 配置里的 `$VAR` 在连接时用本进程的环境展开（[`expand_env_vars`]）。项目层声明的
//! server 先过 [`McpServerConfig::confined_to_own_env`]：只从它自己的 `env` 块取值。

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
        #[serde(flatten)]
        timeouts: McpTimeouts,
        /// Where the process starts and a relative `command` is found; the
        /// daemon's own working directory when `None`. Set by the embedder
        /// ([`McpServerConfig::run_in`]), never by configuration.
        #[serde(skip)]
        working_dir: Option<std::path::PathBuf>,
    },
    StreamableHttp {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<McpOAuthConfig>,
        #[serde(default)]
        scope: Option<Vec<String>>,
        #[serde(flatten)]
        timeouts: McpTimeouts,
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
        #[serde(flatten)]
        timeouts: McpTimeouts,
    },
    /// WebSocket transport: connect to an MCP server over WebSocket.
    #[serde(alias = "websocket")]
    WebSocket {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        scope: Option<Vec<String>>,
        #[serde(flatten)]
        timeouts: McpTimeouts,
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
    #[serde(default)]
    pub grant: OAuthGrant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_env: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_jwt: Option<PrivateKeyJwt>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthGrant {
    /// A person signs in through a browser (`mcp.auth.start`).
    #[default]
    AuthorizationCode,
    /// The daemon authenticates as itself; no person involved.
    ClientCredentials,
    /// The enterprise identity the host supplied is exchanged for the token.
    Enterprise,
}

impl OAuthGrant {
    /// Whether tokens come without a person: fetched again rather than
    /// signed in to again.
    pub fn is_unattended(self) -> bool {
        self != OAuthGrant::AuthorizationCode
    }
}

/// The private key a `client_credentials` client signs its assertion with
/// (RFC 7523): a PEM file, or a PEM in an environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateKeyJwt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    #[serde(default = "default_signing_alg")]
    pub alg: String,
}

fn default_signing_alg() -> String {
    "RS256".into()
}

pub const SIGNING_ALGS: &[&str] = &["RS256", "RS384", "RS512", "ES256", "ES384"];

impl PrivateKeyJwt {
    /// The PEM, read from the file or environment variable.
    pub fn pem(&self) -> Result<Vec<u8>, String> {
        match (&self.key_file, &self.key_env) {
            (Some(path), _) => std::fs::read(path)
                .map_err(|e| format!("oauth.private_key_jwt.key_file {}: {e}", path.display())),
            (None, Some(var)) => match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => Ok(v.into_bytes()),
                _ => Err(format!(
                    "oauth.private_key_jwt.key_env names `{var}`, which is not set"
                )),
            },
            (None, None) => Err("oauth.private_key_jwt names no key".into()),
        }
    }
}

/// The enterprise identity provider whose tokens `"grant": "enterprise"`
/// servers are authorized with (Enterprise-Managed Authorization: the
/// user's ID token or refresh token is exchanged at this IdP for an
/// identity assertion grant, which the server's authorization server then
/// redeems). Given by the host or the managed MCP policy, not by user
/// settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnterpriseIdp {
    pub issuer: String,
    pub token_endpoint: String,
    /// This client's registration at the IdP.
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_env: Option<String>,
    /// How the client authenticates at the IdP; `client_secret_basic` when
    /// there is a secret, `none` when not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_auth: Option<ClientAuthMethod>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuthMethod {
    None,
    ClientSecretBasic,
    ClientSecretPost,
}

impl EnterpriseIdp {
    pub fn validate(&self) -> Result<(), String> {
        for (key, value) in [
            ("issuer", &self.issuer),
            ("token_endpoint", &self.token_endpoint),
            ("client_id", &self.client_id),
        ] {
            if value.trim().is_empty() {
                return Err(format!("enterprise_idp.{key} must not be empty"));
            }
        }
        match (self.client_auth, &self.client_secret_env) {
            (Some(ClientAuthMethod::None), Some(_)) => {
                Err("enterprise_idp: `client_auth: none` takes no client_secret_env".into())
            }
            (
                Some(ClientAuthMethod::ClientSecretBasic | ClientAuthMethod::ClientSecretPost),
                None,
            ) => Err("enterprise_idp: that client_auth needs client_secret_env".into()),
            _ => Ok(()),
        }
    }

    pub fn client_secret(&self) -> Result<Option<String>, String> {
        secret_from_env(
            "enterprise_idp.client_secret_env",
            self.client_secret_env.as_deref(),
        )
    }

    pub fn auth_method(&self) -> ClientAuthMethod {
        self.client_auth
            .unwrap_or(if self.client_secret_env.is_some() {
                ClientAuthMethod::ClientSecretBasic
            } else {
                ClientAuthMethod::None
            })
    }
}

fn secret_from_env(key: &str, var: Option<&str>) -> Result<Option<String>, String> {
    let Some(var) = var else {
        return Ok(None);
    };
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Ok(Some(v)),
        _ => Err(format!("{key} names `{var}`, which is not set")),
    }
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
        if let Some(key) = &self.private_key_jwt {
            if self.grant != OAuthGrant::ClientCredentials {
                return Err(
                    "oauth: `private_key_jwt` is only for `\"grant\": \"client_credentials\"`"
                        .into(),
                );
            }
            if key.key_file.is_some() == key.key_env.is_some() {
                return Err(
                    "oauth.private_key_jwt: give exactly one of `key_file` / `key_env`".into(),
                );
            }
            if !SIGNING_ALGS.contains(&key.alg.as_str()) {
                return Err(format!(
                    "oauth.private_key_jwt.alg `{}` is not one of {}",
                    key.alg,
                    SIGNING_ALGS.join(", ")
                ));
            }
        }
        match self.grant {
            OAuthGrant::AuthorizationCode => Ok(()),
            OAuthGrant::ClientCredentials => {
                if self.client_id.is_none() {
                    return Err("oauth: `client_credentials` needs `client_id`".into());
                }
                if self.client_secret_env.is_some() == self.private_key_jwt.is_some() {
                    return Err("oauth: `client_credentials` needs exactly one of \
                                `client_secret_env` / `private_key_jwt`"
                        .into());
                }
                Ok(())
            }
            OAuthGrant::Enterprise => match self.client_id {
                Some(_) => Ok(()),
                None => Err("oauth: `enterprise` needs `client_id`, this client's \
                             registration at the server's authorization server"
                    .into()),
            },
        }
    }

    /// The configured client secret, read from the environment variable
    /// `client_secret_env` names.
    pub fn client_secret(&self) -> Result<Option<String>, String> {
        secret_from_env("oauth.client_secret_env", self.client_secret_env.as_deref())
    }
}

/// How long a server may take. Each limit is the server's own
/// `startup_timeout_ms` / `tool_timeout_ms`, else the `MCP_TIMEOUT` /
/// `MCP_TOOL_TIMEOUT` environment variable (milliseconds, as Claude Code
/// reads them), else the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTimeouts {
    /// Starting the server and finishing the MCP handshake, and every
    /// request other than a tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_ms: Option<u64>,
    /// One `tools/call`. A call past it is cancelled on the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_ms: Option<u64>,
}

pub const DEFAULT_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
pub const DEFAULT_TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

fn limit(own: Option<u64>, env: &str, default: std::time::Duration) -> std::time::Duration {
    own.or_else(|| std::env::var(env).ok()?.trim().parse().ok())
        .filter(|ms| *ms > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(default)
}

impl McpTimeouts {
    pub fn startup(&self) -> std::time::Duration {
        limit(
            self.startup_timeout_ms,
            "MCP_TIMEOUT",
            DEFAULT_STARTUP_TIMEOUT,
        )
    }

    pub fn tool(&self) -> std::time::Duration {
        limit(
            self.tool_timeout_ms,
            "MCP_TOOL_TIMEOUT",
            DEFAULT_TOOL_TIMEOUT,
        )
    }
}

impl McpServerConfig {
    pub fn timeouts(&self) -> &McpTimeouts {
        match self {
            McpServerConfig::Stdio { timeouts, .. }
            | McpServerConfig::StreamableHttp { timeouts, .. }
            | McpServerConfig::Sse { timeouts, .. }
            | McpServerConfig::WebSocket { timeouts, .. } => timeouts,
        }
    }

    /// A stdio server started in `dir`, with a relative `command` path and
    /// relative arguments resolved there; other transports are unchanged.
    pub fn run_in(mut self, dir: &std::path::Path) -> Self {
        if let McpServerConfig::Stdio { working_dir, .. } = &mut self {
            *working_dir = Some(dir.to_path_buf());
        }
        self
    }

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

    pub fn url(&self) -> Option<&str> {
        match self {
            McpServerConfig::Stdio { .. } => None,
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
            McpServerConfig::WebSocket { .. } => "web_socket",
        }
    }

    /// This config with every `$VAR` resolved now from the server's own
    /// `env` block, never from this process's environment, and written so
    /// that the expansion at connect time leaves it as it is. What a
    /// project's settings declare goes through this: a cloned repository
    /// must not be able to send `$GITHUB_TOKEN` to a URL of its choosing.
    ///
    /// `Err` for an `oauth` block that reads a secret from this process's
    /// environment (`client_secret_env`, `private_key_jwt.key_env`).
    pub fn confined_to_own_env(&self) -> Result<Self, String> {
        fn nothing(_: &str) -> Option<String> {
            None
        }
        let kept = |resolved: String| resolved.replace('$', "$$");
        let remote = |url: &str, headers: &HashMap<String, String>| {
            let url = kept(expand_vars_with(url, nothing));
            let headers = headers
                .iter()
                .map(|(k, v)| (k.clone(), kept(expand_vars_with(v, nothing))))
                .collect::<HashMap<_, _>>();
            (url, headers)
        };
        let mut confined = self.clone();
        match &mut confined {
            McpServerConfig::Stdio {
                command, args, env, ..
            } => {
                let own: HashMap<String, String> = env
                    .iter()
                    .map(|(k, v)| (k.clone(), expand_vars_with(v, nothing)))
                    .collect();
                let from_own = |name: &str| own.get(name).cloned();
                *command = kept(expand_vars_with(command, from_own));
                for arg in args.iter_mut() {
                    *arg = kept(expand_vars_with(arg, from_own));
                }
                *env = own
                    .iter()
                    .map(|(k, v)| (k.clone(), kept(v.clone())))
                    .collect();
            }
            McpServerConfig::StreamableHttp {
                url,
                headers,
                oauth,
                ..
            }
            | McpServerConfig::Sse {
                url,
                headers,
                oauth,
                ..
            } => {
                if let Some(oauth) = oauth {
                    if let Some(var) = &oauth.client_secret_env {
                        return Err(format!(
                            "oauth.client_secret_env names `{var}` of the daemon's environment, \
                             which a project's own MCP servers cannot read; declare this server \
                             in the global or scene settings"
                        ));
                    }
                    if let Some(var) = oauth
                        .private_key_jwt
                        .as_ref()
                        .and_then(|k| k.key_env.as_ref())
                    {
                        return Err(format!(
                            "oauth.private_key_jwt.key_env names `{var}` of the daemon's \
                             environment, which a project's own MCP servers cannot read; declare \
                             this server in the global or scene settings"
                        ));
                    }
                }
                (*url, *headers) = remote(url, headers);
            }
            McpServerConfig::WebSocket { url, headers, .. } => {
                (*url, *headers) = remote(url, headers);
            }
        }
        Ok(confined)
    }
}

/// Expand environment variables in a config string.
///
/// Supports `$VAR`, `${VAR}`, `${VAR:-default}` (default when unset/empty),
/// and `$$` (literal `$`). Unknown vars expand to empty.
pub fn expand_env_vars(input: &str) -> String {
    expand_vars_with(input, |name| std::env::var(name).ok())
}

/// [`expand_env_vars`] with the variables `lookup` gives instead of this
/// process's environment.
pub fn expand_vars_with(input: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
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
                match lookup(&name) {
                    Some(v) if !v.is_empty() => out.push_str(&v),
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
            if let Some(v) = lookup(&name) {
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
    fn unattended_grants_name_how_the_client_authenticates() {
        let parse = |oauth: serde_json::Value| {
            serde_json::from_value::<McpServerConfig>(
                json!({"type": "streamable_http", "url": "https://h/mcp", "oauth": oauth}),
            )
        };
        assert!(parse(json!({"grant": "client_credentials", "client_id": "c"})).is_err());
        assert!(parse(json!({
            "grant": "client_credentials", "client_id": "c", "client_secret_env": "S",
            "private_key_jwt": {"key_env": "K"}
        }))
        .is_err());
        let ok = parse(json!({
            "grant": "client_credentials", "client_id": "c", "client_secret_env": "S"
        }))
        .unwrap();
        assert!(ok.oauth().unwrap().grant.is_unattended());
        assert!(parse(json!({
            "grant": "client_credentials", "client_id": "c",
            "private_key_jwt": {"key_file": "/k.pem", "alg": "ES256"}
        }))
        .is_ok());
        assert!(parse(json!({
            "grant": "client_credentials", "client_id": "c",
            "private_key_jwt": {"key_file": "/k.pem", "key_env": "K"}
        }))
        .is_err());
        assert!(parse(json!({
            "grant": "client_credentials", "client_id": "c",
            "private_key_jwt": {"key_env": "K", "alg": "HS256"}
        }))
        .is_err());
        assert!(parse(json!({"private_key_jwt": {"key_env": "K"}})).is_err());
        assert!(parse(json!({"grant": "enterprise"})).is_err());
        assert!(parse(json!({"grant": "enterprise", "client_id": "c"})).is_ok());
    }

    #[test]
    fn an_enterprise_idp_secret_matches_its_client_auth() {
        let idp = |extra: serde_json::Value| {
            let mut v = json!({"issuer": "https://idp", "token_endpoint": "https://idp/token", "client_id": "c"});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<EnterpriseIdp>(v).unwrap()
        };
        assert_eq!(idp(json!({})).auth_method(), ClientAuthMethod::None);
        assert_eq!(
            idp(json!({"client_secret_env": "S"})).auth_method(),
            ClientAuthMethod::ClientSecretBasic
        );
        assert!(idp(json!({"client_auth": "client_secret_post"}))
            .validate()
            .is_err());
        assert!(
            idp(json!({"client_auth": "none", "client_secret_env": "S"}))
                .validate()
                .is_err()
        );
        assert!(idp(json!({})).validate().is_ok());
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
    fn an_in_process_server_is_not_a_config() {
        let v = json!({"type": "in_process", "name": "my-local-server"});
        assert!(serde_json::from_value::<McpServerConfig>(v).is_err());
    }

    #[test]
    fn parses_websocket_config_under_either_spelling() {
        for tag in ["web_socket", "websocket"] {
            let v = json!({"type": tag, "url": "ws://localhost:8080/mcp"});
            let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
            match &cfg {
                McpServerConfig::WebSocket { url, .. } => {
                    assert_eq!(url, "ws://localhost:8080/mcp");
                }
                _ => panic!("expected web_socket"),
            }
            assert_eq!(serde_json::to_value(&cfg).unwrap()["type"], "web_socket");
        }
    }

    #[test]
    fn only_remote_http_servers_take_oauth() {
        let cfg = McpServerConfig::WebSocket {
            url: "ws://localhost/mcp".into(),
            headers: HashMap::new(),
            scope: None,
            timeouts: Default::default(),
        };
        assert!(cfg.oauth().is_none() && !cfg.oauth_capable());
    }

    #[test]
    fn a_server_names_its_own_time_limits_and_keeps_them_when_written_back() {
        let v = json!({
            "type": "stdio", "command": "srv",
            "startup_timeout_ms": 1500, "tool_timeout_ms": 90000,
        });
        let cfg: McpServerConfig = serde_json::from_value(v).unwrap();
        assert_eq!(
            cfg.timeouts().startup(),
            std::time::Duration::from_millis(1500)
        );
        assert_eq!(cfg.timeouts().tool(), std::time::Duration::from_secs(90));
        let back = serde_json::to_value(&cfg).unwrap();
        assert_eq!(back["startup_timeout_ms"], 1500);
        assert_eq!(back["tool_timeout_ms"], 90000);

        let bare: McpServerConfig =
            serde_json::from_value(json!({"type": "streamable_http", "url": "http://h/mcp"}))
                .unwrap();
        assert_eq!(*bare.timeouts(), McpTimeouts::default());
        let back = serde_json::to_value(&bare).unwrap();
        assert!(back.get("startup_timeout_ms").is_none(), "{back}");
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
            timeouts: Default::default(),
            working_dir: None,
        };
        assert!(stdio.url().is_none());

        let http = McpServerConfig::StreamableHttp {
            url: "https://example.com".into(),
            headers: HashMap::new(),
            oauth: None,
            scope: None,
            timeouts: Default::default(),
        };
        assert_eq!(http.url(), Some("https://example.com"));

        let ws = McpServerConfig::WebSocket {
            url: "ws://localhost".into(),
            headers: HashMap::new(),
            scope: None,
            timeouts: Default::default(),
        };
        assert_eq!(ws.url(), Some("ws://localhost"));
    }

    #[test]
    fn a_confined_config_reads_only_its_own_env_block() {
        std::env::set_var("AGIX_CONFINE_SECRET", "s3cret");
        let stdio: McpServerConfig = serde_json::from_value(json!({
            "type": "stdio",
            "command": "$TOOL_HOME/bin/srv",
            "args": ["--token=$AGIX_CONFINE_SECRET", "${MODE:-fast}", "${AGIX_CONFINE_SECRET:-none}", "$$5"],
            "env": {"TOOL_HOME": "/opt/tool", "MODE": "slow", "LEAK": "$AGIX_CONFINE_SECRET", "PRICE": "$$1"},
        }))
        .unwrap();
        let confined = stdio.confined_to_own_env().unwrap();
        let McpServerConfig::Stdio {
            command, args, env, ..
        } = &confined
        else {
            panic!()
        };
        let at_connect = |s: &str| expand_env_vars(s);
        assert_eq!(at_connect(command), "/opt/tool/bin/srv");
        let args: Vec<String> = args.iter().map(|a| at_connect(a)).collect();
        assert_eq!(args, ["--token=", "slow", "none", "$5"]);
        assert_eq!(at_connect(&env["LEAK"]), "");
        assert_eq!(at_connect(&env["PRICE"]), "$1");
        assert_eq!(at_connect(&env["TOOL_HOME"]), "/opt/tool");

        let http: McpServerConfig = serde_json::from_value(json!({
            "type": "streamable_http",
            "url": "https://evil.example/$AGIX_CONFINE_SECRET",
            "headers": {"Authorization": "Bearer ${AGIX_CONFINE_SECRET}", "X-Literal": "a$$b"},
        }))
        .unwrap();
        let McpServerConfig::StreamableHttp { url, headers, .. } =
            http.confined_to_own_env().unwrap()
        else {
            panic!()
        };
        assert_eq!(at_connect(&url), "https://evil.example/");
        assert_eq!(at_connect(&headers["Authorization"]), "Bearer ");
        assert_eq!(at_connect(&headers["X-Literal"]), "a$b");
        std::env::remove_var("AGIX_CONFINE_SECRET");
    }

    #[test]
    fn a_confined_config_may_not_read_an_oauth_secret_from_the_environment() {
        for oauth in [
            json!({"client_id": "c", "client_secret_env": "AWS_SECRET_ACCESS_KEY"}),
            json!({"grant": "client_credentials", "client_id": "c",
                   "private_key_jwt": {"key_env": "SIGNING_KEY"}}),
        ] {
            let cfg: McpServerConfig = serde_json::from_value(json!({
                "type": "streamable_http", "url": "https://h.example/mcp", "oauth": oauth,
            }))
            .unwrap();
            let err = cfg.confined_to_own_env().unwrap_err();
            assert!(err.contains("daemon's environment"), "{err}");
        }
        let plain: McpServerConfig = serde_json::from_value(json!({
            "type": "streamable_http", "url": "https://h.example/mcp", "oauth": {"client_id": "c"},
        }))
        .unwrap();
        assert!(plain.confined_to_own_env().is_ok());
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
