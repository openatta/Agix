//! `RuleSet` —— 规则集合 + 匹配引擎。
//!
//! 裁决只看行为：命中的规则里只要有一条 deny 就是 deny，否则有 ask 就是 ask，
//! 否则才是 allow。来源层级与规则写得多具体都不参与裁决——deny 是硬边界，
//! 不能被任何一层、任何一条更长的 allow 推翻。同一行为里有多条命中时，按
//! （特异性、来源优先级）挑一条作为命中记录返回，只影响报出哪条规则。
//!
//! 不做"默认拒"语义；规则不命中就返回 None，由 mode 分派定夺。

use crate::dangerous::{detect_dangerous_rules, log_dangerous_warnings};
use crate::path_rule::{self, ResolvedPath};
use crate::shadow::{detect_shadowed_rules, log_shadow_warnings};
use base::permission::{PermissionMode, PermissionRule, RuleBehavior};
use base::tool::PermissionContentKind;
use std::path::Path;

/// Tools whose rule content is a path.
const PATH_RULE_TOOLS: &[&str] = &["Read", "Grep", "Glob", "Edit", "Write", "NotebookEdit"];

/// The rule tool name whose path rules also govern every tool of `kind`:
/// `Read(...)` covers whatever reads a file, `Edit(...)` whatever writes one.
pub fn path_rule_group(kind: PermissionContentKind) -> Option<&'static str> {
    match kind {
        PermissionContentKind::ReadPath => Some("Read"),
        PermissionContentKind::WritePath => Some("Edit"),
        PermissionContentKind::Opaque | PermissionContentKind::ShellCommand => None,
    }
}

/// Tools that are considered safe (read-only, no side effects).
///
/// These are auto-allowed even in strict modes (DontAsk, Plan) and are the
/// only tools auto-allowed in BypassPermissions without path safety checks.
pub const SAFE_TOOLS: &[&str] = &["Read", "Glob", "Grep", "TaskList", "TaskGet", "ToolSearch"];

/// Result of the safe-tool / mode-based quick check via `RuleSet::check`.
///
/// - `Allowed` — the tool is auto-allowed by the safe-tool allowlist or mode policy.
/// - `Deferred` — needs further evaluation by rules and standard permission flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    Allowed,
    Deferred,
}

/// 规则匹配结果。
pub enum RuleHit {
    Allow(PermissionRule),
    Deny(PermissionRule),
    Ask(PermissionRule),
    /// 没有规则命中
    None,
}

impl RuleHit {
    /// `self`, unless `other` is a stronger refusal: a deny overrides
    /// anything that is not a deny, an ask overrides an allow or no hit.
    /// `other`'s allow never widens `self`.
    pub(crate) fn restricted_by(self, other: RuleHit) -> RuleHit {
        match (&self, &other) {
            (RuleHit::Deny(_), _) => self,
            (_, RuleHit::Deny(_)) => other,
            (RuleHit::Ask(_), _) => self,
            (_, RuleHit::Ask(_)) => other,
            _ => self,
        }
    }
}

#[derive(Default, Clone)]
pub struct RuleSet {
    rules: Vec<PermissionRule>,
}

impl RuleSet {
    /// Empty/default instance with no state.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct a new instance.
    ///
    /// Detects shadowed rules and dangerous allow rules, logging warnings
    /// for each finding.
    pub fn new(rules: Vec<PermissionRule>) -> Self {
        let shadows = detect_shadowed_rules(&rules);
        if !shadows.is_empty() {
            log_shadow_warnings(&shadows);
        }
        let dangers = detect_dangerous_rules(&rules);
        if !dangers.is_empty() {
            log_dangerous_warnings(&dangers);
        }
        for r in &rules {
            let Some(content) = r.rule_content.as_deref() else {
                continue;
            };
            if PATH_RULE_TOOLS.contains(&r.tool_name.as_str())
                && path_rule::star_meaning_changed(content)
            {
                tracing::warn!(
                    rule = %crate::rule::format_rule_string(r),
                    "`*` in a path rule no longer matches across `/`; use `**` to reach subdirectories"
                );
            }
        }
        Self { rules }
    }

