use super::*;
use base::interface::exec::ExecProviders;
use base::tool::ToolContext;

trait CheckWith {
    async fn check_with(
        &self,
        tool: &FakeTool,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<PermissionOutcome, std::convert::Infallible>;
}

impl CheckWith for Policy {
    async fn check_with(
        &self,
        tool: &FakeTool,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<PermissionOutcome, std::convert::Infallible> {
        let tool: Arc<dyn Tool> = Arc::new(tool.clone());
        let req = PermissionRequest::new(
            tool.name(),
            Some(&tool),
            input,
            &ctx.cwd,
            &ctx.session_id,
            &ctx.session,
        )
        .with_ctx(ctx);
        Ok(self.decide(&req).await)
    }
}
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
    let gate = Policy::new(RuleSet::new(vec![rule(
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

/// A tool whose every answer the test sets.
#[derive(Clone)]
struct FakeTool {
    name: &'static str,
    read_only: bool,
    own_decision: Option<base::tool::PermissionDecision>,
    match_content: Option<String>,
    invalid: bool,
    leaves_plan: bool,
    kind: PermissionContentKind,
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
            kind: PermissionContentKind::Opaque,
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
    fn shell(mut self) -> Self {
        self.kind = PermissionContentKind::ShellCommand;
        self
    }
}

#[async_trait]
impl Tool for FakeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
            .permission_content(self.kind)
            .read_only(self.read_only)
            .leaves_plan_mode(self.leaves_plan)
    }

    fn description(&self) -> &str {
        "fake tool (test tool)"
    }

    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        json!({"type":"object"})
    }
    async fn guide(&self, _: &base::tool::PromptContext) -> Option<String> {
        Some("fake".into())
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
    let mut ctx = ToolContext::for_test_with(PathBuf::from("/tmp"), ExecProviders::in_process());
    ctx.permission_mode = mode;
    ctx.session = Arc::new(SessionState::new(PathBuf::from("/tmp")).with_permission_mode(mode));
    ctx.tool_use_id = "test".into();
    ctx
}

/// A user's more specific ask matching too must not hide that the
/// managed layer asks: that is what keeps a hook from answering it.
#[tokio::test]
async fn an_ask_the_managed_layer_makes_is_reported_as_its_own() {
    let ask = |source, content: &str| PermissionRule {
        source,
        behavior: RuleBehavior::Ask,
        tool_name: "X".into(),
        rule_content: Some(content.into()),
    };
    let managed = ask(RuleSource::PolicySettings, "touch:*");
    let user = ask(RuleSource::UserSettings, "touch /tmp/a");
    let tool = FakeTool::new("X").matches("touch /tmp/a");
    let decide = |rules: Vec<PermissionRule>, policy: Vec<PermissionRule>| {
        let gate = Policy::new(RuleSet::new(rules)).with_managed(ManagedLayer::of_rules(policy));
        let tool = &tool;
        async move {
            match gate
                .check_with(tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
                .await
                .unwrap()
            {
                PermissionOutcome::Prompt { answerable, .. } => {
                    answerable == Answerable::PersonOnly
                }
                other => panic!("expected an ask, got {other:?}"),
            }
        }
    };

    assert!(decide(vec![managed.clone(), user.clone()], vec![managed.clone()]).await);
    assert!(!decide(vec![user], vec![]).await);

    let bypass = Policy::new(RuleSet::new(vec![managed.clone()]))
        .with_managed(ManagedLayer::of_rules(vec![managed]));
    let d = bypass
        .check_with(
            &tool,
            &json!({}),
            &ctx_with_mode(PermissionMode::BypassPermissions),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            &d,
            PermissionOutcome::Prompt {
                answerable: Answerable::PersonOnly,
                ..
            }
        ),
        "{d:?}"
    );
}

#[tokio::test]
async fn tool_own_allow_is_honoured_when_nothing_else_objects() {
    // 工具自判 Allow 仍然是"不弹窗"的快速通道 —— 只是不再短路：这里没有
    // deny 规则、没踩 bypass-immune 路径、模式也是 Default，所以兑现。
    let tool = FakeTool::new("X").own(base::tool::PermissionDecision::allow());
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    match d {
        PermissionOutcome::Permit { basis, .. } => assert_eq!(basis, Basis::Contained),
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
    let gate = Policy::new(rules);
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(
        matches!(d, PermissionOutcome::Deny { .. }),
        "deny rule must outrank the tool's own Allow, got {d:?}"
    );
}

/// Plan 模式必须拦住自判 Allow 的非只读工具（典型就是项目内的
/// `Write` / `Edit`）。
#[tokio::test]
async fn plan_mode_denies_self_allowing_non_readonly_tool() {
    let tool = FakeTool::new("Write").own(base::tool::PermissionDecision::allow());
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Plan))
        .await
        .unwrap();
    assert!(
        matches!(d, PermissionOutcome::Deny { .. }),
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
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    match d {
        PermissionOutcome::Deny { ref reason } => assert_eq!(reason.stage, Stage::Credentials),
        other => panic!("expected a credentials Deny, got {other:?}"),
    }
}

#[tokio::test]
async fn credentials_are_refused_in_every_mode() {
    let state = tempfile::tempdir().unwrap();
    let token = state.path().join("mcp/oauth/x.json");
    for (content, mode, shell) in [
        (
            token.display().to_string(),
            PermissionMode::BypassPermissions,
            false,
        ),
        (
            token.display().to_string(),
            PermissionMode::AcceptEdits,
            false,
        ),
        (
            format!("cat '{}/Settings.json'", state.path().display()),
            PermissionMode::BypassPermissions,
            true,
        ),
        (
            "rg key .agix/settings.local.json".to_string(),
            PermissionMode::Default,
            true,
        ),
        (
            format!("--from={}/daemon/auth/x.token", state.path().display()),
            PermissionMode::Default,
            false,
        ),
    ] {
        let tool = FakeTool::new("Read")
            .read_only()
            .own(base::tool::PermissionDecision::allow())
            .matches(&content);
        let tool = if shell { tool.shell() } else { tool };
        let mut ctx = ctx_with_mode(mode);
        ctx.sandbox.paths = std::sync::Arc::new(SensitivePaths::new(Some(state.path()), None));
        let d = Policy::empty()
            .check_with(&tool, &json!({}), &ctx)
            .await
            .unwrap();
        assert!(
            matches!(
                d,
                PermissionOutcome::Deny { ref reason } if reason.stage == Stage::Credentials
            ),
            "{content} under {mode:?}: {d:?}"
        );
    }
}

#[tokio::test]
async fn the_rest_of_the_state_root_is_not_a_credential() {
    let state = tempfile::tempdir().unwrap();
    let tool = FakeTool::new("Read")
        .read_only()
        .own(base::tool::PermissionDecision::allow())
        .matches(&state.path().join("skills/x/SKILL.md").display().to_string());
    let mut ctx = ctx_with_mode(PermissionMode::Default);
    ctx.sandbox.paths = std::sync::Arc::new(SensitivePaths::new(Some(state.path()), None));
    let d = Policy::empty()
        .check_with(&tool, &json!({}), &ctx)
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }), "{d:?}");
}

/// `DontAsk` 是"没有规则就拒绝"，自判 Allow 不该把它变成"什么都放行"。
#[tokio::test]
async fn dontask_mode_denies_self_allowing_tool_without_rule() {
    let tool = FakeTool::new("Write").own(base::tool::PermissionDecision::allow());
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
}

#[tokio::test]
async fn tool_own_deny_short_circuits() {
    let tool = FakeTool::new("X").own(base::tool::PermissionDecision::Deny {
        reason: Some("no".into()),
        decision_reason: None,
    });
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
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
    let gate = Policy::new(rules);
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }));
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
    let gate = Policy::new(rules);
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
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
        FakeTool::new("Read").read_only().matches("/etc/passwd"),
        FakeTool::new("Bash").matches("rm -rf /"),
        FakeTool::new("Write")
            .matches("/repo/.agix/settings.json")
            .own(base::tool::PermissionDecision::Deny {
                reason: Some("guarded".into()),
                decision_reason: None,
            }),
    ];
    for tool in cases {
        let d = Policy::new(deny_bash())
            .check_with(&tool, &json!({}), &ctx)
            .await
            .unwrap();
        assert!(
            matches!(
                d,
                PermissionOutcome::Permit {
                    basis: Basis::Bypass,
                    ..
                }
            ),
            "{spelling} {:?}: {d:?}",
            tool.match_content
        );
    }
}

