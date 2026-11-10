//! The environment a child process on this machine starts from.
//!
//! Every program the engine launches — a `Bash` / `Monitor` command, a command
//! hook, an MCP stdio server, the engine's own git and `pdftotext` — gets its
//! environment from here, so that what a model can read with `printenv` is
//! decided in one place. The engine's own credentials never pass by default;
//! everything else does unless the operator switches to an allowlist.
//!
//! Which variables are credentials is decided where configuration names them:
//! a settings file, a plugin, the MCP policy or the host each report the
//! variables they read a secret from ([`hold_back_credential`]) before any
//! child starts under them.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SubprocessEnvMode {
    /// The host's environment, minus the engine's own credentials.
    #[default]
    Inherit,
    /// Only the variables a shell needs to work, plus `allow`.
    Allowlist,
}

/// `sandbox.subprocess_env` in settings.json.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SubprocessEnv {
    #[serde(default)]
    pub mode: SubprocessEnvMode,
    /// Variables passed through whatever the mode would do with them — in
    /// `allowlist` mode the extra names to keep, in `inherit` mode a way to
    /// hand one of the engine's own credentials (a provider's `api_key_env`,
    /// a variable an MCP server's configuration reads) to every child on
    /// purpose.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

/// What `allowlist` mode keeps without being asked.
const BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TZ",
    #[cfg(windows)]
    "SYSTEMROOT",
    #[cfg(windows)]
    "COMSPEC",
    #[cfg(windows)]
    "PATHEXT",
    #[cfg(windows)]
    "TEMP",
    #[cfg(windows)]
    "TMP",
    #[cfg(windows)]
    "USERPROFILE",
    #[cfg(windows)]
    "APPDATA",
    #[cfg(windows)]
    "LOCALAPPDATA",
];

/// Environment variables the engine has been told hold a secret: a
/// provider's `api_key_env`, what an MCP server's configuration reads
/// ([`variables_an_mcp_server_reads`]), the enterprise IdP's
/// `client_secret_env`, the OTLP exporter's headers. Only ever grows: a
/// settings reload adds what the new files name, and a name the files stop
/// naming still holds the same secret in this process's environment.
static NAMED_CREDENTIALS: std::sync::RwLock<BTreeSet<String>> =
    std::sync::RwLock::new(BTreeSet::new());

/// From now on, treat the variable `name` as one of the engine's own
/// credentials: no child process inherits it unless a policy's `allow` names
/// it. Process-wide, because the environment children inherit from is. The
/// variables a shell cannot work without are never held back, whatever a
/// settings file names.
pub fn hold_back_credential(name: &str) {
    if name.is_empty() || BASELINE.contains(&name) {
        return;
    }
    let mut named = NAMED_CREDENTIALS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    named.insert(name.to_string());
}

/// The engine's own credentials: the `ANTHROPIC_*` variables, the daemon's
/// bearer token, and whatever [`hold_back_credential`] was told.
pub fn is_engine_credential(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with("ANTHROPIC_")
        || name == "AGIX_DAEMON_TOKEN"
        || NAMED_CREDENTIALS
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(name.as_ref())
}

/// The variables `input` refers to as `$VAR`, `${VAR}` or `${VAR:-…}`.
pub fn referenced_vars(input: &str) -> Vec<String> {
    let named = std::cell::RefCell::new(Vec::new());
    expand_vars_with(input, |name| {
        named.borrow_mut().push(name.to_string());
        None
    });
    named.into_inner()
}

