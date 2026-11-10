//! 企业 MCP 策略：一个进程能连哪些 MCP server、它们的 OAuth 能用哪些授权服务器、
//! 企业身份从哪个 IdP 换。
//!
//! 策略来自宿主（`Assembly::mcp_policy`）或系统管理路径上的
//! `mcp-policy.json`（[`managed_policy_path`]），从不来自用户可写的数据目录。
//! [`McpPolicyGate`] 是会话池实际执行的那一份：每个要连的 server 先过
//! [`McpPolicyGate::admit`]，被拒的不会起进程、不会发请求；授权服务器在任何
//! 跳转之前过 [`McpPolicyGate::admit_authorization_server`]。策略文件读不了、
//! 解析不了，一律按"全部拒绝"处理并在 `daemon.doctor` 里报出来。
//!
//! `allowed_servers` 里的 `name` 规则不放行任何 server：name 是写配置的人自己起的键，
//! 放行得落在实际要跑的 command 或要连的 url 上；`name` 只在 `denied_servers` 里起作用。
//!
//! 规则格式与匹配语义见 `docs/ARCHITECTURE.md` §6.4。

use std::path::{Path, PathBuf};

use base::interface::health::{CheckResult, HealthCheck};
use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::config::{expand_env_vars, EnterpriseIdp, McpServerConfig};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpPolicy {
    /// `None`: everything not denied is allowed. `Some(vec![])`: nothing is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_servers: Option<Vec<McpServerRule>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_servers: Vec<McpServerRule>,
    /// Globs over authorization server issuers (`scheme://host[:port][/path]`,
    /// no trailing `/`). `None`: any; otherwise an issuer matching none is
    /// refused before the user is sent anywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_authorization_servers: Option<Vec<String>>,
    /// The IdP `"grant": "enterprise"` servers exchange the user's identity at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_idp: Option<EnterpriseIdp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerRule {
    Name(String),
    Command(String),
    Url(String),
}

impl std::fmt::Display for McpServerRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpServerRule::Name(v) => write!(f, "{{\"name\": {v:?}}}"),
            McpServerRule::Command(v) => write!(f, "{{\"command\": {v:?}}}"),
            McpServerRule::Url(v) => write!(f, "{{\"url\": {v:?}}}"),
        }
    }
}

pub fn managed_policy_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/Agix/mcp-policy.json")
    }
    #[cfg(windows)]
    {
        let root = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        root.join("Agix").join("mcp-policy.json")
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        PathBuf::from("/etc/agix/mcp-policy.json")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyOrigin {
    None,
    Host,
    File(PathBuf),
}

enum Matcher {
    Name(String),
    Command(GlobMatcher),
    Url(GlobMatcher),
}

struct CompiledRule {
    rule: McpServerRule,
    matcher: Matcher,
}

impl CompiledRule {
    fn compile(rule: McpServerRule) -> Result<Self, String> {
        let glob = |pattern: &str| {
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map(|g| g.compile_matcher())
                .map_err(|e| format!("rule {rule}: invalid glob: {e}"))
        };
        let matcher = match &rule {
            McpServerRule::Name(n) => Matcher::Name(n.clone()),
            McpServerRule::Command(pattern) => Matcher::Command(glob(pattern)?),
            McpServerRule::Url(pattern) => Matcher::Url(glob(&normalized_url_pattern(pattern))?),
        };
        Ok(Self { rule, matcher })
    }

    fn admits(&self, name: &str, cfg: &McpServerConfig) -> bool {
        !matches!(self.matcher, Matcher::Name(_)) && self.matches(name, cfg)
    }

    fn matches(&self, name: &str, cfg: &McpServerConfig) -> bool {
        match &self.matcher {
            Matcher::Name(n) => n == name,
            Matcher::Command(m) => match cfg {
                McpServerConfig::Stdio { command, .. } => m.is_match(expand_env_vars(command)),
                _ => false,
            },
            Matcher::Url(m) => cfg
                .url()
                .and_then(|url| normalized_url(&expand_env_vars(url)))
                .is_some_and(|url| m.is_match(url)),
        }
    }
}

/// `scheme://host[:port]/path`, without userinfo, query or fragment, so a
/// `?` or `#` in the URL cannot carry text a host-matching `*` would accept.
/// `ws`/`wss` read as `http`/`https`, the URL a WebSocket server falls back
/// to, and a host's trailing `.` is dropped: it names the same host.
fn normalized_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw).ok()?;
    let host = url.host_str()?.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        other => other,
    };
    let default_port = match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    let port = url
        .port_or_known_default()
        .filter(|p| Some(*p) != default_port)
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    Some(format!("{scheme}://{host}{port}{}", url.path()))
}

