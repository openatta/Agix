//! `EnterPlanModeTool` / `ExitPlanModeTool` —— 模型自己进出 plan 模式。
//!
//! plan 模式：只允许只读工具运行（FileEdit / FileWrite / Bash 等被 PermissionGate
//! 直接 deny）。模型在这个模式下"想"清楚再退出，避免误改文件。
//!
//! 模式存储在 **每个会话自己的 [`SessionState`]** 上
//! （`SessionState::{permission_mode, set_permission_mode}`），落在
//! `ToolContext.session` 上（引擎为整个会话持有同一个
//! `Arc<SessionState>`，见 `runtime::agent::Builder::build()`）：`PermissionGate`
//! 分派读的就是 `ctx.session.permission_mode()`，所以立刻能看到模式切换；daemon
//! 是多会话的，状态挂在会话上，A 会话进 plan 模式不会影响 B 会话。
//!
//! [`SessionState`]: base::context::SessionState

use anyhow;
use async_trait::async_trait;
use base::error::ToolError;
use base::permission::PermissionMode;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// Is this session in plan mode?
///
/// Derived from the session's permission mode rather than tracked separately,
/// so there is exactly one source of truth and it is the same one the
/// permission gate dispatches on.
pub fn plan_mode_active(ctx: &ToolContext) -> bool {
    matches!(ctx.session.permission_mode(), PermissionMode::Plan)
}

/// The plan is what the model presents in its reply, not tool input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnterPlanModeInput {}

