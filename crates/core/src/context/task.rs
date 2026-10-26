//! 后台任务 + todo 条目类型。

/// P1-4: Task type enum — categorises all background/async task kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    /// Shell command executed as a background process.
    Shell,
    /// Sub-agent spawned by AgentTool.
    Agent,
    /// In-process teammate (team/swarm mode).
    Teammate,
    /// Cron/scheduled job.
    CronJob,
    /// Dream/background thinking task.
    Dream,
    /// Workflow orchestration task.
    Workflow,
    /// MCP monitor task.
    Monitor,
}

impl TaskType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskType::Shell => "shell",
            TaskType::Agent => "agent",
            TaskType::Teammate => "teammate",
            TaskType::CronJob => "cron",
            TaskType::Dream => "dream",
            TaskType::Workflow => "workflow",
            TaskType::Monitor => "monitor",
        }
    }

    /// Prefix used in task IDs for quick type recognition.
    pub fn id_prefix(&self) -> &'static str {
        match self {
            TaskType::Shell => "sh",
            TaskType::Agent => "ag",
            TaskType::Teammate => "tm",
            TaskType::CronJob => "cj",
            TaskType::Dream => "dr",
            TaskType::Workflow => "wf",
            TaskType::Monitor => "mo",
        }
    }
}

/// P1-4: Strongly-typed task identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new(task_type: TaskType) -> Self {
        let uuid = uuid::Uuid::new_v4();
        let short = &uuid.to_string()[..8];
        TaskId(format!("{}-{}", task_type.id_prefix(), short))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 一件属于会话、在会话里继续跑的工作：子 Agent、团队成员、执行中的 Shell……
/// output / events_log / status 由跑它的那段代码追加；TaskOutput 工具与宿主的
/// `session.taskOutput` 读快照；
/// TaskStop 与宿主的 `session.stopTask` 触发 cancel token。
#[derive(Debug)]
pub struct RunningTask {
    pub task_id: String,
    /// P1-4: Optional typed task identifier.
    pub typed_id: Option<TaskId>,
    /// P1-4: Task category.
    pub task_type: TaskType,
    /// 累积的 assistant 文本（流式 delta append）
    pub output: std::sync::Mutex<String>,
    /// 工具调用与结果的可读日志（每行一条）
    pub events_log: std::sync::Mutex<Vec<String>>,
    pub status: std::sync::Mutex<RunningStatus>,
    /// What the work runs under: cancelling it stops the work.
    pub cancel: tokio_util::sync::CancellationToken,
    /// Unix millis.
    pub started_ms: i64,
    /// Unix millis; `None` while it runs.
    pub ended_ms: std::sync::Mutex<Option<i64>>,
    /// What a viewer needs to tell this task from another: the command, the
    /// sub-agent's type, the tool call it belongs to.
    pub detail: std::sync::Mutex<serde_json::Value>,
    /// Whether its start was announced; a task that never was ends silently.
    pub announced: std::sync::atomic::AtomicBool,
    /// Whether `append_output` has let the head of `output` go.
    pub output_dropped: std::sync::atomic::AtomicBool,
}

impl RunningTask {
    /// Create a new running task with defaults.
    pub fn new(task_id: String, task_type: TaskType) -> Self {
        Self::with_cancel(
            task_id,
            task_type,
            tokio_util::sync::CancellationToken::new(),
        )
    }