/// A `url` rule written for `ws://`/`wss://` matches what [`normalized_url`]
/// makes of those URLs.
fn normalized_url_pattern(pattern: &str) -> String {
    for (ws, http) in [("ws://", "http://"), ("wss://", "https://")] {
        if pattern
            .get(..ws.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(ws))
        {
            return format!("{http}{}", &pattern[ws.len()..]);
        }
    }
    pattern.to_string()
}

fn normalized_issuer(raw: &str) -> Option<String> {
    let url = normalized_url(raw)?;
    Some(url.trim_end_matches('/').to_string())
}

/// Whether two issuers name the same authorization server, written either
/// way ([`normalized_issuer`]).
pub fn same_issuer(a: &str, b: &str) -> bool {
    normalized_issuer(a).is_some_and(|a| Some(a) == normalized_issuer(b))
}

enum Rules {
    Absent,
    Active {
        allowed: Option<Vec<CompiledRule>>,
        denied: Vec<CompiledRule>,
        issuers: Option<Vec<GlobMatcher>>,
    },
    Unreadable(String),
}

pub struct McpPolicyGate {
    origin: PolicyOrigin,
    rules: Rules,
    enterprise_idp: Option<EnterpriseIdp>,
}

impl McpPolicyGate {
    pub fn open() -> Self {
        Self {
            origin: PolicyOrigin::None,
            rules: Rules::Absent,
            enterprise_idp: None,
        }
    }

    pub fn from_host(policy: McpPolicy) -> Self {
        Self {
            origin: PolicyOrigin::Host,
            enterprise_idp: policy.enterprise_idp.clone(),
            rules: compile(policy),
        }
    }

    /// Read the policy at `path`. A missing file means no policy unless
    /// `required` (the path was named explicitly); anything else that keeps
    /// the file from being read as a policy denies every server.
    pub fn load(path: &Path, required: bool) -> Self {
        let mut enterprise_idp = None;
        let rules = match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<McpPolicy>(&text) {
                Ok(policy) => {
                    enterprise_idp = policy.enterprise_idp.clone();
                    compile(policy)
                }
                Err(e) => Rules::Unreadable(format!("invalid policy: {e}")),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Rules::Absent,
            Err(e) => Rules::Unreadable(format!("cannot read: {e}")),
        };
        if !matches!(rules, Rules::Active { .. }) {
            enterprise_idp = None;
        }
        Self {
            origin: PolicyOrigin::File(path.to_path_buf()),
            rules,
            enterprise_idp,
        }
    }

    pub fn enterprise_idp(&self) -> Option<&EnterpriseIdp> {
        self.enterprise_idp.as_ref()
    }

