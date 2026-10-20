//! Detection of permission rules that can never take effect.
//!
//! Under the rule engine's verdict order (any matching deny wins, then any
//! ask, then allow — see `ruleset`), a rule is dead when every call it could
//! match is also matched by a rule of a stronger behavior: an allow covered
//! by a deny or an ask, or an ask covered by a deny. Position in the list and
//! the source tier play no part, exactly as they play no part in evaluation.
//! Warning about these catches the common mistake of expecting a narrow
//! allow to carve an exception out of a broad deny.

use crate::rule::format_rule_string;
use crate::ruleset::{matches_content, matches_tool_name};
use base::permission::{PermissionRule, RuleBehavior, RuleSource};

/// One rule that never takes effect, and a rule that overrides it.
#[derive(Debug, Clone)]
pub struct ShadowedRule {
    /// The rule that never takes effect.
    pub shadowed: PermissionRule,
    /// A stronger-behavior rule covering every call `shadowed` matches.
    pub shadowing: PermissionRule,
    /// Index of the shadowed rule in the original rules list (0-based).
    pub shadowed_index: usize,
    /// Index of the shadowing rule in the original rules list (0-based).
    pub shadowing_index: usize,
}

/// Every rule that is fully covered by a rule of a stronger behavior,
/// reported once each against the first such rule in list order.
///
/// | Overriding | Overridden | Detected? |
/// |---|---|---|
/// | `Bash` deny | `Bash(git status)` allow | Yes |
/// | `Bash(git:*)` ask | `Bash(git status)` allow | Yes |
/// | `Read(/etc/**)` deny | `Read(/etc/passwd)` ask | Yes |
/// | `Bash(git status)` deny | `Bash(git:*)` allow | No — the allow still covers `git log` |
/// | `Bash` allow | `Bash(rm:*)` deny | No — the deny is the one that wins |
/// | `Bash` deny | `Bash` deny | No — redundant, but it changes nothing |
pub fn detect_shadowed_rules(rules: &[PermissionRule]) -> Vec<ShadowedRule> {
    let mut result = Vec::new();
    for (j, weaker) in rules.iter().enumerate() {
        let overriding = rules.iter().enumerate().find(|(_, stronger)| {
            strength(stronger.behavior) > strength(weaker.behavior)
                && matches_tool_name(&stronger.tool_name, &weaker.tool_name)
                && is_shadowed_by(stronger, weaker)
        });
        if let Some((i, stronger)) = overriding {
            result.push(ShadowedRule {
                shadowed: weaker.clone(),
                shadowing: stronger.clone(),
                shadowed_index: j,
                shadowing_index: i,
            });
        }
    }
    result
}

fn strength(b: RuleBehavior) -> u8 {
    match b {
        RuleBehavior::Deny => 2,
        RuleBehavior::Ask => 1,
        RuleBehavior::Allow => 0,
    }
}

/// Whether `covering`'s pattern matches every content `covered`'s does.
///
/// Not a full containment check for arbitrary globs (undecidable in general):
/// `covered`'s pattern is tried as a content string against `covering`'s, which
/// catches the practical cases — `*` over `git status`, `git:*` over
/// `git status`, `/etc/**` over `/etc/passwd`.
pub(crate) fn is_shadowed_by(covering: &PermissionRule, covered: &PermissionRule) -> bool {
    match (&covering.rule_content, &covered.rule_content) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(hc), Some(lc)) => hc == lc || matches_content(hc, lc),
    }
}

/// One human-readable warning, naming each rule's settings source.
pub fn format_shadow_warning(shadow: &ShadowedRule) -> String {
    format!(
        "Permission rule never takes effect: \"{}\" [{}] from {} is overridden by \"{}\" [{}] from {} \
         (a matching deny always wins, then ask, then allow)",
        format_rule_string(&shadow.shadowed),
        behavior_label(shadow.shadowed.behavior),
        source_label(shadow.shadowed.source),
        format_rule_string(&shadow.shadowing),
        behavior_label(shadow.shadowing.behavior),
        source_label(shadow.shadowing.source),
    )
}

/// Log each warning separately so individual findings survive log filtering.
pub fn log_shadow_warnings(shadows: &[ShadowedRule]) {
    for s in shadows {
        tracing::warn!("{}", format_shadow_warning(s));
    }
}

fn behavior_label(b: RuleBehavior) -> &'static str {
    match b {
        RuleBehavior::Allow => "allow",
        RuleBehavior::Deny => "deny",
        RuleBehavior::Ask => "ask",
    }
}

