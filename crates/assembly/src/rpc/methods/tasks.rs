//! 会话后台任务方法（`session.tasks` / `taskOutput` / `stopTask` /
//! `messageTask`）的参数与线上回执。`session.startAgent` 的参数见
//! [`crate::rpc::StartAgentParams`]。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TaskRef {
    pub session_id: String,
    pub task_id: String,
}

/// One of `tail_bytes` and `from_byte`; neither: the last 8 KiB.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TaskOutputParams {
    pub session_id: String,
    pub task_id: String,
    /// At most 256 KiB.
    #[serde(default)]
    pub tail_bytes: Option<u64>,
    #[serde(default)]
    pub from_byte: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Stopped {
    /// `false`: no such task, or it had already ended.
    pub stopped: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MessageTaskParams {
    pub session_id: String,
    pub task_id: String,
    pub message: String,
}
