//! `PermissionGate` —— 权限决策入口。
//!
//! 决策顺序（`PreToolUse` hook 不在此处——由调用方 `runtime::turn` 在进入
//! 本 gate 之前执行，见 `execute_tool_inner`）：
//!
//! 0. 部署方的 managed 层（`ManagedPolicy`）读不了 → 一律 deny；它关掉了
//!    bypass 的，`BypassPermissions` 按 `Default` 判
//! 1. `tool.validate_input` —— 输入合法性。`BypassPermissions` 到此为止：
//!    输入合法即 allow，下面的规则、受保护路径、`.git` / `.agix` 确认和工具
//!    自己的判定一概不看——只看 managed 层的 deny / ask
//! 2. `tool.check_permissions` —— 工具自带的判定：
//!    - `Deny` **直接返回**（工具拒绝自己是权威的：只有工具知道自己的输入
//!      为什么不该跑）
//!    - `Allow` **不直接返回**，只记成一个"待生效的快速通道"（pending
//!      fast-path），要等下面 3–5 步都没否掉才兑现
//!    - `Ask` = 工具没意见
//! 3. 通用规则引擎（`RuleSet`，deny > ask > allow；路径类工具先规整路径，
//!    shell 命令按简单命令逐段匹配）：`Deny` 直接返回
//! 4. bypass-immune 路径检查（`is_path_bypass_immune`）→ deny
//! 5. 模式级硬拒绝：
//!    - `Plan` 且非只读工具 → deny（离开计划模式的那个工具除外，见
//!      `Tool::leaves_plan_mode`：它照常走规则与询问）
//!    - `DontAsk` 且没有 allow 规则命中 → deny
//!
//!    之后第 3 步的 `Ask` 命中 → ask（`DontAsk` 下 deny）
//! 6. 兑现第 2 步记下的工具自判 `Allow` → allow
//! 7. 规则引擎的 `Allow` → allow
//! 8. 其余 PermissionMode 分派：
//!    - `AcceptEdits` 且工具认可（`Tool::accept_edits_allows`，缺省即只读）→
//!      allow；Edit / Write 走到这里说明工具自己没放行（写项目外）或要确认
//!      （`.git` / `.agix`），照样 ask。向模型选的主机发请求的只读工具
//!      （`WebFetch`、`Ping`）只在 `sandbox.allowed_domains` 列出该主机时认可
//!    - 其余（`Default`，以及只读工具下的 `Plan`）→ ask
//!
//! **为什么 2 的 Allow 要等到第 6 步才兑现**：`Write` / `Edit` / `Read` /
//! `Grep` / `Glob` / 只读 `Bash` 全都会自判 Allow。在第 2 步就返回的话，用户写在
//! settings.json 里的 `deny` 规则、Plan 模式、`DontAsk` 和第 4 步的 bypass-immune
//! 名单对这批工具都不可达。挪到 3–5 步之后，Default 模式下项目内编辑、只读命令
//! 照样不弹窗（3–5 步不会否掉它们），而这些约束仍凌驾于工具自身的意见之上。
//!
//! 输入校验不属于权限，所以 bypass 下照样生效：读后再写的过期检查、大小
//! 上限、`~` / `$(` / UNC 这类会被误读的路径，都在各工具的 `validate_input`
//! 里。Bash 另由 OS 沙箱约束（`dangerously_disable_sandbox` 除外），与权限
//! 模式无关。
//!
//! 关键：gate **不**直接调 `effects.ask_user`。它返回 `PermissionDecision::Ask`，
//! 由调用方（engine）决定怎么把它升级成 allow / deny。

use crate::error::GateError;
use crate::path_rule::ResolvedPath;
use crate::rule::format_rule_string;
use crate::ruleset::{path_rule_group, shell_rule_group, RuleHit, RuleSet};
use base::paths::{PROJECT_WORKTREES_DIR, STATE_ROOT_WORKING_DIRS};
use base::permission::{DecisionReason, PermissionDecision, PermissionMode};
use base::permission::{PermissionRule, RuleBehavior, RuleSource};
use base::tool::ToolContext;
use base::tool::{PermissionContentKind, Tool};
use serde_json::Value;
use std::sync::{Arc, RwLock};

pub struct PermissionGate {
    rules: Arc<RwLock<RuleSet>>,
    deny_rules_only: bool,
    managed: ManagedPolicy,
}

/// What the deployment's managed settings (`base::managed_settings`) hold every
/// call to, whatever the session's mode. Its rules are also among the gate's
/// own, where they weigh like any other; here they are what a
/// `bypassPermissions` session still answers to.
#[derive(Clone, Default)]
pub struct ManagedPolicy {
    rules: RuleSet,
    bypass_disabled: bool,
    unreadable: Option<String>,
}

impl ManagedPolicy {
    /// The managed layer `settings` carry. A rule in it that does not parse
    /// makes the layer unreadable rather than dropping the rule.
    pub fn from_settings(settings: &base::interface::settings::Settings) -> Self {
        let Some(managed) = &settings.managed else {
            return Self::default();
        };
        let mut unreadable = managed.error.clone();
        let mut rules = RuleSet::empty();
        for rule in &managed.permission_rules {
            let behavior = match rule.action {
                base::interface::settings::PermissionAction::Allow => RuleBehavior::Allow,
                base::interface::settings::PermissionAction::Deny => RuleBehavior::Deny,
                base::interface::settings::PermissionAction::Ask => RuleBehavior::Ask,
            };
            match crate::rule::parse_rule_string(&rule.tool, RuleSource::PolicySettings, behavior) {
                Ok(parsed) => rules.add(parsed),
                Err(e) => {
                    unreadable.get_or_insert_with(|| {
                        format!(
                            "{}: permission rule `{}` does not parse: {e}",
                            managed.origin, rule.tool
                        )
                    });
                }
            }
        }
        Self {
            rules,
            bypass_disabled: managed.disable_bypass_permissions_mode,
            unreadable,
        }
    }

    /// Why the layer cannot be honoured, when it cannot.
    pub fn unreadable(&self) -> Option<&str> {
        self.unreadable.as_deref()
    }
}