/// The variables of this process's environment an `mcp_servers` entry reads
/// when it connects: every `$VAR` in its `command`, `args`, `env`, `url` and
/// `headers`, and the variables its `oauth` block names
/// (`client_secret_env`, `private_key_jwt.key_env`).
pub fn variables_an_mcp_server_reads(server: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;
    let field = |key: &str| server.get(key);
    let strings = ["command", "url"]
        .into_iter()
        .filter_map(|key| field(key)?.as_str())
        .chain(
            field("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .chain(
            ["env", "headers"]
                .into_iter()
                .filter_map(|key| field(key)?.as_object())
                .flatten()
                .filter_map(|(_, v)| v.as_str()),
        );
    let mut names: Vec<String> = strings.flat_map(referenced_vars).collect();
    let oauth = field("oauth");
    names.extend(
        [
            oauth.and_then(|o| o.get("client_secret_env")),
            oauth
                .and_then(|o| o.get("private_key_jwt"))
                .and_then(|k| k.get("key_env")),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string),
    );
    names
}

/// [`hold_back_credential`] for every variable
/// [`variables_an_mcp_server_reads`] finds in `server`.
pub fn hold_back_what_an_mcp_server_reads(server: &serde_json::Value) {
    for name in variables_an_mcp_server_reads(server) {
        hold_back_credential(&name);
    }
}

/// Expand `$VAR`, `${VAR}`, `${VAR:-default}` (the default when unset or
/// empty) and `$$` (a literal `$`) in `input`, with the variables `lookup`
/// gives. A variable it does not give expands to nothing.
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
            out.push('$');
            i += 1;
            continue;
        }
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

/// Never inherited: `CDPATH` would make a child shell's `cd foo` reach a
/// directory outside the project that the permission check, which reads the
/// literal `foo`, never saw.
fn never_inherited(name: &OsStr) -> bool {
    name == "CDPATH"
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Remove(Vec<OsString>),
    Replace(Vec<(OsString, OsString)>),
}

impl SubprocessEnv {
    /// Set `cmd`'s environment from this process's, as the policy says.
    pub fn apply(&self, cmd: &mut tokio::process::Command) {
        match self.plan(std::env::vars_os()) {
            Plan::Remove(names) => {
                for name in names {
                    cmd.env_remove(name);
                }
            }
            Plan::Replace(vars) => {
                cmd.env_clear();
                cmd.envs(vars);
            }
        }
    }