    /// May an MCP server's OAuth use the authorization server `issuer`?
    /// Asked before any redirect or token request.
    pub fn admit_authorization_server(&self, issuer: Option<&str>) -> Result<(), String> {
        match &self.rules {
            Rules::Absent | Rules::Active { issuers: None, .. } => Ok(()),
            Rules::Unreadable(e) => Err(format!(
                "denied: {} is unreadable ({e}), so no authorization server is allowed",
                self.describe_origin()
            )),
            Rules::Active {
                issuers: Some(globs),
                ..
            } => {
                let Some(issuer) = issuer.and_then(normalized_issuer) else {
                    return Err(format!(
                        "denied by {}: the authorization server names no issuer, and \
                         allowed_authorization_servers is set",
                        self.describe_origin()
                    ));
                };
                if globs.iter().any(|g| g.is_match(&issuer)) {
                    Ok(())
                } else {
                    Err(format!(
                        "denied by {}: authorization server {issuer} matches no \
                         allowed_authorization_servers rule",
                        self.describe_origin()
                    ))
                }
            }
        }
    }

    /// Whether `allowed_authorization_servers` names the authorization
    /// servers that may be used, rather than leaving it to any.
    pub fn restricts_authorization_servers(&self) -> bool {
        match &self.rules {
            Rules::Absent | Rules::Active { issuers: None, .. } => false,
            Rules::Active {
                issuers: Some(_), ..
            }
            | Rules::Unreadable(_) => true,
        }
    }

    pub fn origin(&self) -> &PolicyOrigin {
        &self.origin
    }

    pub fn error(&self) -> Option<&str> {
        match &self.rules {
            Rules::Unreadable(e) => Some(e),
            _ => None,
        }
    }

    /// May `name`, configured as `cfg`, be connected? `Err` says why not,
    /// naming the policy.
    pub fn admit(&self, name: &str, cfg: &McpServerConfig) -> Result<(), String> {
        match &self.rules {
            Rules::Absent => Ok(()),
            Rules::Unreadable(e) => Err(format!(
                "denied: {} is unreadable ({e}), so every MCP server is denied",
                self.describe_origin()
            )),
            Rules::Active {
                allowed, denied, ..
            } => {
                if let Some(rule) = denied.iter().find(|r| r.matches(name, cfg)) {
                    return Err(format!(
                        "denied by {}: matches denied_servers rule {}",
                        self.describe_origin(),
                        rule.rule
                    ));
                }
                match allowed {
                    Some(allowed) if !allowed.iter().any(|r| r.admits(name, cfg)) => {
                        let by_name_only = allowed.iter().any(|r| r.matches(name, cfg));
                        Err(format!(
                            "denied by {}: matches no allowed_servers rule{}",
                            self.describe_origin(),
                            if by_name_only {
                                " (a `name` rule there admits nothing; allow its command or url)"
                            } else {
                                ""
                            }
                        ))
                    }
                    _ => Ok(()),
                }
            }
        }
    }

    fn describe_origin(&self) -> String {
        match &self.origin {
            PolicyOrigin::None => "MCP policy".into(),
            PolicyOrigin::Host => "the host's MCP policy".into(),
            PolicyOrigin::File(path) => format!("MCP policy {}", path.display()),
        }
    }
}

