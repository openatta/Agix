//! Append a permission rule to a settings.json /
//! settings.local.json file, preserving unknown fields. Used by the
//! interactive ask dialog's "always allow" shortcut.
//!
//! The read-modify-write goes through `base::settings_file`, the same lock
//! and atomic replace every other settings writer uses — so it neither races
//! `config.setProvider` / `mcp.addServer` on the same file nor widens a
//! private `settings.local.json`.

use std::path::Path;

use base::interface::permission::GrantMatch;
use base::settings_file::{SettingsDir, SettingsFileError};

/// Which behavior bucket to append to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendTarget {
    Allow,
    Deny,
    Ask,
}

impl AppendTarget {
    fn action(&self) -> &'static str {
        match self {
            AppendTarget::Allow => "allow",
            AppendTarget::Deny => "deny",
            AppendTarget::Ask => "ask",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    #[error(transparent)]
    File(#[from] SettingsFileError),
    #[error("{} has no file name", .0.display())]
    NoFileName(std::path::PathBuf),
    #[error("permission_rules in the settings file is not a list")]
    NotAList,
}

/// Append `rule_string` to `permission_rules` in the given settings.json
/// file, as `{"tool": <rule_string>, "action": <target>}`. If the file
/// doesn't exist, it's created with `{}` first. If an identical entry is
/// already present, the file is left untouched.
///
/// The shape here is the one `base::settings::Settings`
/// deserializes — writing anything else produces a file `LoadedSettings::load`
/// silently drops as an unknown key.
///
/// Returns `Ok(true)` if the file was modified, `Ok(false)` if the rule was
/// already present.
pub fn append_permission_rule(
    settings_path: &Path,
    target: AppendTarget,
    rule_string: &str,
) -> Result<bool, AppendError> {
    append(settings_path, target, rule_string, GrantMatch::Pattern)
}

/// [`append_permission_rule`] for an allow whose content is matched only by
/// the same text, written with `"match": "exact"`.
pub fn append_exact_allow(settings_path: &Path, rule_string: &str) -> Result<bool, AppendError> {
    append(
        settings_path,
        AppendTarget::Allow,
        rule_string,
        GrantMatch::Exact,
    )
}

fn append(
    settings_path: &Path,
    target: AppendTarget,
    rule_string: &str,
    matching: GrantMatch,
) -> Result<bool, AppendError> {
    let file = settings_path
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or_else(|| AppendError::NoFileName(settings_path.to_path_buf()))?;
    let dir = match settings_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let tier = SettingsDir::lock(dir)?;
    let mut value = tier.read(file)?;

    let arr = value
        .entry("permission_rules".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let arr = arr.as_array_mut().ok_or(AppendError::NotAList)?;

    let action = target.action();
    let mut entry = serde_json::json!({ "tool": rule_string, "action": action });
    if !matching.is_pattern() {
        entry["match"] = serde_json::to_value(matching).unwrap_or_default();
    }
    let already = arr.iter().any(|v| {
        ["tool", "action", "match"]
            .iter()
            .all(|key| v.get(key) == entry.get(key))
    });
    if already {
        return Ok(false);
    }
    arr.push(entry);

    tier.write(file, &value)?;
    Ok(true)
}

/// The rule string `<tool_name>(<content>)` for a call whose content is
/// `match_content`. None when there is no content to scope it to.
pub fn build_rule_string(tool_name: &str, match_content: Option<&str>) -> Option<String> {
    let content = match_content?.trim();
    if content.is_empty() {
        return None;
    }
    Some(format!("{tool_name}({content})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// N threads appending distinct rules to the *same* settings file
    /// concurrently must all survive: unserialized, a thread reads the file
    /// before an earlier thread's rename lands, then overwrites it on its own
    /// write.
    #[test]
    fn concurrent_appends_to_the_same_file_do_not_lose_updates() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        let n = 12;

        std::thread::scope(|scope| {
            for i in 0..n {
                let p = &p;
                scope.spawn(move || {
                    append_permission_rule(p, AppendTarget::Allow, &format!("Bash(cmd{i})"))
                        .unwrap();
                });
            }
        });

        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let arr = v["permission_rules"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            n,
            "expected all {n} concurrently-appended rules to survive, got {arr:?}"
        );
    }

    /// The written shape must be the one the loader actually reads — a file
    /// whose rules land under a key the loader doesn't know is
    /// indistinguishable from an empty one once loaded.
    #[test]
    fn appended_rules_round_trip_through_settings_load() {
        let dir = TempDir::new().unwrap();
        let local = dir.path().join(base::settings::SETTINGS_LOCAL_FILE);
        append_permission_rule(&local, AppendTarget::Allow, "Bash(git status)").unwrap();
        append_permission_rule(&local, AppendTarget::Deny, "Bash(rm -rf:*)").unwrap();

        let empty = TempDir::new().unwrap();
        let settings = base::settings::LoadedSettings::load(
            empty.path().to_path_buf(),
            empty.path().to_path_buf(),
            dir.path().to_path_buf(),
            "code",
            "m",
        );

        let loaded: Vec<_> = settings
            .rules
            .iter()
            .filter(|r| r.machine_local)
            .map(|r| (r.rule.tool.as_str(), r.rule.action))
            .collect();
        assert_eq!(
            loaded,
            vec![
                ("Bash(git status)", base::permission::RuleBehavior::Allow),
                ("Bash(rm -rf:*)", base::permission::RuleBehavior::Deny),
            ]
        );
    }

    #[test]
    fn append_creates_file_if_absent() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("subdir").join("settings.json");
        let modified = append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();
        assert!(modified);
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let arr = v["permission_rules"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0],
            serde_json::json!({"tool":"Bash(ls)","action":"allow"})
        );
    }

    #[test]
    fn append_idempotent_when_rule_present() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(
            &p,
            r#"{"permission_rules":[{"tool":"Bash(git status)","action":"allow"}]}"#,
        )
        .unwrap();
        let modified = append_permission_rule(&p, AppendTarget::Allow, "Bash(git status)").unwrap();
        assert!(!modified);
    }

    /// Same tool, different action is a different rule — the idempotency
    /// check keys on both.
    #[test]
    fn append_same_tool_with_a_different_action_is_not_a_duplicate() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();
        let modified = append_permission_rule(&p, AppendTarget::Deny, "Bash(ls)").unwrap();
        assert!(modified);
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["permission_rules"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn append_preserves_other_keys() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(
            &p,
            r#"{"model":"x","weird":42,"permission_rules":[{"tool":"Bash(rm:*)","action":"deny"}]}"#,
        )
        .unwrap();
        append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["model"], "x");
        assert_eq!(v["weird"], 42);
        assert_eq!(
            v["permission_rules"][0],
            serde_json::json!({"tool":"Bash(rm:*)","action":"deny"})
        );
        assert_eq!(
            v["permission_rules"][1],
            serde_json::json!({"tool":"Bash(ls)","action":"allow"})
        );
    }

