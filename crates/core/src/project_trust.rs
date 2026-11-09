//! Workspace trust: whether a project's own configuration counts as the
//! user's, or is held to what an installed plugin package may do.
//!
//! The project tier — `<root>/.agix/settings*.json`, `.agix/agents`,
//! `.agix/hooks`, `.agents/skills` — is repository content: on a cloned
//! repository, whatever its author wrote. Trust is a property of the project
//! root and lives in the global tier (`project_trust`), so nothing inside the
//! project can grant it. An untrusted project tier may add and tighten, never
//! loosen — the same rule the engine already applies to what arrives with a
//! plugin. [`gate_layer`] applies it to a settings file; agent types, skill
//! commands and the hooks search path read [`ProjectTrust`] off
//! `Settings::project_layer`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::context::config::NetworkModeConfig;
use crate::settings::PermissionMode;

/// What a project no `trusted` pattern names is.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    #[default]
    Trusted,
    Untrusted,
}

/// `project_trust` in settings.json. Read from the global tier only; a scene
/// or project tier that sets it is ignored with a warning.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProjectTrustSettings {
    /// A project that no `trusted` pattern matches. `trusted` in 0.3.x;
    /// 0.4 makes it `untrusted`.
    pub default: TrustLevel,
    /// Trusted project roots: absolute paths, or globs where `*` stays within
    /// one path segment and `**` crosses them. A leading `~/` is the home
    /// directory. The global `settings.json` and `settings.local.json` lists
    /// add up rather than one replacing the other; `project.trust` writes to
    /// the second.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trusted: Vec<String>,
}

impl ProjectTrustSettings {
    /// The `project_trust` sections of `layers`, lowest first: the last
    /// `default` wins, the `trusted` lists are concatenated.
    pub fn from_layers<'a>(layers: impl IntoIterator<Item = &'a Value>) -> Self {
        let mut out = Self::default();
        for section in layers.into_iter().filter_map(|v| v.get("project_trust")) {
            if let Some(default) = section.get("default") {
                match serde_json::from_value(default.clone()) {
                    Ok(level) => out.default = level,
                    Err(e) => {
                        tracing::warn!(error = %e, "project_trust.default is not `trusted` or `untrusted`, ignoring it")
                    }
                }
            }
            for pattern in section
                .get("trusted")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !out.trusted.iter().any(|p| p == pattern) {
                    out.trusted.push(pattern.to_string());
                }
            }
        }
        out
    }

    /// What the global tier in `global_dir` says, read straight from its
    /// `settings.json` and `settings.local.json` — for a caller that needs
    /// the answer without loading a whole `Settings`.
    pub fn read_global(global_dir: &Path) -> Self {
        let layers: Vec<Value> = [
            crate::settings::SETTINGS_FILE,
            crate::settings::SETTINGS_LOCAL_FILE,
        ]
        .iter()
        .filter_map(|file| std::fs::read_to_string(global_dir.join(file)).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect();
        Self::from_layers(&layers)
    }

    /// Whether `root` is trusted, and the pattern that said so.
    pub fn decide(&self, root: &Path) -> (ProjectTrust, Option<String>) {
        if let Some(pattern) = self.trusted.iter().find(|p| pattern_matches(p, root)) {
            return (ProjectTrust::Trusted, Some(pattern.clone()));
        }
        let trust = match self.default {
            TrustLevel::Trusted => ProjectTrust::Trusted,
            TrustLevel::Untrusted => ProjectTrust::Untrusted,
        };
        (trust, None)
    }
}

/// Whether the project tier of a `Settings` is in effect as written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProjectTrust {
    /// No project tier of its own: no project at all, or one whose `.agix`
    /// is the global or scene directory.
    #[default]
    NoProject,
    Trusted,
    Untrusted,
}

impl ProjectTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoProject => "none",
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }

    pub fn is_untrusted(self) -> bool {
        self == Self::Untrusted
    }
}

