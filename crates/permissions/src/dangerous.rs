//! Dangerous rule detection for permission rules.
//!
//! A rule is "dangerous" when it allows operations that could compromise
//! system security — overly broad allow rules, destructive tool access,
//! or unrestricted access to sensitive capabilities.

use crate::rule::format_rule_string;
use base::permission::{PermissionRule, RuleBehavior};

/// Severity level for a dangerous rule finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Potentially risky but may be intentional (e.g., Bash with rm:* allow).
    Warning,
    /// High-risk rule that severely weakens security (e.g., Bash allow with
    /// no content restriction).
    Critical,
}

impl Severity {
    /// Human-readable label for log output.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Warning => "WARNING",
            Self::Critical => "CRITICAL",
        }
    }
}

/// Describes one dangerous rule finding.
#[derive(Debug, Clone)]
pub struct DangerousRule {
    /// The rule that is considered dangerous.
    pub rule: PermissionRule,
    /// Index of the rule in the original rules list (0-based).
    pub rule_index: usize,
    /// Severity of the finding.
    pub severity: Severity,
    /// Human-readable explanation of why this rule is dangerous.
    pub reason: String,
}

/// Tools that should never have unrestricted allow rules. These are tools
/// that can execute arbitrary commands or access arbitrary files.
const DANGEROUS_TOOLS: &[&str] = &["Bash", "Write", "Edit", "FileWrite", "FileEdit"];

/// Bash subcommands whose broad allow (prefix wildcard) is risky. Not an
/// exhaustive list — just the most obviously destructive ones.
const DESTRUCTIVE_BASH_SUBCOMMANDS: &[&str] = &[
    "rm",
    "dd",
    "mkfs",
    "chmod",
    "chown",
    "sudo",
    "python",
    "python3",
    "node",
    "npm",
    "npx",
    "pip",
    "pip3",
    "curl",
    "wget",
    "systemctl",
    "passwd",
    "usermod",
    "groupmod",
    "fdisk",
    "parted",
    "mkswap",
    "swapon",
    "swapoff",
    "mount",
    "umount",
    "insmod",
    "rmmod",
    "iwconfig",
    "ifconfig",
    "ip",
];

/// Check a ruleset for dangerous rules.
///
/// Scans all rules and flags those that represent security risks:
///
/// | Pattern | Severity | Reason |
/// |---|---|---|
/// | `Bash` / `Bash(*)` with Allow | Critical | Unrestricted command execution |
/// | `Write` / `Write(*)` with Allow | Critical | Unrestricted file writes |
/// | `Edit` / `Edit(*)` with Allow | Critical | Unrestricted file edits |
/// | `Bash(rm:*)` with Allow | Warning | Broad allow for destructive subcommands |
/// | `Bash(dd:*)` with Allow | Warning | Broad allow for disk destruction |
/// | `Bash(python:*)` with Allow | Warning | Broad allow for arbitrary code execution |
pub fn detect_dangerous_rules(rules: &[PermissionRule]) -> Vec<DangerousRule> {
    let mut result: Vec<DangerousRule> = Vec::new();

    for (i, rule) in rules.iter().enumerate() {
        if rule.behavior != RuleBehavior::Allow {
            continue;
        }

        // 1. Overly broad allow on a dangerous tool (Critical).
        if let Some(danger) = is_broad_allow(rule) {
            result.push(DangerousRule {
                rule: rule.clone(),
                rule_index: i,
                severity: Severity::Critical,
                reason: danger,
            });
            continue; // skip subcommand checks — Critical subsumes Warning
        }

        // 2. Destructive subcommand prefix wildcard on Bash (Warning).
        if let Some(danger) = is_destructive_bash_allow(rule) {
            result.push(DangerousRule {
                rule: rule.clone(),
                rule_index: i,
                severity: Severity::Warning,
                reason: danger,
            });
        }
    }

    result
}

