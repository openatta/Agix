//! 规则字符串与 `PermissionRule` 之间的双向转换。
//!
//! grammar：
//! ```text
//! RULE      ::= TOOL ( '(' CONTENT ')' )?
//! TOOL      ::= IDENTIFIER ( '__' IDENTIFIER )*
//! CONTENT   ::= 任意字符（不含未转义的 `)`）
//! ```
//!
//! 例：
//! - `Bash`                       —— 任何 Bash 调用
//! - `Bash(git status)`           —— 命令精确匹配 `git status`
//! - `Bash(git log:*)`            —— 命令以 `git log` 开头
//! - `Read(/etc/**)`              —— 文件路径 glob `/etc/**`
//! - `mcp__github`                —— github MCP server 的任意工具

use crate::error::ParseRuleError;
use base::permission::{PermissionRule, RuleBehavior, RuleSource};

/// 解析单条规则字符串。
pub fn parse_rule_string(
    s: &str,
    source: RuleSource,
    behavior: RuleBehavior,
) -> Result<PermissionRule, ParseRuleError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseRuleError::Empty);
    }

    if let Some(open_idx) = s.find('(') {
        if !s.ends_with(')') {
            return Err(ParseRuleError::Unbalanced(s.to_string()));
        }
        let tool_name = s[..open_idx].trim().to_string();
        if tool_name.is_empty() {
            return Err(ParseRuleError::Malformed(s.to_string()));
        }
        let content = s[open_idx + 1..s.len() - 1].to_string();
        return Ok(PermissionRule {
            source,
            behavior,
            tool_name,
            rule_content: Some(content),
        });
    }

    Ok(PermissionRule {
        source,
        behavior,
        tool_name: s.to_string(),
        rule_content: None,
    })
}

/// `PermissionRule` → 规则字符串。
pub fn format_rule_string(r: &PermissionRule) -> String {
    match &r.rule_content {
        Some(c) => format!("{}({})", r.tool_name, c),
        None => r.tool_name.clone(),
    }
}

/// `settings.json` 的 `permission_rules` → 规则引擎认识的 `PermissionRule`。
///
/// 两边是两个不同的类型：settings 侧是 `{"tool": "Bash(git push:*)",
/// "action": "deny"}`（面向人手写的一行字符串），引擎侧是拆好的
/// `{tool_name, rule_content, behavior, source}`。
///
/// 解析不了的条目（空串、括号不配对……）**跳过并 warn**，不整体失败：一条
/// 手写错的规则不该让整个 session 拒绝启动；跳过的后果是"这条规则不生效"，
/// 在 `ask` 默认下是更保守的方向（该问的还是会问），不是静默放行。
///
/// 多数调用方要的是 [`rules_from_all_tiers`]——它按来源分别调用本函数，
/// 直接用本函数意味着调用方自己决定 `source`。
pub fn rules_from_settings(
    rules: &[base::interface::settings::PermissionRule],
    source: RuleSource,
) -> Vec<PermissionRule> {
    use base::interface::settings::PermissionAction;
    rules
        .iter()
        .filter_map(|r| {
            let behavior = match r.action {
                PermissionAction::Allow => RuleBehavior::Allow,
                PermissionAction::Deny => RuleBehavior::Deny,
                PermissionAction::Ask => RuleBehavior::Ask,
            };
            match parse_rule_string(&r.tool, source, behavior) {
                Ok(rule) => Some(rule),
                Err(e) => {
                    tracing::warn!(
                        rule = %r.tool,
                        error = %e,
                        "unparsable permission_rules entry in settings.json, skipping"
                    );
                    None
                }
            }
        })
        .collect()
}