impl PermissionGate {
    /// Construct a new instance.
    pub fn new(rules: RuleSet) -> Self {
        Self {
            rules: Arc::new(RwLock::new(rules)),
            deny_rules_only: false,
            managed: ManagedPolicy::default(),
        }
    }

    /// Hold every call to `managed` as well, in every mode.
    pub fn with_managed(mut self, managed: ManagedPolicy) -> Self {
        self.managed = managed;
        self
    }

    /// A gate whose only verdict of its own is a matching `deny` rule: any
    /// other call is allowed, whatever the session's mode, so it can sit in
    /// front of another permission and refuse only what the rules refuse.
    pub fn deny_rules_only(rules: RuleSet) -> Self {
        Self {
            deny_rules_only: true,
            ..Self::new(rules)
        }
    }

    /// Empty/default instance with no state.
    pub fn empty() -> Self {
        Self::new(RuleSet::empty())
    }

    /// Snapshot view of the rule list.
    pub fn rules(&self) -> Vec<PermissionRule> {
        self.rules
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rules()
            .to_vec()
    }

    /// 运行时加规则。用于"允许此项目"流程 —— 用户选
    /// "allow for this project" 后，Engine 构造 ProjectSettings 规则写入。
    pub fn add_rules(&self, additional: Vec<PermissionRule>) {
        self.rules
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(additional);
    }

    /// Drop every rule from `source` — see `RuleSet::remove_by_source`.
    pub fn remove_rules_by_source(&self, source: base::permission::RuleSource) {
        self.rules
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove_by_source(source);
    }

    /// The rule this call matches, and every string the rules were matched
    /// against (the content and each resolved form of its path).
    fn rule_hit(
        &self,
        tool: &dyn Tool,
        input: &Value,
        ctx: &ToolContext,
    ) -> (RuleHit, Vec<String>) {
        let rules = self
            .rules
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::rule_hit_in(&rules, tool, input, ctx)
    }

    /// The managed layer's verdict on a `bypassPermissions` call: its deny
    /// refuses, its ask asks, anything else is allowed.
    fn bypass_decision(
        &self,
        tool: &dyn Tool,
        input: &Value,
        ctx: &ToolContext,
    ) -> PermissionDecision {
        let hit = if self.managed.rules.is_empty() {
            RuleHit::None
        } else {
            Self::rule_hit_in(&self.managed.rules, tool, input, ctx).0
        };
        match hit {
            RuleHit::Deny(rule) => PermissionDecision::Deny {
                message: format!("denied by managed rule: {}", format_rule_string(&rule)),
                decision_reason: DecisionReason::Rule(rule),
            },
            RuleHit::Ask(rule) => PermissionDecision::Ask {
                message: format!(
                    "{} matches managed rule {}, which requires confirmation",
                    tool.name(),
                    format_rule_string(&rule)
                ),
                decision_reason: Some(DecisionReason::Rule(rule)),
            },
            RuleHit::Unclear(rule) => PermissionDecision::Ask {
                message: unclear_message(tool.name(), &rule),
                decision_reason: Some(DecisionReason::Rule(rule)),
            },
            RuleHit::Allow(_) | RuleHit::None => PermissionDecision::Allow {
                updated_input: None,
                decision_reason: Some(DecisionReason::Mode(PermissionMode::BypassPermissions)),
            },
        }
    }

    /// The rule in `rules` this call matches, and every string the rules were
    /// matched against (the content and each resolved form of its path).
    fn rule_hit_in(
        rules: &RuleSet,
        tool: &dyn Tool,
        input: &Value,
        ctx: &ToolContext,
    ) -> (RuleHit, Vec<String>) {
        let content = tool.permission_match_content(input);
        let kind = tool.permission_content_kind();
        let path_group = path_rule_group(kind);
        let resolved = path_group
            .zip(content.as_deref())
            .map(|(_, c)| ResolvedPath::new(c, &ctx.cwd));
        let hit = {
            match (path_group, &resolved, content.as_deref()) {
                (Some(group), Some(path), _) => {
                    rules.evaluate_path(tool.name(), group, path, &ctx.cwd)
                }
                (_, _, Some(line)) if kind == PermissionContentKind::ShellCommand => {
                    rules.evaluate_shell(tool.name(), shell_rule_group(kind), line)
                }
                _ => rules.evaluate(tool.name(), content.as_deref()),
            }
        };
        // The protected-path checks compare path components, which quotes
        // inside a word (`~/".ssh"/id_rsa`) would otherwise hide.
        let shell_line = content
            .as_deref()
            .filter(|_| kind == PermissionContentKind::ShellCommand);
        // Each argument on its own, quotes removed, so a protected path is a
        // whole string the component checks can isolate — `~/".ssh"/id_rsa`
        // becomes `~/.ssh/id_rsa`, and `.agix/settings.local.json` is not glued
        // to the command word before it.
        let unquoted = shell_line
            .into_iter()
            .flat_map(crate::shell::segments)
            .flat_map(|segment| crate::shell::lex(&segment.source))
            .map(|w| w.text);
        // A segment that is only an assignment (`OLDPWD=~/.ssh;`) never reaches
        // `segments`, but its value can name a protected path a later command
        // reads through — so the values are checked too.
        let assignment_values = shell_line
            .into_iter()
            .flat_map(crate::shell::assignments)
            .map(|(_, value)| value);
        let checked = content
            .iter()
            .cloned()
            .chain(
                resolved
                    .iter()
                    .flat_map(|r| r.all().map(|p| p.to_string_lossy().into_owned())),
            )
            .chain(unquoted)
            .chain(assignment_values)
            .collect();
        (hit, checked)
    }