/// A delegation's own deny rules hold its delegate's calls in any mode, and
/// only its delegate's: the session relaying them is not held to them.
#[tokio::test]
async fn a_delegations_deny_rules_refuse_its_calls_in_any_mode_and_nothing_else() {
    let deny_bash = [PermissionRule {
        source: base::permission::RuleSource::Session,
        behavior: base::permission::RuleBehavior::Deny,
        tool_name: "Bash".into(),
        rule_content: None,
    }];
    let bash: Arc<dyn Tool> = Arc::new(FakeTool::new("Bash").matches("ls"));
    let input = json!({});
    for mode in [PermissionMode::BypassPermissions, PermissionMode::Default] {
        let ctx = ctx_with_mode(mode);
        let delegate =
            Arc::new(SessionState::new(PathBuf::from("/tmp")).with_permission_mode(mode));
        let request = PermissionRequest::new(
            "Bash",
            Some(&bash),
            &input,
            &ctx.cwd,
            &ctx.session_id,
            &ctx.session,
        )
        .with_ctx(&ctx);
        let delegated = request.for_delegate(base::interface::permission::Delegate::new(
            &delegate, mode, &deny_bash,
        ));
        match Policy::empty().decide(&delegated).await {
            PermissionOutcome::Deny { reason } => {
                assert_eq!(reason.stage, Stage::DelegateRule, "{mode:?}: {reason}")
            }
            other => panic!("{mode:?}: {other:?}"),
        }
        if mode == PermissionMode::BypassPermissions {
            assert!(matches!(
                Policy::empty().decide(&request).await,
                PermissionOutcome::Permit { .. }
            ));
        }
    }
}

