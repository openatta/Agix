//! MCP server 配置。`type` 字段做 enum tag。

use globset::Glob;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use base::paths::ConfigPaths;

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
}

/// Expand environment variables in a config string.
///
/// Supports `$VAR`, `${VAR}`, `${VAR:-default}` (default when unset/empty),
/// and `$$` (literal `$`). Unknown vars expand to empty (matching TS).
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

// ── Scope / multi-source loading ──

/// MCP config scope with priority ordering.
/// Higher-priority scopes override lower-priority ones.
/// Priority (low to high): Enterprise < User < Project < Local
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum McpConfigScope {
    /// Organization-wide enterprise policy (lowest priority).
    Enterprise,
    /// Per-user global configuration.
    User,
    /// Per-project (workspace) configuration.
    Project,
    /// Per-local (overrides everything, highest priority).
    Local,
}

impl McpConfigScope {
    /// Numerical priority: higher number = higher priority.
    pub fn priority(&self) -> u8 {
        match self {
            McpConfigScope::Enterprise => 0,
            McpConfigScope::User => 1,
            McpConfigScope::Project => 2,
            McpConfigScope::Local => 3,
        }
    }
}

/// A pattern that can match an MCP server by name, command, or URL.
///
/// Used in [`McpPolicy::allowed_servers`] and [`McpPolicy::denied_servers`]
/// to allow/deny servers by various attributes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum McpServerPattern {
    /// Exact server name match.
    Name(String),
    /// Substring or glob match against the stdio command.
    Command(String),
    /// Glob/wildcard match against the server URL.
    UrlPattern(String),
}

impl McpServerPattern {
    /// Check whether this pattern matches the given server.
    pub fn matches(&self, name: &str, cfg: &McpServerConfig) -> bool {
        match self {
            McpServerPattern::Name(n) => n == name,
            McpServerPattern::Command(cmd) => match cfg {
                McpServerConfig::Stdio { command, .. } => matches_glob_or_substring(cmd, command),
                _ => false,
            },
            McpServerPattern::UrlPattern(pattern) => cfg
                .url()
                .map(|url| matches_glob_or_substring(pattern, url))
                .unwrap_or(false),
        }
    }
}

/// Test whether a pattern string matches a target.
/// If the pattern contains `*` or `?`, it is interpreted as a glob;
/// otherwise it is treated as a substring match.
fn matches_glob_or_substring(pattern: &str, target: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        Glob::new(pattern)
            .map(|g| g.compile_matcher().is_match(target))
            .unwrap_or(false)
    } else {
        target.contains(pattern)
    }
}

/// Enterprise MCP policy: enterprise-mandated servers + allow/deny lists.
///
/// Loaded from `{data_dir}/policy/mcp.json`. Deny overrides allow.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct McpPolicy {
    /// Enterprise-mandated MCP servers (lowest priority in merge).
    pub servers: HashMap<String, McpServerConfig>,
    /// If set, only servers matching these patterns are allowed.
    /// Empty list = block all. `None` = no allow restriction.
    pub allowed_servers: Option<Vec<McpServerPattern>>,
    /// Servers matching these patterns are always denied.
    pub denied_servers: Option<Vec<McpServerPattern>>,
    /// If true, only plugin-discovered servers are used;
    /// user/project/local config file scopes are skipped.
    pub plugin_only: bool,
}

/// Apply an enterprise policy to filter a server map.
///
/// Deny overrides allow. An empty allowlist blocks all servers.
pub fn apply_policy(servers: &mut HashMap<String, McpServerConfig>, policy: &McpPolicy) {
    // 1. Apply deny list first (deny always overrides allow)
    if let Some(ref denied) = policy.denied_servers {
        servers.retain(|name, cfg| !denied.iter().any(|p| p.matches(name, cfg)));
    }
    // 2. Apply allow list
    if let Some(ref allowed) = policy.allowed_servers {
        if allowed.is_empty() {
            servers.clear(); // Empty allowlist = block all
        } else {
            servers.retain(|name, cfg| allowed.iter().any(|p| p.matches(name, cfg)));
        }
    }
}