/// Check if a rule is an overly broad allow on a dangerous tool.
///
/// Returns a reason string if dangerous, None if not.
fn is_broad_allow(rule: &PermissionRule) -> Option<String> {
    if !DANGEROUS_TOOLS.contains(&rule.tool_name.as_str()) {
        return None;
    }

    let tool_name = &rule.tool_name;
    match &rule.rule_content {
        None => Some(format!(
            "Allow rule with no content restriction on {}: permits unrestricted use \
             of a powerful tool that can access any file or execute arbitrary commands",
            tool_name
        )),
        Some(content) if is_unscoped_grant(tool_name, Some(content)) => Some(format!(
            "Allow rule with wildcard '{content}' on {tool_name}: nothing literal comes \
             before its first wildcard, so it effectively permits unrestricted use, same \
             as having no content restriction"
        )),
        _ => None,
    }
}

/// Whether a grant with this rule content reaches every call its tool can
/// make: no content at all, or a pattern that matches from an empty prefix —
/// nothing but `/`, `.`, `~` or blanks ahead of its first wildcard (`*`,
/// `**`, `/**`, `~/**`, `* --version`, `:*`). A path without a wildcard
/// covers the directory it names, so `/`, `~` and `.` are unscoped too. For a
/// URL-valued tool, a pattern whose host pins no domain (`https://*/**`,
/// `https://*.com/**`, see `base::url_rule::UrlPattern::host_is_open`) is
/// unscoped as well.
pub fn is_unscoped_grant(tool_name: &str, content: Option<&str>) -> bool {
    let Some(content) = content else {
        return true;
    };
    if crate::ruleset::URL_RULE_TOOLS.contains(&tool_name)
        && base::url_rule::UrlPattern::parse(content).is_ok_and(|p| p.host_is_open())
    {
        return true;
    }
    let literal = match content.strip_suffix(":*") {
        Some(prefix) => prefix,
        None => content
            .find(['*', '?', '[', '{'])
            .map_or(content, |wildcard| &content[..wildcard]),
    };
    literal
        .trim_matches(|c: char| matches!(c, '/' | '.' | '~') || c.is_whitespace())
        .is_empty()
}

/// Check if a Bash allow rule has a destructive subcommand prefix pattern.
///
/// Only flags `prefix:*` patterns (e.g. `rm:*`, `python:*`). Exact command
/// matches like `rm -rf /tmp` are specific and intentional, not flagged.
fn is_destructive_bash_allow(rule: &PermissionRule) -> Option<String> {
    if rule.tool_name != "Bash" {
        return None;
    }

    let content = rule.rule_content.as_deref()?;

    for cmd in DESTRUCTIVE_BASH_SUBCOMMANDS.iter().copied() {
        let prefix_pattern = format!("{cmd}:*");
        if content != prefix_pattern {
            continue;
        }
        let risk = match cmd {
            "rm" => "allows broad file deletion commands (rm:*)",
            "dd" => "allows raw disk I/O commands (dd:*) that can wipe storage",
            "mkfs" => "allows filesystem creation commands (mkfs:*) that can destroy data",
            "chmod" => "allows broad permission modification (chmod:*)",
            "chown" => "allows broad ownership changes (chown:*)",
            "sudo" => "allows privilege escalation (sudo:*)",
            "python" | "python3" => "allows arbitrary code execution (python:*)",
            "node" => "allows arbitrary code execution (node:*)",
            "npm" | "npx" => "allows arbitrary package installation/execution (npm:*)",
            "pip" | "pip3" => "allows arbitrary package installation (pip:*)",
            "curl" | "wget" => {
                "allows network data transfer (curl:*) that may lead to code execution"
            }
            "systemctl" => "allows system service management (systemctl:*)",
            "passwd" => "allows password changes (passwd:*)",
            "usermod" | "groupmod" => "allows user/group management (cmd:*)",
            "fdisk" | "parted" | "mkswap" | "swapon" | "swapoff" => {
                "allows disk partition management (cmd:*)"
            }
            "mount" | "umount" => "allows filesystem mount operations (mount:*)",
            "insmod" | "rmmod" => "allows kernel module management (insmod:*)",
            "iwconfig" | "ifconfig" | "ip" => "allows network interface configuration (ip:*)",
            _ => "allows potentially destructive subcommands via prefix wildcard (:*)",
        };
        return Some(format!(
            "Bash allow rule with content '{content}' {risk} — consider narrowing \
             to specific subcommands rather than the whole category"
        ));
    }

    None
}

