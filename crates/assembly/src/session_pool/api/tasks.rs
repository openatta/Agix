//! 会话的后台任务与上下文操作的结果：任务清单、一段输出、投递消息、起子 Agent，
//! 以及去掉或改写消息。

use base::context::{TaskInfo, TaskStatus};
use schemars::JsonSchema;
use serde::Serialize;

pub use runtime::agent::ContextReport;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionTasks {
    /// Whether the engine is in memory; when it is not, only scheduled jobs
    /// are listed.
    pub active: bool,
    pub tasks: Vec<TaskInfo>,
}

/// Part of what one task has produced. Offsets count every byte the work
/// ever printed.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TaskOutput {
    pub output: String,
    pub truncated: bool,
    pub status: TaskStatus,
    pub from_byte: u64,
    pub next_byte: u64,
    pub dropped: u64,
    /// The output has ended; `next_byte` will not grow.
    #[serde(rename = "final")]
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct MessageDelivery {
    pub delivered: bool,
    /// Why not, when not delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AgentStarted {
    pub task_id: String,
    /// The sidechain session the agent runs in.
    pub agent_session_id: String,
}
