//! `PermissionGate` —— 权限决策入口。
//!
//! 决策顺序（`PreToolUse` hook 不在此处——由调用方 `runtime::turn` 在进入
//! 本 gate 之前执行，见 `execute_tool_inner`）：
//!
//! 1. `tool.validate_input` —— 输入合法性
//! 2. `tool.check_permissions` —— 工具自带的判定：
//!    - `Deny` **直接返回**（工具拒绝自己是权威的：只有工具知道自己的输入
//!      为什么不该跑）
//!    - `Allow` **不再直接返回**，只记成一个"待生效的快速通道"（pending
//!      fast-path），要等下面 3–5 步都没否掉才兑现
//!    - `Ask` = 工具没意见
//! 3. 通用规则引擎（`RuleSet::evaluate`）：`Deny` 直接返回
//! 4. bypass-immune 路径检查（`is_path_bypass_immune`）→ deny
//! 5. 模式级硬拒绝：
//!    - `Plan` 且非只读工具 → deny
//!    - `DontAsk` 且没有 allow 规则命中 → deny
//! 6. 兑现第 2 步记下的工具自判 `Allow` → allow
//! 7. 规则引擎的 `Allow` → allow
//! 8. 其余 PermissionMode 分派：
//!    - `BypassPermissions` → allow
//!    - `AcceptEdits` 且工具是 Edit / Write（或只读）→ allow
//!    - `Auto` / `Yolo` → classifier，Defer 则落到 ask
//!    - 其余（含只读工具下的 `Plan`）→ ask / allow
//!
//! **为什么 2 的 Allow 要往后挪（2026-08-11 审计 N-2）**：老实现里
//! `tool.check_permissions` 返回 `Allow` 就立即 return，而 `Write` / `Edit` /
//! `Read` / `Grep` / `Glob` / 只读 `Bash` 全都会自判 Allow。后果是用户写在
//! settings.json 里的 `deny` 规则对这批工具**根本不可达**，Plan 模式拦不住项目
//! 内的 `Write` / `Edit`，`DontAsk` 形同虚设，第 4 步的 bypass-immune 名单也永远
//! 不执行。改成"pending allow"后，UX 不变（Default 模式下项目内编辑、只读命令
//! 依然不弹窗，因为 3–5 步都不会否掉它们），但 deny 规则 / Plan / DontAsk /
//! bypass-immune 名单重新凌驾于工具自身的意见之上 —— 这是同类 agent 通用的优先级。
//!
//! 注意 `BypassPermissions` 依旧要被规则 `Deny` 与 bypass-immune 路径压制
//! （见测试 `rule_engine_deny_short_circuits`）：这两项检查排在模式分派之前。
//!
//! 关键：gate **不**直接调 `effects.ask_user`。它返回 `PermissionDecision::Ask`，
//! 由调用方（engine）决定怎么把它升级成 allow / deny。

use crate::error::GateError;
use crate::path_rule::ResolvedPath;
use crate::rule::format_rule_string;
use crate::ruleset::{path_rule_group, RuleHit, RuleSet};
use async_trait::async_trait;
use base::permission::PermissionRule;
use base::permission::{DecisionReason, PermissionDecision, PermissionMode};
use base::tool::Tool;
use base::tool::ToolContext;
use serde_json::Value;
use std::sync::{Arc, RwLock};

/// Auto 模式下的 LLM-based 决策器。在没有规则匹配 + 工具自身没结论时，让小
/// 模型（典型 haiku）看一眼工具调用，给出 allow / defer。
///
/// **职责非常窄**：classifier 不能 Deny —— 拒绝由 RuleSet 或工具自身负责。
/// 如果 classifier 不能给出明确 Allow，就 Defer 让 gate fall through 到 Ask。
/// 这保留了用户最终决定权，避免 model 过度自信酿成 destructive ops。
#[async_trait]
pub trait AutoClassifier: Send + Sync {
    /// 决策入参：工具名、tool prompt（让 classifier 知道工具能干啥）、tool input（看具体调用）
    async fn classify(
        &self,
        tool_name: &str,
        tool_description: &str,
        input: &Value,
    ) -> ClassifyDecision;
}