    pub async fn check(
        &self,
        tool: &dyn Tool,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<PermissionDecision, GateError> {
        if let Some(why) = &self.managed.unreadable {
            return Ok(PermissionDecision::Deny {
                message: format!(
                    "the managed settings cannot be honoured, so every tool call is refused \
                     until they are fixed: {why}"
                ),
                decision_reason: DecisionReason::Other("managed settings unreadable".into()),
            });
        }

        // 1. validateInput
        if let base::tool::ValidationResult::Err(msg, code) = tool.validate_input(input, ctx).await
        {
            return Err(GateError::InvalidInput { message: msg, code });
        }

        if self.deny_rules_only {
            return Ok(match self.rule_hit(tool, input, ctx).0 {
                RuleHit::Deny(rule) => PermissionDecision::Deny {
                    message: format!("denied by rule: {}", format_rule_string(&rule)),
                    decision_reason: DecisionReason::Rule(rule),
                },
                RuleHit::Unclear(rule) => PermissionDecision::Deny {
                    message: unclear_message(tool.name(), &rule),
                    decision_reason: DecisionReason::Rule(rule),
                },
                _ => PermissionDecision::Allow {
                    updated_input: None,
                    decision_reason: None,
                },
            });
        }

        let mode = match ctx.permission_mode {
            PermissionMode::BypassPermissions if self.managed.bypass_disabled => {
                PermissionMode::Default
            }
            mode => mode,
        };
        if mode == PermissionMode::BypassPermissions {
            return Ok(self.bypass_decision(tool, input, ctx));
        }

        // 2. tool.check_permissions —— 工具自带判定
        //
        // Deny 是权威的、立即返回：工具比 gate 更清楚自己的这组输入为什么不能跑
        // （例如 Write 检测到路径逃出 cwd）。Allow 则**只是备选**：见模块级注释
        // —— 直接 return 会让 deny 规则 / Plan / DontAsk / bypass-immune
        // 名单对所有自判 Allow 的工具（Write/Edit/Read/Grep/Glob/只读 Bash）失效。
        // 这里把它记成 pending，等 3–5 步都放行后再在第 6 步兑现。
        let mut tool_self_allow = false;
        match tool.check_permissions(input, ctx).await {
            base::tool::PermissionDecision::Allow { .. } => {
                tool_self_allow = true;
            }
            base::tool::PermissionDecision::Deny { reason, .. } => {
                return Ok(PermissionDecision::Deny {
                    message: reason.unwrap_or_default(),
                    decision_reason: DecisionReason::Other("tool denied".into()),
                })
            }
            base::tool::PermissionDecision::Ask { .. } => { /* 工具没意见 */ }
        }

        // 3. 通用规则引擎 —— Deny 短路（必须压过 pending 的工具自判 Allow）。
        //    Allow 先存着，等模式级硬拒绝（Plan / DontAsk）判完再用：DontAsk
        //    就是靠"有没有 allow 规则"决定放行还是拒绝的。
        let (rule_hit, checked_strings) = self.rule_hit(tool, input, ctx);
        let (rule_allow, rule_ask) = match rule_hit {
            RuleHit::Deny(rule) => {
                return Ok(PermissionDecision::Deny {
                    message: format!("denied by rule: {}", format_rule_string(&rule)),
                    decision_reason: DecisionReason::Rule(rule),
                });
            }
            RuleHit::Allow(rule) => (Some(rule), None),
            RuleHit::Ask(rule) => {
                let message = format!(
                    "{} matches {}, which requires confirmation",
                    tool.name(),
                    format_rule_string(&rule)
                );
                (None, Some((rule, message)))
            }
            RuleHit::Unclear(rule) => {
                let message = unclear_message(tool.name(), &rule);
                (None, Some((rule, message)))
            }
            RuleHit::None => (None, None),
        };

        // 4. Sensitive paths — even when the tool allowed itself in step 2.
        if let Some(content) = checked_strings.iter().find(|s| is_path_bypass_immune(s)) {
            return Ok(PermissionDecision::Deny {
                message: format!(
                    "access to protected path '{content}' is blocked regardless of \
                     permission mode — this path is bypass-immune"
                ),
                decision_reason: DecisionReason::ToolBuiltin("bypass_immune".into()),
            });
        }

        // 5. 模式级硬拒绝 —— 模式取 `ctx.permission_mode`：调用方每次从会话的当前
        //    模式填它（委派收窄后的模式也落在这里），不是 config 的初始值。
        //    这两条同样要压过 pending 的工具自判 Allow，否则 Plan 模式下
        //    `Write`（自判 Allow）照样能落盘，`DontAsk` 也拦不住任何自判工具。
        // A tool may opt its self-`Allow` out of *mode* denials when that
        // `Allow` is about the tool's nature rather than its arguments — see
        // `Tool::self_allow_overrides_mode`. Deliberately narrow: it does not
        // affect the rule engine or the bypass-immune list above, both of
        // which have already had their say.
        let mode_denials_apply = !(tool_self_allow && tool.self_allow_overrides_mode());
        if mode_denials_apply
            && matches!(mode, PermissionMode::Plan)
            && !tool.is_read_only(input)
            && !tool.leaves_plan_mode()
        {
            return Ok(PermissionDecision::Deny {
                message: "plan mode forbids non-readonly tools".into(),
                decision_reason: DecisionReason::Mode(mode),
            });
        }
        if mode_denials_apply && matches!(mode, PermissionMode::DontAsk) && rule_allow.is_none() {
            return Ok(PermissionDecision::Deny {
                message: "dontAsk mode and no rule matched".into(),
                decision_reason: DecisionReason::Mode(mode),
            });
        }

        // An ask rule outranks the tool's own allow: it prompts in every mode
        // that prompts at all and denies under dontAsk.
        if let Some((rule, message)) = rule_ask {
            match mode {
                PermissionMode::DontAsk => {
                    return Ok(PermissionDecision::Deny {
                        message: format!("dontAsk mode: {message}"),
                        decision_reason: DecisionReason::Rule(rule),
                    });
                }
                _ => {
                    return Ok(PermissionDecision::Ask {
                        message,
                        decision_reason: Some(DecisionReason::Rule(rule)),
                    });
                }
            }
        }

        // 6. 现在才兑现工具自判 Allow —— 没有 deny 规则、没踩 bypass-immune 路径、
        //    也不在会硬拒的模式里。Default 模式下项目内的 Write/Edit 与只读
        //    Bash 因此静默通过。
        //
        //    唯一的例外是 agent / 仓库配置目录（`.git` / `.claude` / `.agix`）：
        //    它们不是凭据（凭据在第 4 步就硬拒了），用户完全可能想让 agent 去
        //    改；但"工具自己说没问题"不足以让一次改写 agent 自身规则的调用静默
        //    通过。所以这里只是**跳过快速通道**，让它落到下面的模式分派去走正常
        //    确认流程 —— 该问就问，而不是一律拒绝。只读的调用改不了规则，不算
        //    （读 agent 配置是正常工作，写才要确认）。改写 `.git` / `.agix` 的
        //    shell 命令不是只读（分类器会否掉 `git config k v` 这类），走的是上面
        //    的非只读分支。
        let needs_confirmation = !tool.is_read_only(input)
            && checked_strings
                .iter()
                .any(|s| is_path_confirmation_required(s));
        if tool_self_allow && !needs_confirmation {
            return Ok(PermissionDecision::Allow {
                updated_input: None,
                decision_reason: Some(DecisionReason::Other("tool allowed".into())),
            });
        }

        // 7. 规则引擎的 allow。
        if let Some(rule) = rule_allow {
            return Ok(PermissionDecision::Allow {
                updated_input: None,
                decision_reason: Some(DecisionReason::Rule(rule)),
            });
        }

        // 8. 其余模式分派
        match mode {
            PermissionMode::AcceptEdits => {
                if tool.accept_edits_allows(input, ctx) {
                    Ok(PermissionDecision::Allow {
                        updated_input: None,
                        decision_reason: Some(DecisionReason::Mode(mode)),
                    })
                } else {
                    Ok(PermissionDecision::Ask {
                        message: format!("Allow {}?", tool.name()),
                        decision_reason: Some(DecisionReason::Mode(mode)),
                    })
                }
            }

            // 走到这里的 DontAsk 在第 5 步已经证明"有 allow 规则命中"，而那条
            // 规则在第 7 步就 return 了 —— 所以这个分支实际不可达。留着一是
            // match 要穷尽，二是万一上面的顺序被改动，默认仍然是拒绝（fail closed）。
            PermissionMode::DontAsk => Ok(PermissionDecision::Deny {
                message: "dontAsk mode and no rule matched".into(),
                decision_reason: DecisionReason::Mode(mode),
            }),

            // Bubble mode — forward permission requests to parent agent
            // rather than prompting the user. The parent agent decides allow/deny.
            PermissionMode::Bubble => Ok(PermissionDecision::Ask {
                message: "Bubble: forwarding permission request to parent agent".into(),
                decision_reason: Some(DecisionReason::Mode(mode)),
            }),

            // Plan 只比 Default 多一条"非只读拒绝"(第 5 步)：只读工具走到这里，说明它
            // 自己没放行(读项目外、联网)，和 Default 一样要问。
            PermissionMode::Default
            | PermissionMode::Plan
            // Bypass 在第 1 步之后就返回了；列在这里只为 match 穷尽。
            | PermissionMode::BypassPermissions => Ok(PermissionDecision::Ask {
                message: format!("Allow {}?", tool.name()),
                decision_reason: Some(DecisionReason::Mode(mode)),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base::context::SessionState;
    use base::error::ToolError;
    use base::permission::PermissionMode;
    use base::tool::Tool;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn rule(source: base::permission::RuleSource, tool: &str) -> PermissionRule {
        PermissionRule {
            source,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: tool.into(),
            rule_content: None,
        }
    }

    /// Unloading a plugin must take its rules with it — `RuleSource::Plugin`
    /// exists so that removal is a single call rather than a diff against
    /// whatever the plugin happened to add.
    #[test]
    fn plugin_rules_are_removable_as_a_batch_without_touching_others() {
        let gate = PermissionGate::new(RuleSet::new(vec![rule(
            base::permission::RuleSource::UserSettings,
            "Read",
        )]));
        gate.add_rules(vec![
            rule(base::permission::RuleSource::Plugin, "Grep"),
            rule(base::permission::RuleSource::Plugin, "Glob"),
        ]);
        assert_eq!(gate.rules().len(), 3);

        gate.remove_rules_by_source(base::permission::RuleSource::Plugin);

        let remaining = gate.rules();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].tool_name, "Read");
    }

    /// A plugin is third-party content pulled off the network; its allow
    /// must never open up what the user's own settings or the org's managed
    /// policy deny.
    #[test]
    fn a_plugin_allow_never_overrides_a_deny_from_any_source() {
        use base::permission::{RuleBehavior, RuleSource};
        for denier in [
            RuleSource::PolicySettings,
            RuleSource::UserSettings,
            RuleSource::ProjectSettings,
        ] {
            let rules = RuleSet::new(vec![
                PermissionRule {
                    source: denier,
                    behavior: RuleBehavior::Deny,
                    tool_name: "Bash".into(),
                    rule_content: None,
                },
                PermissionRule {
                    source: RuleSource::Plugin,
                    behavior: RuleBehavior::Allow,
                    tool_name: "Bash".into(),
                    rule_content: Some("curl:*".into()),
                },
            ]);
            assert!(
                matches!(rules.evaluate("Bash", Some("curl x")), RuleHit::Deny(_)),
                "plugin allow overrode a {denier:?} deny"
            );
        }
    }

    /// 一个可配置的 fake tool，用来覆盖 gate 各分支。
    struct FakeTool {
        name: &'static str,
        read_only: bool,
        own_decision: Option<base::tool::PermissionDecision>,
        match_content: Option<String>,
        invalid: bool,
        leaves_plan: bool,
    }

    impl FakeTool {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                read_only: false,
                own_decision: None,
                match_content: None,
                invalid: false,
                leaves_plan: false,
            }
        }
        fn leaves_plan(mut self) -> Self {
            self.leaves_plan = true;
            self
        }
        fn invalid(mut self) -> Self {
            self.invalid = true;
            self
        }
        fn read_only(mut self) -> Self {
            self.read_only = true;
            self
        }
        fn own(mut self, d: base::tool::PermissionDecision) -> Self {
            self.own_decision = Some(d);
            self
        }
        fn matches(mut self, c: &str) -> Self {
            self.match_content = Some(c.into());
            self
        }
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn prompt(&self, _: &base::tool::PromptContext) -> String {
            "fake".into()
        }
        fn is_read_only(&self, _: &Value) -> bool {
            self.read_only
        }
        fn leaves_plan_mode(&self) -> bool {
            self.leaves_plan
        }
        async fn validate_input(
            &self,
            _: &Value,
            _: &base::tool::ToolContext,
        ) -> base::tool::ValidationResult {
            if self.invalid {
                base::tool::ValidationResult::err("bad input", 1)
            } else {
                base::tool::ValidationResult::Ok
            }
        }
        async fn check_permissions(
            &self,
            _: &Value,
            _: &base::tool::ToolContext,
        ) -> base::tool::PermissionDecision {
            self.own_decision
                .clone()
                .unwrap_or(base::tool::PermissionDecision::ask("?"))
        }
        fn permission_match_content(&self, _: &Value) -> Option<String> {
            self.match_content.clone()
        }
        async fn call(
            &self,
            _: Value,
            _: base::tool::ToolContext,
            _: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, ToolError> {
            Ok(base::tool::ToolResult::text("ok"))
        }
    }

    fn ctx_with_mode(mode: PermissionMode) -> base::tool::ToolContext {
        let mut ctx = base::tool::ToolContext::for_test(PathBuf::from("/tmp"));
        ctx.permission_mode = mode;
        ctx.session = Arc::new(SessionState::new(PathBuf::from("/tmp")).with_permission_mode(mode));
        ctx.tool_use_id = "test".into();
        ctx
    }

    #[tokio::test]
    async fn tool_own_allow_is_honoured_when_nothing_else_objects() {
        // 工具自判 Allow 仍然是"不弹窗"的快速通道 —— 只是不再短路：这里没有
        // deny 规则、没踩 bypass-immune 路径、模式也是 Default，所以兑现。
        let tool = FakeTool::new("X").own(base::tool::PermissionDecision::allow());
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        match d {
            PermissionDecision::Allow {
                decision_reason: Some(DecisionReason::Other(label)),
                ..
            } => assert_eq!(label, "tool allowed"),
            other => panic!("expected Allow(tool allowed), got {other:?}"),
        }
    }

    /// 用户在 settings.json 里写的 deny 规则必须压过工具的自判 Allow。
    #[tokio::test]
    async fn deny_rule_beats_self_allowing_tool() {
        let tool = FakeTool::new("Write")
            .own(base::tool::PermissionDecision::allow())
            .matches("/repo/secrets.env");
        let rules = RuleSet::new(vec![PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Deny,
            tool_name: "Write".into(),
            rule_content: Some("/repo/secrets.env".into()),
        }]);
        let gate = PermissionGate::new(rules);
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(
            matches!(d, PermissionDecision::Deny { .. }),
            "deny rule must outrank the tool's own Allow, got {d:?}"
        );
    }

    /// Plan 模式必须拦住自判 Allow 的非只读工具（典型就是项目内的
    /// `Write` / `Edit`）。
    #[tokio::test]
    async fn plan_mode_denies_self_allowing_non_readonly_tool() {
        let tool = FakeTool::new("Write").own(base::tool::PermissionDecision::allow());
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Plan))
            .await
            .unwrap();
        assert!(
            matches!(d, PermissionDecision::Deny { .. }),
            "plan mode must outrank the tool's own Allow, got {d:?}"
        );
    }

    /// bypass-immune 路径必须压过工具自判 Allow —— 否则
    /// `Read(~/.ssh/id_rsa)` 在第 2 步就返回了，第 4 步的检查永远跑不到。
    #[tokio::test]
    async fn bypass_immune_path_beats_self_allowing_tool() {
        let tool = FakeTool::new("Read")
            .read_only()
            .own(base::tool::PermissionDecision::allow())
            .matches("~/.ssh/id_rsa");
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        match d {
            PermissionDecision::Deny {
                decision_reason: DecisionReason::ToolBuiltin(ref which),
                ..
            } => assert_eq!(which, "bypass_immune"),
            other => panic!("expected bypass_immune Deny, got {other:?}"),
        }
    }

    /// `DontAsk` 是"没有规则就拒绝"，自判 Allow 不该把它变成"什么都放行"。
    #[tokio::test]
    async fn dontask_mode_denies_self_allowing_tool_without_rule() {
        let tool = FakeTool::new("Write").own(base::tool::PermissionDecision::allow());
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn tool_own_deny_short_circuits() {
        let tool = FakeTool::new("X").own(base::tool::PermissionDecision::Deny {
            reason: Some("no".into()),
            decision_reason: None,
        });
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn rule_engine_allow_overrides_default_ask() {
        let tool = FakeTool::new("Bash").matches("ls");
        let rules = RuleSet::new(vec![PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: "Bash".into(),
            rule_content: Some("ls".into()),
        }]);
        let gate = PermissionGate::new(rules);
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn rule_engine_deny_short_circuits() {
        let tool = FakeTool::new("Bash").matches("rm -rf /");
        let rules = RuleSet::new(vec![PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Deny,
            tool_name: "Bash".into(),
            rule_content: Some("rm -rf:*".into()),
        }]);
        let gate = PermissionGate::new(rules);
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn bypass_mode_consults_no_rule_no_protected_path_and_no_tool_verdict() {
        for spelling in ["bypassPermissions", "auto", "yolo"] {
            bypass_spelling_consults_nothing(spelling).await;
        }
    }

    async fn bypass_spelling_consults_nothing(spelling: &str) {
        let deny_bash = || {
            RuleSet::new(vec![PermissionRule {
                source: base::permission::RuleSource::UserSettings,
                behavior: base::permission::RuleBehavior::Deny,
                tool_name: "Bash".into(),
                rule_content: None,
            }])
        };
        let mode: PermissionMode = serde_json::from_value(json!(spelling)).unwrap();
        let ctx = ctx_with_mode(mode);
        let cases = [
            FakeTool::new("Bash").matches("rm -rf build"),
            FakeTool::new("Read").read_only().matches("~/.ssh/id_rsa"),
            FakeTool::new("Bash").matches("rm -rf /"),
            FakeTool::new("Write")
                .matches("/repo/.agix/settings.json")
                .own(base::tool::PermissionDecision::Deny {
                    reason: Some("guarded".into()),
                    decision_reason: None,
                }),
        ];
        for tool in cases {
            let d = PermissionGate::new(deny_bash())
                .check(&tool, &json!({}), &ctx)
                .await
                .unwrap();
            assert!(
                matches!(
                    d,
                    PermissionDecision::Allow {
                        decision_reason: Some(DecisionReason::Mode(
                            PermissionMode::BypassPermissions
                        )),
                        ..
                    }
                ),
                "{spelling} {:?}: {d:?}",
                tool.match_content
            );
        }
    }

    #[tokio::test]
    async fn a_deny_rules_only_gate_refuses_what_a_rule_denies_in_any_mode_and_nothing_else() {
        let deny_bash = || {
            RuleSet::new(vec![PermissionRule {
                source: base::permission::RuleSource::Session,
                behavior: base::permission::RuleBehavior::Deny,
                tool_name: "Bash".into(),
                rule_content: None,
            }])
        };
        for mode in [PermissionMode::BypassPermissions, PermissionMode::Default] {
            let ctx = ctx_with_mode(mode);
            let denied = PermissionGate::deny_rules_only(deny_bash())
                .check(&FakeTool::new("Bash").matches("ls"), &json!({}), &ctx)
                .await
                .unwrap();
            assert!(
                matches!(denied, PermissionDecision::Deny { .. }),
                "{mode:?}: {denied:?}"
            );
            let other = PermissionGate::deny_rules_only(deny_bash())
                .check(
                    &FakeTool::new("Write").matches("/repo/.agix/settings.json"),
                    &json!({}),
                    &ctx,
                )
                .await
                .unwrap();
            assert!(
                matches!(other, PermissionDecision::Allow { .. }),
                "{mode:?}: {other:?}"
            );
        }
    }

    #[tokio::test]
    async fn bypass_mode_still_rejects_invalid_input() {
        let tool = FakeTool::new("Write").invalid();
        let r = PermissionGate::empty()
            .check(
                &tool,
                &json!({}),
                &ctx_with_mode(PermissionMode::BypassPermissions),
            )
            .await;
        assert!(matches!(r, Err(GateError::InvalidInput { .. })), "{r:?}");
    }

    #[tokio::test]
    async fn bypass_mode_allows_when_no_rule() {
        let tool = FakeTool::new("Whatever");
        let gate = PermissionGate::empty();
        let d = gate
            .check(
                &tool,
                &json!({}),
                &ctx_with_mode(PermissionMode::BypassPermissions),
            )
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn plan_mode_denies_non_readonly() {
        let tool = FakeTool::new("Write");
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Plan))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn the_way_out_of_plan_mode_is_asked_about_or_decided_by_a_rule() {
        let exit = || FakeTool::new("ExitPlanMode").leaves_plan();
        let plan = ctx_with_mode(PermissionMode::Plan);
        let asked = PermissionGate::empty()
            .check(&exit(), &json!({}), &plan)
            .await
            .unwrap();
        assert!(matches!(asked, PermissionDecision::Ask { .. }), "{asked:?}");

        let rule = |behavior| PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior,
            tool_name: "ExitPlanMode".into(),
            rule_content: None,
        };
        let allowed = PermissionGate::new(RuleSet::new(vec![rule(
            base::permission::RuleBehavior::Allow,
        )]))
        .check(&exit(), &json!({}), &plan)
        .await
        .unwrap();
        assert!(
            matches!(allowed, PermissionDecision::Allow { .. }),
            "{allowed:?}"
        );
        let denied = PermissionGate::new(RuleSet::new(vec![rule(
            base::permission::RuleBehavior::Deny,
        )]))
        .check(&exit(), &json!({}), &plan)
        .await
        .unwrap();
        assert!(
            matches!(denied, PermissionDecision::Deny { .. }),
            "{denied:?}"
        );

        let write = PermissionGate::empty()
            .check(&FakeTool::new("Write"), &json!({}), &plan)
            .await
            .unwrap();
        assert!(
            matches!(write, PermissionDecision::Deny { .. }),
            "{write:?}"
        );
    }

    #[tokio::test]
    async fn plan_mode_asks_for_a_read_outside_the_project_as_default_mode_does() {
        let inside = FakeTool::new("Read")
            .read_only()
            .matches("/tmp/a.txt")
            .own(base::tool::PermissionDecision::allow());
        let outside = FakeTool::new("Read")
            .read_only()
            .matches("/elsewhere/a.txt");
        for mode in [PermissionMode::Plan, PermissionMode::Default] {
            let ctx = ctx_with_mode(mode);
            let gate = PermissionGate::empty();
            let d = gate.check(&inside, &json!({}), &ctx).await.unwrap();
            assert!(
                matches!(d, PermissionDecision::Allow { .. }),
                "{mode:?}: {d:?}"
            );
            let d = gate.check(&outside, &json!({}), &ctx).await.unwrap();
            assert!(
                matches!(d, PermissionDecision::Ask { .. }),
                "{mode:?}: {d:?}"
            );
        }
    }

    #[tokio::test]
    async fn accept_edits_allows_a_write_the_tool_allowed_and_asks_for_one_it_did_not() {
        let ctx = ctx_with_mode(PermissionMode::AcceptEdits);
        let inside = FakeTool::new("Write")
            .matches("/tmp/a.txt")
            .own(base::tool::PermissionDecision::allow());
        let d = PermissionGate::empty()
            .check(&inside, &json!({}), &ctx)
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }), "{d:?}");

        for tool in [
            FakeTool::new("Write").matches("/elsewhere/a.txt"),
            FakeTool::new("Edit").matches("/elsewhere/a.txt"),
            FakeTool::new("Write")
                .matches("/tmp/.agix/hooks/x.sh")
                .own(base::tool::PermissionDecision::allow()),
        ] {
            let d = PermissionGate::empty()
                .check(&tool, &json!({}), &ctx)
                .await
                .unwrap();
            assert!(
                matches!(d, PermissionDecision::Ask { .. }),
                "{:?}: {d:?}",
                tool.match_content
            );
        }
    }

    #[tokio::test]
    async fn accept_edits_allows_a_read_only_tool() {
        let tool = FakeTool::new("Read")
            .read_only()
            .matches("/elsewhere/a.txt");
        let d = PermissionGate::empty()
            .check(
                &tool,
                &json!({}),
                &ctx_with_mode(PermissionMode::AcceptEdits),
            )
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }), "{d:?}");
    }

    #[tokio::test]
    async fn dontask_mode_denies_all_unmatched_tools() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn dontask_mode_with_allow_rule_permits() {
        let tool = FakeTool::new("Read").read_only();
        let rules = RuleSet::new(vec![PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: "Read".into(),
            rule_content: None,
        }]);
        let gate = PermissionGate::new(rules);
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[test]
    fn dontask_mode_parses_from_config() {
        let mode: PermissionMode = serde_json::from_str("\"dontAsk\"").unwrap();
        assert_eq!(mode, PermissionMode::DontAsk);
    }

    #[tokio::test]
    async fn default_mode_asks_when_no_rule() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }

    use base::permission::PermissionRule;
}