/// What `Settings::load_from` found in the project tier.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectLayer {
    pub trust: ProjectTrust,
    /// The `project_trust.trusted` pattern that matched the root; `None`
    /// when the default decided.
    pub matched: Option<String>,
    /// The project-tier settings that take effect only while the project is
    /// trusted, as dotted keys (`hooks_config`, `mcp_servers.github`,
    /// `permission_rules.allow(Bash)`). Withheld while it is not.
    pub requires_trust: Vec<String>,
    /// The project tier's own `mcp_servers` entries, both of its files
    /// merged — whether or not they were withheld.
    pub mcp_servers: BTreeMap<String, Value>,
}

/// Whether `project` (a `.agix` directory) is a project tier of its own,
/// rather than the global or scene directory a project rooted at `$HOME` (or
/// no project) resolves to.
pub fn is_own_project_dir(project: &Path, global: &Path, scene: &Path) -> bool {
    !same_dir(project, global) && !same_dir(project, scene)
}

pub fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Whether a `project_trust.trusted` pattern names `root`. Compared against
/// the root as given and as the file system resolves it, so a pattern written
/// either way matches a root reached through a symlink.
pub fn pattern_matches(pattern: &str, root: &Path) -> bool {
    let pattern = expand_home(pattern);
    let roots: Vec<PathBuf> = std::iter::once(root.to_path_buf())
        .chain(root.canonicalize().ok())
        .collect();
    if pattern.contains(['*', '?', '[']) {
        let Ok(glob) = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
        else {
            tracing::warn!(%pattern, "project_trust.trusted pattern does not parse, ignoring it");
            return false;
        };
        let glob = glob.compile_matcher();
        return roots.iter().any(|r| glob.is_match(r));
    }
    let path = Path::new(&pattern);
    roots
        .iter()
        .any(|r| r == path || path.canonicalize().is_ok_and(|p| &p == r))
}

fn expand_home(pattern: &str) -> String {
    match (pattern.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest).display().to_string(),
        _ => pattern.to_string(),
    }
}

/// Inspect one project-tier settings file for what only a trusted project
/// may set, judged against `lower` — everything merged beneath it. Returns
/// what it found as dotted keys; with `withhold` set, also removes it from
/// `layer`, so what is left can only add or tighten.
///
/// `permission_rules` are not looked at here: the loader holds them apart
/// from the merge and drops the `allow` ones itself.
pub fn gate_layer(layer: &mut Map<String, Value>, lower: &Value, withhold: bool) -> Vec<String> {
    let mut found = Vec::new();

    if layer.get("hooks_config").is_some_and(|v| !v.is_null()) {
        found.push("hooks_config".to_string());
        if withhold {
            layer.remove("hooks_config");
        }
    }
    if layer
        .get("scripts")
        .is_some_and(|v| v.as_array().map_or(true, |a| !a.is_empty()))
    {
        found.push("scripts".to_string());
        if withhold {
            layer.remove("scripts");
        }
    }
    if let Some(servers) = layer.get("mcp_servers").and_then(Value::as_object) {
        found.extend(servers.keys().map(|name| format!("mcp_servers.{name}")));
        if withhold {
            layer.remove("mcp_servers");
        }
    }

    if let Some(mode) = layer.get("permission_mode") {
        let parse = |v: &Value| serde_json::from_value::<PermissionMode>(v.clone()).ok();
        let below = lower
            .get("permission_mode")
            .and_then(parse)
            .unwrap_or_default();
        if parse(mode).is_some_and(|m| m.permissiveness() > below.permissiveness()) {
            found.push("permission_mode".to_string());
            if withhold {
                layer.remove("permission_mode");
            }
        }
    }
    for (key, loose) in [
        ("allow_client_permission_override", true),
        ("disable_skill_shell_execution", false),
    ] {
        if loosens_bool(layer, lower, key, loose) {
            found.push(key.to_string());
            if withhold {
                layer.remove(key);
            }
        }
    }

    if let Some(Value::Object(sandbox)) = layer.get_mut("sandbox") {
        let below = lower.get("sandbox").cloned().unwrap_or(Value::Null);
        for key in gate_sandbox(sandbox, &below, withhold) {
            found.push(format!("sandbox.{key}"));
        }
    }

    found
}

