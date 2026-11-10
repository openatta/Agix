//! 后台任务 + todo 条目类型。

/// Task type enum — categorises all background/async task kinds.
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
    /// MCP monitor task.
    Monitor,
    /// A program the model wrote (`RunCode`).
    Code,
}

impl TaskType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskType::Shell => "shell",
            TaskType::Agent => "agent",
            TaskType::Teammate => "teammate",
            TaskType::CronJob => "cron",
            TaskType::Monitor => "monitor",
            TaskType::Code => "code",
        }
    }

    /// Prefix used in task IDs for quick type recognition.
    pub fn id_prefix(&self) -> &'static str {
        match self {
            TaskType::Shell => "sh",
            TaskType::Agent => "ag",
            TaskType::Teammate => "tm",
            TaskType::CronJob => "cj",
            TaskType::Monitor => "mo",
            TaskType::Code => "cd",
        }
    }
}

/// Strongly-typed task identifier.
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
/// 输出（`append_output`）/ events_log / status 由跑它的那段代码追加；TaskOutput
/// 工具与宿主的 `session.taskOutput` 读快照，或按字节偏移接着读；
/// TaskStop 与宿主的 `session.stopTask` 触发 cancel token。
#[derive(Debug)]
pub struct RunningTask {
    pub task_id: String,
    pub typed_id: Option<TaskId>,
    pub task_type: TaskType,
    output: std::sync::Mutex<TaskOutput>,
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
    /// How much of the output has gone out as `task_progress`, in bytes.
    progress_sent: std::sync::Mutex<u64>,
    /// The session's task event channel, once the task is registered.
    events: std::sync::OnceLock<tokio::sync::broadcast::Sender<TaskEvent>>,
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
            output: std::sync::Mutex::new(TaskOutput::default()),
            events_log: std::sync::Mutex::new(Vec::new()),
            status: std::sync::Mutex::new(RunningStatus::Running),
            cancel,
            started_ms: unix_ms(),
            ended_ms: std::sync::Mutex::new(None),
            detail: std::sync::Mutex::new(serde_json::Value::Null),
            announced: std::sync::atomic::AtomicBool::new(false),
            ended: tokio::sync::Notify::new(),
            inbox: std::sync::OnceLock::new(),
            usage: std::sync::Mutex::new(
                matches!(task_type, TaskType::Agent | TaskType::Teammate)
                    .then(crate::interface::model::Usage::default),
            ),
            structured_output: std::sync::OnceLock::new(),
            waiting_for: std::sync::Mutex::new(Vec::new()),
            progress_sent: std::sync::Mutex::new(0),
            events: std::sync::OnceLock::new(),
        }
    }

    /// Where this task's `task_*` events go; set once, by the session that
    /// registers it.
    pub fn report_to(&self, events: tokio::sync::broadcast::Sender<TaskEvent>) {
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
            let _ = events.send(TaskEvent::Updated { task: self.info() });
        }
    }

    /// Whether what the work prints is streamed as it runs (`task_progress`):
    /// a shell command's and a monitor's. A sub-agent's output is its final
    /// answer, reported when it finishes.
    pub fn streams_output(&self) -> bool {
        matches!(
            self.task_type,
            TaskType::Shell | TaskType::Monitor | TaskType::Code
        )
    }

    /// The most of the new output one `task_progress` carries.
    pub const PROGRESS_FRAME_BYTES: usize = 8 * 1024;

    /// Send what the work printed since the last `task_progress`, if anything.
    /// Nothing goes out once the output is [closed](Self::close_output), so
    /// none can follow `task_finished`.
    pub fn report_progress(&self) {
        let mut sent = self.progress_sent.lock().unwrap_or_else(|e| e.into_inner());
        let output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        if !output.closed {
            self.send_progress(&output, &mut sent);
        }
    }

    /// End the output: what the work printed since the last `task_progress`
    /// goes out as the last one, and nothing is added after. `false` if it
    /// was already closed.
    pub fn close_output(&self) -> bool {
        let mut sent = self.progress_sent.lock().unwrap_or_else(|e| e.into_inner());
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        if output.closed {
            return false;
        }
        output.closed = true;
        self.send_progress(&output, &mut sent);
        true
    }

    pub fn output_closed(&self) -> bool {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).closed
    }

    fn send_progress(&self, output: &TaskOutput, sent: &mut u64) {
        if !self.streams_output()
            || !self.announced.load(std::sync::atomic::Ordering::SeqCst)
            || output.total == *sent
        {
            return;
        }
        let Some(events) = self.events.get() else {
            return;
        };
        let new = output.total - *sent;
        let shown = (new.min(Self::PROGRESS_FRAME_BYTES as u64) as usize).min(output.kept.len());
        let cut = ceil_char_boundary(&output.kept, output.kept.len() - shown);
        let appended = &output.kept[cut..];
        let from_byte = output.total - appended.len() as u64;
        let _ = events.send(TaskEvent::Progress {
            task_id: self.task_id.clone(),
            skipped_bytes: from_byte - *sent,
            appended: appended.to_string(),
            from_byte,
            next_byte: output.total,
        });
        *sent = output.total;
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

    /// Add to the output as the work produces it, keeping its last
    /// [`Self::OUTPUT_KEPT_BYTES`]. Nothing is added once it is closed.
    pub fn append_output(&self, text: &str) {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        if output.closed {
            return;
        }
        output.kept.push_str(text);
        output.total += text.len() as u64;
        if output.kept.len() > Self::OUTPUT_KEPT_BYTES {
            let cut = ceil_char_boundary(&output.kept, output.kept.len() - Self::OUTPUT_KEPT_BYTES);
            output.kept.drain(..cut);
        }
    }

    /// All of the output still kept.
    pub fn output_text(&self) -> String {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .kept
            .clone()
    }

    /// The last `max_bytes` of the output.
    pub fn output_tail(&self, max_bytes: usize) -> OutputSlice {
        let output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        let cut = ceil_char_boundary(&output.kept, output.kept.len().saturating_sub(max_bytes));
        output.slice(cut, 0)
    }

    /// The output from byte `from_byte` of everything ever appended to its
    /// end, as much of it as is still kept. `Err` carries the end when
    /// `from_byte` is past it.
    pub fn output_from(&self, from_byte: u64) -> Result<OutputSlice, u64> {
        let output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        if from_byte > output.total {
            return Err(output.total);
        }
        let earliest = output.earliest_kept();
        let at = ceil_char_boundary(&output.kept, (from_byte.max(earliest) - earliest) as usize);
        let dropped = earliest + at as u64 - from_byte;
        Ok(output.slice(at, dropped))
    }

    /// This task as hosts see it (`session.tasks`, `task_*` events).
    pub fn info(&self) -> TaskInfo {
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
        let (status, error, waiting_for) = match status {
            RunningStatus::Running if !waiting_for.is_empty() => {
                (TaskStatus::Waiting, None, waiting_for)
            }
            RunningStatus::Running => (TaskStatus::Running, None, Vec::new()),
            RunningStatus::Completed => (TaskStatus::Done, None, Vec::new()),
            RunningStatus::Cancelled => (TaskStatus::Stopped, None, Vec::new()),
            RunningStatus::Failed(e) => (TaskStatus::Failed, Some(e), Vec::new()),
        };
        TaskInfo {
            task_id: self.task_id.clone(),
            kind: self.task_type,
            status,
            started_ms: self.started_ms,
            ended_ms: *self.ended_ms.lock().unwrap_or_else(|e| e.into_inner()),
            detail: self
                .detail
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            error,
            waiting_for,
            usage: *self.usage.lock().unwrap_or_else(|e| e.into_inner()),
            structured_output: self.structured_output.get().cloned(),
        }
    }
}