    fn plan(&self, host: impl IntoIterator<Item = (OsString, OsString)>) -> Plan {
        let allowed = |name: &OsStr| self.allow.iter().any(|a| OsStr::new(a) == name);
        match self.mode {
            SubprocessEnvMode::Inherit => Plan::Remove(
                host.into_iter()
                    .map(|(name, _)| name)
                    .filter(|name| {
                        never_inherited(name) || (is_engine_credential(name) && !allowed(name))
                    })
                    .collect(),
            ),
            SubprocessEnvMode::Allowlist => Plan::Replace(
                host.into_iter()
                    .filter(|(name, _)| {
                        !never_inherited(name)
                            && (BASELINE.iter().any(|b| OsStr::new(b) == name) || allowed(name))
                    })
                    .collect(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Vec<(OsString, OsString)> {
        [
            ("PATH", "/bin"),
            ("HOME", "/home/u"),
            ("ANTHROPIC_API_KEY", "sk-canary"),
            ("ANTHROPIC_BASE_URL", "https://example"),
            ("AGIX_DAEMON_TOKEN", "t"),
            ("GITHUB_TOKEN", "gh"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("CDPATH", "/"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
    }

    fn names(plan: Plan) -> Vec<String> {
        let mut names: Vec<String> = match plan {
            Plan::Remove(n) => n.into_iter().map(|n| n.into_string().unwrap()).collect(),
            Plan::Replace(v) => v
                .into_iter()
                .map(|(n, _)| n.into_string().unwrap())
                .collect(),
        };
        names.sort();
        names
    }

    #[test]
    fn inherit_drops_the_engines_credentials_and_cdpath() {
        let plan = SubprocessEnv::default().plan(host());
        assert_eq!(
            names(plan),
            [
                "AGIX_DAEMON_TOKEN",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_BASE_URL",
                "CDPATH",
            ]
        );
    }

    #[test]
    fn a_variable_a_provider_key_is_read_from_is_held_back_too() {
        hold_back_credential("AGIX_ENV_TEST_PROVIDER_KEY");
        let mut host = host();
        host.push(("AGIX_ENV_TEST_PROVIDER_KEY".into(), "sk-canary".into()));
        assert!(names(SubprocessEnv::default().plan(host.clone()))
            .contains(&"AGIX_ENV_TEST_PROVIDER_KEY".to_string()));
        let passed_on_purpose = SubprocessEnv {
            allow: vec!["AGIX_ENV_TEST_PROVIDER_KEY".into()],
            ..Default::default()
        };
        assert!(!names(passed_on_purpose.plan(host))
            .contains(&"AGIX_ENV_TEST_PROVIDER_KEY".to_string()));
    }

    #[test]
    fn what_an_mcp_server_reads_from_this_process() {
        let server = serde_json::json!({
            "type": "streamable_http",
            "command": "$BIN_DIR/x",
            "args": ["--token", "${ARG_TOKEN:-none}", "$$LITERAL"],
            "env": {"GITHUB_TOKEN": "$GH_TOKEN", "FIXED": "value"},
            "url": "https://h/mcp?key=${URL_KEY}",
            "headers": {"Authorization": "Bearer $HEADER_TOKEN"},
            "oauth": {"client_id": "c", "client_secret_env": "SECRET_VAR",
                      "private_key_jwt": {"key_env": "SIGNING_KEY"}},
        });
        let mut names = variables_an_mcp_server_reads(&server);
        names.sort();
        assert_eq!(
            names,
            [
                "ARG_TOKEN",
                "BIN_DIR",
                "GH_TOKEN",
                "HEADER_TOKEN",
                "SECRET_VAR",
                "SIGNING_KEY",
                "URL_KEY"
            ]
        );
    }

    #[test]
    fn a_variable_an_mcp_server_reads_is_held_back() {
        let server = serde_json::json!({"type": "stdio", "command": "x",
            "env": {"T": "$AGIX_ENV_TEST_MCP_TOKEN"}});
        hold_back_what_an_mcp_server_reads(&server);
        assert!(is_engine_credential(OsStr::new("AGIX_ENV_TEST_MCP_TOKEN")));
        assert!(!is_engine_credential(OsStr::new("T")));
    }

    #[test]
    fn a_variable_a_shell_needs_is_never_held_back() {
        hold_back_credential("PATH");
        assert!(!is_engine_credential(OsStr::new("PATH")));
    }

    #[test]
    fn cdpath_is_never_kept_even_when_named() {
        let policy = SubprocessEnv {
            mode: SubprocessEnvMode::Allowlist,
            allow: vec!["CDPATH".into()],
        };
        assert_eq!(names(policy.plan(host())), ["HOME", "PATH"]);
    }

    #[test]
    fn inherit_can_be_told_to_pass_a_credential_on_purpose() {
        let policy = SubprocessEnv {
            allow: vec!["ANTHROPIC_API_KEY".into()],
            ..Default::default()
        };
        assert_eq!(
            names(policy.plan(host())),
            ["AGIX_DAEMON_TOKEN", "ANTHROPIC_BASE_URL", "CDPATH"]
        );
    }

    #[test]
    fn allowlist_keeps_the_baseline_and_what_was_named() {
        let policy = SubprocessEnv {
            mode: SubprocessEnvMode::Allowlist,
            allow: vec!["SSH_AUTH_SOCK".into()],
        };
        assert_eq!(
            names(policy.plan(host())),
            ["HOME", "PATH", "SSH_AUTH_SOCK"]
        );
    }

    #[test]
    fn the_settings_shape() {
        let p: SubprocessEnv =
            serde_json::from_str(r#"{"mode":"allowlist","allow":["GITHUB_TOKEN"]}"#).unwrap();
        assert_eq!(p.mode, SubprocessEnvMode::Allowlist);
        assert_eq!(p.allow, ["GITHUB_TOKEN"]);
        let d: SubprocessEnv = serde_json::from_str("{}").unwrap();
        assert_eq!(d, SubprocessEnv::default());
    }
}