fn gate_sandbox(sandbox: &mut Map<String, Value>, lower: &Value, withhold: bool) -> Vec<String> {
    let mut found = Vec::new();
    let mut flag = |sandbox: &mut Map<String, Value>, key: String, remove: &str| {
        found.push(key);
        if withhold {
            sandbox.remove(remove);
        }
    };

    for (key, loose) in [
        ("dangerously_disable_sandbox", true),
        ("read_anywhere", true),
        ("require_enforcement", false),
        ("unattended_requires_network_policy", false),
    ] {
        if loosens_bool(sandbox, lower, key, loose) {
            flag(sandbox, key.to_string(), key);
        }
    }

    for key in [
        "additional_directories",
        "read_directories",
        "allow_read",
        "allow_write",
        "allowed_domains",
    ] {
        if let Some(listed) = sandbox.get(key) {
            if !is_subset(listed, lower.get(key)) {
                flag(sandbox, key.to_string(), key);
            }
        }
    }

    if let Some(listed) = sandbox.get("deny_write") {
        if !is_subset(
            lower.get("deny_write").unwrap_or(&Value::Null),
            Some(listed),
        ) {
            flag(sandbox, "deny_write".to_string(), "deny_write");
        }
    }
    // An empty `deny_read` means the built-in credential list; any non-empty
    // one replaces that list rather than adding to it.
    if let Some(listed) = sandbox.get("deny_read") {
        let below = lower.get("deny_read").unwrap_or(&Value::Null);
        let keeps = if is_empty_list(below) {
            is_empty_list(listed)
        } else {
            is_subset(below, Some(listed))
        };
        if !keeps {
            flag(sandbox, "deny_read".to_string(), "deny_read");
        }
    }

    if let Some(mode) = sandbox.get("network_mode") {
        let rank = |v: &Value| {
            serde_json::from_value::<NetworkModeConfig>(v.clone())
                .ok()
                .map(|m| match m {
                    NetworkModeConfig::DenyAll => 0,
                    NetworkModeConfig::Allowlist => 1,
                    NetworkModeConfig::Unrestricted => 2,
                })
        };
        let below = lower.get("network_mode").and_then(rank).unwrap_or(2);
        if rank(mode).is_some_and(|r| r > below) {
            flag(sandbox, "network_mode".to_string(), "network_mode");
        }
    }

    if let Some(Value::Object(groups)) = sandbox.get_mut("cache_directories") {
        let below = lower.get("cache_directories");
        let widened: Vec<String> = groups
            .iter()
            .filter(|(group, dirs)| !is_subset(dirs, below.and_then(|b| b.get(group.as_str()))))
            .map(|(group, _)| group.clone())
            .collect();
        for group in widened {
            found.push(format!("cache_directories.{group}"));
            if withhold {
                groups.remove(&group);
            }
        }
    }

    if let Some(Value::Object(env)) = sandbox.get_mut("subprocess_env") {
        let below = lower.get("subprocess_env");
        let below_mode = below
            .and_then(|b| b.get("mode"))
            .and_then(Value::as_str)
            .unwrap_or("inherit");
        if env.get("mode").and_then(Value::as_str) == Some("inherit") && below_mode == "allowlist" {
            found.push("subprocess_env.mode".to_string());
            if withhold {
                env.remove("mode");
            }
        }
        if env
            .get("allow")
            .is_some_and(|allow| !is_subset(allow, below.and_then(|b| b.get("allow"))))
        {
            found.push("subprocess_env.allow".to_string());
            if withhold {
                env.remove("allow");
            }
        }
    }

    found
}

/// Whether `layer` sets `key` to its `loose` value where `lower` did not.
fn loosens_bool(layer: &Map<String, Value>, lower: &Value, key: &str, loose: bool) -> bool {
    layer.get(key).and_then(Value::as_bool) == Some(loose)
        && lower.get(key).and_then(Value::as_bool) != Some(loose)
}

/// Whether every entry of the array `small` is in the array `big` (a
/// missing `big` holds nothing).
fn is_subset(small: &Value, big: Option<&Value>) -> bool {
    let big = big.and_then(Value::as_array);
    small.as_array().map_or(true, |items| {
        items.iter().all(|i| big.is_some_and(|b| b.contains(i)))
    })
}