fn source_label(s: RuleSource) -> &'static str {
    match s {
        RuleSource::UserSettings => "user settings",
        RuleSource::ProjectSettings => "project settings",
        RuleSource::LocalSettings => "local settings",
        RuleSource::CliArg => "command-line arguments",
        RuleSource::Session => "this session",
        RuleSource::PolicySettings => "managed policy",
        RuleSource::Command => "an active skill",
        RuleSource::Plugin => "a plugin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn pairs(rules: &[PermissionRule]) -> Vec<(usize, usize)> {
        detect_shadowed_rules(rules)
            .iter()
            .map(|s| (s.shadowed_index, s.shadowing_index))
            .collect()
    }

    #[test]
    fn a_broad_deny_overrides_a_narrow_allow_listed_before_it() {
        let rules = vec![
            rule(
                "Bash",
                Some("git merge:*"),
                RuleBehavior::Allow,
                RuleSource::ProjectSettings,
            ),
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
        ];
        assert_eq!(pairs(&rules), vec![(0, 1)]);
    }

    #[test]
    fn a_narrow_deny_inside_a_broad_allow_is_not_reported() {
        let rules = vec![
            rule("Bash", None, RuleBehavior::Allow, RuleSource::UserSettings),
            rule(
                "Bash",
                Some("git merge:*"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
        ];
        assert!(pairs(&rules).is_empty());
    }

    #[test]
    fn an_ask_overrides_a_covered_allow_and_a_deny_overrides_a_covered_ask() {
        let rules = vec![
            rule(
                "Bash",
                Some("git:*"),
                RuleBehavior::Ask,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::LocalSettings,
            ),
            rule(
                "Bash",
                Some("*"),
                RuleBehavior::Deny,
                RuleSource::PolicySettings,
            ),
        ];
        assert_eq!(pairs(&rules), vec![(0, 2), (1, 0)]);
    }

    #[test]
    fn a_path_glob_deny_covers_a_specific_path_allow() {
        let rules = vec![
            rule(
                "Read",
                Some("/etc/**"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Read",
                Some("/etc/passwd"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        assert_eq!(pairs(&rules), vec![(1, 0)]);
    }

    #[test]
    fn a_specific_deny_does_not_cover_a_broader_allow() {
        let rules = vec![
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git:*"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        assert!(pairs(&rules).is_empty());
    }

    #[test]
    fn same_behavior_duplicates_are_not_reported() {
        let rules = vec![
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
            rule(
                "Bash",
                None,
                RuleBehavior::Deny,
                RuleSource::ProjectSettings,
            ),
            rule(
                "Bash",
                Some("ls"),
                RuleBehavior::Deny,
                RuleSource::ProjectSettings,
            ),
        ];
        assert!(pairs(&rules).is_empty());
    }

    #[test]
    fn an_mcp_server_deny_covers_one_of_its_tools() {
        let rules = vec![
            rule(
                "mcp__github__create_issue",
                None,
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "mcp__github",
                None,
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
        ];
        assert_eq!(pairs(&rules), vec![(0, 1)]);
    }

    #[test]
    fn an_mcp_tool_deny_does_not_cover_the_whole_server() {
        let rules = vec![
            rule(
                "mcp__github",
                None,
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "mcp__github__create_issue",
                None,
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
        ];
        assert!(pairs(&rules).is_empty());
    }

    #[test]
    fn rules_for_different_tools_or_disjoint_content_do_not_interact() {
        let rules = vec![
            rule("Write", None, RuleBehavior::Deny, RuleSource::UserSettings),
            rule("Read", None, RuleBehavior::Allow, RuleSource::UserSettings),
            rule(
                "Bash",
                Some("git:*"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("rm:*"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
        ];
        assert!(pairs(&rules).is_empty());
        assert!(pairs(&[]).is_empty());
    }

    #[test]
    fn the_warning_names_both_rules_their_behaviors_and_their_sources() {
        let rules = vec![
            rule(
                "Bash",
                Some("git merge:*"),
                RuleBehavior::Allow,
                RuleSource::ProjectSettings,
            ),
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
        ];
        let msg = format_shadow_warning(&detect_shadowed_rules(&rules)[0]);
        assert!(
            msg.contains("\"Bash(git merge:*)\" [allow] from project settings"),
            "{msg}"
        );
        assert!(msg.contains("\"Bash\" [deny] from user settings"), "{msg}");
    }
}
