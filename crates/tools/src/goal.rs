//! `GoalCreate` / `GoalGet` / `GoalUpdate` — the model's handle on the
//! session goal (`base::context::goal`). Which operations a call may perform
//! depends on who started the turn it runs in; sub-agents have none.

use async_trait::async_trait;
use base::context::goal::{GoalAction, GoalError, TurnOrigin};
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalCreateInput {
    /// What the session should achieve, stated so that its completion can be
    /// checked.
    pub objective: String,
    /// How many automatic rounds the goal may use (default 256).
    #[serde(default)]
    pub max_rounds: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalGetInput {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalUpdateAction {
    Edit,
    Pause,
    Resume,
    Complete,
    Blocked,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalUpdateInput {
    pub goal_id: String,
    /// The revision GoalGet returned; a stale one is refused.
    pub revision: u64,
    /// edit, pause and resume need a direct request from the user; complete
    /// and blocked may also come from a round of this goal.
    pub action: GoalUpdateAction,
    /// With `edit`: the new objective.
    #[serde(default)]
    pub objective: Option<String>,
    /// With `edit`: the new round limit.
    #[serde(default)]
    pub max_rounds: Option<u32>,
    /// With `blocked`: what is in the way.
    #[serde(default)]
    pub blocked_reason: Option<String>,
}

/// Who may act on the goal from this call: nobody below the root session.
fn origin(ctx: &ToolContext) -> Result<TurnOrigin, ToolResult> {
    if ctx.agent_depth > 0 {
        return Err(ToolResult::error_text(
            "goals belong to the top-level session; a sub-agent cannot read or change them",
        ));
    }
    Ok(ctx.session.turn_origin())
}

fn refused(e: GoalError) -> ToolResult {
    ToolResult::error_text(e.to_string())
}

fn view_result(view: Value, note: &str) -> ToolResult {
    let mut result = ToolResult::text(format!(
        "{note}{}",
        serde_json::to_string_pretty(&view).unwrap_or_default()
    ));
    result.structured_content = Some(view);
    result
}

pub struct GoalCreateTool;

#[async_trait]
impl Tool for GoalCreateTool {
    fn name(&self) -> &str {
        "GoalCreate"
    }
    fn description(&self) -> &str {
        "Set a goal that keeps this session working across automatic rounds until it is \
         complete. Use it when the user's request is a long-running objective that one turn \
         won't finish, even if they don't say \"goal\"; not for single-turn work."
    }
    fn input_schema(&self) -> Value {
        crate::input_schema::<GoalCreateInput>()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::defaults_for(self)
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: GoalCreateInput = serde_json::from_value(input)?;
        let origin = match origin(&ctx) {
            Ok(o) => o,
            Err(r) => return Ok(r),
        };
        Ok(
            match ctx
                .session
                .goal_create(input.objective, input.max_rounds, origin)
            {
                Ok(view) => view_result(
                    view,
                    "Goal set. Work on it now; when this turn ends, automatic rounds continue \
                     until you mark it complete.\n",
                ),
                Err(e) => refused(e),
            },
        )
    }
}

pub struct GoalGetTool;

#[async_trait]
impl Tool for GoalGetTool {
    fn name(&self) -> &str {
        "GoalGet"
    }
    fn description(&self) -> &str {
        "Read this session's goal, including the id and revision GoalUpdate needs."
    }
    fn input_schema(&self) -> Value {
        crate::input_schema::<GoalGetInput>()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            read_only: true,
            concurrency_safe: true,
            ..ToolSpec::defaults_for(self)
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        _: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        if let Err(r) = origin(&ctx) {
            return Ok(r);
        }
        let view = ctx.session.goal_view();
        Ok(if view.is_null() {
            ToolResult::text("This session has no goal.")
        } else {
            view_result(view, "")
        })
    }
}

pub struct GoalUpdateTool;

#[async_trait]
impl Tool for GoalUpdateTool {
    fn name(&self) -> &str {
        "GoalUpdate"
    }
    fn description(&self) -> &str {
        "Change this session's goal: edit, pause, resume, complete or blocked. Mark complete \
         only when the whole objective is achieved; difficulty or remaining work is not \
         blocked."
    }
    fn input_schema(&self) -> Value {
        crate::input_schema::<GoalUpdateInput>()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::defaults_for(self)
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: GoalUpdateInput = serde_json::from_value(input)?;
        let origin = match origin(&ctx) {
            Ok(o) => o,
            Err(r) => return Ok(r),
        };
        let (action, note) = match input.action {
            GoalUpdateAction::Edit => (
                GoalAction::Edit {
                    objective: input.objective,
                    max_rounds: input.max_rounds,
                },
                "Goal updated.\n",
            ),
            GoalUpdateAction::Pause => (GoalAction::Pause, "Goal paused.\n"),
            GoalUpdateAction::Resume => (
                GoalAction::Resume,
                "Goal resumed; automatic rounds continue after this turn.\n",
            ),
            GoalUpdateAction::Complete => (
                GoalAction::Complete,
                "Goal complete. Write your final message to the user now — what was achieved \
                 and the evidence — and call no more tools.\n",
            ),
            GoalUpdateAction::Blocked => {
                let Some(reason) = input.blocked_reason else {
                    return Ok(ToolResult::error_text(
                        "blocked needs a blocked_reason saying what is in the way",
                    ));
                };
                (
                    GoalAction::Blocked { reason },
                    "Goal blocked. Tell the user what is in the way and what would unblock it, \
                     and call no more tools.\n",
                )
            }
        };
        Ok(
            match ctx
                .session
                .goal_update(&input.goal_id, input.revision, action, origin)
            {
                Ok(view) => view_result(view, note),
                Err(e) => refused(e),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn call(tool: &dyn Tool, input: Value, ctx: &ToolContext) -> ToolResult {
        tool.call(input, ctx.clone(), ProgressSender::noop("t"))
            .await
            .unwrap()
    }

    fn text(r: &ToolResult) -> String {
        match &r.content {
            base::tool::ToolResultContent::Text(s) => s.clone(),
            other => format!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn create_get_and_complete() {
        let ctx = platform::tool_context_for_test("/tmp".into());
        let created = call(&GoalCreateTool, json!({"objective": "tests green"}), &ctx).await;
        assert!(!created.is_error, "{}", text(&created));
        let goal = call(&GoalGetTool, json!({}), &ctx).await;
        let view = goal.structured_content.unwrap();
        assert_eq!(view["phase"], "active");
        assert_eq!(view["armed"], true);
        assert_eq!(view["max_rounds"], 256);

        let done = call(
            &GoalUpdateTool,
            json!({"goal_id": view["id"], "revision": view["revision"], "action": "complete"}),
            &ctx,
        )
        .await;
        assert!(!done.is_error, "{}", text(&done));
        assert!(text(&done).contains("call no more tools"));
        assert_eq!(ctx.session.goal_view()["phase"], "complete");
        assert_eq!(ctx.session.unrecorded_goal_changes().await.changes.len(), 2);
    }

    #[tokio::test]
    async fn a_sub_agent_has_no_goal() {
        let mut ctx = platform::tool_context_for_test("/tmp".into());
        ctx.agent_depth = 1;
        let r = call(&GoalCreateTool, json!({"objective": "x"}), &ctx).await;
        assert!(r.is_error);
        assert!(ctx.session.goal_view().is_null());
    }

    #[tokio::test]
    async fn an_automatic_round_cannot_pause() {
        let ctx = platform::tool_context_for_test("/tmp".into());
        call(&GoalCreateTool, json!({"objective": "x"}), &ctx).await;
        let round = ctx.session.goal_next_round().unwrap();
        ctx.session
            .set_turn_origin(TurnOrigin::from_turn_id(&round.turn_id));
        let view = ctx.session.goal_view();
        let r = call(
            &GoalUpdateTool,
            json!({"goal_id": view["id"], "revision": view["revision"], "action": "pause"}),
            &ctx,
        )
        .await;
        assert!(r.is_error);
        assert!(text(&r).contains("only a direct request from the user"));
    }

    #[tokio::test]
    async fn blocked_needs_a_reason() {
        let ctx = platform::tool_context_for_test("/tmp".into());
        call(&GoalCreateTool, json!({"objective": "x"}), &ctx).await;
        let view = ctx.session.goal_view();
        let r = call(
            &GoalUpdateTool,
            json!({"goal_id": view["id"], "revision": view["revision"], "action": "blocked"}),
            &ctx,
        )
        .await;
        assert!(r.is_error);
    }
}
