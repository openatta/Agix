//! The deployment's settings layer — what an IT department or an unattended
//! host puts above everything a user, a project or a plugin can write.
//!
//! It is read from a fixed system path ([`managed_settings_path`]) that the
//! user's data directory, the project and the model have no say in; a daemon
//! started with `--managed-settings <path>` reads that file instead. Only
//! [`MANAGED_KEYS`] may appear in it. A file that is there but cannot be
//! honoured — unreadable, not JSON, a key outside that list, a value of the
//! wrong shape — is not skipped the way a broken user tier is: it sets
//! [`ManagedSettings::error`], and the permission gate then refuses every tool
//! call until it is fixed. Skipping it would silently drop every deny the
//! deployment wrote.
//!
//! What the layer means for a tool call is the permission gate's business
//! (`permissions::gate::ManagedPolicy`); this module only says what the file
//! holds. Semantics: `docs/ARCHITECTURE.md` §6.1.

use crate::interface::config_source::Tier;
use crate::settings::PermissionRule;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The keys a managed settings file may set. `$schema` is the editor's and is
/// accepted too.
pub const MANAGED_KEYS: &[&str] = &["permission_rules", "disable_bypass_permissions_mode"];

/// Where the managed settings live on this platform.
pub fn managed_settings_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/Agix/managed-settings.json")
    }
    #[cfg(windows)]
    {
        let root = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        root.join("Agix").join("managed-settings.json")
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        PathBuf::from("/etc/agix/managed-settings.json")
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ManagedSettings {
    /// The file (or other source) the layer came from.
    pub origin: String,
    /// Why the layer cannot be honoured. While set, every tool call is refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Each tagged `Tier::Managed`.
    pub permission_rules: Vec<PermissionRule>,
    pub disable_bypass_permissions_mode: bool,
}

impl ManagedSettings {
    /// The file at `path`. `None` when it does not exist and `required` is
    /// false; a `required` file that does not exist is an error, so a path the
    /// operator named cannot quietly mean "no managed settings".
    pub fn read(path: &Path, required: bool) -> Option<Self> {
        let origin = path.display().to_string();
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return None,
            Err(e) => return Some(Self::broken(origin, format!("cannot be read: {e}"))),
        };
        Some(match serde_json::from_str(&text) {
            Ok(value) => Self::from_value(origin, value),
            Err(e) => Self::broken(origin, format!("is not valid JSON: {e}")),
        })
    }

    pub fn from_value(origin: impl Into<String>, value: serde_json::Value) -> Self {
        let origin = origin.into();
        let serde_json::Value::Object(mut fields) = value else {
            return Self::broken(origin, "is not a JSON object".into());
        };
        fields.remove("$schema");
        let mut refused: Vec<&str> = fields
            .keys()
            .map(String::as_str)
            .filter(|key| !MANAGED_KEYS.contains(key))
            .collect();
        if !refused.is_empty() {
            refused.sort_unstable();
            return Self::broken(
                origin,
                format!(
                    "sets keys managed settings may not set: {} (allowed: {})",
                    refused.join(", "),
                    MANAGED_KEYS.join(", ")
                ),
            );
        }
        let permission_rules = match fields.remove("permission_rules") {
            None => Vec::new(),
            Some(rules) => match serde_json::from_value::<Vec<PermissionRule>>(rules) {
                Ok(rules) => rules
                    .into_iter()
                    .map(|rule| PermissionRule {
                        tier: Some(Tier::Managed),
                        ..rule
                    })
                    .collect(),
                Err(e) => return Self::broken(origin, format!("permission_rules: {e}")),
            },
        };
        let disable_bypass_permissions_mode = match fields.remove("disable_bypass_permissions_mode")
        {
            None => false,
            Some(serde_json::Value::Bool(b)) => b,
            Some(other) => {
                return Self::broken(
                    origin,
                    format!("disable_bypass_permissions_mode must be true or false, not {other}"),
                )
            }
        };
        Self {
            origin,
            error: None,
            permission_rules,
            disable_bypass_permissions_mode,
        }
    }

    fn broken(origin: String, error: String) -> Self {
        tracing::error!(origin = %origin, error = %error, "managed settings cannot be honoured; every tool call is refused until they are fixed");
        Self {
            error: Some(format!("{origin} {error}")),
            origin,
            ..Default::default()
        }
    }

    /// Two managed layers as one: both sets of rules, either one's switch,
    /// and broken if either is.
    pub fn followed_by(mut self, later: Self) -> Self {
        self.origin = format!("{}, {}", self.origin, later.origin);
        self.error = self.error.or(later.error);
        self.permission_rules.extend(later.permission_rules);
        self.disable_bypass_permissions_mode |= later.disable_bypass_permissions_mode;
        self
    }

    /// Whether a `bypassPermissions` session under this layer can still be
    /// asked about a call: the layer takes bypass away, or holds an `ask` or
    /// `deny` rule. A `deny` counts because a command it cannot read through
    /// (`$CMD x`) is asked about rather than refused.
    pub fn asks_under_bypass(&self) -> bool {
        self.error.is_none()
            && (self.disable_bypass_permissions_mode
                || self
                    .permission_rules
                    .iter()
                    .any(|r| r.action != crate::settings::PermissionAction::Allow))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::PermissionAction;
    use serde_json::json;

    #[test]
    fn rules_and_the_bypass_switch_are_read_and_tagged_managed() {
        let m = ManagedSettings::from_value(
            "it",
            json!({
                "$schema": "x",
                "permission_rules": [{"tool": "Bash(curl:*)", "action": "deny"}],
                "disable_bypass_permissions_mode": true,
            }),
        );
        assert_eq!(m.error, None);
        assert!(m.disable_bypass_permissions_mode);
        assert_eq!(m.permission_rules.len(), 1);
        assert_eq!(m.permission_rules[0].action, PermissionAction::Deny);
        assert_eq!(m.permission_rules[0].tier, Some(Tier::Managed));
    }

    #[test]
    fn a_bypass_session_can_be_asked_when_the_layer_asks_denies_or_takes_bypass_away() {
        let rule = |action: &str| json!({"permission_rules": [{"tool": "Bash", "action": action}]});
        let asks = |value| ManagedSettings::from_value("it", value).asks_under_bypass();
        assert!(asks(rule("ask")));
        assert!(asks(rule("deny")));
        assert!(asks(json!({"disable_bypass_permissions_mode": true})));
        assert!(!asks(rule("allow")));
        assert!(!asks(json!({})));
        assert!(!asks(json!({"permission_mode": "default"})));
    }

    #[test]
    fn a_key_outside_the_allowed_set_breaks_the_whole_layer() {
        for key in ["permission_mode", "sandbox", "permision_rules"] {
            let m = ManagedSettings::from_value(
                "it",
                json!({ key: {}, "permission_rules": [{"tool": "Bash", "action": "deny"}] }),
            );
            let error = m.error.unwrap_or_else(|| panic!("{key} was accepted"));
            assert!(error.contains(key), "{error}");
            assert!(m.permission_rules.is_empty());
        }
    }

    #[test]
    fn a_value_of_the_wrong_shape_breaks_the_layer() {
        for value in [
            json!({"disable_bypass_permissions_mode": "yes"}),
            json!({"permission_rules": [{"tool": "Bash", "action": "forbid"}]}),
            json!(["not", "an", "object"]),
        ] {
            assert!(
                ManagedSettings::from_value("it", value.clone())
                    .error
                    .is_some(),
                "{value}"
            );
        }
    }

    #[test]
    fn an_absent_file_is_no_layer_unless_it_was_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("managed-settings.json");
        assert!(ManagedSettings::read(&path, false).is_none());
        assert!(ManagedSettings::read(&path, true).unwrap().error.is_some());

        std::fs::write(&path, "{not json").unwrap();
        let broken = ManagedSettings::read(&path, false).unwrap();
        assert!(broken.error.unwrap().contains("not valid JSON"));
    }
}