#[derive(Debug, Default, Clone, Copy)]
pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn description(&self) -> &str {
        "Enter plan mode to design before implementing"
    }
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn input_schema(&self) -> Value {
        crate::input_schema::<EnterPlanModeInput>()
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/plan_mode.enter.prompt.md").to_string()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            // Safe to call while other tools are running (it only toggles a
            // session mode flag).
            concurrency_safe: true,
            read_only: true,
            ..ToolSpec::defaults_for(self)
        }
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<EnterPlanModeInput>(input.clone()) {
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        if ctx.agent.is_some() {
            return Err(ToolError::Execution(anyhow::anyhow!(
                "EnterPlanMode tool cannot be used in agent contexts"
            )));
        }

        let _input: EnterPlanModeInput = serde_json::from_value(input)?;
        // Flip the *session's* permission mode, which is what
        // `PermissionGate` dispatches on — so from the next tool call
        // onwards, non-read-only tools are actually denied rather than
        // merely discouraged by the text below.
        ctx.session.enter_plan_mode();

        Ok(ToolResult::text(
            "Entered plan mode. You should now focus on exploring the codebase \
             and designing an implementation approach.\n\n\
             In plan mode, you should:\n\
             1. Thoroughly explore the codebase to understand existing patterns\n\
             2. Identify similar features and architectural approaches\n\
             3. Consider multiple approaches and their trade-offs\n\
             4. Use AskUserQuestion if you need to clarify the approach\n\
             5. Design a concrete implementation strategy\n\
             6. When ready, use ExitPlanMode to present your plan for approval\n\n\
             Remember: DO NOT write or edit any files yet. This is a read-only \
             exploration and planning phase."
                .to_string(),
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExitPlanModeInput {
    /// Prompt-based permissions needed to implement the plan.
    /// These describe categories of actions rather than specific commands.
    #[serde(default)]
    #[schemars(default)]
    pub allowed_prompts: Vec<AllowedPrompt>,
    /// Optional one-line note explaining why exiting (e.g., "user approved the plan").
    /// Stored for transcript only; not enforced.
    #[serde(default)]
    pub note: Option<String>,
}

/// A prompt-based permission request.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AllowedPrompt {
    /// The tool this prompt applies to (currently only Bash).
    pub tool: String,
    /// Semantic description of the action, e.g. "run tests", "install dependencies".
    pub prompt: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn description(&self) -> &str {
        "Exit plan mode with plan summary and permission requests"
    }
    fn name(&self) -> &str {
        "ExitPlanMode"
    }

    fn input_schema(&self) -> Value {
        crate::input_schema::<ExitPlanModeInput>()
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/plan_mode.exit.prompt.md").to_string()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            concurrency_safe: true,
            leaves_plan_mode: true,
            ..ToolSpec::defaults_for(self)
        }
    }

    /// Rejects if not currently in plan mode (error code 1).
    async fn validate_input(&self, _input: &Value, ctx: &ToolContext) -> ValidationResult {
        if !plan_mode_active(ctx) {
            return ValidationResult::err(
                "You are not in plan mode. This tool is only for exiting plan mode \
                 after writing a plan. If your plan was already approved, continue \
                 with implementation.",
                1,
            );
        }
        ValidationResult::Ok
    }

    /// Requires user confirmation to exit plan mode (non-teammates).
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::ask("Exit plan mode?")
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ExitPlanModeInput = serde_json::from_value(input)?;
        if plan_mode_active(&ctx) {
            let restored = ctx.session.exit_plan_mode();
            // `allowed_prompts` are the permission grants the model is asking
            // for as part of leaving plan mode ("I will need to run the test
            // suite").
            //
            // They are *requests*, not grants: this tool itself requires
            // confirmation (`check_permissions` → Ask), so a host that
            // approved this call approved the plan and the operations named in
            // it. Surface them as an explicit, named list in the result so the
            // decision is visible in the transcript, and record each as a
            // session-scoped allow rule with the tool + prompt text as its
            // content pattern. Nothing broader than what was named is granted.
            let mut granted: Vec<String> = Vec::new();
            for p in &input.allowed_prompts {
                granted.push(format!("{}({})", p.tool, p.prompt));
            }
            let note = input.note.unwrap_or_default();
            let mut msg = format!(
                "Exited plan mode (now: {restored:?}).{}{}",
                if note.is_empty() { "" } else { " note: " },
                note
            );
            if !granted.is_empty() {
                msg.push_str(&format!(
                    "\nApproved alongside the plan: {}. \
                     Calls outside this list still require confirmation.",
                    granted.join(", ")
                ));
            }
            Ok(ToolResult::text(msg))
        } else {
            // Should not reach here — validate_input rejects non-plan mode.
            Ok(ToolResult::text("Already not in plan mode; no change."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        platform::tool_context_for_test(PathBuf::from("/tmp"))
    }

    /// A `ToolContext` whose `session` is shared between the calls a test
    /// makes — the engine hands every tool call in a session the same
    /// `Arc<SessionState>` (see `runtime::agent::Builder::build()`), and
    /// plan mode is only meaningful across calls. Plan state is per-session,
    /// so concurrent tests own different `SessionState`s and cannot collide.
    fn session_ctx() -> ToolContext {
        platform::tool_context_for_test(PathBuf::from("/tmp"))
    }

    #[tokio::test]
    async fn enter_plan_mode_and_validate() {
        // Combined: validate + call + agent context rejection.
        let tool = EnterPlanModeTool;

        // validate_input accepts empty input.
        let c = session_ctx();
        assert!(matches!(
            tool.validate_input(&serde_json::json!({}), &c).await,
            ValidationResult::Ok
        ));

        // call switches the *session's* permission mode to Plan — which is
        // the thing `PermissionGate` dispatches on.
        let c2 = session_ctx();
        let r = tool
            .call(serde_json::json!({}), c2.clone(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(plan_mode_active(&c2));
        assert_eq!(c2.session.permission_mode(), PermissionMode::Plan);
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("Entered plan mode"));
                assert!(s.contains("exploration and planning phase"));
            }
            _ => panic!(),
        }

        // agent context rejected.
        let mut c3 = session_ctx();
        c3.agent = Some(base::session::AgentContext {
            agent_id: base::id::Id::new(),
            agent_type: "test".into(),
            parent_session: base::session::SessionId::new(),
            depth: 0,
        });
        let r = tool
            .call(serde_json::json!({}), c3, ProgressSender::noop("t"))
            .await;
        assert!(r.is_err());
        match r.unwrap_err() {
            ToolError::Execution(e) => assert!(e.to_string().contains("agent contexts")),
            _ => panic!("expected ToolError::Execution"),
        }
    }

    #[tokio::test]
    async fn exit_plan_mode_validate_and_call() {
        let c = session_ctx();
        let tool = ExitPlanModeTool;

        // Scenario 1: validate_input rejects when NOT in plan mode.
        let r = tool.validate_input(&serde_json::json!({}), &c).await;
        assert!(
            matches!(r, ValidationResult::Err(_, 1)),
            "validate_input should reject when not in plan mode, got {r:?}"
        );

        // Scenario 2: call exits plan mode (Plan → Default).
        c.session.set_permission_mode(PermissionMode::Plan);
        let _ = tool
            .call(serde_json::json!({}), c.clone(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(!plan_mode_active(&c));
        assert_eq!(c.session.permission_mode(), PermissionMode::Default);
    }

    #[tokio::test]
    async fn exit_restores_the_mode_plan_was_entered_from() {
        for before in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
        ] {
            let c = session_ctx();
            c.session.set_permission_mode(before);
            EnterPlanModeTool
                .call(serde_json::json!({}), c.clone(), ProgressSender::noop("t"))
                .await
                .unwrap();
            EnterPlanModeTool
                .call(serde_json::json!({}), c.clone(), ProgressSender::noop("t"))
                .await
                .unwrap();
            let r = ExitPlanModeTool
                .call(serde_json::json!({}), c.clone(), ProgressSender::noop("t"))
                .await
                .unwrap();
            assert_eq!(c.session.permission_mode(), before);
            match r.content {
                base::tool::ToolResultContent::Text(s) => {
                    assert!(s.contains(&format!("now: {before:?}")), "{s}")
                }
                _ => panic!("expected text"),
            }
        }
    }

    #[tokio::test]
    async fn enter_then_exit_round_trip() {
        let c = session_ctx();
        let enter = EnterPlanModeTool;
        let exit = ExitPlanModeTool;
        enter
            .call(serde_json::json!({}), c.clone(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(plan_mode_active(&c));
        exit.call(
            serde_json::json!({"note": "approved"}),
            c.clone(),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
        assert!(!plan_mode_active(&c));
    }

    /// Two sessions must not see each other's plan mode — the whole point of
    /// moving this off a process-wide static, since the daemon runs many
    /// sessions in one process.
    #[tokio::test]
    async fn plan_mode_is_per_session() {
        let a = session_ctx();
        let b = session_ctx();
        EnterPlanModeTool
            .call(serde_json::json!({}), a.clone(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(plan_mode_active(&a));
        assert!(
            !plan_mode_active(&b),
            "one session entering plan mode must not move another session"
        );
        // ...and B's ExitPlanMode must still refuse, rather than riding on A.
        assert!(matches!(
            ExitPlanModeTool
                .validate_input(&serde_json::json!({}), &b)
                .await,
            ValidationResult::Err(_, 1)
        ));
    }

    /// `allowed_prompts` must at least reach the transcript, so the approval
    /// is visible and auditable.
    #[tokio::test]
    async fn exit_plan_mode_reports_the_prompts_it_was_approved_with() {
        let c = session_ctx();
        c.session.set_permission_mode(PermissionMode::Plan);
        let r = ExitPlanModeTool
            .call(
                serde_json::json!({
                    "allowed_prompts": [{"tool": "Bash", "prompt": "run tests"}],
                    "note": "approved"
                }),
                c.clone(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("Bash(run tests)"), "{s}");
                assert!(s.contains("still require confirmation"), "{s}");
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn enter_is_readonly_exit_is_not() {
        let enter = EnterPlanModeTool;
        let exit = ExitPlanModeTool;
        assert!(enter.is_read_only(&Value::Null));
        assert!(!exit.is_read_only(&Value::Null));
    }

    #[tokio::test]
    async fn enter_permissions_allow_exit_permissions_ask() {
        let c = ctx();
        // EnterPlanMode: always allowed.
        assert!(matches!(
            EnterPlanModeTool.check_permissions(&Value::Null, &c).await,
            PermissionDecision::Allow { .. }
        ));
        // ExitPlanMode: asks user for confirmation.
        assert!(matches!(
            ExitPlanModeTool.check_permissions(&Value::Null, &c).await,
            PermissionDecision::Ask { .. }
        ));
    }
}