    /// A running task whose work runs under `cancel`.
    pub fn with_cancel(
        task_id: String,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            task_id,
            typed_id: None,
            task_type,
            output: std::sync::Mutex::new(String::new()),
            events_log: std::sync::Mutex::new(Vec::new()),
            status: std::sync::Mutex::new(RunningStatus::Running),
            cancel,
            started_ms: unix_ms(),
            ended_ms: std::sync::Mutex::new(None),
            detail: std::sync::Mutex::new(serde_json::Value::Null),
            announced: std::sync::atomic::AtomicBool::new(false),
            output_dropped: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The most of a streamed output (a shell command's, a monitor's) a task
    /// keeps; beyond it the oldest goes.
    pub const OUTPUT_KEPT_BYTES: usize = 256 * 1024;

    /// Add to `output` as the work produces it, keeping its last
    /// [`Self::OUTPUT_KEPT_BYTES`].
    pub fn append_output(&self, text: &str) {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        output.push_str(text);
        if output.len() > Self::OUTPUT_KEPT_BYTES {
            let cut = ceil_char_boundary(&output, output.len() - Self::OUTPUT_KEPT_BYTES);
            output.drain(..cut);
            self.output_dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The last `max_bytes` of `output`, and whether anything before them is
    /// missing.
    pub fn output_tail(&self, max_bytes: usize) -> (String, bool) {
        let output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = self
            .output_dropped
            .load(std::sync::atomic::Ordering::SeqCst);
        if output.len() <= max_bytes {
            return (output.clone(), dropped);
        }
        let cut = ceil_char_boundary(&output, output.len() - max_bytes);
        (output[cut..].to_string(), true)
    }

    /// The wire shape of this task (`session.tasks`, `task_*` events).
    pub fn info(&self) -> serde_json::Value {
        let status = self
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (status, error) = match status {
            RunningStatus::Running => ("running", None),
            RunningStatus::Completed => ("done", None),
            RunningStatus::Cancelled => ("stopped", None),
            RunningStatus::Failed(e) => ("failed", Some(e)),
        };
        let mut info = serde_json::json!({
            "task_id": self.task_id,
            "kind": self.task_type.as_str(),
            "status": status,
            "started_ms": self.started_ms,
            "ended_ms": *self.ended_ms.lock().unwrap_or_else(|e| e.into_inner()),
            "detail": self.detail.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        });
        if let Some(error) = error {
            info["error"] = serde_json::json!(error);
        }
        info
    }
}

fn ceil_char_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

pub(crate) fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn a_tail_is_cut_on_a_character_boundary() {
        let task = RunningTask::new("sh-1".into(), TaskType::Shell);
        task.append_output("ab\u{4e2d}\n");
        assert_eq!(task.output_tail(64), ("ab\u{4e2d}\n".to_string(), false));
        assert_eq!(task.output_tail(2), ("\n".to_string(), true));
        assert_eq!(task.output_tail(4), ("\u{4e2d}\n".to_string(), true));
    }

    #[test]
    fn the_oldest_output_goes_past_the_cap() {
        let task = RunningTask::new("sh-2".into(), TaskType::Shell);
        task.append_output("head\n");
        task.append_output(&"x".repeat(RunningTask::OUTPUT_KEPT_BYTES));
        let (whole, truncated) = task.output_tail(usize::MAX);
        assert_eq!(whole.len(), RunningTask::OUTPUT_KEPT_BYTES);
        assert!(!whole.contains("head"));
        assert!(truncated, "what was dropped is reported");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunningStatus {
    /// 还在跑
    Running,
    /// 完成（自然结束）
    Completed,
    /// TaskStop 触发取消
    Cancelled,
    /// 内部错误（API / 工具）
    Failed(String),
}

/// 一条 todo —— 模型用 TodoWrite 一次性替换整个列表。
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq,
)]
pub struct TodoItem {
    /// 给用户看的简短描述
    pub content: String,
    /// 状态
    pub status: TodoStatus,
    /// 现在时态描述（spinner 用），如 "Writing tests"。必填。
    /// **Q4 **: serde alias so payloads using `activeForm` (camelCase)
    /// deserialize into our `active_form` (snake_case) field. Models trained
    /// on TS schema can call our TodoWrite without contortion.
    #[serde(alias = "activeForm")]
    pub active_form: String,
}

#[derive(
    Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// 后台 agent 进度通知数据。AgentTool（`call_background`）在 drain
/// 循环中通过 ToolCtx.events_tx 发送，Engine 包装为 EngineEvent::BackgroundAgentProgress
/// 供 CLI/TUI 实时显示。
#[derive(Debug, Clone)]
pub struct BackgroundAgentProgressData {
    pub task_id: String,
    pub text: String,
    pub tool_name: Option<String>,
}