    #[test]
    fn append_can_target_deny_or_ask() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        append_permission_rule(&p, AppendTarget::Deny, "Bash(rm -rf:*)").unwrap();
        append_permission_rule(&p, AppendTarget::Ask, "Bash(git push:*)").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            v["permission_rules"][0],
            serde_json::json!({"tool":"Bash(rm -rf:*)","action":"deny"})
        );
        assert_eq!(
            v["permission_rules"][1],
            serde_json::json!({"tool":"Bash(git push:*)","action":"ask"})
        );
    }

    /// `settings.local.json` holds provider keys and MCP headers; answering
    /// "always allow" must not leave it readable by everyone.
    #[cfg(unix)]
    #[test]
    fn the_local_settings_file_stays_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let p = dir.path().join(base::settings::SETTINGS_LOCAL_FILE);
        fs::write(&p, r#"{"providers":{"x":{"api_key":"sk"}}}"#).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();

        append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();

        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let fresh = dir
            .path()
            .join("fresh")
            .join(base::settings::SETTINGS_LOCAL_FILE);
        append_permission_rule(&fresh, AppendTarget::Allow, "Bash(ls)").unwrap();
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_not_written_through() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("dotfiles.json");
        fs::write(&target, "{}").unwrap();
        let p = dir.path().join("settings.local.json");
        std::os::unix::fs::symlink(&target, &p).unwrap();

        assert!(append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
    }

    #[test]
    fn build_rule_string_returns_none_for_empty() {
        assert!(build_rule_string("Bash", None).is_none());
        assert!(build_rule_string("Bash", Some("")).is_none());
        assert!(build_rule_string("Bash", Some("   ")).is_none());
    }

    #[test]
    fn build_rule_string_basic() {
        assert_eq!(
            build_rule_string("Bash", Some("git status")).as_deref(),
            Some("Bash(git status)")
        );
    }

    #[test]
    fn a_rule_string_carries_the_content_as_written() {
        assert_eq!(
            build_rule_string("WebFetch", Some("https://h.example/**")).as_deref(),
            Some("WebFetch(https://h.example/**)")
        );
    }

    /// An exact allow is written with `"match": "exact"`, once, and apart
    /// from a pattern rule of the same text.
    #[test]
    fn an_exact_allow_is_written_with_its_match() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.local.json");
        assert!(append_permission_rule(&p, AppendTarget::Allow, "Bash(ls *)").unwrap());
        assert!(append_exact_allow(&p, "Bash(ls *)").unwrap());
        assert!(!append_exact_allow(&p, "Bash(ls *)").unwrap());
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            written["permission_rules"],
            serde_json::json!([
                {"tool": "Bash(ls *)", "action": "allow"},
                {"tool": "Bash(ls *)", "action": "allow", "match": "exact"},
            ])
        );
    }
}