/// Format a single dangerous rule warning for human-readable display.
///
/// Returns a string like:
/// ```text
/// [CRITICAL] Dangerous permission rule: "Bash  [allow]" at line 3 — Allow rule with no content restriction ...
/// ```
pub fn format_dangerous_warning(danger: &DangerousRule) -> String {
    let rule_str = format_rule_string(&danger.rule);
    let severity_label = danger.severity.label();

    format!(
        "[{severity_label}] Dangerous permission rule: \"{rule_str}  [allow]\" at line {} — {}",
        danger.rule_index + 1,
        danger.reason,
    )
}

/// Format and log all dangerous rule warnings via `tracing::warn!`.
///
/// Each dangerous rule is logged as a separate warning so individual
/// findings are visible in filtered log output.
pub fn log_dangerous_warnings(dangers: &[DangerousRule]) {
    for d in dangers {
        tracing::warn!("{}", format_dangerous_warning(d));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::permission::{RuleBehavior, RuleSource};

    fn rule(
        tool: &str,
        content: Option<&str>,
        behavior: RuleBehavior,
        source: RuleSource,
    ) -> PermissionRule {
        PermissionRule {
            source,
            behavior,
            tool_name: tool.into(),
            rule_content: content.map(|s| s.into()),
        }
    }

    #[test]
    fn unrestricted_bash_allow_is_critical() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Critical);
        assert!(dangers[0].reason.contains("Bash"));
    }

    #[test]
    fn bash_wildcard_allow_is_critical() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            Some("*"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Critical);
    }

    #[test]
    fn unrestricted_write_allow_is_critical() {
        let dangers = detect_dangerous_rules(&[rule(
            "Write",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Critical);
    }

    #[test]
    fn unrestricted_edit_allow_is_critical() {
        let dangers = detect_dangerous_rules(&[rule(
            "Edit",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Critical);
    }

    #[test]
    fn edit_wildcard_allow_is_critical() {
        let dangers = detect_dangerous_rules(&[rule(
            "Edit",
            Some("*"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Critical);
    }

    #[test]
    fn deny_rules_not_dangerous() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            None,
            RuleBehavior::Deny,
            RuleSource::UserSettings,
        )]);
        assert!(dangers.is_empty());
    }

    #[test]
    fn ask_rules_not_dangerous() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            None,
            RuleBehavior::Ask,
            RuleSource::UserSettings,
        )]);
        assert!(dangers.is_empty());
    }

    #[test]
    fn specific_bash_allow_not_dangerous() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            Some("git status"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(dangers.is_empty());
    }

    #[test]
    fn safe_tools_not_dangerous() {
        let dangers = detect_dangerous_rules(&[rule(
            "Read",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(
            dangers.is_empty(),
            "Read should not be flagged as dangerous"
        );
    }

    #[test]
    fn glob_allow_not_dangerous() {
        let dangers = detect_dangerous_rules(&[rule(
            "Glob",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(dangers.is_empty());
    }

    #[test]
    fn a_grant_is_unscoped_when_it_can_match_from_an_empty_prefix() {
        for content in [
            None,
            Some("*"),
            Some("**"),
            Some("/**"),
            Some("**/*"),
            Some("~/**"),
            Some("./**"),
            Some("* --version"),
            Some("?"),
            Some("[a-z]*"),
            Some("{a,b}"),
            Some(":*"),
            Some(" :*"),
            Some("/"),
            Some("~"),
            Some("."),
        ] {
            assert!(is_unscoped_grant("Bash", content), "{content:?}");
        }
        for content in [
            "git status:*",
            "cargo test",
            "src/**",
            "/tmp/build/*",
            "./build.sh",
            "~/notes/**",
        ] {
            assert!(!is_unscoped_grant("Bash", Some(content)), "{content}");
        }
        for content in ["https://*/**", "https://*.com/**", "http*://**"] {
            assert!(is_unscoped_grant("WebFetch", Some(content)), "{content}");
        }
        assert!(!is_unscoped_grant(
            "WebFetch",
            Some("https://*.example.com/**")
        ));
    }

    #[test]
    fn bash_rm_wildcard_is_warning() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            Some("rm:*"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Warning);
        assert!(dangers[0].reason.contains("rm"));
    }

    #[test]
    fn bash_dd_wildcard_is_warning() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            Some("dd:*"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Warning);
    }

    #[test]
    fn bash_python_wildcard_is_warning() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            Some("python:*"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Warning);
    }

    #[test]
    fn bash_sudo_wildcard_is_warning() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            Some("sudo:*"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Warning);
    }

    #[test]
    fn bash_curl_wildcard_is_warning() {
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            Some("curl:*"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert_eq!(dangers.len(), 1);
        assert_eq!(dangers[0].severity, Severity::Warning);
    }

    #[test]
    fn bash_specific_subcommand_not_destructive() {
        // "git status" is not a destructive subcommand
        let dangers = detect_dangerous_rules(&[rule(
            "Bash",
            Some("git status"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(dangers.is_empty());
    }

    #[test]
    fn multiple_dangers_detected() {
        let rules = vec![
            // Critical
            rule("Bash", None, RuleBehavior::Allow, RuleSource::UserSettings),
            // Warning (but subsumed by the Critical above in practice;
            // each rule is evaluated independently)
            rule(
                "Bash",
                Some("rm:*"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            // Safe
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        let dangers = detect_dangerous_rules(&rules);
        assert_eq!(dangers.len(), 2);
        assert_eq!(dangers[0].severity, Severity::Critical);
        assert_eq!(dangers[1].severity, Severity::Warning);
    }

    #[test]
    fn empty_rules_produce_no_dangers() {
        let rules: Vec<PermissionRule> = vec![];
        let dangers = detect_dangerous_rules(&rules);
        assert!(dangers.is_empty());
    }

    #[test]
    fn single_safe_rule_produces_no_dangers() {
        let rules = vec![rule(
            "Bash",
            Some("git status"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )];
        let dangers = detect_dangerous_rules(&rules);
        assert!(dangers.is_empty());
    }

    #[test]
    fn format_dangerous_warning_output() {
        let danger = DangerousRule {
            rule: PermissionRule {
                source: RuleSource::UserSettings,
                behavior: RuleBehavior::Allow,
                tool_name: "Bash".into(),
                rule_content: None,
            },
            rule_index: 2,
            severity: Severity::Critical,
            reason: "unrestricted command execution".into(),
        };
        let msg = format_dangerous_warning(&danger);
        assert!(msg.contains("[CRITICAL]"), "msg: {msg}");
        assert!(msg.contains("Bash"), "msg: {msg}");
        assert!(msg.contains("at line 3"), "msg: {msg}");
        assert!(msg.contains("unrestricted command execution"), "msg: {msg}");
    }

    #[test]
    fn format_warning_severity_label() {
        let danger = DangerousRule {
            rule: PermissionRule {
                source: RuleSource::UserSettings,
                behavior: RuleBehavior::Allow,
                tool_name: "Bash".into(),
                rule_content: Some("rm:*".into()),
            },
            rule_index: 0,
            severity: Severity::Warning,
            reason: "broad rm allow".into(),
        };
        let msg = format_dangerous_warning(&danger);
        assert!(msg.contains("[WARNING]"), "msg: {msg}");
        assert!(msg.contains("rm:*"), "msg: {msg}");
    }
}
