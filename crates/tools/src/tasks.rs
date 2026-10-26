//! Task 5 工具 —— 结构化任务追踪（subject + description + status + 生成 ID）。
//!
//! 与 TodoWrite 的区别：
//! - TodoWrite 一次性整列表替换，无 ID，无 description；适合临时清单
//! - Task 族有稳定 ID + subject + description + activeForm + 创建/更新时间；
//!   适合"派发给 sub-agent + 长时间追踪"的场景
//!
//! 任务表存在本会话的 `SessionState` 上（`ToolContext::session`）：会话之间、
//! 同一进程里的几个会话池之间都互不可见，会话结束就随之释放。只在内存里，
//! 不跨重启。一个会话一张平铺的表：团队成员的 `owner` 字段只是约定，不是边界。

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use base::context::SessionState;

/// Callers that need to read then write should do both inside one `f`, not
/// two separate calls.
fn with_session_tasks<R>(session: &SessionState, f: impl FnOnce(&mut Vec<TaskEntry>) -> R) -> R {
    session.with_tasks(f)
}

fn remove_task_from_store(session: &SessionState, task_id: &str) -> bool {
    with_session_tasks(session, |tasks| {
        if let Some(pos) = tasks.iter().position(|t| t.id == task_id) {
            tasks.remove(pos);
            true
        } else {
            false
        }
    })
}

fn update_task_in_store(
    session: &SessionState,
    task_id: &str,
    f: impl FnOnce(&mut TaskEntry),
) -> Option<TaskEntry> {
    with_session_tasks(session, |tasks| {
        if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
            f(t);
            Some(t.clone())
        } else {
            None
        }
    })
}

/// Task 状态。与 TodoStatus 不重叠 —— 这里有 Cancelled/Deleted，TodoStatus 没有。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
    Deleted,
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Deleted
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskEntry {
    /// 系统生成的稳定 ID（BASE58 UUID v4 短形式 —— 与 Atta 顶层 ID 铁律对齐）
    pub id: String,
    pub subject: String,
    pub description: String,
    /// 现在时态描述（例如 "Running tests"）；UI spinner 用
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "activeForm")]
    pub active_form: Option<String>,
    pub status: TaskStatus,
    /// Agent that owns/claimed this task
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Task IDs that this task blocks (depends that must wait for this one)
    #[serde(default)]
    pub blocks: Vec<String>,
    /// Task IDs that block this task (must be completed first)
    #[serde(default)]
    #[serde(alias = "blockedBy")]
    pub blocked_by: Vec<String>,
    /// Arbitrary metadata
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// epoch 秒；测试稳定性考虑只看相对顺序
    pub created_at: i64,
    pub updated_at: i64,
}