/// Load MCP server configurations from all scopes, merged with priority.
///
/// Scopes (low to high priority):
/// 1. Enterprise: `{global_data_dir}/policy/mcp.json`
/// 2. Global: `{global_data_dir}/mcp/servers.json` — **not** scene-scoped
///    (same reasoning as `memory`/`sessions`/`recordings`, see `base::paths` module
///    docs: which scene a session runs under has no bearing on which MCP
///    servers a project wants connected).
/// 3. Project: `{local_data_dir}/mcp/servers.json`
/// 4. Local: `{local_data_dir}/mcp.local.json` (highest priority)
///
/// Higher-priority scopes override lower-priority ones (same server name -> higher wins).
/// Enterprise allow/deny lists are applied after merge. Environment variables in
/// server config values (command, args, url, headers, env values) are expanded using
/// `$VAR`, `${VAR}`, `${VAR:-default}`, and `$$` (escaped) syntax.
pub fn load_mcp_configs(paths: &ConfigPaths) -> HashMap<String, McpServerConfig> {
    let mut merged: HashMap<String, McpServerConfig> = HashMap::new();
    let mut policy: Option<McpPolicy> = None;

    // 1. Enterprise policy (lowest priority)
    let policy_path = enterprise_policy_path(paths);
    match load_enterprise_policy_file(&policy_path) {
        Some(mut p) => {
            let n_allowed = p.allowed_servers.as_ref().map(|v| v.len()).unwrap_or(0);
            let n_denied = p.denied_servers.as_ref().map(|v| v.len()).unwrap_or(0);
            info!(
                path = %policy_path.display(),
                allowed = n_allowed,
                denied = n_denied,
                n_servers = p.servers.len(),
                plugin_only = p.plugin_only,
                "Loaded enterprise MCP policy"
            );
            // Expand env vars in enterprise servers first, then merge (lowest priority)
            for (name, mut cfg) in std::mem::take(&mut p.servers) {
                expand_env_vars_in_config(&mut cfg);
                merged.insert(name, cfg);
            }
            policy = Some(p);
        }
        None => {
            debug!(
                path = %policy_path.display(),
                "No enterprise MCP policy found or unparseable; skipping"
            );
        }
    }

    // 2-4. File scopes (skip if plugin_only is active)
    let skip_file_scopes = policy.as_ref().map(|p| p.plugin_only).unwrap_or(false);
    if !skip_file_scopes {
        // 2. Global scope (not scene-scoped)
        let user_path = paths.global_mcp_dir().join("servers.json");
        load_scoped_servers_file(&user_path, &mut merged);

        // 3. Project scope
        let project_path = paths.local_mcp_dir().join("servers.json");
        load_scoped_servers_file(&project_path, &mut merged);

        // 4. Local scope (highest priority)
        let local_path = paths.local_data_dir.join("mcp.local.json");
        load_scoped_servers_file(&local_path, &mut merged);
    } else {
        info!("MCP plugin_only policy active; skipping user/project/local config file scopes");
    }

    // Apply enterprise policy filtering (deny overrides allow)
    if let Some(ref policy) = policy {
        let before = merged.len();
        apply_policy(&mut merged, policy);
        let removed = before - merged.len();
        if removed > 0 {
            info!(removed, "Enterprise policy filtered MCP servers");
        }
    }

    merged
}

// ── Private helpers ──

/// Compute the enterprise policy file path: `{global_data_dir}/policy/mcp.json`
/// (e.g. `$HOME/.agix/policy/mcp.json`). Uses `global_data_dir` directly —
/// **not** derived from `user_data_dir`'s parent, since `user_data_dir` is
/// now nested two levels deep (`$HOME/.agix/scenes/<scope>/`).
fn enterprise_policy_path(paths: &ConfigPaths) -> PathBuf {
    paths.global_data_dir.join("policy").join("mcp.json")
}

/// Load an enterprise policy file. Returns `None` if the file doesn't exist
/// or fails to parse (logged at debug/warn level).
fn load_enterprise_policy_file(path: &Path) -> Option<McpPolicy> {
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<McpPolicy>(&content) {
        Ok(policy) => Some(policy),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "Failed to parse enterprise MCP policy file"
            );
            None
        }
    }
}

/// Load a scoped MCP servers JSON file and merge into `merged`.
/// Missing files and parse errors are silently skipped.
fn load_scoped_servers_file(path: &Path, merged: &mut HashMap<String, McpServerConfig>) {
    if !path.exists() {
        debug!(path = %path.display(), "MCP scoped servers file not found; skipping");
        return;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to read MCP scoped servers file");
            return;
        }
    };
    let servers: HashMap<String, McpServerConfig> = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to parse MCP scoped servers file");
            return;
        }
    };
    for (name, mut cfg) in servers {
        expand_env_vars_in_config(&mut cfg);
        // Higher priority wins: later scopes override earlier ones.
        merged.insert(name, cfg);
    }
}