/// classifier 决策。
///
/// `Serialize` + `Deserialize` 允许 LlmClassifier 持久化决策缓存到 ~/.atta/。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ClassifyDecision {
    /// classifier 判定为安全，应允许执行。
    Allow { reason: String },
    /// classifier 判定为危险（仅 LLM-based classifier 使用）。对应的
    /// `PermissionDecision::Deny` 将包含此 reason。
    Deny { reason: String },
    /// classifier 认为安全但建议改输入后执行（例如加 `--dry-run`）。
    /// Gate 将其映射为 `Allow` 并在 decision_reason 附带建议内容。
    AllowWithEdit {
        reason: String,
        /// 建议的修改内容（文本描述，不直接做 input 改写）。
        suggested_edits: String,
    },
    /// classifier 不确定，让 gate 继续走 ask 流程
    Defer,
}

pub struct PermissionGate {
    rules: Arc<RwLock<RuleSet>>,
    /// `Auto` 模式生效时的 classifier。`None` 时 Auto 行为同 Default（fall to ask）。
    auto_classifier: Option<Arc<dyn AutoClassifier>>,
    /// Denial counter for tracking and circuit-breaking: tracks consecutive
    /// and total denials to decide when to fall back from auto-mode to prompting.
    denial_count: std::sync::atomic::AtomicU64,
    total_denial_count: std::sync::atomic::AtomicU64,
}