    /// Append a new entry.
    pub fn add(&mut self, r: PermissionRule) {
        self.rules.push(r);
    }

    /// Append all entries from `rs`.
    pub fn extend<I: IntoIterator<Item = PermissionRule>>(&mut self, rs: I) {
        self.rules.extend(rs);
    }

    /// Drop every rule whose `source` is `source` — used to clear temporary
    /// rules (`RuleSource::Command`) a skill's `allowed_tools` injected once
    /// the skill goes inactive, without disturbing rules from any other
    /// source (`settings.json`, CLI flags, etc).
    pub fn remove_by_source(&mut self, source: base::permission::RuleSource) {
        self.rules.retain(|r| r.source != source);
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Read-only view of the rule list.
    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// 给定工具名 + 可选匹配内容，返回裁决（见模块头）。
    ///
    /// `content` 是 `Tool::permission_match_content(input)` 的结果（如 Bash 命令、
    /// Read 文件路径），原样比对。`None` 表示工具没提供匹配内容；此时只按工具名
    /// （无 content 限定的）规则匹配。
    pub fn evaluate(&self, tool_name: &str, content: Option<&str>) -> RuleHit {
        self.decide(|r| {
            if !matches_tool_name(&r.tool_name, tool_name) {
                return None;
            }
            match (&r.rule_content, content) {
                (None, _) => Some(0),
                (Some(rc), Some(c)) => matches_content(rc, c).then_some(rc.len() as i32),
                (Some(_), None) => None,
            }
        })
    }

    /// 路径类工具调用的裁决：规则内容按路径解读（见 `path_rule`），工具自己名下的
    /// 规则和 `group`（`Read` / `Edit`）名下的规则都适用。别名路径上命中的 deny /
    /// ask 同样生效；allow 只认工具真正打开的那个路径。
    pub fn evaluate_path(
        &self,
        tool_name: &str,
        group: &str,
        path: &ResolvedPath,
        cwd: &Path,
    ) -> RuleHit {
        let decide_for = |p: &Path| {
            self.decide(|r| {
                if !(matches_tool_name(&r.tool_name, tool_name) || r.tool_name == group) {
                    return None;
                }
                match &r.rule_content {
                    None => Some(0),
                    Some(rc) => path_rule::matches(&path_rule::resolve_pattern(rc, cwd), p)
                        .then_some(rc.len() as i32),
                }
            })
        };
        path.aliases
            .iter()
            .fold(decide_for(&path.primary), |verdict, alias| {
                verdict.restricted_by(decide_for(alias))
            })
    }

    /// `matches` 对命中的规则返回其特异性，不命中返回 `None`。
    fn decide(&self, mut matches: impl FnMut(&PermissionRule) -> Option<i32>) -> RuleHit {
        let mut best: Option<(&PermissionRule, MatchScore)> = None;
        for r in &self.rules {
            let Some(specificity) = matches(r) else {
                continue;
            };
            let score = MatchScore {
                behavior_rank: behavior_rank(r.behavior),
                specificity,
                source_priority: r.source.priority() as i32,
            };
            if best.as_ref().map_or(true, |(_, b)| score > *b) {
                best = Some((r, score));
            }
        }

        match best {
            None => RuleHit::None,
            Some((r, _)) => match r.behavior {
                RuleBehavior::Allow => RuleHit::Allow(r.clone()),
                RuleBehavior::Deny => RuleHit::Deny(r.clone()),
                RuleBehavior::Ask => RuleHit::Ask(r.clone()),
            },
        }
    }

    /// 基于安全工具白名单与权限模式的快速决策。
    ///
    /// - **BypassPermissions**: 安全工具直接允许；非安全工具仍返回 `Deferred`，
    ///   由调用方应用路径安全检查后再决定。
    /// - **DontAsk / Plan**: 安全工具直接允许（只读操作不应被严格模式误杀）。
    /// - 其它模式: 一律 `Deferred`，走标准规则匹配与模式分派流程。
    pub fn check(
        &self,
        tool_name: &str,
        _content: Option<&str>,
        mode: PermissionMode,
    ) -> CheckResult {
        match mode {
            PermissionMode::BypassPermissions => {
                // 安全工具绕过
                if SAFE_TOOLS.contains(&tool_name) {
                    return CheckResult::Allowed;
                }
                // 非安全工具：调用方应继续执行路径安全检查
                CheckResult::Deferred
            }
            PermissionMode::DontAsk | PermissionMode::Plan => {
                // 严格模式下安全工具自动放行
                if SAFE_TOOLS.contains(&tool_name) {
                    return CheckResult::Allowed;
                }
                CheckResult::Deferred
            }
            _ => CheckResult::Deferred,
        }
    }
}

/// Field order is the comparison order: behavior decides, the other two only
/// pick which rule of the winning behavior gets reported.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MatchScore {
    behavior_rank: i32,
    specificity: i32,
    source_priority: i32,
}