/// Directory names that are bypass-immune when they appear as a **whole path
/// component**: touching one is denied outright, in every permission mode.
/// Compared component-wise, never as substrings — see
/// [`is_path_bypass_immune`].
///
/// Only credential stores belong here. The bar is "no answer the user could
/// give at a prompt would make this a good idea" — a tool call that wants to
/// read `~/.ssh` is either a mistake or an exfiltration attempt, and there is
/// nothing to confirm.
const BYPASS_IMMUNE_COMPONENTS: &[&str] = &[".ssh", ".aws", ".gnupg"];

/// Directory names that must never be reached by a *silent* fast path, but
/// which the user can legitimately approve: agent and repository
/// configuration.
///
/// They are not in [`BYPASS_IMMUNE_COMPONENTS`]: that would make editing
/// `.claude/settings.json` or `.agix/agents/*.md` impossible, and configuring
/// the agent is a thing users routinely ask the agent to do. They are not
/// credentials; the actual risk is a tool quietly rewriting the rules it is
/// judged by. So they get the weaker treatment that risk calls for: the
/// tool's own `Allow` does not apply to them, and the call goes to the
/// normal confirmation flow instead.
///
/// `.git` is here rather than above for the same reason — `git` plumbing
/// through `Bash` is normal work, and a blanket deny on any path containing
/// a `.git` component blocks far more than it protects.
const CONFIRMATION_REQUIRED_COMPONENTS: &[&str] = &[".git", ".claude", ".agix"];