/// Input validation is the caller's, once, before it asks: the policy
/// judges what the tool says about the call, not whether the call parses.
#[tokio::test]
async fn the_policy_leaves_input_validation_to_its_caller() {
    let tool = FakeTool::new("Write").invalid();
    let r = Policy::empty()
        .check_with(
            &tool,
            &json!({}),
            &ctx_with_mode(PermissionMode::BypassPermissions),
        )
        .await;
    assert!(
        matches!(
            &r,
            Ok(PermissionOutcome::Permit {
                basis: Basis::Bypass,
                ..
            })
        ),
        "{r:?}"
    );
}

#[tokio::test]
async fn bypass_mode_allows_when_no_rule() {
    let tool = FakeTool::new("Whatever");
    let gate = Policy::empty();
    let d = gate
        .check_with(
            &tool,
            &json!({}),
            &ctx_with_mode(PermissionMode::BypassPermissions),
        )
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }));
}

#[tokio::test]
async fn plan_mode_denies_non_readonly() {
    let tool = FakeTool::new("Write");
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Plan))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
}

#[tokio::test]
async fn the_way_out_of_plan_mode_is_asked_about_or_decided_by_a_rule() {
    let exit = || FakeTool::new("ExitPlanMode").leaves_plan();
    let plan = ctx_with_mode(PermissionMode::Plan);
    let asked = Policy::empty()
        .check_with(&exit(), &json!({}), &plan)
        .await
        .unwrap();
    assert!(
        matches!(asked, PermissionOutcome::Prompt { .. }),
        "{asked:?}"
    );

    let rule = |behavior| PermissionRule {
        source: base::permission::RuleSource::UserSettings,
        behavior,
        tool_name: "ExitPlanMode".into(),
        rule_content: None,
    };
    let allowed = Policy::new(RuleSet::new(vec![rule(
        base::permission::RuleBehavior::Allow,
    )]))
    .check_with(&exit(), &json!({}), &plan)
    .await
    .unwrap();
    assert!(
        matches!(allowed, PermissionOutcome::Permit { .. }),
        "{allowed:?}"
    );
    let denied = Policy::new(RuleSet::new(vec![rule(
        base::permission::RuleBehavior::Deny,
    )]))
    .check_with(&exit(), &json!({}), &plan)
    .await
    .unwrap();
    assert!(
        matches!(denied, PermissionOutcome::Deny { .. }),
        "{denied:?}"
    );

    let write = Policy::empty()
        .check_with(&FakeTool::new("Write"), &json!({}), &plan)
        .await
        .unwrap();
    assert!(matches!(write, PermissionOutcome::Deny { .. }), "{write:?}");
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
        let gate = Policy::empty();
        let d = gate.check_with(&inside, &json!({}), &ctx).await.unwrap();
        assert!(
            matches!(d, PermissionOutcome::Permit { .. }),
            "{mode:?}: {d:?}"
        );
        let d = gate.check_with(&outside, &json!({}), &ctx).await.unwrap();
        assert!(
            matches!(d, PermissionOutcome::Prompt { .. }),
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
    let d = Policy::empty()
        .check_with(&inside, &json!({}), &ctx)
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }), "{d:?}");

    for tool in [
        FakeTool::new("Write").matches("/elsewhere/a.txt"),
        FakeTool::new("Edit").matches("/elsewhere/a.txt"),
        FakeTool::new("Write")
            .matches("/tmp/.agix/hooks/x.sh")
            .own(base::tool::PermissionDecision::allow()),
    ] {
        let d = Policy::empty()
            .check_with(&tool, &json!({}), &ctx)
            .await
            .unwrap();
        assert!(
            matches!(d, PermissionOutcome::Prompt { .. }),
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
    let d = Policy::empty()
        .check_with(
            &tool,
            &json!({}),
            &ctx_with_mode(PermissionMode::AcceptEdits),
        )
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }), "{d:?}");
}

#[tokio::test]
async fn dontask_mode_denies_all_unmatched_tools() {
    let tool = FakeTool::new("Bash");
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
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
    let gate = Policy::new(rules);
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }));
}

#[test]
fn dontask_mode_parses_from_config() {
    let mode: PermissionMode = serde_json::from_str("\"dontAsk\"").unwrap();
    assert_eq!(mode, PermissionMode::DontAsk);
}

#[tokio::test]
async fn default_mode_asks_when_no_rule() {
    let tool = FakeTool::new("Bash");
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Prompt { .. }));
}