fn is_empty_list(v: &Value) -> bool {
    v.as_array().map_or(true, Vec::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lower() -> Value {
        serde_json::to_value(crate::settings::Settings::defaults_for("m")).unwrap()
    }

    fn gate(layer: Value, withhold: bool) -> (Value, Vec<String>) {
        let mut layer = layer.as_object().unwrap().clone();
        let found = gate_layer(&mut layer, &lower(), withhold);
        (Value::Object(layer), found)
    }

    #[test]
    fn what_runs_code_or_reaches_out_is_withheld_whole() {
        let (left, found) = gate(
            json!({
                "hooks_config": {"PreToolUse": []},
                "scripts": [{"path": "x.js", "point": "prompt.block", "entry": "f"}],
                "mcp_servers": {"fs": {"type": "stdio", "command": "x"}},
                "language": "en",
            }),
            true,
        );
        assert_eq!(
            found,
            ["hooks_config", "scripts", "mcp_servers.fs"].map(String::from)
        );
        assert_eq!(left, json!({"language": "en"}));
    }

    #[test]
    fn a_mode_may_tighten_but_not_loosen() {
        let (left, found) = gate(json!({"permission_mode": "dontAsk"}), true);
        assert!(found.is_empty());
        assert_eq!(left["permission_mode"], "dontAsk");

        let (left, found) = gate(json!({"permission_mode": "yolo"}), true);
        assert_eq!(found, ["permission_mode"]);
        assert!(left.get("permission_mode").is_none());
    }

    #[test]
    fn sandbox_keys_that_widen_are_withheld_and_ones_that_narrow_kept() {
        let (left, found) = gate(
            json!({"sandbox": {
                "read_anywhere": true,
                "require_enforcement": true,
                "additional_directories": ["/etc"],
                "network_mode": "deny_all",
                "deny_read": [],
                "cache_directories": {"python": [], "zig": ["~/.zig"]},
                "subprocess_env": {"mode": "allowlist", "allow": ["AWS_SECRET_ACCESS_KEY"]},
            }}),
            true,
        );
        assert_eq!(
            found,
            [
                "sandbox.read_anywhere",
                "sandbox.additional_directories",
                "sandbox.cache_directories.zig",
                "sandbox.subprocess_env.allow",
            ]
            .map(String::from)
        );
        assert_eq!(
            left,
            json!({"sandbox": {
                "require_enforcement": true,
                "network_mode": "deny_all",
                "deny_read": [],
                "cache_directories": {"python": []},
                "subprocess_env": {"mode": "allowlist"},
            }})
        );
    }

    #[test]
    fn a_deny_list_must_keep_everything_beneath_it() {
        let (_, found) = gate(json!({"sandbox": {"deny_read": ["/srv/secret"]}}), true);
        assert_eq!(
            found,
            ["sandbox.deny_read"],
            "a non-empty list would replace the built-in credential list"
        );
    }

    #[test]
    fn without_withholding_the_layer_is_untouched_but_still_reported() {
        let layer = json!({"hooks_config": {}, "sandbox": {"read_anywhere": true}});
        let (left, found) = gate(layer.clone(), false);
        assert_eq!(left, layer);
        assert_eq!(found, ["hooks_config", "sandbox.read_anywhere"]);
    }

    #[test]
    fn patterns_name_roots_exactly_or_by_glob() {
        let root = Path::new("/srv/ipd/lines/a");
        assert!(pattern_matches("/srv/ipd/lines/a", root));
        assert!(pattern_matches("/srv/ipd/lines/a/", root));
        assert!(pattern_matches("/srv/ipd/lines/*", root));
        assert!(pattern_matches("/srv/**", root));
        assert!(!pattern_matches("/srv/*", root));
        assert!(!pattern_matches("/srv/ipd/lines", root));
    }

    #[test]
    fn the_trusted_lists_add_up_and_the_last_default_wins() {
        let file = json!({"project_trust": {"default": "untrusted", "trusted": ["/a"]}});
        let local = json!({"project_trust": {"trusted": ["/b", "/a"]}});
        let settings = ProjectTrustSettings::from_layers([&file, &local]);
        assert_eq!(settings.default, TrustLevel::Untrusted);
        assert_eq!(settings.trusted, ["/a", "/b"]);
        assert_eq!(
            settings.decide(Path::new("/b")),
            (ProjectTrust::Trusted, Some("/b".into()))
        );
        assert_eq!(
            settings.decide(Path::new("/c")),
            (ProjectTrust::Untrusted, None)
        );
    }
}