fn compile(policy: McpPolicy) -> Rules {
    let compile_all = |rules: Vec<McpServerRule>| -> Result<Vec<CompiledRule>, String> {
        rules.into_iter().map(CompiledRule::compile).collect()
    };
    let allowed = match policy.allowed_servers.map(compile_all).transpose() {
        Ok(allowed) => allowed,
        Err(e) => return Rules::Unreadable(e),
    };
    let issuers = policy
        .allowed_authorization_servers
        .map(|patterns| {
            patterns
                .iter()
                .map(|p| {
                    GlobBuilder::new(p)
                        .literal_separator(true)
                        .build()
                        .map(|g| g.compile_matcher())
                        .map_err(|e| {
                            format!("allowed_authorization_servers {p:?}: invalid glob: {e}")
                        })
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose();
    let issuers = match issuers {
        Ok(issuers) => issuers,
        Err(e) => return Rules::Unreadable(e),
    };
    if let Some(Err(e)) = policy.enterprise_idp.as_ref().map(EnterpriseIdp::validate) {
        return Rules::Unreadable(e);
    }
    match compile_all(policy.denied_servers) {
        Ok(denied) => Rules::Active {
            allowed,
            denied,
            issuers,
        },
        Err(e) => Rules::Unreadable(e),
    }
}

impl HealthCheck for McpPolicyGate {
    fn name(&self) -> &str {
        "mcp.policy"
    }

    fn check(&self) -> CheckResult {
        let (source, path) = match &self.origin {
            PolicyOrigin::None => ("none", None),
            PolicyOrigin::Host => ("host", None),
            PolicyOrigin::File(path) => ("file", Some(path.display().to_string())),
        };
        let (allowed, denied, issuers, inert) = match &self.rules {
            Rules::Active {
                allowed,
                denied,
                issuers,
            } => (
                allowed.as_ref().map(Vec::len),
                Some(denied.len()),
                issuers.as_ref().map(Vec::len),
                allowed
                    .iter()
                    .flatten()
                    .filter(|r| matches!(r.matcher, Matcher::Name(_)))
                    .map(|r| r.rule.to_string())
                    .collect::<Vec<_>>(),
            ),
            _ => (None, None, None, Vec::new()),
        };
        let details = serde_json::json!({
            "source": source,
            "path": path,
            "in_force": matches!(self.rules, Rules::Active { .. } | Rules::Unreadable(_)),
            "allowed_servers": allowed,
            "denied_servers": denied,
            "allowed_authorization_servers": issuers,
            "enterprise_idp": self.enterprise_idp.as_ref().map(|idp| idp.issuer.clone()),
            "inert_allowed_rules": inert,
            "error": self.error(),
        });
        match &self.rules {
            Rules::Absent => CheckResult::ok("no MCP policy").with_details(details),
            Rules::Active { .. } if !inert.is_empty() => CheckResult::degraded(format!(
                "{} in force; allowed_servers rules {} admit nothing: a `name` rule \
                 only denies, allow a command or url instead",
                self.describe_origin(),
                inert.join(", ")
            ))
            .with_details(details),
            Rules::Active { .. } => CheckResult::ok(format!("{} in force", self.describe_origin()))
                .with_details(details),
            Rules::Unreadable(e) => CheckResult::failing(format!(
                "{} is unreadable ({e}); every MCP server is denied",
                self.describe_origin()
            ))
            .with_details(details),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::health::HealthStatus;
    use std::collections::HashMap;

    fn stdio(command: &str) -> McpServerConfig {
        McpServerConfig::Stdio {
            command: command.into(),
            args: vec![],
            env: HashMap::new(),
            scope: None,
            timeouts: Default::default(),
            working_dir: None,
        }
    }

    fn http(url: &str) -> McpServerConfig {
        McpServerConfig::StreamableHttp {
            url: url.into(),
            headers: HashMap::new(),
            oauth: None,
            scope: None,
            timeouts: Default::default(),
        }
    }

    fn gate(json: &str) -> McpPolicyGate {
        McpPolicyGate::from_host(serde_json::from_str(json).expect("policy parses"))
    }

    #[test]
    fn no_policy_admits_everything() {
        assert!(McpPolicyGate::open().admit("any", &stdio("x")).is_ok());
        assert!(gate("{}").admit("any", &stdio("x")).is_ok());
    }

    #[test]
    fn a_denied_name_is_refused_and_the_reason_names_the_rule() {
        let g = gate(r#"{"denied_servers": [{"name": "shell"}]}"#);
        let err = g.admit("shell", &stdio("sh")).unwrap_err();
        assert!(err.contains("the host's MCP policy"), "{err}");
        assert!(err.contains(r#"{"name": "shell"}"#), "{err}");
        assert!(g.admit("shell2", &stdio("sh")).is_ok());
    }

    #[test]
    fn an_allow_list_admits_only_what_it_matches() {
        let g = gate(r#"{"allowed_servers": [{"command": "github-mcp"}]}"#);
        assert!(g.admit("github", &stdio("github-mcp")).is_ok());
        let err = g.admit("github", &stdio("x")).unwrap_err();
        assert!(err.contains("no allowed_servers rule"), "{err}");
    }

    #[test]
    fn a_name_alone_in_allowed_servers_does_not_admit_a_command() {
        let g = gate(r#"{"allowed_servers": [{"name": "github"}]}"#);
        let err = g.admit("github", &stdio("/tmp/anything")).unwrap_err();
        assert!(err.contains("a `name` rule there admits nothing"), "{err}");
        assert!(g.admit("github", &http("https://evil.example/")).is_err());

        let check = g.check();
        assert_eq!(check.status, HealthStatus::Degraded, "{}", check.summary);
        assert_eq!(
            check.details["inert_allowed_rules"],
            serde_json::json!([r#"{"name": "github"}"#])
        );

        let both = gate(r#"{"allowed_servers": [{"name": "github"}, {"command": "github-mcp"}]}"#);
        assert!(both.admit("github", &stdio("github-mcp")).is_ok());
        assert!(both.admit("github", &stdio("/tmp/anything")).is_err());
    }

    #[test]
    fn an_empty_allow_list_admits_nothing() {
        let g = gate(r#"{"allowed_servers": []}"#);
        assert!(g.admit("github", &stdio("x")).is_err());
        assert!(g.admit("web", &http("https://a.example/")).is_err());
    }

    #[test]
    fn deny_wins_over_allow() {
        let g = gate(
            r#"{"allowed_servers": [{"command": "npx"}],
                "denied_servers": [{"name": "github"}]}"#,
        );
        assert!(g.admit("github", &stdio("npx")).is_err());
        assert!(g.admit("other", &stdio("npx")).is_ok());
    }

    #[test]
    fn a_command_rule_is_an_exact_path_or_glob_never_a_substring() {
        let g = gate(r#"{"allowed_servers": [{"command": "npx"}, {"command": "/opt/mcp/bin/*"}]}"#);
        assert!(g.admit("a", &stdio("npx")).is_ok());
        assert!(g.admit("a", &stdio("/tmp/evil-npx")).is_err());
        assert!(g.admit("a", &stdio("/usr/local/bin/npx")).is_err());
        assert!(g.admit("a", &stdio("/opt/mcp/bin/fs")).is_ok());
        assert!(
            g.admit("a", &stdio("/opt/mcp/bin/sub/fs")).is_err(),
            "`*` does not cross a `/`"
        );
    }

    #[test]
    fn a_command_rule_matches_the_expanded_command() {
        std::env::set_var("AGIX_POLICY_TEST_BIN", "/opt/mcp/bin");
        let g = gate(r#"{"allowed_servers": [{"command": "/opt/mcp/bin/fs"}]}"#);
        assert!(g.admit("a", &stdio("$AGIX_POLICY_TEST_BIN/fs")).is_ok());
        let d = gate(r#"{"denied_servers": [{"command": "/opt/mcp/bin/*"}]}"#);
        assert!(d.admit("a", &stdio("${AGIX_POLICY_TEST_BIN}/fs")).is_err());
        std::env::remove_var("AGIX_POLICY_TEST_BIN");
    }

    #[test]
    fn a_command_rule_does_not_match_a_remote_server_nor_a_url_rule_a_stdio_one() {
        let g = gate(r#"{"allowed_servers": [{"command": "*"}]}"#);
        assert!(g.admit("a", &http("https://x.example/")).is_err());
        let g = gate(r#"{"allowed_servers": [{"url": "**"}]}"#);
        assert!(g.admit("a", &stdio("npx")).is_err());
    }

    #[test]
    fn a_url_rule_matches_host_and_path_only() {
        let g = gate(r#"{"allowed_servers": [{"url": "https://*.corp.example/**"}]}"#);
        let admits = |url: &str| g.admit("a", &http(url)).is_ok();
        for url in [
            "https://mcp.corp.example/v1/mcp",
            "https://mcp.corp.example/",
            "https://MCP.corp.example/v1",
            "https://mcp.corp.example:443/v1",
            "https://mcp.corp.example/v1?x=1#f",
        ] {
            assert!(admits(url), "{url}");
        }
        for url in [
            "http://mcp.corp.example/v1",
            "https://evil.example/.corp.example/x",
            "https://evil.example?.corp.example/x",
            "https://evil.example#.corp.example/x",
            "https://mcp.corp.example.evil/x",
            "https://mcp.corp.example:8443/x",
            "not a url",
        ] {
            assert!(!admits(url), "{url}");
        }

        let one_level = gate(r#"{"allowed_servers": [{"url": "https://h.example/*"}]}"#);
        assert!(one_level.admit("a", &http("https://h.example/mcp")).is_ok());
        assert!(one_level
            .admit("a", &http("https://h.example/v1/mcp"))
            .is_err());
    }

    fn websocket(url: &str) -> McpServerConfig {
        McpServerConfig::WebSocket {
            url: url.into(),
            headers: HashMap::new(),
            scope: None,
            timeouts: Default::default(),
        }
    }

    #[test]
    fn a_websocket_server_is_matched_by_its_http_rule() {
        let deny = gate(r#"{"denied_servers": [{"url": "https://evil.example/**"}]}"#);
        assert!(deny
            .admit("a", &websocket("wss://evil.example/mcp"))
            .is_err());
        assert!(deny
            .admit("a", &websocket("wss://evil.example:443/mcp"))
            .is_err());
        assert!(deny.admit("a", &websocket("wss://ok.example/mcp")).is_ok());

        let plain = gate(r#"{"denied_servers": [{"url": "http://evil.example/**"}]}"#);
        assert!(plain
            .admit("a", &websocket("ws://evil.example/mcp"))
            .is_err());

        let ws_rule = gate(r#"{"allowed_servers": [{"url": "wss://corp.example/**"}]}"#);
        assert!(ws_rule
            .admit("a", &websocket("wss://corp.example/mcp"))
            .is_ok());
        assert!(ws_rule
            .admit("a", &http("https://corp.example/mcp"))
            .is_ok());
        assert!(ws_rule
            .admit("a", &websocket("ws://corp.example/mcp"))
            .is_err());
    }

    #[test]
    fn a_trailing_dot_host_is_the_same_host() {
        let deny = gate(r#"{"denied_servers": [{"url": "https://evil.example/**"}]}"#);
        assert!(deny.admit("a", &http("https://evil.example./mcp")).is_err());
        assert!(deny
            .admit("a", &http("https://EVIL.example../mcp"))
            .is_err());
        let allow = gate(r#"{"allowed_servers": [{"url": "https://corp.example/**"}]}"#);
        assert!(allow.admit("a", &http("https://corp.example./mcp")).is_ok());
    }

    #[test]
    fn a_missing_managed_file_is_no_policy_but_a_named_one_denies_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-policy.json");
        assert!(McpPolicyGate::load(&path, false)
            .admit("a", &stdio("x"))
            .is_ok());
        let named = McpPolicyGate::load(&path, true);
        assert!(named.admit("a", &stdio("x")).is_err());
        assert_eq!(named.check().status, HealthStatus::Failing);
    }

    #[test]
    fn a_policy_file_that_does_not_parse_denies_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-policy.json");
        for broken in [
            "{",
            r#"{"allow_servers": [{"name": "a"}]}"#,
            r#"{"allowed_servers": [{"name": "a", "url": "b"}]}"#,
            r#"{"allowed_servers": [{"host": "a"}]}"#,
            r#"{"denied_servers": [{"command": "[unclosed"}]}"#,
            r#"{"servers": {}}"#,
        ] {
            std::fs::write(&path, broken).unwrap();
            let g = McpPolicyGate::load(&path, false);
            let err = g.admit("a", &stdio("x")).unwrap_err();
            assert!(err.contains(&path.display().to_string()), "{broken}: {err}");
            assert!(err.contains("unreadable"), "{broken}: {err}");
            assert_eq!(g.check().status, HealthStatus::Failing, "{broken}");
            assert!(g.error().is_some(), "{broken}");
        }
    }

    #[test]
    fn a_host_policy_with_an_invalid_glob_denies_everything() {
        let g = McpPolicyGate::from_host(McpPolicy {
            allowed_servers: None,
            denied_servers: vec![McpServerRule::Url("[".into())],
            ..Default::default()
        });
        assert!(g.admit("a", &stdio("x")).is_err());
    }

    #[test]
    fn the_check_reports_where_the_policy_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-policy.json");
        std::fs::write(
            &path,
            r#"{"allowed_servers": [{"command": "a"}], "denied_servers": [{"name": "b"}, {"name": "c"}]}"#,
        )
        .unwrap();
        let result = McpPolicyGate::load(&path, false).check();
        assert_eq!(result.status, HealthStatus::Ok);
        assert_eq!(result.details["source"], "file");
        assert_eq!(result.details["path"], path.display().to_string());
        assert_eq!(result.details["in_force"], true);
        assert_eq!(result.details["allowed_servers"], 1);
        assert_eq!(result.details["denied_servers"], 2);

        let none = McpPolicyGate::open().check();
        assert_eq!(none.details["source"], "none");
        assert_eq!(none.details["in_force"], false);
    }

    #[test]
    fn authorization_servers_are_admitted_by_issuer_glob() {
        assert!(gate("{}").admit_authorization_server(None).is_ok());
        let g = gate(
            r#"{"allowed_authorization_servers": ["https://login.corp.example", "https://*.okta.com/oauth2/**"]}"#,
        );
        assert!(g
            .admit_authorization_server(Some("https://login.corp.example/"))
            .is_ok());
        assert!(g
            .admit_authorization_server(Some("https://acme.okta.com/oauth2/default"))
            .is_ok());
        let err = g
            .admit_authorization_server(Some("https://evil.example"))
            .unwrap_err();
        assert!(err.contains("allowed_authorization_servers"), "{err}");
        assert!(g
            .admit_authorization_server(Some("https://login.corp.example.evil.example"))
            .is_err());
        assert!(g.admit_authorization_server(None).is_err());
        assert!(gate(r#"{"allowed_authorization_servers": []}"#)
            .admit_authorization_server(Some("https://login.corp.example"))
            .is_err());
    }

    #[test]
    fn an_unreadable_policy_admits_no_authorization_server_and_names_no_idp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-policy.json");
        std::fs::write(
            &path,
            r#"{"enterprise_idp": {"issuer": "https://idp", "token_endpoint": "https://idp/t", "client_id": "c"}, "typo": 1}"#,
        )
        .unwrap();
        let g = McpPolicyGate::load(&path, true);
        assert!(g.admit_authorization_server(Some("https://as")).is_err());
        assert!(g.enterprise_idp().is_none());

        std::fs::write(
            &path,
            r#"{"enterprise_idp": {"issuer": "https://idp", "token_endpoint": "https://idp/t", "client_id": "c"}}"#,
        )
        .unwrap();
        let g = McpPolicyGate::load(&path, true);
        assert_eq!(g.enterprise_idp().unwrap().issuer, "https://idp");
        assert!(g.admit_authorization_server(Some("https://as")).is_ok());
    }

    #[test]
    fn the_managed_path_is_a_system_location() {
        let path = managed_policy_path();
        assert!(path.is_absolute(), "{}", path.display());
        assert!(path.ends_with("mcp-policy.json"));
    }
}