/// Is this entry directly in `.agix` the agent's working space rather than
/// its configuration: the scratchpad its prompt sends temporary files to,
/// the memory directory it is told to write to directly, or the worktrees
/// its sessions work in.
fn is_agix_working_dir(name: &str) -> bool {
    STATE_ROOT_WORKING_DIRS.contains(&name) || name == PROJECT_WORKTREES_DIR
}

/// Does this path (or command argument) name something under
/// [`CONFIRMATION_REQUIRED_COMPONENTS`]?
///
/// Same whole-component matching as [`is_path_bypass_immune`] — `.github`
/// must not match `.git`.
fn is_path_confirmation_required(content: &str) -> bool {
    let lowered = content.to_lowercase();
    let components: Vec<&str> = lowered.split('/').collect();
    lowered.contains('/')
        && components.iter().enumerate().any(|(i, component)| {
            CONFIRMATION_REQUIRED_COMPONENTS.contains(component)
                && !(*component == ".agix"
                    && components.get(i + 1).is_some_and(|next| {
                        // In a shell command the name runs on into whatever follows it.
                        let name = next
                            .split(|c: char| !(c.is_alphanumeric() || "._-".contains(c)))
                            .next()
                            .unwrap_or_default();
                        is_agix_working_dir(name)
                    }))
        })
}

