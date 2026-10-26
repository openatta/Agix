//! TodoWriteTool —— "TodoWrite"。
//!
//! 模型自己维护一份 todo 列表，跨 turn 持续。每次 call 一次性替换整个列表。
//! 列表存在本会话的 `SessionState` 上（`ToolContext::session`），会话之间互不可见。

use async_trait::async_trait;
use base::context::TodoStatus;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    #[serde(default)]
    pub active_form: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoWriteInput {
    /// Full replacement set of todos; pass the complete list every call.
    pub todos: Vec<TodoItem>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "TodoWrite"
    }

    fn description(&self) -> &str {
        "Maintain a structured task list for the current turn."
    }

    /// **P3f **: deferred -- only Bash/Read/Edit/ToolSearch 4 eager.
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TodoWriteInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/todo_write.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &Value) -> bool {
        // 改 session 状态而非文件；语义上"只是写 list"，标 read_only=true 让 plan
        // 模式下也能维护 todo（plan 模式只挡破坏性 / 文件类工具）
        true
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<TodoWriteInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) => {
                // 检查 in_progress 数量 ≤ 1
                let in_progress = p
                    .todos
                    .iter()
                    .filter(|t| t.status == TodoStatus::InProgress)
                    .count();
                if in_progress > 1 {
                    return ValidationResult::err(
                        format!(
                            "at most one todo may be in_progress at a time; you have {in_progress}"
                        ),
                        1,
                    );
                }
                // 检查 content 非空
                for (i, t) in p.todos.iter().enumerate() {
                    if t.content.trim().is_empty() {
                        return ValidationResult::err(format!("todo[{i}].content is empty"), 2);
                    }
                }
                ValidationResult::Ok
            }
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        // 改 session 内部状态；不需用户确认
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TodoWriteInput = serde_json::from_value(input)?;
        let total = input.todos.len();
        let (p, i, c) = count_by_status(&input.todos);
        ctx.session.set_todos(
            input
                .todos
                .into_iter()
                .map(|t| base::context::TodoItem {
                    content: t.content,
                    status: t.status,
                    active_form: t.active_form,
                })
                .collect(),
        );
        Ok(ToolResult::text(format!(
            "{total} todos: {c} completed, {i} in_progress, {p} pending"
        )))
    }
}

fn count_by_status(todos: &[TodoItem]) -> (usize, usize, usize) {
    let mut pending = 0;
    let mut in_progress = 0;
    let mut completed = 0;
    for t in todos {
        match t.status {
            TodoStatus::Pending => pending += 1,
            TodoStatus::InProgress => in_progress += 1,
            TodoStatus::Completed => completed += 1,
        }
    }
    (pending, in_progress, completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext::for_test(PathBuf::from("/tmp"))
    }

    async fn write(tool: &TodoWriteTool, ctx: &ToolContext, todos: Value) -> ToolResult {
        tool.call(
            json!({ "todos": todos }),
            ctx.clone(),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn writes_todos() {
        let ctx = ctx();
        let r = write(
            &TodoWriteTool,
            &ctx,
            json!([
                {"content": "do A", "status": "pending", "active_form": "doing A"},
                {"content": "do B", "status": "in_progress", "active_form": "Doing B"},
            ]),
        )
        .await;
        assert!(!r.is_error);
        let stored = ctx.session.todos();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1].status, TodoStatus::InProgress);
        assert_eq!(stored[1].active_form, "Doing B");
    }

    #[tokio::test]
    async fn replaces_entire_list() {
        let ctx = ctx();
        write(
            &TodoWriteTool,
            &ctx,
            json!([{"content": "old", "status": "pending"}]),
        )
        .await;
        write(
            &TodoWriteTool,
            &ctx,
            json!([{"content": "new", "status": "pending", "active_form": "doing new"}]),
        )
        .await;
        let stored = ctx.session.todos();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].content, "new");
    }

    #[tokio::test]
    async fn empty_list_clears_todos() {
        let ctx = ctx();
        write(
            &TodoWriteTool,
            &ctx,
            json!([{"content": "x", "status": "pending"}]),
        )
        .await;
        write(&TodoWriteTool, &ctx, json!([])).await;
        assert!(ctx.session.todos().is_empty());
    }

    #[tokio::test]
    async fn sessions_do_not_see_each_others_todos() {
        let (a, b) = (ctx(), ctx());
        write(
            &TodoWriteTool,
            &a,
            json!([{"content": "only a", "status": "pending"}]),
        )
        .await;
        assert_eq!(a.session.todos().len(), 1);
        assert!(b.session.todos().is_empty());
    }

    #[tokio::test]
    async fn multiple_in_progress_validates_err() {
        let tool = TodoWriteTool;
        let r = tool
            .validate_input(
                &json!({"todos": [
                    {"content": "a", "status": "in_progress", "active_form": "doing a"},
                    {"content": "b", "status": "in_progress", "active_form": "doing b"},
                ]}),
                &ctx(),
            )
            .await;
        match r {
            ValidationResult::Err { .. } => {}
            _ => panic!("expected validation error"),
        }
    }

    #[tokio::test]
    async fn empty_content_validates_err() {
        let tool = TodoWriteTool;
        let r = tool
            .validate_input(
                &json!({"todos": [{"content": "  ", "status": "pending", "active_form": ""}]}),
                &ctx(),
            )
            .await;
        match r {
            ValidationResult::Err { .. } => {}
            _ => panic!("expected validation error"),
        }
    }

    #[tokio::test]
    async fn returns_summary() {
        let tool = TodoWriteTool;
        let r = tool
            .call(
                json!({"todos": [
                    {"content": "a", "status": "completed", "active_form": "done a"},
                    {"content": "b", "status": "completed", "active_form": "done b"},
                    {"content": "c", "status": "in_progress", "active_form": "doing c"},
                    {"content": "d", "status": "pending", "active_form": "doing d"},
                    {"content": "e", "status": "pending", "active_form": "doing e"},
                ]}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("5 todos"));
                assert!(s.contains("2 completed"));
                assert!(s.contains("1 in_progress"));
                assert!(s.contains("2 pending"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn flags_say_readonly_so_plan_mode_allows() {
        let tool = TodoWriteTool;
        assert!(tool.is_read_only(&Value::Null));
    }

    #[tokio::test]
    async fn permissions_allow() {
        let tool = TodoWriteTool;
        let r = tool.check_permissions(&Value::Null, &ctx()).await;
        assert!(matches!(r, PermissionDecision::Allow { .. }));
    }
}