/// 一个 `Settings` 里全部权限规则，各自带上所在层对应的 [`RuleSource`]：
/// 全局与 scene 层是用户自己的设置（`UserSettings`），项目层是仓库里提交的
/// （`ProjectSettings`），`settings.local.json` 是 `LocalSettings`。代码里直接
/// 构造、没有层信息的规则按 `ProjectSettings` 算。
pub fn rules_from_all_tiers(settings: &base::interface::settings::Settings) -> Vec<PermissionRule> {
    use base::interface::config_source::Tier;
    let mut rules: Vec<PermissionRule> = settings
        .permission_rules
        .iter()
        .flat_map(|r| {
            let source = match r.tier {
                Some(Tier::Global | Tier::Scene) => RuleSource::UserSettings,
                Some(Tier::Project) | None => RuleSource::ProjectSettings,
            };
            rules_from_settings(std::slice::from_ref(r), source)
        })
        .collect();
    rules.extend(rules_from_settings(
        &settings.local_permission_rules,
        RuleSource::LocalSettings,
    ));
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<PermissionRule, ParseRuleError> {
        parse_rule_string(s, RuleSource::UserSettings, RuleBehavior::Allow)
    }

    #[test]
    fn parses_tool_only() {
        let r = parse("Bash").unwrap();
        assert_eq!(r.tool_name, "Bash");
        assert!(r.rule_content.is_none());
    }

    #[test]
    fn parses_tool_with_content() {
        let r = parse("Bash(git status)").unwrap();
        assert_eq!(r.tool_name, "Bash");
        assert_eq!(r.rule_content, Some("git status".into()));
    }

    #[test]
    fn parses_prefix_glob() {
        let r = parse("Bash(git push:*)").unwrap();
        assert_eq!(r.rule_content, Some("git push:*".into()));
    }

    #[test]
    fn parses_path_glob() {
        let r = parse("Read(/etc/**)").unwrap();
        assert_eq!(r.tool_name, "Read");
        assert_eq!(r.rule_content, Some("/etc/**".into()));
    }

    #[test]
    fn parses_mcp_prefix() {
        let r = parse("mcp__github").unwrap();
        assert_eq!(r.tool_name, "mcp__github");
        assert!(r.rule_content.is_none());

        let r = parse("mcp__github__create_issue").unwrap();
        assert_eq!(r.tool_name, "mcp__github__create_issue");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(parse("").unwrap_err(), ParseRuleError::Empty);
        assert_eq!(parse("   ").unwrap_err(), ParseRuleError::Empty);
    }

    #[test]
    fn rejects_unbalanced() {
        assert!(matches!(
            parse("Bash(git status").unwrap_err(),
            ParseRuleError::Unbalanced(_)
        ));
    }

    #[test]
    fn rejects_no_tool() {
        assert!(matches!(
            parse("(git status)").unwrap_err(),
            ParseRuleError::Malformed(_)
        ));
    }

    #[test]
    fn format_roundtrip() {
        for s in &[
            "Bash",
            "Bash(git status)",
            "Bash(git push:*)",
            "Read(/etc/**)",
            "mcp__github__create_issue",
        ] {
            let r = parse(s).unwrap();
            assert_eq!(format_rule_string(&r), *s);
        }
    }

    #[test]
    fn trims_whitespace() {
        let r = parse("  Bash(git status)  ").unwrap();
        assert_eq!(r.tool_name, "Bash");
        assert_eq!(r.rule_content, Some("git status".into()));
    }

    fn settings_rule(
        tool: &str,
        action: base::interface::settings::PermissionAction,
    ) -> base::interface::settings::PermissionRule {
        base::interface::settings::PermissionRule {
            tool: tool.to_string(),
            action,
            tier: None,
        }
    }

    #[test]
    fn settings_rules_convert_with_action_mapped_to_behavior() {
        use base::interface::settings::PermissionAction;
        let converted = rules_from_settings(
            &[
                settings_rule("Bash(git push:*)", PermissionAction::Deny),
                settings_rule("Read", PermissionAction::Allow),
                settings_rule("Write(/etc/**)", PermissionAction::Ask),
            ],
            RuleSource::ProjectSettings,
        );
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].tool_name, "Bash");
        assert_eq!(converted[0].rule_content, Some("git push:*".into()));
        assert_eq!(converted[0].behavior, RuleBehavior::Deny);
        assert_eq!(converted[1].behavior, RuleBehavior::Allow);
        assert!(converted[1].rule_content.is_none());
        assert_eq!(converted[2].behavior, RuleBehavior::Ask);
        assert!(converted
            .iter()
            .all(|r| r.source == RuleSource::ProjectSettings));
    }

    #[test]
    fn settings_rules_skip_unparsable_entries_instead_of_failing_all() {
        use base::interface::settings::PermissionAction;
        let converted = rules_from_settings(
            &[
                settings_rule("Bash(unbalanced", PermissionAction::Deny),
                settings_rule("   ", PermissionAction::Allow),
                settings_rule("Read", PermissionAction::Allow),
            ],
            RuleSource::UserSettings,
        );
        assert_eq!(converted.len(), 1, "only the well-formed rule survives");
        assert_eq!(converted[0].tool_name, "Read");
    }
}
