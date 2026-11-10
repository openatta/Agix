//! The environment a child process on this machine starts from.
//!
//! Every program the engine launches — a `Bash` command, a command hook, an
//! MCP stdio server — gets its environment from here, so that what a model can
//! read with `printenv` is decided in one place. The engine's own credentials
//! never pass by default; everything else does unless the operator switches to
//! an allowlist.

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
    /// hand one of the engine's own credentials to a child on purpose.
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

/// Environment variables the engine has been told hold one of its own
/// credentials — every `api_key_env` a provider names, whatever its name.
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

/// The engine's own credentials: the model provider's keys — the
/// `ANTHROPIC_*` variables and whatever [`hold_back_credential`] was told —
/// and the daemon's bearer token.
pub fn is_engine_credential(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with("ANTHROPIC_")
        || name == "AGIX_DAEMON_TOKEN"
        || NAMED_CREDENTIALS
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(name.as_ref())
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