/// A task as `session.tasks` lists it and the `task_*` events carry it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TaskInfo {
    pub task_id: String,
    #[serde(serialize_with = "wire_kind")]
    pub kind: TaskType,
    pub status: TaskStatus,
    /// Unix millis.
    pub started_ms: i64,
    /// Unix millis; `null` while it runs.
    pub ended_ms: Option<i64>,
    pub detail: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The prompt ids the work waits on; empty unless `status` is `waiting`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub waiting_for: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::interface::model::Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<serde_json::Value>,
}

/// On the wire a task's kind goes by [`TaskType::as_str`], not its serde
/// name: `cron`, not `cron_job`.
fn wire_kind<S: serde::Serializer>(kind: &TaskType, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(kind.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    /// Running, and waiting on the host to answer a question.
    Waiting,
    Done,
    Stopped,
    Failed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// What a session's tasks report as they run
/// ([`super::SessionState::subscribe_tasks`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind")]
pub enum TaskEvent {
    #[serde(rename = "task_started")]
    Started { task: TaskInfo },
    #[serde(rename = "task_updated")]
    Updated { task: TaskInfo },
    /// What a streamed task printed since the last one: bytes
    /// `from_byte..next_byte` of everything it ever printed, after
    /// `skipped_bytes` that did not fit in one frame.
    #[serde(rename = "task_progress")]
    Progress {
        task_id: String,
        skipped_bytes: u64,
        appended: String,
        from_byte: u64,
        next_byte: u64,
    },
    #[serde(rename = "task_finished")]
    Finished { task: TaskInfo },
}

/// A task's output, as the offsets of everything ever appended count it.
#[derive(Debug, Default)]
struct TaskOutput {
    /// The tail of it, at most [`RunningTask::OUTPUT_KEPT_BYTES`]. Every
    /// append is whole text and the head is cut on a character boundary, so
    /// every offset handed out is one too.
    kept: String,
    total: u64,
    closed: bool,
}

impl TaskOutput {
    fn earliest_kept(&self) -> u64 {
        self.total - self.kept.len() as u64
    }

    fn slice(&self, at: usize, dropped: u64) -> OutputSlice {
        OutputSlice {
            text: self.kept[at..].to_string(),
            from_byte: self.earliest_kept() + at as u64,
            next_byte: self.total,
            dropped,
            closed: self.closed,
        }
    }
}

/// A stretch of a task's output: bytes `from_byte..next_byte` of everything
/// the work ever printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSlice {
    pub text: String,
    pub from_byte: u64,
    pub next_byte: u64,
    /// Of what was asked for, the bytes no longer kept: the asked-for start
    /// plus these is `from_byte`.
    pub dropped: u64,
    /// The work's output has ended; `next_byte` will not grow.
    pub closed: bool,
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
        let tail = |max| {
            let tail = task.output_tail(max);
            (tail.text, tail.from_byte)
        };
        assert_eq!(tail(64), ("ab\u{4e2d}\n".to_string(), 0));
        assert_eq!(tail(2), ("\n".to_string(), 5));
        assert_eq!(tail(4), ("\u{4e2d}\n".to_string(), 2));
        assert_eq!(task.output_tail(2).next_byte, 6);
    }

    #[test]
    fn the_oldest_output_goes_past_the_cap() {
        let task = RunningTask::new("sh-2".into(), TaskType::Shell);
        task.append_output("head\n");
        task.append_output(&"x".repeat(RunningTask::OUTPUT_KEPT_BYTES));
        let whole = task.output_tail(usize::MAX);
        assert_eq!(whole.text.len(), RunningTask::OUTPUT_KEPT_BYTES);
        assert!(!whole.text.contains("head"));
        assert_eq!(whole.from_byte, 5, "what was dropped is reported");
    }

    #[test]
    fn reads_from_each_next_byte_join_into_the_whole_output() {
        let task = RunningTask::new("sh-3".into(), TaskType::Shell);
        let mut joined = String::new();
        let mut next = 0;
        for chunk in ["one\n", "\u{4e2d}two\n", "three\n"] {
            task.append_output(chunk);
            let read = task.output_from(next).unwrap();
            assert_eq!(read.from_byte, next);
            assert_eq!(read.dropped, 0);
            joined.push_str(&read.text);
            next = read.next_byte;
        }
        assert_eq!(joined, "one\n\u{4e2d}two\nthree\n");
        assert_eq!(next, joined.len() as u64);
        let idle = task.output_from(next).unwrap();
        assert_eq!((idle.text.as_str(), idle.next_byte), ("", next));
    }

    #[test]
    fn a_read_after_more_than_is_kept_says_how_much_was_dropped() {
        let task = RunningTask::new("sh-4".into(), TaskType::Shell);
        task.append_output("start\n");
        let first = task.output_from(0).unwrap();
        task.append_output(&"x".repeat(RunningTask::OUTPUT_KEPT_BYTES + 100));
        let second = task.output_from(first.next_byte).unwrap();
        let earliest_kept = second.next_byte - RunningTask::OUTPUT_KEPT_BYTES as u64;
        assert_eq!(second.from_byte, earliest_kept);
        assert_eq!(second.dropped, earliest_kept - first.next_byte);
        assert_eq!(second.text, task.output_text());
        assert_eq!(
            second.from_byte + second.text.len() as u64,
            second.next_byte
        );
    }

    #[test]
    fn a_read_from_inside_a_character_starts_after_it() {
        let task = RunningTask::new("sh-5".into(), TaskType::Shell);
        task.append_output("a\u{4e2d}b");
        let read = task.output_from(2).unwrap();
        assert_eq!(read.text, "b");
        assert_eq!((read.from_byte, read.dropped), (4, 2));
    }

    #[test]
    fn a_read_past_the_end_is_refused_with_the_end() {
        let task = RunningTask::new("sh-6".into(), TaskType::Shell);
        task.append_output("abc");
        assert_eq!(task.output_from(4), Err(3));
        assert!(task.output_from(3).is_ok());
    }

    #[test]
    fn a_closed_output_takes_nothing_more() {
        let task = RunningTask::new("sh-7".into(), TaskType::Shell);
        task.append_output("done\n");
        assert!(!task.output_tail(64).closed);
        assert!(task.close_output());
        assert!(!task.close_output(), "only once");
        task.append_output("late\n");
        let read = task.output_from(0).unwrap();
        assert_eq!(read.text, "done\n");
        assert!(read.closed);
    }

    fn reporting(
        task_type: TaskType,
    ) -> (RunningTask, tokio::sync::broadcast::Receiver<TaskEvent>) {
        let (events, rx) = tokio::sync::broadcast::channel(64);
        let task = RunningTask::new("t-1".into(), task_type);
        task.report_to(events);
        (task, rx)
    }

    fn drain(rx: &mut tokio::sync::broadcast::Receiver<TaskEvent>) -> Vec<serde_json::Value> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    #[test]
    fn progress_goes_out_only_between_announcement_and_close() {
        let (task, mut rx) = reporting(TaskType::Shell);
        task.append_output("early\n");
        task.report_progress();
        assert!(drain(&mut rx).is_empty(), "not announced yet");

        task.announced
            .store(true, std::sync::atomic::Ordering::SeqCst);
        task.report_progress();
        task.append_output("more\n");
        task.report_progress();
        task.report_progress();
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
        task.close_output();
        assert_eq!(drain(&mut rx)[0]["appended"], "last\n");
        task.report_progress();
        assert!(drain(&mut rx).is_empty(), "nothing follows the close");
    }

    #[test]
    fn progress_frames_carry_offsets_that_account_for_every_byte() {
        let (task, mut rx) = reporting(TaskType::Shell);
        task.announced
            .store(true, std::sync::atomic::Ordering::SeqCst);
        for chunk in [
            "a\n".to_string(),
            "x".repeat(RunningTask::PROGRESS_FRAME_BYTES + 10),
            "\u{4e2d}\n".to_string(),
        ] {
            task.append_output(&chunk);
            task.report_progress();
        }
        let frames = drain(&mut rx);
        assert_eq!(frames.len(), 3);
        let mut previous = 0;
        for frame in &frames {
            let from = frame["from_byte"].as_u64().unwrap();
            let next = frame["next_byte"].as_u64().unwrap();
            assert_eq!(from - previous, frame["skipped_bytes"].as_u64().unwrap());
            assert_eq!(
                from + frame["appended"].as_str().unwrap().len() as u64,
                next
            );
            previous = next;
        }
        assert_eq!(frames[1]["skipped_bytes"], 10);
        assert_eq!(previous, task.output_tail(1).next_byte);
    }

    #[test]
    fn a_burst_beyond_a_frame_is_sent_as_its_tail() {
        let (task, mut rx) = reporting(TaskType::Monitor);
        task.announced
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let burst = "x".repeat(RunningTask::PROGRESS_FRAME_BYTES * 3);
        task.append_output(&burst);
        task.report_progress();
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
        task.close_output();
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
    // Models trained on camelCase tool schemas send `activeForm`.
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