/// Check whether a path (or command argument) references a sensitive path
/// that every mode consulting rules refuses — neither the tool's own allow
/// nor an allow rule reaches past it. `BypassPermissions` consults nothing.
///
/// **按组件匹配**，不做子串查找：按 `/` 切成组件、整段做相等比较，因此
/// `.github` / `.gitignore` / `.gitlab-ci.yml` 不会撞上 `.git`（否则 agent 改不了
/// CI 配置），而且每一个组件都会被检查，不只看第一次出现。前导 `~` 自成一个
/// 组件，不影响后面的 `.ssh` 被识别。
///
/// `/etc/passwd` 之类的绝对路径检查与破坏性命令模式检查保持原样 —— 它们是
/// 另外两件事（子串匹配在那里就是想要的语义）。
fn is_path_bypass_immune(content: &str) -> bool {
    let lowered = content.to_lowercase();
    // Check if this looks like a file path (contains /)
    let looks_like_path = lowered.contains('/');
    if looks_like_path {
        // Whole-component comparison. Empty components (leading `/`, `//`,
        // trailing `/`) simply never match anything in the list.
        if lowered
            .split('/')
            .any(|component| BYPASS_IMMUNE_COMPONENTS.contains(&component))
        {
            return true;
        }
        // Critical system files
        for sys_path in &["/etc/passwd", "/etc/shadow", "/etc/ssh/"] {
            if lowered.contains(sys_path) {
                return true;
            }
        }
    }
    // Destructive patterns — check on whitespace boundaries to avoid false
    // positives like "echo rm -rf /" in descriptions.
    for destructive in &["rm -rf /", "mkfs.", "dd if="] {
        if let Some(idx) = lowered.find(destructive) {
            let at_start = idx == 0;
            let after_space = idx > 0 && lowered.as_bytes().get(idx - 1) == Some(&b' ');
            if at_start || after_space {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod bypass_immune_tests {
    use super::{
        is_agix_working_dir, is_path_bypass_immune, is_path_confirmation_required,
        PROJECT_WORKTREES_DIR, STATE_ROOT_WORKING_DIRS,
    };

    /// `.github` / `.gitignore` / `.gitlab-ci.yml` 只是**前缀**撞上
    /// `.git`，不是同一个路径组件。把它们判成 bypass-immune 会让 agent 改不了
    /// CI 配置。
    #[test]
    fn dot_git_prefix_matches_are_not_immune() {
        assert!(!is_path_bypass_immune("/repo/.github/workflows/ci.yml"));
        assert!(!is_path_bypass_immune("/repo/.gitignore"));
        assert!(!is_path_bypass_immune("/repo/.gitmodules"));
        assert!(!is_path_bypass_immune("/repo/.gitlab-ci.yml"));
        assert!(!is_path_bypass_immune("repo/.github/dependabot.yml"));
    }

    /// `~/.ssh` 里 `.ssh` 前面是 `~`，嵌套在别的目录下的敏感目录也一样要拦。
    #[test]
    fn tilde_prefixed_and_nested_sensitive_dirs_are_immune() {
        assert!(is_path_bypass_immune("~/.ssh/id_rsa"));
        assert!(is_path_bypass_immune("cat ~/.ssh/id_rsa"));
        assert!(is_path_bypass_immune("/home/u/.ssh/config"));
        assert!(is_path_bypass_immune("~/.aws/credentials"));
        assert!(is_path_bypass_immune("/Users/me/.gnupg/secring.gpg"));
    }

    /// 每一个组件都要检查，而不只是第一次出现的那个 —— 敏感组件出现在很深的
    /// 位置同样要拦。
    #[test]
    fn every_component_is_checked_not_just_the_first() {
        assert!(is_path_bypass_immune(
            "/repo/.github/workflows/../.ssh/id_rsa"
        ));
        assert!(is_path_bypass_immune("/a/b/c/d/e/f/.aws/credentials"));
    }

    /// 配置目录（`.git` / `.claude` / `.agix`）不是凭据，走的是"要确认"而不是
    /// "一律拒绝"——否则 agent 连自己的 `.claude/settings.json` 都改不了，而
    /// 这恰恰是用户经常要求它做的事。
    #[test]
    fn config_dirs_require_confirmation_rather_than_being_denied() {
        for p in [
            "repo/.git/config",
            "/Users/me/.agix/settings.json",
            "/a/b/.claude/settings.local.json",
        ] {
            assert!(
                !is_path_bypass_immune(p),
                "{p} must not be hard-denied — the user can legitimately approve it"
            );
            assert!(
                is_path_confirmation_required(p),
                "{p} must not take the silent tool-self-allow fast path either"
            );
        }
        // 同样的组件边界规则：`.github` 不是 `.git`。
        assert!(!is_path_confirmation_required(
            "/repo/.github/workflows/ci.yml"
        ));
        assert!(!is_path_confirmation_required("/repo/src/main.rs"));
    }

    #[test]
    fn the_agents_working_space_under_agix_is_not_configuration() {
        for p in [
            "/repo/.agix/scratchpad/cases.txt",
            "/Users/me/.agix/memory/MEMORY.md",
            "cd /repo\nS=/repo/.agix/scratchpad\nmkdir -p \"$S\"",
            "printf x > \"$d/.agix/scratchpad\"",
            "/repo/.agix/worktrees/feat-a/feature.txt",
            "cd /repo/.agix/worktrees/feat-a && git add -A",
        ] {
            assert!(!is_path_confirmation_required(p), "{p}");
        }
        for p in [
            "/repo/.agix/agents/reviewer.md",
            "/repo/.agix/scratchpad.md",
            "/repo/.agix/worktrees/feat-a/.agix/agents/reviewer.md",
            "/repo/.agix/worktrees/feat-a/.claude/settings.json",
            "/repo/.agix/worktrees/feat-a/.git",
        ] {
            assert!(is_path_confirmation_required(p), "{p}");
        }
    }

    #[test]
    fn what_the_gate_treats_as_working_space_the_sandbox_leaves_writable() {
        use base::paths::{ENGINE_CONFIG_DIRS, ENGINE_CONFIG_FILES};
        for name in STATE_ROOT_WORKING_DIRS
            .iter()
            .chain([&PROJECT_WORKTREES_DIR])
        {
            assert!(is_agix_working_dir(name), "{name}");
            assert!(
                !ENGINE_CONFIG_DIRS.contains(name) && !ENGINE_CONFIG_FILES.contains(name),
                "{name}"
            );
        }
    }

    #[test]
    fn ordinary_paths_are_not_immune() {
        assert!(!is_path_bypass_immune("/repo/src/main.rs"));
        assert!(!is_path_bypass_immune("crates/tools/src/bash/mod.rs"));
        // 无 `/` 的裸文件名不是路径，不参与组件判定
        assert!(!is_path_bypass_immune(".gitignore"));
    }

    #[test]
    fn system_paths_and_destructive_patterns_still_immune() {
        assert!(is_path_bypass_immune("/etc/passwd"));
        assert!(is_path_bypass_immune("/etc/shadow"));
        assert!(is_path_bypass_immune("/etc/ssh/sshd_config"));
        assert!(is_path_bypass_immune("rm -rf /"));
        assert!(is_path_bypass_immune("sudo rm -rf /"));
        assert!(is_path_bypass_immune("dd if=/dev/zero of=/dev/disk0"));
        // 描述里提到但不在词边界上的不算
        assert!(!is_path_bypass_immune("echo norm -rf / please"));
    }
}

fn unclear_message(tool_name: &str, rule: &PermissionRule) -> String {
    format!(
        "{tool_name}: which program a command here runs cannot be told from its text, so it \
         cannot be checked against {}",
        format_rule_string(rule)
    )
}
