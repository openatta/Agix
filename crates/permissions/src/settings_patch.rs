//! Append a permission rule to a settings.json /
//! settings.local.json file atomically, preserving unknown fields. Used by
//! the interactive ask dialog's "always allow" shortcut.
//!
//! Atomicity: write to `<path>.tmp-<pid>` then rename. If the target dir
//! doesn't exist, it's created.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;

/// Serializes concurrent callers of [`append_permission_rule`] within this
/// process. The function itself is read-modify-write (read whole file,
/// mutate in memory, write via tmp+rename) with no cross-call atomicity —
/// two calls racing on the *same* settings file (e.g. two tool calls in one
/// turn both answered with an "always allow" decision concurrently, see
/// `crates/runtime/src/turn.rs`'s `PermitAlways` handling) can otherwise
/// silently lose one of the two appended rules. A single global lock is
/// enough: this path is a rare, user-interactive event, not a hot loop, so
/// serializing it process-wide costs nothing measurable. Does not protect
/// against a *different process* writing the same file concurrently — that
/// remains an accepted, documented limitation (see `settings_patch`'s
/// design notes).
static WRITE_LOCK: Mutex<()> = Mutex::new(());

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
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("settings.json malformed: {0}")]
    BadJson(#[from] serde_json::Error),
    #[error("settings.json root must be an object")]
    NotObject,
}

/// Append `rule_string` to `permission_rules` in the given settings.json
/// file, as `{"tool": <rule_string>, "action": <target>}`. If the file
/// doesn't exist, it's created with `{}` first. If an identical entry is
/// already present, the file is left untouched.
///
/// The shape here is the one `base::interface::settings::Settings`
/// deserializes — writing anything else produces a file `Settings::load`
/// silently drops as an unknown key.
///
/// Returns `Ok(true)` if the file was modified, `Ok(false)` if the rule was
/// already present.
pub fn append_permission_rule(
    settings_path: &Path,
    target: AppendTarget,
    rule_string: &str,
) -> Result<bool, AppendError> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(parent) = settings_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    // Load or initialize
    let mut value: serde_json::Value = if settings_path.exists() {
        let bytes = fs::read(settings_path)?;
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            serde_json::json!({})
        } else {
            serde_json::from_slice(&bytes)?
        }
    } else {
        serde_json::json!({})
    };

    let root = value.as_object_mut().ok_or(AppendError::NotObject)?;

    let arr = root
        .entry("permission_rules".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let arr = arr.as_array_mut().ok_or(AppendError::NotObject)?;

    let action = target.action();
    let already = arr.iter().any(|v| {
        v.get("tool").and_then(|t| t.as_str()) == Some(rule_string)
            && v.get("action").and_then(|a| a.as_str()) == Some(action)
    });
    if already {
        return Ok(false);
    }
    arr.push(serde_json::json!({ "tool": rule_string, "action": action }));

    // Atomic write
    let tmp = settings_path.with_extension(format!("tmp-{}", std::process::id()));
    let body = serde_json::to_string_pretty(&value)?;
    fs::write(&tmp, body)?;
    fs::rename(&tmp, settings_path)?;
    Ok(true)
}

/// Build a canonical rule string `<tool_name>(<content>)`. Returns None when
/// content is empty (no useful pattern can be derived).
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
    use tempfile::TempDir;

    /// N threads appending distinct rules to the *same* settings file
    /// concurrently must all survive — without `WRITE_LOCK` serializing them,
    /// a thread reads the file before an earlier thread's rename lands, then
    /// overwrites it on its own write.
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

    /// The written shape must be the one `Settings` actually deserializes —
    /// a file whose rules land under a key `Settings` doesn't know is
    /// indistinguishable from an empty one once loaded.
    #[test]
    fn appended_rules_round_trip_through_settings_load() {
        let dir = TempDir::new().unwrap();
        let local = dir
            .path()
            .join(base::interface::settings::SETTINGS_LOCAL_FILE);
        append_permission_rule(&local, AppendTarget::Allow, "Bash(git status)").unwrap();
        append_permission_rule(&local, AppendTarget::Deny, "Bash(rm -rf:*)").unwrap();

        let empty = TempDir::new().unwrap();
        let settings = base::interface::settings::Settings::load(
            empty.path().to_path_buf(),
            empty.path().to_path_buf(),
            dir.path().to_path_buf(),
            "code",
            "m",
        );

        let loaded: Vec<_> = settings
            .local_permission_rules
            .iter()
            .map(|r| (r.tool.as_str(), r.action))
            .collect();
        assert_eq!(
            loaded,
            vec![
                (
                    "Bash(git status)",
                    base::interface::settings::PermissionAction::Allow
                ),
                (
                    "Bash(rm -rf:*)",
                    base::interface::settings::PermissionAction::Deny
                ),
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
}