/// 生成短 ID —— 简单 base58(uuid v4)；agix 里没引 uuid crate（避免膨胀），
/// 用 std::time + 计数器自滚（够给 in-memory 任务用，不需要全局唯一）
fn new_task_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 用 base36 编码（0-9a-z）—— 比 base58 简单，纯字母数字
    fn b36(mut x: u64) -> String {
        if x == 0 {
            return "0".into();
        }
        let chars: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut buf = Vec::new();
        while x > 0 {
            buf.push(chars[(x % 36) as usize]);
            x /= 36;
        }
        buf.reverse();
        String::from_utf8(buf).unwrap()
    }
    format!("task_{}-{}", b36(secs), b36(n))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---- TaskCreate ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCreateInput {
    /// Brief title (imperative form; e.g. "Run tests")
    #[serde(alias = "name")]
    pub subject: String,
    /// Full description of what needs doing
    pub description: String,
    /// Present continuous form for spinner; e.g. "Running tests"
    #[serde(default)]
    pub active_form: Option<String>,
    /// Agent to assign ownership to
    #[serde(default)]
    pub owner: Option<String>,
    /// Task IDs that this task blocks
    #[serde(default)]
    pub blocks: Vec<String>,
    /// Task IDs that block this task
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

pub struct TaskCreateTool;

#[async_trait]
impl Tool for TaskCreateTool {
    fn description(&self) -> &str {
        "Create a task in the task list"
    }
    fn name(&self) -> &str {
        "TaskCreate"
    }

    fn is_deferred(&self) -> bool {
        false // eager — model needs to see task tools without ToolSearch discovery
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TaskCreateInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/task_create.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false // 写 SessionState
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<TaskCreateInput>(input.clone()) {
            Ok(p) if p.subject.trim().is_empty() => {
                ValidationResult::err("subject must not be empty", 1)
            }
            Ok(p) if p.description.trim().is_empty() => {
                ValidationResult::err("description must not be empty", 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
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
        let input: TaskCreateInput = serde_json::from_value(input)?;
        let now = now_secs();
        let entry = TaskEntry {
            id: new_task_id(),
            subject: input.subject,
            description: input.description,
            active_form: input.active_form,
            owner: input.owner,
            blocks: input.blocks,
            blocked_by: input.blocked_by,
            metadata: None,
            status: TaskStatus::Pending,
            created_at: now,
            updated_at: now,
        };
        with_session_tasks(&ctx.session, |tasks| tasks.push(entry.clone()));
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(format!(
                "Task created: id={} subject={}",
                entry.id, entry.subject
            )),
            is_error: false,
            structured_content: Some(json!({"id": entry.id, "task": entry})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

// ---- TaskGet ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskGetInput {
    pub task_id: String,
}

pub struct TaskGetTool;

#[async_trait]
impl Tool for TaskGetTool {
    fn description(&self) -> &str {
        "Retrieve a task by ID from the task list"
    }
    fn name(&self) -> &str {
        "TaskGet"
    }

    fn is_deferred(&self) -> bool {
        false // eager — model needs to see task tools without ToolSearch discovery
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TaskGetInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/task_get.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true // read-only
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<TaskGetInput>(input.clone()) {
            Ok(p) if p.task_id.trim().is_empty() => {
                ValidationResult::err("task_id must not be empty", 1)
            }
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
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TaskGetInput = serde_json::from_value(input)?;
        let task: Option<TaskEntry> = with_session_tasks(&ctx.session, |tasks| {
            tasks.iter().find(|t| t.id == input.task_id).cloned()
        });
        match task {
            Some(t) => Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(
                    serde_json::to_string_pretty(&t).unwrap_or_default(),
                ),
                is_error: false,
                structured_content: Some(json!(t)),
                mcp_meta: None,
                new_messages: Some(vec![]),
            }),
            None => Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "no task with id={}",
                    input.task_id
                )),
                is_error: true,
                structured_content: None,
                mcp_meta: None,
                new_messages: Some(vec![]),
            }),
        }
    }
}

// ---- TaskList ----

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct TaskListInput {
    /// Filter by status; None = all
    #[serde(default)]
    pub status: Option<TaskStatus>,
}

pub struct TaskListTool;

#[async_trait]
impl Tool for TaskListTool {
    fn description(&self) -> &str {
        "List all tasks in the task list"
    }
    fn name(&self) -> &str {
        "TaskList"
    }

    fn is_deferred(&self) -> bool {
        false // eager — model needs to see task tools without ToolSearch discovery
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TaskListInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/task_list.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
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
        let input: TaskListInput = serde_json::from_value(input).unwrap_or_default();
        let all: Vec<TaskEntry> = with_session_tasks(&ctx.session, |tasks| tasks.clone());
        let filtered: Vec<TaskEntry> = match input.status {
            Some(s) => all.into_iter().filter(|t| t.status == s).collect(),
            None => all,
        };
        let summary = filtered
            .iter()
            .map(|t| {
                let owner_tag = t
                    .owner
                    .as_ref()
                    .map(|o| format!(" @{o}"))
                    .unwrap_or_default();
                let blocker_tag = if !t.blocked_by.is_empty() {
                    format!(" [blocked by {}]", t.blocked_by.join(","))
                } else {
                    String::new()
                };
                format!(
                    "  · [{:?}]{}{} {} — {}",
                    t.status, owner_tag, blocker_tag, t.id, t.subject
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let body = if filtered.is_empty() {
            "(no tasks)".to_string()
        } else {
            format!("{} task(s):\n{summary}", filtered.len())
        };
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: false,
            structured_content: Some(json!(filtered)),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

// ---- TaskUpdate ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskUpdateInput {
    pub task_id: String,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default, alias = "name")]
    pub subject: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    #[serde(alias = "activeForm")]
    pub active_form: Option<String>,
    #[serde(default)]
    pub owner: Option<Option<String>>,
    #[serde(default)]
    pub blocks: Option<Vec<String>>,
    #[serde(default)]
    #[serde(alias = "blockedBy")]
    pub blocked_by: Option<Vec<String>>,
}

pub struct TaskUpdateTool;

#[async_trait]
impl Tool for TaskUpdateTool {
    fn description(&self) -> &str {
        "Update task status, description, or dependencies"
    }
    fn name(&self) -> &str {
        "TaskUpdate"
    }

    fn is_deferred(&self) -> bool {
        false // eager — model needs to see task tools without ToolSearch discovery
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TaskUpdateInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/task_update.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<TaskUpdateInput>(input.clone()) {
            Ok(p) if p.task_id.trim().is_empty() => {
                ValidationResult::err("task_id must not be empty", 1)
            }
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
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TaskUpdateInput = serde_json::from_value(input)?;

        // Blocking check: when transitioning to InProgress, verify blockers are resolved
        if matches!(input.status, Some(TaskStatus::InProgress)) {
            if let Some(task) = with_session_tasks(&ctx.session, |tasks| {
                tasks.iter().find(|t| t.id == input.task_id).cloned()
            }) {
                if !task.blocked_by.is_empty() {
                    let all_tasks = with_session_tasks(&ctx.session, |tasks| tasks.clone());
                    let unresolved: Vec<String> = task
                        .blocked_by
                        .into_iter()
                        .filter(|bid| {
                            all_tasks
                                .iter()
                                .any(|t| t.id == *bid && t.status != TaskStatus::Completed)
                        })
                        .collect();
                    if !unresolved.is_empty() {
                        return Ok(ToolResult {
                            content: base::tool::ToolResultContent::Text(format!(
                                "Cannot start task {}: blocked by unresolved task(s): {}",
                                input.task_id,
                                unresolved.join(", ")
                            )),
                            is_error: true,
                            structured_content: Some(
                                json!({"task_id": input.task_id, "blocked_by": unresolved}),
                            ),
                            mcp_meta: None,
                            new_messages: Some(vec![]),
                        });
                    }
                }
            }
        }

        // Handle Deleted status specially: remove from store
        if matches!(input.status, Some(TaskStatus::Deleted)) {
            let removed = remove_task_from_store(&ctx.session, &input.task_id);
            match removed {
                true => Ok(ToolResult {
                    content: base::tool::ToolResultContent::Text(format!(
                        "Deleted task {}",
                        input.task_id
                    )),
                    is_error: false,
                    structured_content: Some(json!({"task_id": input.task_id, "deleted": true})),
                    mcp_meta: None,
                    new_messages: Some(vec![]),
                }),
                false => Ok(ToolResult {
                    content: base::tool::ToolResultContent::Text(format!(
                        "no task with id={}",
                        input.task_id
                    )),
                    is_error: true,
                    structured_content: None,
                    mcp_meta: None,
                    new_messages: Some(vec![]),
                }),
            }
        } else {
            let updated: Option<TaskEntry> =
                update_task_in_store(&ctx.session, &input.task_id, |t: &mut TaskEntry| {
                    if let Some(s) = input.status {
                        t.status = s;
                    }
                    if let Some(s) = input.subject {
                        t.subject = s;
                    }
                    if let Some(d) = input.description {
                        t.description = d;
                    }
                    if let Some(a) = input.active_form {
                        t.active_form = Some(a);
                    }
                    if let Some(o) = input.owner {
                        t.owner = o;
                    }
                    if let Some(b) = input.blocks {
                        t.blocks = b;
                    }
                    if let Some(b) = input.blocked_by {
                        t.blocked_by = b;
                    }
                    t.updated_at = now_secs();
                });
            match updated {
                Some(t) => Ok(ToolResult {
                    content: base::tool::ToolResultContent::Text(format!(
                        "Updated task {}: status={:?}, subject={}",
                        t.id, t.status, t.subject
                    )),
                    is_error: false,
                    structured_content: Some(json!(t)),
                    mcp_meta: None,
                    new_messages: Some(vec![]),
                }),
                None => Ok(ToolResult {
                    content: base::tool::ToolResultContent::Text(format!(
                        "no task with id={}",
                        input.task_id
                    )),
                    is_error: true,
                    structured_content: None,
                    mcp_meta: None,
                    new_messages: Some(vec![]),
                }),
            }
        }
    }
}

// ---- TaskStop ----

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskStopInput {
    pub task_id: String,
}

pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn description(&self) -> &str {
        "Stop a running background task"
    }
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn is_deferred(&self) -> bool {
        false // eager — model needs to see task tools without ToolSearch discovery
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TaskStopInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/task_stop.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<TaskStopInput>(input.clone()) {
            Ok(p) if p.task_id.trim().is_empty() => {
                ValidationResult::err("task_id must not be empty", 1)
            }
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
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TaskStopInput = serde_json::from_value(input)?;

        // : 优先 cancel running task（后台 sub-agent）—— 触发 cancel token
        if ctx
            .running_tasks
            .as_ref()
            .map(|rt| rt.cancel(&input.task_id))
            .unwrap_or(false)
        {
            // 也把 status 标 Cancelled（防 sub-agent 还没收到 token 信号时
            // TaskOutput 显示 Running）
            if let Some((_, _, _)) = ctx
                .running_tasks
                .as_ref()
                .and_then(|rt| rt.find(&input.task_id))
            {
                // status already returned from RunningTasksCallback; no mutation needed
            }
            return Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "Cancel signal sent to background task {}",
                    input.task_id
                )),
                is_error: false,
                structured_content: Some(json!({"task_id": input.task_id, "background": true})),
                mcp_meta: None,
                new_messages: Some(vec![]),
            });
        }

        // 否则走 declarative task 路径
        let updated: Option<TaskEntry> =
            update_task_in_store(&ctx.session, &input.task_id, |t: &mut TaskEntry| {
                t.status = TaskStatus::Cancelled;
                t.updated_at = now_secs();
            });
        match updated {
            Some(t) => Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "Cancelled task {}: {}",
                    t.id, t.subject
                )),
                is_error: false,
                structured_content: Some(json!(t)),
                mcp_meta: None,
                new_messages: Some(vec![]),
            }),
            None => Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "no task with id={}",
                    input.task_id
                )),
                is_error: true,
                structured_content: None,
                mcp_meta: None,
                new_messages: Some(vec![]),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::ProgressSender;

    fn ctx_for(session_id: &str) -> ToolContext {
        let mut ctx = ToolContext::for_test(std::env::temp_dir());
        ctx.session_id = session_id.to_string();
        ctx
    }

    #[tokio::test]
    async fn task_lists_are_isolated_by_session() {
        let session_a = ctx_for("session-a");
        let create_input = serde_json::json!({
            "subject": "only in session a",
            "description": "d",
        });
        TaskCreateTool
            .call(
                create_input,
                session_a.clone(),
                ProgressSender::noop("t"),
            )
            .await
            .expect("create should succeed");

        let list_a = TaskListTool
            .call(
                serde_json::json!({}),
                session_a.clone(),
                ProgressSender::noop("t"),
            )
            .await
            .expect("list should succeed");
        let text_a = match list_a.content {
            base::tool::ToolResultContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(
            text_a.contains("only in session a"),
            "session-a's own task should be visible to session-a: {text_a}"
        );

        let list_b = TaskListTool
            .call(
                serde_json::json!({}),
                ctx_for("session-b"),
                ProgressSender::noop("t"),
            )
            .await
            .expect("list should succeed");
        let text_b = match list_b.content {
            base::tool::ToolResultContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(
            !text_b.contains("only in session a"),
            "session-b must not see session-a's task, got: {text_b}"
        );
        assert!(
            text_b.contains("no tasks"),
            "session-b should have an empty task list, got: {text_b}"
        );
    }
}