/// Expand environment variables in all string fields of an `McpServerConfig`.
fn expand_env_vars_in_config(cfg: &mut McpServerConfig) {
    match cfg {
        McpServerConfig::Stdio {
            command, args, env, ..
        } => {
            *command = expand_env(command);
            for arg in args.iter_mut() {
                *arg = expand_env(arg);
            }
            for val in env.values_mut() {
                *val = expand_env(val);
            }
        }
        McpServerConfig::StreamableHttp { url, headers, .. }
        | McpServerConfig::Sse { url, headers, .. }
        | McpServerConfig::WebSocket { url, headers, .. } => {
            *url = expand_env(url);
            for val in headers.values_mut() {
                *val = expand_env(val);
            }
        }
        McpServerConfig::InProcess { .. } => {
            // No string fields that reference external env vars
        }
    }
}

/// Expand `$VAR`, `${VAR}`, `${VAR:-default}`, and `$$` (escaped `$`) in a
/// string using the process environment. Unknown/missing variables expand to
/// empty string (or `default` when using `${VAR:-default}` syntax).
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            None => out.push('$'),
            Some('$') => {
                // $$ -> escaped $
                chars.next();
                out.push('$');
            }
            Some('{') => {
                // ${VAR} or ${VAR:-default} syntax
                chars.next(); // consume '{'
                let mut inner = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    inner.push(c);
                }
                if closed {
                    // Check for ${VAR:-default}
                    if let Some((var_name, default)) = inner.split_once(":-") {
                        let val = std::env::var(var_name)
                            .ok()
                            .filter(|v| !v.is_empty())
                            .unwrap_or_else(|| default.to_string());
                        out.push_str(&val);
                    } else {
                        let val = std::env::var(&inner).unwrap_or_default();
                        out.push_str(&val);
                    }
                } else {
                    // No closing brace -> emit as literal
                    out.push_str("${");
                    out.push_str(&inner);
                }
            }
            Some(_) => {
                // $VAR syntax — consume alphanumeric + underscore
                let mut var = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_alphanumeric() || c == '_' {
                        var.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
                let val = std::env::var(&var).unwrap_or_default();
                out.push_str(&val);
            }
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

    // ── expand_env tests ──

    #[test]
    fn expand_env_simple_var() {
        let val = expand_env("hello $USER");
        assert!(val.starts_with("hello "));
        assert!(val.len() > "hello ".len());
    }

    #[test]
    fn expand_env_braces_with_default() {
        let val = expand_env("prefix-${UNSET_VAR_XYZ:-fallback}-suffix");
        assert_eq!(val, "prefix-fallback-suffix");
    }

    #[test]
    fn expand_env_braces_with_empty_var_falls_back() {
        let val = expand_env("${UNSET_VAR_XYZ:-default_val}");
        assert_eq!(val, "default_val");
    }

    #[test]
    fn expand_env_braces_without_default() {
        let val = expand_env("${UNSET_VAR_XYZ}");
        assert_eq!(val, "");
    }

    #[test]
    fn expand_env_escaped_dollar() {
        let val = expand_env("cost $$5.00");
        assert_eq!(val, "cost $5.00");
    }

    #[test]
    fn expand_env_no_vars() {
        let val = expand_env("plain string");
        assert_eq!(val, "plain string");
    }

    #[test]
    fn expand_env_mixed_braces_and_simple() {
        let val = expand_env("${UNSET_VAR_XYZ}-$ABC_DEF");
        assert_eq!(val, "-");
    }

    #[test]
    fn expand_env_incomplete_brace() {
        let val = expand_env("${UNSET_VAR_XYZ");
        assert_eq!(val, "${UNSET_VAR_XYZ");
    }

    // ── McpServerPattern tests ──

    #[test]
    fn mcppattern_name_exact_match() {
        let pattern = McpServerPattern::Name("my-server".into());
        let cfg = McpServerConfig::Stdio {
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            scope: None,
        };
        assert!(pattern.matches("my-server", &cfg));
        assert!(!pattern.matches("other", &cfg));
    }

    #[test]
    fn mcppattern_command_substring_match() {
        let pattern = McpServerPattern::Command("npx".into());
        let cfg = McpServerConfig::Stdio {
            command: "/usr/local/bin/npx".into(),
            args: vec![],
            env: HashMap::new(),
            scope: None,
        };
        assert!(pattern.matches("any-name", &cfg));
    }

    #[test]
    fn mcppattern_command_no_match_on_url_based() {
        let pattern = McpServerPattern::Command("npx".into());
        let cfg = McpServerConfig::StreamableHttp {
            url: "https://mcp.example.com".into(),
            headers: HashMap::new(),
            oauth_provider: None,
            scope: None,
        };
        assert!(!pattern.matches("any-name", &cfg));
    }

    #[test]
    fn mcppattern_url_glob_match() {
        let pattern = McpServerPattern::UrlPattern("https://*.example.com/*".into());
        let cfg = McpServerConfig::StreamableHttp {
            url: "https://api.example.com/mcp".into(),
            headers: HashMap::new(),
            oauth_provider: None,
            scope: None,
        };
        assert!(pattern.matches("any-name", &cfg));
    }

    #[test]
    fn mcppattern_url_glob_no_match() {
        let pattern = McpServerPattern::UrlPattern("https://*.example.com/*".into());
        let cfg = McpServerConfig::StreamableHttp {
            url: "https://other.com/mcp".into(),
            headers: HashMap::new(),
            oauth_provider: None,
            scope: None,
        };
        assert!(!pattern.matches("any-name", &cfg));
    }

    #[test]
    fn mcppattern_url_no_match_on_stdio() {
        let pattern = McpServerPattern::UrlPattern("https://*".into());
        let cfg = McpServerConfig::Stdio {
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            scope: None,
        };
        assert!(!pattern.matches("any-name", &cfg));
    }

    // ── apply_policy tests ──

    #[test]
    fn apply_policy_no_restrictions_passes_all() {
        let mut servers = HashMap::new();
        servers.insert(
            "s1".into(),
            McpServerConfig::Stdio {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                scope: None,
            },
        );
        servers.insert(
            "s2".into(),
            McpServerConfig::Stdio {
                command: "cat".into(),
                args: vec![],
                env: HashMap::new(),
                scope: None,
            },
        );
        let policy = McpPolicy::default();
        apply_policy(&mut servers, &policy);
        assert_eq!(servers.len(), 2);
    }

    #[test]
    fn apply_policy_deny_removes_matching() {
        let mut servers = HashMap::new();
        servers.insert(
            "blocked".into(),
            McpServerConfig::Stdio {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                scope: None,
            },
        );
        servers.insert(
            "allowed".into(),
            McpServerConfig::Stdio {
                command: "cat".into(),
                args: vec![],
                env: HashMap::new(),
                scope: None,
            },
        );
        let policy = McpPolicy {
            denied_servers: Some(vec![McpServerPattern::Name("blocked".into())]),
            ..McpPolicy::default()
        };
        apply_policy(&mut servers, &policy);
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("allowed"));
    }

    #[test]
    fn apply_policy_empty_allowlist_blocks_all() {
        let mut servers = HashMap::new();
        servers.insert(
            "s1".into(),
            McpServerConfig::Stdio {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                scope: None,
            },
        );
        let policy = McpPolicy {
            allowed_servers: Some(vec![]),
            ..McpPolicy::default()
        };
        apply_policy(&mut servers, &policy);
        assert!(servers.is_empty());
    }

    #[test]
    fn apply_policy_allowlist_keeps_only_matching() {
        let mut servers = HashMap::new();
        servers.insert(
            "keep".into(),
            McpServerConfig::Stdio {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                scope: None,
            },
        );
        servers.insert(
            "remove".into(),
            McpServerConfig::Stdio {
                command: "cat".into(),
                args: vec![],
                env: HashMap::new(),
                scope: None,
            },
        );
        let policy = McpPolicy {
            allowed_servers: Some(vec![McpServerPattern::Name("keep".into())]),
            ..McpPolicy::default()
        };
        apply_policy(&mut servers, &policy);
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("keep"));
    }

    #[test]
    fn apply_policy_deny_overrides_allow() {
        let mut servers = HashMap::new();
        servers.insert(
            "server".into(),
            McpServerConfig::Stdio {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                scope: None,
            },
        );
        let policy = McpPolicy {
            allowed_servers: Some(vec![McpServerPattern::Name("server".into())]),
            denied_servers: Some(vec![McpServerPattern::Name("server".into())]),
            ..McpPolicy::default()
        };
        apply_policy(&mut servers, &policy);
        assert!(servers.is_empty());
    }

    // ── McpConfigScope tests ──

    #[test]
    fn config_scope_priorities_are_correct() {
        assert_eq!(McpConfigScope::Enterprise.priority(), 0);
        assert_eq!(McpConfigScope::User.priority(), 1);
        assert_eq!(McpConfigScope::Project.priority(), 2);
        assert_eq!(McpConfigScope::Local.priority(), 3);
    }

    // ── McpPolicy serde tests ──

    #[test]
    fn mcp_policy_deserializes_with_optional_fields() {
        let json_str = r#"{"servers": {}}"#;
        let policy: McpPolicy = serde_json::from_str(json_str).unwrap();
        assert!(policy.servers.is_empty());
        assert!(policy.allowed_servers.is_none());
        assert!(policy.denied_servers.is_none());
        assert!(!policy.plugin_only);
    }

    #[test]
    fn mcp_policy_with_allow_deny_lists() {
        let json_str = r#"{
            "servers": {},
            "allowed_servers": [
                {"type": "name", "value": "server1"},
                {"type": "command", "value": "npx"}
            ],
            "denied_servers": [
                {"type": "url_pattern", "value": "https://blocked.example.com/*"}
            ],
            "plugin_only": true
        }"#;
        let policy: McpPolicy = serde_json::from_str(json_str).unwrap();
        assert_eq!(policy.allowed_servers.as_ref().unwrap().len(), 2);
        assert_eq!(policy.denied_servers.as_ref().unwrap().len(), 1);
        assert!(policy.plugin_only);
    }

    // ── enterprise_policy_path tests ──

    #[test]
    fn enterprise_policy_path_uses_global_data_dir() {
        let paths = ConfigPaths {
            user_data_dir: PathBuf::from("/home/user/.agix/scenes/code"),
            global_data_dir: PathBuf::from("/home/user/.agix"),
            local_data_dir: PathBuf::from("/tmp/proj/.agix"),
        };
        let path = enterprise_policy_path(&paths);
        assert_eq!(path, PathBuf::from("/home/user/.agix/policy/mcp.json"));
    }

    #[test]
    fn load_enterprise_policy_file_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_enterprise_policy_file(&dir.path().join("nonexistent.json"));
        assert!(result.is_none());
    }

    #[test]
    fn load_enterprise_policy_file_parses_valid_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, r#"{"servers": {}, "plugin_only": true}"#).unwrap();
        let result = load_enterprise_policy_file(&path);
        assert!(result.is_some());
        assert!(result.unwrap().plugin_only);
    }

    // ── load_scoped_servers_file tests ──

    #[test]
    fn load_scoped_servers_file_applies_env_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("servers.json");
        std::fs::write(
            &path,
            r#"{
            "my-server": {
                "type": "stdio",
                "command": "echo",
                "args": ["${UNSET_TEST_VAR_123:-hello}"]
            }
        }"#,
        )
        .unwrap();
        let mut merged = HashMap::new();
        load_scoped_servers_file(&path, &mut merged);
        assert_eq!(merged.len(), 1);
        match merged.get("my-server").unwrap() {
            McpServerConfig::Stdio { args, .. } => {
                assert_eq!(args[0], "hello");
            }
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn load_scoped_servers_file_later_wins_on_same_name() {
        let dir = tempfile::tempdir().unwrap();
        // First file: lower priority
        let path1 = dir.path().join("servers.json");
        std::fs::write(
            &path1,
            r#"{
            "same": {
                "type": "stdio",
                "command": "first"
            }
        }"#,
        )
        .unwrap();
        let mut merged = HashMap::new();
        load_scoped_servers_file(&path1, &mut merged);

        // Second file: higher priority should override
        let path2 = dir.path().join("local.json");
        std::fs::write(
            &path2,
            r#"{
            "same": {
                "type": "stdio",
                "command": "second"
            }
        }"#,
        )
        .unwrap();
        load_scoped_servers_file(&path2, &mut merged);

        assert_eq!(merged.len(), 1);
        match merged.get("same").unwrap() {
            McpServerConfig::Stdio { command, .. } => {
                assert_eq!(command, "second", "later scope must override earlier");
            }
            _ => panic!("expected stdio"),
        }
    }

    // ── url method tests ──

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