impl PermissionGate {
    /// Construct a new instance.
    pub fn new(rules: RuleSet) -> Self {
        Self {
            rules: Arc::new(RwLock::new(rules)),
            auto_classifier: None,
            denial_count: std::sync::atomic::AtomicU64::new(0),
            total_denial_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Empty/default instance with no state.
    pub fn empty() -> Self {
        Self::new(RuleSet::empty())
    }

    /// Record a permission denial for tracking.
    pub fn record_denial(&self) {
        self.denial_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.total_denial_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a success — resets the consecutive denial counter.
    pub fn record_success(&self) {
        self.denial_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Number of consecutive denials (reset on success).
    pub fn consecutive_denials(&self) -> u64 {
        self.denial_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total denials since gate creation.
    pub fn total_denials(&self) -> u64 {
        self.total_denial_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check if the denial threshold has been exceeded (consecutive denials >= max).
    pub fn should_fallback_to_prompting(&self) -> bool {
        self.consecutive_denials() >= 3 || self.total_denials() >= 20
    }

    /// 注入 Auto mode classifier。CLI 在 permission_mode == Auto 时构造 LLM-based
    /// 实现；测试可以注入 mock。
    pub fn with_auto_classifier(mut self, classifier: Arc<dyn AutoClassifier>) -> Self {
        self.auto_classifier = Some(classifier);
        self
    }

    /// 注入 YOLO classifier 用于 Yolo 权限模式。与 with_auto_classifier 互不干扰；
    /// Yolo 模式使用 YoloClassifier 的特定规则，Auto 模式使用通用 LLM-based classifier。
    pub fn with_yolo_classifier(mut self, classifier: Arc<dyn AutoClassifier>) -> Self {
        self.auto_classifier = Some(classifier);
        self
    }

    /// Snapshot view of the rule list.
    pub fn rules(&self) -> Vec<PermissionRule> {
        self.rules.read().unwrap().rules().to_vec()
    }

    /// **P (2026-05-17)**: 运行时加规则。用于"允许此项目"流程 —— 用户选
    /// "allow for this project" 后，Engine 构造 ProjectSettings 规则写入。
    pub fn add_rules(&self, additional: Vec<PermissionRule>) {
        self.rules.write().unwrap().extend(additional);
    }

    /// Drop every rule from `source` — see `RuleSet::remove_by_source`.
    pub fn remove_rules_by_source(&self, source: base::permission::RuleSource) {
        self.rules.write().unwrap().remove_by_source(source);
    }

    /// Produce a human-readable explanation of what permission decision
    /// the rule engine would make for a given tool call.
    ///
    /// This is a lightweight, synchronous alternative to `check()` that
    /// only consults the rule engine (no tool-specific checks, no mode
    /// dispatch). It explains whether a rule matches, and if so, which one.
    ///
    /// Examples:
    /// - `"Allowed by rule: Bash(git:*) matches \"git status\""`
    /// - `"Denied by path safety: /etc/passwd is in bypass-immune deny list"`
    /// - `"Ask: no matching rule found for Bash(kubectl) — no rules configured for this tool"`
    /// - `"Rule matches: Bash(git status) [ask] — requires confirmation"`
    pub fn explain_decision(&self, tool_name: &str, tool_input: &Value) -> String {
        // Best-effort content extraction mirroring tool implementations.
        let content = extract_content_from_input(tool_input);

        // 1. Check bypass-immune paths even in the explainer, since this
        //    check always runs before mode dispatch.
        if let Some(ref c) = content {
            if is_path_bypass_immune(c) {
                return format!("Denied by path safety: \"{c}\" is in the bypass-immune deny list");
            }
        }

        // 2. Rule engine evaluation.
        let rules = self.rules.read().unwrap();
        match rules.evaluate(tool_name, content.as_deref()) {
            RuleHit::Allow(rule) => {
                let rule_str = format_rule_string(&rule);
                let content_info = content
                    .as_ref()
                    .map(|c| format!(" matches \"{c}\""))
                    .unwrap_or_default();
                format!("Allowed by rule: {rule_str}{content_info}")
            }
            RuleHit::Deny(rule) => {
                let rule_str = format_rule_string(&rule);
                let content_info = content
                    .as_ref()
                    .map(|c| format!(" matches \"{c}\""))
                    .unwrap_or_default();
                format!("Denied by rule: {rule_str}{content_info}")
            }
            RuleHit::Ask(rule) => {
                let rule_str = format_rule_string(&rule);
                let content_info = content
                    .as_ref()
                    .map(|c| format!(" matches \"{c}\""))
                    .unwrap_or_default();
                format!("Rule matches: {rule_str} [ask]{content_info} — requires confirmation")
            }
            RuleHit::None => {
                let content_display = content.as_deref().unwrap_or("<no content>");
                let has_rules_for_tool = rules
                    .rules()
                    .iter()
                    .any(|r| crate::ruleset::matches_tool_name(&r.tool_name, tool_name));

                if has_rules_for_tool {
                    format!(
                        "Ask: no matching rule found for {}({content_display}) — \
                         the tool has rules but none match this specific content",
                        tool_name,
                    )
                } else {
                    format!(
                        "Ask: no matching rule found for {}({content_display}) — \
                         no rules have been configured for this tool",
                        tool_name,
                    )
                }
            }
        }
    }

    pub async fn check(
        &self,
        tool: &dyn Tool,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<PermissionDecision, GateError> {
        // 1. validateInput
        if let base::tool::ValidationResult::Err(msg, code) = tool.validate_input(input, ctx).await
        {
            return Err(GateError::InvalidInput { message: msg, code });
        }

        // 2. tool.check_permissions —— 工具自带判定
        //
        // Deny 是权威的、立即返回：工具比 gate 更清楚自己的这组输入为什么不能跑
        // （例如 Write 检测到路径逃出 cwd）。Allow 则**只是备选**：见模块级注释的
        // N-2 说明 —— 直接 return 会让 deny 规则 / Plan / DontAsk / bypass-immune
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

        // 3. 通用规则引擎 —— Deny 短路（必须压过 pending 的工具自判 Allow，
        //    也必须压过后面的 BypassPermissions）。Allow 先存着，等模式级硬拒绝
        //    （Plan / DontAsk）判完再用：DontAsk 就是靠"有没有 allow 规则"决定
        //    放行还是拒绝的。
        let content = tool.permission_match_content(input);
        let path_group = path_rule_group(tool.permission_content_kind());
        let resolved = path_group
            .zip(content.as_deref())
            .map(|(_, c)| ResolvedPath::new(c, &ctx.cwd));
        let rule_hit = {
            let rules = self.rules.read().unwrap();
            match (path_group, &resolved) {
                (Some(group), Some(path)) => {
                    rules.evaluate_path(tool.name(), group, path, &ctx.cwd)
                }
                _ => rules.evaluate(tool.name(), content.as_deref()),
            }
        };
        let checked_strings: Vec<String> = content
            .iter()
            .cloned()
            .chain(
                resolved
                    .iter()
                    .flat_map(|r| r.all().map(|p| p.to_string_lossy().into_owned())),
            )
            .collect();
        let rule_allow = match rule_hit {
            RuleHit::Deny(rule) => {
                return Ok(PermissionDecision::Deny {
                    message: format!(
                        "denied by rule: {}{}",
                        rule.tool_name,
                        rule.rule_content
                            .as_deref()
                            .map(|c| format!("({c})"))
                            .unwrap_or_default()
                    ),
                    decision_reason: DecisionReason::Rule(rule),
                });
            }
            RuleHit::Allow(rule) => Some(rule),
            RuleHit::Ask(_) | RuleHit::None => None,
        };

        // 4. Bypass-immune safety: even in BypassPermissions mode — and even when
        //    the tool allowed itself in step 2 — block access to sensitive paths
        //    that should never be touched without explicit user confirmation.
        //    Equivalent to TS step 1g (safety checks).
        if let Some(content) = checked_strings.iter().find(|s| is_path_bypass_immune(s)) {
            return Ok(PermissionDecision::Deny {
                message: format!(
                    "access to protected path '{content}' is blocked regardless of \
                     permission mode — this path is bypass-immune"
                ),
                decision_reason: DecisionReason::ToolBuiltin("bypass_immune".into()),
            });
        }

        // 5. 模式级硬拒绝 —— 读 session（运行时可变），不读 config 的初始值。
        //    这两条同样要压过 pending 的工具自判 Allow，否则 Plan 模式下
        //    `Write`（自判 Allow）照样能落盘，`DontAsk` 也拦不住任何自判工具。
        // A tool may opt its self-`Allow` out of *mode* denials when that
        // `Allow` is about the tool's nature rather than its arguments — see
        // `Tool::self_allow_overrides_mode`. Deliberately narrow: it does not
        // affect the rule engine or the bypass-immune list above, both of
        // which have already had their say.
        let mode = ctx.session.permission_mode();
        let mode_denials_apply = !(tool_self_allow && tool.self_allow_overrides_mode());
        if mode_denials_apply && matches!(mode, PermissionMode::Plan) && !tool.is_read_only(input) {
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

        // 6. 现在才兑现工具自判 Allow —— 没有 deny 规则、没踩 bypass-immune 路径、
        //    也不在会硬拒的模式里。保住了原来的 UX：Default 模式下项目内的
        //    Write/Edit 与只读 Bash 依旧静默通过。
        //
        //    唯一的例外是 agent / 仓库配置目录（`.git` / `.claude` / `.atta`）：
        //    它们不是凭据（凭据在第 4 步就硬拒了），用户完全可能想让 agent 去
        //    改；但"工具自己说没问题"不足以让一次改写 agent 自身规则的调用静默
        //    通过。所以这里只是**跳过快速通道**，让它落到下面的模式分派去走正常
        //    确认流程 —— 该问就问，而不是一律拒绝。
        let needs_confirmation = checked_strings
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
            PermissionMode::BypassPermissions => Ok(PermissionDecision::Allow {
                updated_input: None,
                decision_reason: Some(DecisionReason::Mode(mode)),
            }),

            PermissionMode::AcceptEdits => {
                let read_only = tool.is_read_only(input);
                let is_edit_or_write = tool.name() == "Edit" || tool.name() == "Write";
                if read_only || is_edit_or_write {
                    Ok(PermissionDecision::Allow {
                        updated_input: None,
                        decision_reason: Some(DecisionReason::Mode(mode)),
                    })
                } else {
                    // acceptEdits 不适用 → 走默认 ask
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

            // P2: Bubble mode — forward permission requests to parent agent
            // rather than prompting the user. The parent agent decides allow/deny.
            PermissionMode::Bubble => Ok(PermissionDecision::Ask {
                message: "Bubble: forwarding permission request to parent agent".into(),
                decision_reason: Some(DecisionReason::Mode(mode)),
            }),

            PermissionMode::Default
            | PermissionMode::Plan
            | PermissionMode::Auto
            | PermissionMode::Yolo => {
                // Plan 命中此处说明 tool 是 read_only（非只读的在第 5 步已经
                // deny 了）—— 默认 allow
                if matches!(mode, PermissionMode::Plan) && tool.is_read_only(input) {
                    return Ok(PermissionDecision::Allow {
                        updated_input: None,
                        decision_reason: Some(DecisionReason::Mode(mode)),
                    });
                }
                // Auto / Yolo 模式：如果挂了 classifier 就让它判一下。
                // - Allow → 允许
                // - Deny → 拒绝（仅 LLM-based classifier 产生）
                // - AllowWithEdit → 允许（reason 附带建议内容）
                // - Defer → 继续走 ask。Default 和 Plan(write) 直接走 ask。
                if matches!(mode, PermissionMode::Auto | PermissionMode::Yolo) {
                    if let Some(classifier) = &self.auto_classifier {
                        let prompt_ctx = base::tool::PromptContext::default();
                        let description = tool.prompt(&prompt_ctx).await;
                        let decision = classifier.classify(tool.name(), &description, input).await;
                        match decision {
                            ClassifyDecision::Allow { reason } => {
                                return Ok(PermissionDecision::Allow {
                                    updated_input: None,
                                    decision_reason: Some(DecisionReason::Other(format!(
                                        "auto-classifier: {reason}"
                                    ))),
                                });
                            }
                            ClassifyDecision::Deny { reason } => {
                                return Ok(PermissionDecision::Deny {
                                    message: format!("classifier denied: {reason}"),
                                    decision_reason: DecisionReason::Classifier {
                                        classifier: "llm".into(),
                                    },
                                });
                            }
                            ClassifyDecision::AllowWithEdit {
                                reason,
                                suggested_edits,
                            } => {
                                let mut label = format!("auto-classifier: {reason}");
                                if !suggested_edits.is_empty() {
                                    label.push_str(&format!(
                                        " (suggested edits: {suggested_edits})"
                                    ));
                                }
                                return Ok(PermissionDecision::Allow {
                                    updated_input: None,
                                    decision_reason: Some(DecisionReason::Other(label)),
                                });
                            }
                            ClassifyDecision::Defer => { /* fall through to Ask */ }
                        }
                    }
                }
                Ok(PermissionDecision::Ask {
                    message: format!("Allow {}?", tool.name()),
                    decision_reason: Some(DecisionReason::Mode(mode)),
                })
            }
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
    }

    impl FakeTool {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                read_only: false,
                own_decision: None,
                match_content: None,
            }
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

    /// N-2: 用户在 settings.json 里写的 deny 规则必须压过工具的自判 Allow。
    /// 老实现里 `Write`/`Edit`/`Read`/`Grep`/`Glob`/只读 `Bash` 全都自判 Allow，
    /// 于是这类 deny 规则对它们**完全不可达**。
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

    /// N-2: Plan 模式必须拦住自判 Allow 的非只读工具（典型就是项目内的
    /// `Write` / `Edit` —— 它们过去在 Plan 模式下照样落盘）。
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

    /// N-2 + N-15: bypass-immune 路径必须压过工具自判 Allow —— 否则
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

    /// N-2: `DontAsk` 是"没有规则就拒绝"，自判 Allow 不该把它变成"什么都放行"。
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
            .check(
                &tool,
                &json!({}),
                &ctx_with_mode(PermissionMode::BypassPermissions),
            )
            .await
            .unwrap();
        // 即便 Bypass 模式，更早的 rule deny 已生效
        assert!(matches!(d, PermissionDecision::Deny { .. }));
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
    async fn plan_mode_allows_readonly() {
        let tool = FakeTool::new("Read").read_only();
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Plan))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn accept_edits_allows_write_tool() {
        let tool = FakeTool::new("Write");
        let gate = PermissionGate::empty();
        let d = gate
            .check(
                &tool,
                &json!({}),
                &ctx_with_mode(PermissionMode::AcceptEdits),
            )
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
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

    #[tokio::test]
    async fn auto_mode_without_classifier_falls_to_ask() {
        // 没挂 classifier 时 Auto 退化为 Default ask
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty();
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }

    /// 测试用 stub classifier：固定返回某 ClassifyDecision。
    struct StubClassifier(ClassifyDecision);

    #[async_trait]
    impl AutoClassifier for StubClassifier {
        async fn classify(&self, _: &str, _: &str, _: &Value) -> ClassifyDecision {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_allow_returns_allow() {
        let tool = FakeTool::new("Read");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::Allow {
                reason: "obviously read-only".into(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        match d {
            PermissionDecision::Allow {
                decision_reason: Some(DecisionReason::Other(label)),
                ..
            } => {
                assert!(label.contains("auto-classifier"));
                assert!(label.contains("obviously read-only"));
            }
            other => panic!("expected Allow with auto-classifier reason, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_defer_falls_to_ask() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty()
            .with_auto_classifier(Arc::new(StubClassifier(ClassifyDecision::Defer)));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_deny_returns_deny() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::Deny {
                reason: "rm -rf / is destructive".into(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        match d {
            PermissionDecision::Deny {
                message,
                decision_reason: DecisionReason::Classifier { classifier },
            } => {
                assert!(message.contains("destructive"));
                assert_eq!(classifier, "llm");
            }
            other => panic!("expected Deny with classifier reason, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_allow_with_edit_returns_allow() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::AllowWithEdit {
                reason: "safe with dry-run".into(),
                suggested_edits: "add --dry-run flag".into(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        match d {
            PermissionDecision::Allow {
                decision_reason: Some(DecisionReason::Other(label)),
                ..
            } => {
                assert!(label.contains("dry-run"));
                assert!(label.contains("add --dry-run flag"));
            }
            other => panic!("expected Allow with classifier reason, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_deny_short_circuits_to_deny() {
        let tool = FakeTool::new("Bash");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::Deny {
                reason: "dangerous across any mode".into(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn auto_mode_with_classifier_allow_with_edit_empty_edits() {
        let tool = FakeTool::new("Read");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(StubClassifier(
            ClassifyDecision::AllowWithEdit {
                reason: "fine as-is".into(),
                suggested_edits: String::new(),
            },
        )));
        let d = gate
            .check(&tool, &json!({}), &ctx_with_mode(PermissionMode::Auto))
            .await
            .unwrap();
        assert!(matches!(d, PermissionDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn classifier_does_not_run_in_default_mode() {
        // Default 模式根本不该调 classifier；这里给个会 panic 的 classifier 也安全
        struct ExplodingClassifier;
        #[async_trait]
        impl AutoClassifier for ExplodingClassifier {
            async fn classify(&self, _: &str, _: &str, _: &Value) -> ClassifyDecision {
                panic!("should never be called in Default mode");
            }
        }
        let tool = FakeTool::new("Read");
        let gate = PermissionGate::empty().with_auto_classifier(Arc::new(ExplodingClassifier));
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
/// These were originally in [`BYPASS_IMMUNE_COMPONENTS`], which made editing
/// `.claude/settings.json` or `.atta/agents/*.md` impossible — not merely
/// gated, but denied with no way to say yes, even though configuring the
/// agent is a thing users routinely ask the agent to do. They are not
/// credentials; the actual risk is a tool quietly rewriting the rules it is
/// judged by. So they get the weaker treatment that risk calls for: the
/// tool's own `Allow` does not apply to them, and the call goes to the
/// normal confirmation flow instead.
///
/// `.git` is here rather than above for the same reason — `git` plumbing
/// through `Bash` is normal work, and a blanket deny on any path containing
/// a `.git` component blocks far more than it protects.
const CONFIRMATION_REQUIRED_COMPONENTS: &[&str] = &[".git", ".claude", ".atta"];

/// Does this path (or command argument) name something under
/// [`CONFIRMATION_REQUIRED_COMPONENTS`]?
///
/// Same whole-component matching as [`is_path_bypass_immune`] — `.github`
/// must not match `.git`.
fn is_path_confirmation_required(content: &str) -> bool {
    let lowered = content.to_lowercase();
    lowered.contains('/')
        && lowered
            .split('/')
            .any(|component| CONFIRMATION_REQUIRED_COMPONENTS.contains(&component))
}

/// Check whether a path (or command argument) references a bypass-immune
/// sensitive path. Even in BypassPermissions mode, these paths should never
/// be accessed without explicit user awareness.
///
/// **Component matching (2026-08-11 审计 N-15)**: the old implementation did
/// `lowered.find(sensitive)` and then checked whether the byte *before* the hit
/// was `/`. That was wrong in both directions:
///
/// - **假阳性**：`.git` 命中 `.github` 的前四个字符，而它前面正好是 `/` →
///   `/repo/.github/workflows/ci.yml`、`.gitignore`、`.gitmodules`、
///   `.gitlab-ci.yml` 全被判成 bypass-immune。以前被工具自判 Allow 掩盖着
///   （gate 第 2 步就 return 了，压根跑不到这里）；N-2 把那个短路去掉之后，
///   它会立刻变成"agent 改不了 CI 配置"的日常故障。
/// - **假阴性**：`find` 只看**第一次**出现的位置。`~/.ssh/id_rsa` 里 `.ssh`
///   前面是 `~` 不是 `/`，第一次命中就判否，循环直接放弃这个模式 —— 于是
///   `cat ~/.ssh/id_rsa` 完全不设防。
///
/// 现在按 `/` 切成组件、整段做相等比较，因此 `.github != .git`，而且每一个
/// 组件都会被检查（不再只看第一次出现）。前导 `~` 自成一个组件，天然不影响
/// 后面的 `.ssh` 被识别。
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
    use super::{is_path_bypass_immune, is_path_confirmation_required};

    /// N-15 假阳性：`.github` / `.gitignore` / `.gitlab-ci.yml` 只是**前缀**撞上
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

    /// N-15 假阴性：`~/.ssh` 里 `.ssh` 前面是 `~`，老实现只看第一次出现 + 前一
    /// 字符是否为 `/`，于是整个模式被放过。
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

    /// 配置目录（`.git` / `.claude` / `.atta`）不是凭据，走的是"要确认"而不是
    /// "一律拒绝"——否则 agent 连自己的 `.claude/settings.json` 都改不了，而
    /// 这恰恰是用户经常要求它做的事。
    #[test]
    fn config_dirs_require_confirmation_rather_than_being_denied() {
        for p in [
            "repo/.git/config",
            "/Users/me/.atta/settings.json",
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
    fn ordinary_paths_are_not_immune() {
        assert!(!is_path_bypass_immune("/repo/src/main.rs"));
        assert!(!is_path_bypass_immune("crates/tools/src/bash.rs"));
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

/// Best-effort extraction of permission-match content from a JSON tool
/// input, mirroring what individual tool implementations do in their
/// `permission_match_content` methods. This is used by the explainer
/// for human-readable descriptions; it does not need to be exhaustive
/// since the real decision always goes through the proper Tool trait.
fn extract_content_from_input(input: &Value) -> Option<String> {
    // Bash, Monitor, and similar command-based tools
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        return Some(cmd.to_string());
    }
    // FileRead, FileWrite, FileEdit, and similar path-based tools
    if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
        return Some(path.to_string());
    }
    if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
        return Some(path.to_string());
    }
    // WebFetch
    if let Some(url) = input.get("url").and_then(|v| v.as_str()) {
        return Some(url.to_string());
    }
    // Glob, Grep
    if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
        return Some(pattern.to_string());
    }
    // WebSearch
    if let Some(query) = input.get("query").and_then(|v| v.as_str()) {
        return Some(query.to_string());
    }
    None
}

#[cfg(test)]
mod explain_tests {
    use super::*;
    use base::permission::{RuleBehavior, RuleSource};
    use serde_json::json;

    fn make_gate_with_rules(rules: Vec<(bool, &str, &str)>) -> PermissionGate {
        let permission_rules: Vec<PermissionRule> = rules
            .into_iter()
            .map(|(is_allow, tool, content)| {
                let behavior = if is_allow {
                    RuleBehavior::Allow
                } else {
                    RuleBehavior::Deny
                };
                PermissionRule {
                    source: RuleSource::UserSettings,
                    behavior,
                    tool_name: tool.into(),
                    rule_content: if content.is_empty() {
                        None
                    } else {
                        Some(content.into())
                    },
                }
            })
            .collect();
        PermissionGate::new(RuleSet::new(permission_rules))
    }

    #[test]
    fn explain_allowed_by_rule_matches_content() {
        let gate = make_gate_with_rules(vec![(true, "Bash", "git status")]);
        let explanation = gate.explain_decision("Bash", &json!({"command": "git status"}));
        assert!(
            explanation.contains("Allowed by rule"),
            "expected Allowed by rule, got: {explanation}"
        );
        assert!(
            explanation.contains("Bash(git status)"),
            "expected rule ref, got: {explanation}"
        );
        assert!(explanation.contains("git status"));
    }

    #[test]
    fn explain_allowed_by_prefix_rule() {
        let gate = make_gate_with_rules(vec![(true, "Bash", "git:*")]);
        let explanation = gate.explain_decision("Bash", &json!({"command": "git log"}));
        assert!(explanation.contains("Allowed by rule"));
        assert!(explanation.contains("Bash(git:*)"));
        assert!(explanation.contains("git log"));
    }

    #[test]
    fn explain_denied_by_rule() {
        let gate = make_gate_with_rules(vec![(false, "Read", "/tmp/secret/**")]);
        let explanation =
            gate.explain_decision("Read", &json!({"file_path": "/tmp/secret/data.txt"}));
        assert!(
            explanation.contains("Denied by rule"),
            "expected Denied by rule, got: {explanation}"
        );
        assert!(explanation.contains("/tmp/secret/**"));
    }

    #[test]
    fn explain_ask_no_matching_rule() {
        let gate = make_gate_with_rules(vec![(true, "Bash", "git:*")]);
        let explanation = gate.explain_decision("Read", &json!({"file_path": "/tmp/x"}));
        assert!(
            explanation.contains("Ask: no matching rule found"),
            "expected Ask: no matching rule found, got: {explanation}"
        );
        assert!(explanation.contains("Read"));
    }

    #[test]
    fn explain_bypass_immune_path() {
        let gate = PermissionGate::empty();
        let explanation = gate.explain_decision("Read", &json!({"file_path": "/etc/passwd"}));
        assert!(
            explanation.contains("Denied by path safety"),
            "expected Denied by path safety, got: {explanation}"
        );
        assert!(explanation.contains("/etc/passwd"));
    }

    #[test]
    fn explain_ask_rule() {
        let rules = vec![PermissionRule {
            source: RuleSource::UserSettings,
            behavior: RuleBehavior::Ask,
            tool_name: "Bash".into(),
            rule_content: None,
        }];
        let gate = PermissionGate::new(RuleSet::new(rules));
        let explanation = gate.explain_decision("Bash", &json!({"command": "ls"}));
        assert!(
            explanation.contains("[ask]"),
            "expected [ask] marker, got: {explanation}"
        );
        assert!(explanation.contains("requires confirmation"));
    }

    #[test]
    fn extract_content_from_command_field() {
        let input = json!({"command": "git push origin main"});
        assert_eq!(
            extract_content_from_input(&input),
            Some("git push origin main".into())
        );
    }

    #[test]
    fn extract_content_from_file_path_field() {
        let input = json!({"file_path": "/etc/hosts"});
        assert_eq!(
            extract_content_from_input(&input),
            Some("/etc/hosts".into())
        );
    }

    #[test]
    fn extract_content_returns_none_for_empty_input() {
        let input = json!({});
        assert_eq!(extract_content_from_input(&input), None);
    }

    #[test]
    fn extract_content_from_url_field() {
        let input = json!({"url": "https://example.com"});
        assert_eq!(
            extract_content_from_input(&input),
            Some("https://example.com".into())
        );
    }

    #[test]
    fn extract_content_from_path_field() {
        let input = json!({"path": "/tmp/file.txt"});
        assert_eq!(
            extract_content_from_input(&input),
            Some("/tmp/file.txt".into())
        );
    }
}