fn behavior_rank(b: RuleBehavior) -> i32 {
    match b {
        RuleBehavior::Deny => 2,
        RuleBehavior::Ask => 1,
        RuleBehavior::Allow => 0,
    }
}

/// 工具名是 `__` 分段的命名空间路径（`mcp__github__create_issue`、
/// `plugin__github-tools__open_pr`),规则可以只写它的任意一段前缀,
/// 命中该前缀下的全部工具。
///
/// 按段匹配而不是按已知命名空间列表匹配:写死 `mcp__` 意味着每加一种载体都要
/// 回来改这里,而 `plugin__` 就是这么漏掉的——`plugin__github-tools` 一条规则
/// 谁也匹配不上。段边界的检查不能省,否则 `mcp__git` 会命中 `mcp__github__…`。
pub(crate) fn matches_tool_name(rule: &str, actual: &str) -> bool {
    if rule == actual {
        return true;
    }
    actual
        .strip_prefix(rule)
        .is_some_and(|rest| rest.starts_with("__"))
}

pub(crate) fn matches_content(pattern: &str, content: &str) -> bool {
    // 1. `prefix:*` —— 命令以 prefix 开头（exactly 等于 prefix 或 prefix + 空格 + ...）
    if let Some(prefix) = pattern.strip_suffix(":*") {
        return content == prefix || content.starts_with(&format!("{prefix} "));
    }
    // 2. shell glob —— `*` / `?` / `[...]`
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        if let Ok(g) = globset::Glob::new(pattern) {
            return g.compile_matcher().is_match(content);
        }
        return false;
    }
    // 3. 精确匹配
    pattern == content
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
    fn name_only_rule_matches_tool() {
        let rs = RuleSet::new(vec![rule(
            "Bash",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(rs.evaluate("Bash", Some("ls")), RuleHit::Allow(_)));
        assert!(matches!(rs.evaluate("Read", None), RuleHit::None));
    }

    #[test]
    fn exact_content_match() {
        let rs = RuleSet::new(vec![rule(
            "Bash",
            Some("git status"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("Bash", Some("git status")),
            RuleHit::Allow(_)
        ));
        assert!(matches!(
            rs.evaluate("Bash", Some("git push")),
            RuleHit::None
        ));
    }

    #[test]
    fn prefix_glob_match() {
        let rs = RuleSet::new(vec![rule(
            "Bash",
            Some("git log:*"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("Bash", Some("git log")),
            RuleHit::Allow(_)
        ));
        assert!(matches!(
            rs.evaluate("Bash", Some("git log --oneline")),
            RuleHit::Allow(_)
        ));
        assert!(matches!(
            rs.evaluate("Bash", Some("git logger")),
            RuleHit::None
        ));
    }

    #[test]
    fn path_glob_match() {
        let rs = RuleSet::new(vec![rule(
            "Read",
            Some("/etc/**"),
            RuleBehavior::Deny,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("Read", Some("/etc/passwd")),
            RuleHit::Deny(_)
        ));
        assert!(matches!(
            rs.evaluate("Read", Some("/etc/some/sub/file")),
            RuleHit::Deny(_)
        ));
        assert!(matches!(
            rs.evaluate("Read", Some("/var/log")),
            RuleHit::None
        ));
    }

    #[test]
    fn mcp_prefix_match() {
        let rs = RuleSet::new(vec![rule(
            "mcp__github",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("mcp__github__create_issue", None),
            RuleHit::Allow(_)
        ));
        assert!(matches!(
            rs.evaluate("mcp__slack__post_msg", None),
            RuleHit::None
        ));
    }

    /// WASM 插件工具注册成 `plugin__<插件>__<工具>`,取的就是 MCP 那套命名,
    /// 为的是"一条规则管一个插件"能直接生效。前缀匹配写死 `mcp__` 时它不生效。
    #[test]
    fn plugin_prefix_match() {
        let rs = RuleSet::new(vec![rule(
            "plugin__github-tools",
            None,
            RuleBehavior::Deny,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("plugin__github-tools__open_pr", None),
            RuleHit::Deny(_)
        ));
        assert!(matches!(
            rs.evaluate("plugin__other__open_pr", None),
            RuleHit::None
        ));
    }

    /// 前缀必须落在段边界上,否则一条 `mcp__git` 会连带管住 `mcp__github__*`。
    #[test]
    fn a_prefix_only_matches_on_a_segment_boundary() {
        let rs = RuleSet::new(vec![rule(
            "mcp__git",
            None,
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(
            rs.evaluate("mcp__git__log", None),
            RuleHit::Allow(_)
        ));
        assert!(matches!(
            rs.evaluate("mcp__github__create_issue", None),
            RuleHit::None
        ));
    }

    #[test]
    fn deny_beats_allow_at_same_specificity_and_source() {
        let rs = RuleSet::new(vec![
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
        ]);
        assert!(matches!(
            rs.evaluate("Bash", Some("git status")),
            RuleHit::Deny(_)
        ));
    }

    #[test]
    fn a_deny_from_a_lower_priority_source_is_not_overridden() {
        let rs = RuleSet::new(vec![
            rule(
                "Bash",
                Some("git push"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git push"),
                RuleBehavior::Allow,
                RuleSource::CliArg,
            ),
        ]);
        assert!(matches!(
            rs.evaluate("Bash", Some("git push")),
            RuleHit::Deny(_)
        ));
    }

    #[test]
    fn a_broad_deny_is_not_carved_open_by_a_more_specific_allow() {
        let rs = RuleSet::new(vec![
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
            rule(
                "Bash",
                Some("git status"),
                RuleBehavior::Allow,
                RuleSource::ProjectSettings,
            ),
        ]);
        assert!(matches!(
            rs.evaluate("Bash", Some("git status")),
            RuleHit::Deny(_)
        ));
    }

    #[test]
    fn a_longer_allow_glob_does_not_beat_a_shorter_deny_glob() {
        let rs = RuleSet::new(vec![
            rule(
                "Read",
                Some("**/.env"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
            rule(
                "Read",
                Some("/Users/me/proj/**"),
                RuleBehavior::Allow,
                RuleSource::ProjectSettings,
            ),
        ]);
        assert!(matches!(
            rs.evaluate("Read", Some("/Users/me/proj/.env")),
            RuleHit::Deny(_)
        ));
        assert!(matches!(
            rs.evaluate("Read", Some("/Users/me/proj/src/main.rs")),
            RuleHit::Allow(_)
        ));
    }

    #[test]
    fn ask_beats_allow_and_deny_beats_ask() {
        let rs = RuleSet::new(vec![
            rule("Bash", None, RuleBehavior::Allow, RuleSource::Session),
            rule(
                "Bash",
                Some("git push:*"),
                RuleBehavior::Ask,
                RuleSource::UserSettings,
            ),
            rule(
                "Bash",
                Some("git push --force"),
                RuleBehavior::Deny,
                RuleSource::Plugin,
            ),
        ]);
        assert!(matches!(rs.evaluate("Bash", Some("ls")), RuleHit::Allow(_)));
        assert!(matches!(
            rs.evaluate("Bash", Some("git push origin")),
            RuleHit::Ask(_)
        ));
        assert!(matches!(
            rs.evaluate("Bash", Some("git push --force")),
            RuleHit::Deny(_)
        ));
    }

    #[test]
    fn within_one_behavior_the_most_specific_rule_is_the_one_reported() {
        let rs = RuleSet::new(vec![
            rule("Bash", None, RuleBehavior::Deny, RuleSource::UserSettings),
            rule(
                "Bash",
                Some("rm:*"),
                RuleBehavior::Deny,
                RuleSource::UserSettings,
            ),
        ]);
        match rs.evaluate("Bash", Some("rm -rf /")) {
            RuleHit::Deny(r) => assert_eq!(r.rule_content.as_deref(), Some("rm:*")),
            _ => panic!("expected deny"),
        }
    }

    #[test]
    fn rule_with_content_skipped_when_tool_provides_no_content() {
        let rs = RuleSet::new(vec![rule(
            "AgentTool",
            Some("review"),
            RuleBehavior::Allow,
            RuleSource::UserSettings,
        )]);
        assert!(matches!(rs.evaluate("AgentTool", None), RuleHit::None));
    }

    #[test]
    fn empty_ruleset_yields_none() {
        let rs = RuleSet::empty();
        assert!(matches!(rs.evaluate("Bash", Some("ls")), RuleHit::None));
    }

    #[test]
    fn safe_tools_allowed_in_bypass_mode() {
        let rs = RuleSet::empty();
        for &tool in SAFE_TOOLS {
            assert_eq!(
                rs.check(tool, None, PermissionMode::BypassPermissions),
                CheckResult::Allowed,
                "safe tool {tool} should be Allowed in BypassPermissions"
            );
        }
    }

    #[test]
    fn unsafe_tools_deferred_in_bypass_mode() {
        let rs = RuleSet::empty();
        assert_eq!(
            rs.check("Bash", None, PermissionMode::BypassPermissions),
            CheckResult::Deferred,
            "Bash should be Deferred in BypassPermissions (needs path safety)"
        );
        assert_eq!(
            rs.check("Edit", None, PermissionMode::BypassPermissions),
            CheckResult::Deferred
        );
        assert_eq!(
            rs.check("Write", None, PermissionMode::BypassPermissions),
            CheckResult::Deferred
        );
    }

    #[test]
    fn safe_tools_allowed_in_strict_modes() {
        let rs = RuleSet::empty();
        for &tool in SAFE_TOOLS {
            assert_eq!(
                rs.check(tool, None, PermissionMode::DontAsk),
                CheckResult::Allowed,
                "safe tool {tool} should be Allowed in DontAsk"
            );
            assert_eq!(
                rs.check(tool, None, PermissionMode::Plan),
                CheckResult::Allowed,
                "safe tool {tool} should be Allowed in Plan"
            );
        }
    }

    #[test]
    fn unsafe_tools_deferred_in_strict_modes() {
        let rs = RuleSet::empty();
        assert_eq!(
            rs.check("Bash", None, PermissionMode::DontAsk),
            CheckResult::Deferred
        );
        assert_eq!(
            rs.check("Bash", None, PermissionMode::Plan),
            CheckResult::Deferred
        );
    }

    #[test]
    fn check_defers_in_default_mode() {
        let rs = RuleSet::empty();
        assert_eq!(
            rs.check("Read", None, PermissionMode::Default),
            CheckResult::Deferred,
            "even safe tools should be Deferred in Default mode"
        );
        assert_eq!(
            rs.check("Bash", None, PermissionMode::Default),
            CheckResult::Deferred
        );
    }

    #[test]
    fn safe_tools_list_contains_expected_tools() {
        assert!(SAFE_TOOLS.contains(&"Read"));
        assert!(SAFE_TOOLS.contains(&"Glob"));
        assert!(SAFE_TOOLS.contains(&"Grep"));
        assert!(SAFE_TOOLS.contains(&"TaskList"));
        assert!(SAFE_TOOLS.contains(&"TaskGet"));
        assert!(SAFE_TOOLS.contains(&"ToolSearch"));
        assert_eq!(SAFE_TOOLS.len(), 6);
    }
}
