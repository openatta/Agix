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
    /// Woken once `ended_ms` is set — see [`RunningTask::wait_ended`].
    pub ended: tokio::sync::Notify,
    /// Where messages for the work go while it runs; set by work that can
    /// take them (a sub-agent), absent for the rest.
    pub inbox: std::sync::OnceLock<std::sync::Arc<dyn TaskInbox>>,
    /// What the work's own model calls have cost so far; `None` for work that
    /// makes none (a shell command).
    pub usage: std::sync::Mutex<Option<crate::interface::model::Usage>>,
    /// What a sub-agent with an output schema handed in.
    pub structured_output: std::sync::OnceLock<serde_json::Value>,
    /// The questions the work is waiting on the host to answer, by prompt id.
    pub waiting_for: std::sync::Mutex<Vec<String>>,
    /// Everything `append_output` was ever given, in bytes — `output` keeps
    /// only the tail, so its length cannot say what is new.
    output_total: std::sync::atomic::AtomicU64,
    /// How much of `output_total` has gone out as `task_progress`.
    progress_sent: std::sync::Mutex<u64>,
    /// The session's task event channel, once the task is registered.
    events: std::sync::OnceLock<tokio::sync::broadcast::Sender<serde_json::Value>>,
}

/// Delivers a message to a running task's work.
pub trait TaskInbox: Send + Sync + std::fmt::Debug {
    /// `Err` says why the work cannot take it now.
    fn deliver(&self, message: String) -> Result<(), String>;
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
            ended: tokio::sync::Notify::new(),
            inbox: std::sync::OnceLock::new(),
            usage: std::sync::Mutex::new(
                matches!(task_type, TaskType::Agent | TaskType::Teammate)
                    .then(crate::interface::model::Usage::default),
            ),
            structured_output: std::sync::OnceLock::new(),
            waiting_for: std::sync::Mutex::new(Vec::new()),
            output_total: std::sync::atomic::AtomicU64::new(0),
            progress_sent: std::sync::Mutex::new(0),
            events: std::sync::OnceLock::new(),
        }
    }

    /// Where this task's `task_*` events go; set once, by the session that
    /// registers it.
    pub fn report_to(&self, events: tokio::sync::broadcast::Sender<serde_json::Value>) {
        let _ = self.events.set(events);
    }

    /// Count one more model call of this task's work.
    pub fn add_usage(&self, call: crate::interface::model::Usage) {
        {
            let mut usage = self.usage.lock().unwrap_or_else(|e| e.into_inner());
            *usage = Some(usage.unwrap_or_default().plus(call));
        }
        self.report_update();
    }

    /// The work is waiting on the host's answer to `prompt_id`.
    pub fn begin_waiting(&self, prompt_id: &str) {
        self.waiting_for
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(prompt_id.to_string());
        self.report_update();
    }

    /// The question `prompt_id` was answered, or given up on.
    pub fn end_waiting(&self, prompt_id: &str) {
        let removed = {
            let mut waiting = self.waiting_for.lock().unwrap_or_else(|e| e.into_inner());
            let before = waiting.len();
            waiting.retain(|id| id != prompt_id);
            waiting.len() != before
        };
        if removed {
            self.report_update();
        }
    }

    /// `task_updated`, for a task whose start was announced and which has
    /// not ended: what changed is in the snapshot.
    fn report_update(&self) {
        let Some(events) = self.events.get() else {
            return;
        };
        if self.announced.load(std::sync::atomic::Ordering::SeqCst) && !self.has_ended() {
            let _ = events.send(serde_json::json!({"kind": "task_updated", "task": self.info()}));
        }
    }

    /// Whether what the work prints is streamed as it runs (`task_progress`):
    /// a shell command's and a monitor's. A sub-agent's output is its final
    /// answer, reported when it finishes.
    pub fn streams_output(&self) -> bool {
        matches!(self.task_type, TaskType::Shell | TaskType::Monitor)
    }

    /// The most of the new output one `task_progress` carries.
    pub const PROGRESS_FRAME_BYTES: usize = 8 * 1024;

    /// Send what the work printed since the last `task_progress`, if anything.
    /// Only `last` — the one sent as the task ends, before `task_finished` —
    /// goes out once the task has ended, so none can follow `task_finished`.
    pub fn report_progress(&self, last: bool) {
        let mut sent = self.progress_sent.lock().unwrap_or_else(|e| e.into_inner());
        if !self.streams_output()
            || !self.announced.load(std::sync::atomic::Ordering::SeqCst)
            || (!last && self.has_ended())
        {
            return;
        }
        let Some(events) = self.events.get() else {
            return;
        };
        let (appended, new) = {
            let output = self.output.lock().unwrap_or_else(|e| e.into_inner());
            let total = self.output_total.load(std::sync::atomic::Ordering::SeqCst);
            if total == *sent {
                return;
            }
            let new = (total - *sent) as usize;
            let shown = new.min(Self::PROGRESS_FRAME_BYTES).min(output.len());
            let cut = ceil_char_boundary(&output, output.len() - shown);
            *sent = total;
            (output[cut..].to_string(), new)
        };
        let _ = events.send(serde_json::json!({
            "kind": "task_progress",
            "task_id": self.task_id,
            "skipped_bytes": new - appended.len(),
            "appended": appended,
        }));
    }

    pub fn has_ended(&self) -> bool {
        self.ended_ms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Resolves once the task has ended (at once if it already has).
    pub async fn wait_ended(&self) {
        let notified = self.ended.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.has_ended() {
            return;
        }
        notified.await;
    }

    /// The most of a streamed output (a shell command's, a monitor's) a task
    /// keeps; beyond it the oldest goes.
    pub const OUTPUT_KEPT_BYTES: usize = 256 * 1024;

    /// Add to `output` as the work produces it, keeping its last
    /// [`Self::OUTPUT_KEPT_BYTES`].
    pub fn append_output(&self, text: &str) {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        output.push_str(text);
        self.output_total
            .fetch_add(text.len() as u64, std::sync::atomic::Ordering::SeqCst);
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
        let waiting_for = self
            .waiting_for
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (status, error) = match status {
            RunningStatus::Running if !waiting_for.is_empty() => ("waiting", None),
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
        if status == "waiting" {
            info["waiting_for"] = serde_json::json!(waiting_for);
        }
        if let Some(usage) = *self.usage.lock().unwrap_or_else(|e| e.into_inner()) {
            info["usage"] = serde_json::json!(usage);
        }
        if let Some(output) = self.structured_output.get() {
            info["structured_output"] = output.clone();
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
mod tests {
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

    fn reporting(
        task_type: TaskType,
    ) -> (
        RunningTask,
        tokio::sync::broadcast::Receiver<serde_json::Value>,
    ) {
        let (events, rx) = tokio::sync::broadcast::channel(64);
        let task = RunningTask::new("t-1".into(), task_type);
        task.report_to(events);
        (task, rx)
    }

    fn drain(
        rx: &mut tokio::sync::broadcast::Receiver<serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn progress_goes_out_only_between_announcement_and_end() {
        let (task, mut rx) = reporting(TaskType::Shell);
        task.append_output("early\n");
        task.report_progress(false);
        assert!(drain(&mut rx).is_empty(), "not announced yet");

        task.announced
            .store(true, std::sync::atomic::Ordering::SeqCst);
        task.report_progress(false);
        task.append_output("more\n");
        task.report_progress(false);
        task.report_progress(false);
        let sent: Vec<_> = drain(&mut rx)
            .into_iter()
            .map(|e| e["appended"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            sent,
            ["early\n", "more\n"],
            "each byte once, nothing when idle"
        );

        task.append_output("last\n");
        *task.ended_ms.lock().unwrap() = Some(1);
        task.report_progress(false);
        assert!(
            drain(&mut rx).is_empty(),
            "only the closing report follows the end"
        );
        task.report_progress(true);
        assert_eq!(drain(&mut rx)[0]["appended"], "last\n");
    }

    #[test]
    fn a_burst_beyond_a_frame_is_sent_as_its_tail() {
        let (task, mut rx) = reporting(TaskType::Monitor);
        task.announced
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let burst = "x".repeat(RunningTask::PROGRESS_FRAME_BYTES * 3);
        task.append_output(&burst);
        task.report_progress(false);
        let frame = drain(&mut rx).remove(0);
        assert_eq!(frame["kind"], "task_progress");
        assert_eq!(frame["task_id"], "t-1");
        assert_eq!(
            frame["appended"].as_str().unwrap().len(),
            RunningTask::PROGRESS_FRAME_BYTES
        );
        assert_eq!(
            frame["skipped_bytes"],
            RunningTask::PROGRESS_FRAME_BYTES * 2,
            "what was left out is counted"
        );
    }

    #[test]
    fn a_sub_agents_answer_is_not_streamed() {
        let (task, mut rx) = reporting(TaskType::Agent);
        task.announced
            .store(true, std::sync::atomic::Ordering::SeqCst);
        task.append_output("the answer");
        task.report_progress(true);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn usage_and_waiting_are_reported_as_snapshots() {
        let (task, mut rx) = reporting(TaskType::Agent);
        let call = crate::interface::model::Usage {
            input_tokens: 10,
            output_tokens: 2,
            ..Default::default()
        };
        task.add_usage(call);
        assert!(drain(&mut rx).is_empty(), "not announced yet");

        task.announced
            .store(true, std::sync::atomic::Ordering::SeqCst);
        task.add_usage(call);
        let update = drain(&mut rx).remove(0);
        assert_eq!(update["kind"], "task_updated");
        assert_eq!(update["task"]["usage"]["input_tokens"], 20);
        assert_eq!(update["task"]["status"], "running");

        task.begin_waiting("p-1");
        task.begin_waiting("p-2");
        task.end_waiting("p-1");
        let states: Vec<_> = drain(&mut rx)
            .into_iter()
            .map(|e| {
                (
                    e["task"]["status"].clone(),
                    e["task"]["waiting_for"].clone(),
                )
            })
            .collect();
        assert_eq!(
            states,
            [
                (serde_json::json!("waiting"), serde_json::json!(["p-1"])),
                (
                    serde_json::json!("waiting"),
                    serde_json::json!(["p-1", "p-2"])
                ),
                (serde_json::json!("waiting"), serde_json::json!(["p-2"])),
            ]
        );
        task.end_waiting("p-2");
        task.end_waiting("p-2");
        let back = drain(&mut rx);
        assert_eq!(back.len(), 1, "an answer already counted changes nothing");
        assert_eq!(back[0]["task"]["status"], "running");
        assert!(back[0]["task"].get("waiting_for").is_none());

        *task.ended_ms.lock().unwrap() = Some(1);
        task.add_usage(call);
        assert!(
            drain(&mut rx).is_empty(),
            "an ended task's final word is task_finished"
        );
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
