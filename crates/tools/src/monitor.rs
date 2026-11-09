//! MonitorTool — "Monitor".
//!
//! Starts a script as a task of the session and returns at once. Each line
//! the script prints on stdout becomes an event: lines that arrive close
//! together are batched into one `<task-notification>`, which the session's
//! agent reads at its next step boundary (or, if no turn is running, when the
//! next one opens). Events that arrive while the last notification is still
//! unread join it, keeping its most recent lines, so a watch left running
//! over an idle session is one notification, not hundreds. The script's
//! exit, its timeout, or a flood of events
//! ends the watch with one last notification; `TaskStop` (or the host's
//! `session.stopTask`) ends it silently. What it prints — stderr too — is the
//! task's output (`TaskOutput`, `session.taskOutput`, `task_progress`).
//!
//! The script runs inside the same OS sandbox as `Bash`, under the same
//! `sandbox.require_enforcement`.

use async_trait::async_trait;
use base::context::{RunningStatus, RunningTask, SessionState, TaskType};
use base::error::ToolError;
use base::interface::exec::{OutputStream, ProcessHandle};
use base::tool::{
    InterruptBehavior, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext,
    ToolResult, ValidationResult,
};
use futures::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

/// Default monitor timeout (300 seconds).
const DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Maximum allowed timeout (3600 seconds = 1 hour).
const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// Maximum line length accepted from the FramedRead LinesCodec.
const MAX_LINE_LENGTH: usize = 64 * 1024;

/// Lines that arrive within this of the first one go out as one notification.
const BATCH_WINDOW: Duration = Duration::from_millis(200);

/// The most lines one notification carries — the most recent; the rest are
/// counted and left for `TaskOutput`.
const LINES_PER_NOTICE: usize = 50;

/// More events than this within [`FLOOD_WINDOW`] stops the monitor.
const FLOOD_LINES: usize = 300;
const FLOOD_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MonitorInput {
    /// The shell command to run (via `bash -c`). Each stdout line becomes a notification event.
    pub command: String,

    /// Human-readable description of what is being monitored. Appears in every notification.
    #[serde(alias = "description")]
    pub description: String,

    /// Timeout in milliseconds (default 300000, max 3600000). Ignored when persistent is true.
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// If true, the command runs until TaskStop or session end (no timeout kill).
    #[serde(default)]
    pub persistent: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MonitorTool;

#[async_trait]
impl Tool for MonitorTool {
    fn name(&self) -> &str {
        "Monitor"
    }

    fn description(&self) -> &str {
        "Start a background script that streams stdout lines as events."
    }

    fn input_schema(&self) -> Value {
        crate::input_schema::<MonitorInput>()
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/monitor.prompt.md").to_string()
    }

    fn is_read_only(&self, input: &Value) -> bool {
        serde_json::from_value::<MonitorInput>(input.clone())
            .is_ok_and(|p| crate::bash::classify::is_read_only(&p.command))
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_destructive(&self, _: &Value) -> bool {
        false
    }

    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Cancel
    }

    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<MonitorInput>(input.clone())
            .ok()
            .map(|p| p.command)
    }

    fn permission_content_kind(&self) -> base::tool::PermissionContentKind {
        base::tool::PermissionContentKind::ShellCommand
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<MonitorInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.command.trim().is_empty() => {
                ValidationResult::err("command must not be empty", 1)
            }
            Ok(p) if p.description.trim().is_empty() => {
                ValidationResult::err("description must not be empty", 2)
            }
            Ok(p) if !p.persistent && p.timeout_ms.unwrap_or(0) > MAX_TIMEOUT_MS => {
                ValidationResult::err(format!("timeout_ms exceeds {} ms cap", MAX_TIMEOUT_MS), 3)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 4),
        }
    }

    async fn check_permissions(&self, _input: &Value, ctx: &ToolContext) -> PermissionDecision {
        // Unattended, nobody can answer; the OS sandbox holds the script the
        // same way it holds a `Bash` command, under the same network condition.
        if ctx.session.unattended && crate::bash::unattended_sandbox_allows(ctx) {
            return PermissionDecision::Allow {
                decision_reason: Some("sandboxed".into()),
            };
        }
        PermissionDecision::ask("Monitor command requires confirmation")
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: MonitorInput = serde_json::from_value(input)?;
        let timeout = (!input.persistent).then(|| {
            Duration::from_millis(
                input
                    .timeout_ms
                    .unwrap_or(DEFAULT_TIMEOUT_MS)
                    .min(MAX_TIMEOUT_MS),
            )
        });
        let spec = match crate::bash::confine_shell("Monitor", &input.command, &ctx) {
            Ok(spec) => spec,
            Err(refused) => return Ok(refused),
        };
        // Under the turn's token, like a background sub-agent: it outlives the
        // turn that started it, and goes when the turn is interrupted or the
        // session closes.
        let cancel = ctx.cancel.child_token();
        let task_id = format!("monitor-{}", base::id::Id::new());
        let task = ctx.session.register_task(
            task_id.clone(),
            TaskType::Monitor,
            cancel.clone(),
            serde_json::json!({
                "command": input.command,
                "description": input.description,
                "persistent": input.persistent,
                "tool_use_id": ctx.tool_use_id,
            }),
        );
        let child = match ctx.exec.process.spawn(spec, cancel.clone()).await {
            Ok(child) => child,
            Err(e) => {
                ctx.session
                    .finish_task(&task_id, RunningStatus::Failed(e.to_string()));
                return Err(ToolError::from(e));
            }
        };
        tokio::spawn(
            Watch {
                session: ctx.session.clone(),
                task,
                description: input.description,
                cancel,
                timeout,
            }
            .run(child),
        );
        Ok(ToolResult::text(format!(
            "Monitor started as task {task_id}. Each line it prints reaches you as a \
             <task-notification> while you keep working; its exit is reported the same way. \
             Stop it with TaskStop (task_id {task_id}); read everything it printed with \
             TaskOutput."
        )))
    }
}

/// One running monitor, from its first line to its end.
struct Watch {
    session: Arc<SessionState>,
    task: Arc<RunningTask>,
    description: String,
    cancel: CancellationToken,
    timeout: Option<Duration>,
}

/// How a watch ended.
enum Ending {
    Exited(base::interface::exec::ExitStatus),
    TimedOut(Duration),
    Flooded,
    Stopped,
    Failed(String),
}

impl Watch {
    async fn run(self, mut child: Box<dyn ProcessHandle>) {
        let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
        #[allow(clippy::expect_used)]
        let chunks = child.output().expect("the first call owns the pipes");
        let task = self.task.clone();
        let stop_reading = CancellationToken::new();
        let stop = stop_reading.clone();
        let reader = tokio::spawn(async move {
            // Only stdout is an event. Stderr is kept as output rather than
            // left unread: a pipe nobody drains stalls the child.
            let stdout = tokio_util::io::StreamReader::new(chunks.map(move |chunk| match chunk {
                Ok(c) if c.stream == OutputStream::Stdout => Ok(std::io::Cursor::new(c.bytes)),
                Ok(c) => {
                    task.append_output(&String::from_utf8_lossy(&c.bytes));
                    Ok(std::io::Cursor::new(Vec::new()))
                }
                Err(e) => Err(std::io::Error::other(e.to_string())),
            }));
            let mut framed =
                FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_LENGTH));
            loop {
                let next = tokio::select! {
                    biased;
                    _ = stop.cancelled() => None,
                    n = framed.next() => n,
                };
                match next {
                    Some(Ok(line)) => {
                        if lines_tx.send(line).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        });

        let deadline = self.timeout.map(|t| tokio::time::Instant::now() + t);
        let mut batch: Vec<String> = Vec::new();
        let mut flush_at: Option<tokio::time::Instant> = None;
        let mut recent: std::collections::VecDeque<tokio::time::Instant> = Default::default();
        let mut exited: Option<base::interface::exec::ExitStatus> = None;
        let mut reading = true;
        let mut grace_until: Option<tokio::time::Instant> = None;
        let mut unread = Unread::default();

        let ending = loop {
            let far = tokio::time::Instant::now() + Duration::from_secs(86_400);
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break Ending::Stopped,
                _ = tokio::time::sleep_until(deadline.unwrap_or(far)), if deadline.is_some() && exited.is_none() => {
                    break Ending::TimedOut(self.timeout.unwrap_or_default());
                }
                _ = tokio::time::sleep_until(flush_at.unwrap_or(far)), if flush_at.is_some() => {
                    self.notify(&mut batch, None, &mut unread);
                    flush_at = None;
                }
                line = lines.recv(), if reading => match line {
                    Some(line) => {
                        self.task.append_output(&format!("{line}\n"));
                        let now = tokio::time::Instant::now();
                        recent.push_back(now);
                        while recent.front().is_some_and(|t| now - *t > FLOOD_WINDOW) {
                            recent.pop_front();
                        }
                        if recent.len() > FLOOD_LINES {
                            break Ending::Flooded;
                        }
                        batch.push(line);
                        flush_at.get_or_insert(now + BATCH_WINDOW);
                    }
                    None => {
                        reading = false;
                        if let Some(status) = exited {
                            break Ending::Exited(status);
                        }
                    }
                },
                status = child.wait(), if exited.is_none() => match status {
                    Ok(status) => {
                        if !reading {
                            break Ending::Exited(status);
                        }
                        exited = Some(status);
                        grace_until = Some(
                            tokio::time::Instant::now()
                                + base::interface::exec::OUTPUT_GRACE_AFTER_EXIT,
                        );
                    }
                    Err(e) => break Ending::Failed(e.to_string()),
                },
                _ = tokio::time::sleep_until(grace_until.unwrap_or(far)), if grace_until.is_some() => {
                    if let Some(status) = exited {
                        break Ending::Exited(status);
                    }
                }
            }
        };

        child.kill().await;
        stop_reading.cancel();
        let _ = reader.await;
        while let Ok(line) = lines.try_recv() {
            self.task.append_output(&format!("{line}\n"));
            batch.push(line);
        }
        let (status, closing) = match ending {
            Ending::Stopped => (RunningStatus::Cancelled, None),
            Ending::Exited(status) if status.success => (
                RunningStatus::Completed,
                Some("The command exited with code 0; the watch is over.".to_string()),
            ),
            Ending::Exited(status) => {
                let how = status
                    .code
                    .map_or("was killed by a signal".to_string(), |c| {
                        format!("exited with code {c}")
                    });
                (
                    RunningStatus::Failed(format!("the command {how}")),
                    Some(format!("The command {how}; the watch is over.")),
                )
            }
            Ending::TimedOut(after) => (
                RunningStatus::Failed(format!("timed out after {}s", after.as_secs())),
                Some(format!(
                    "Timed out after {}s and was stopped; set persistent for a session-long watch.",
                    after.as_secs()
                )),
            ),
            Ending::Flooded => (
                RunningStatus::Failed("too many events".into()),
                Some(format!(
                    "Stopped: more than {FLOOD_LINES} events within {}s. Restart it with a \
                     tighter filter.",
                    FLOOD_WINDOW.as_secs()
                )),
            ),
            Ending::Failed(why) => (
                RunningStatus::Failed(why.clone()),
                Some(format!("The watch failed: {why}")),
            ),
        };
        if closing.is_some() || !batch.is_empty() {
            self.notify(&mut batch, closing.as_deref(), &mut unread);
        }
        self.session.finish_task(&self.task.task_id, status);
    }

    /// Tell the session's agent what the script printed since the last time,
    /// and, as the watch ends, how it ended — in the notice it has not read
    /// yet, if there is one.
    fn notify(&self, batch: &mut Vec<String>, closing: Option<&str>, unread: &mut Unread) {
        if batch.is_empty() && closing.is_none() {
            return;
        }
        if !self.session.retract_notice(&self.task.task_id) {
            *unread = Unread::default();
        }
        unread.lines.append(batch);
        if unread.lines.len() > LINES_PER_NOTICE {
            let over = unread.lines.len() - LINES_PER_NOTICE;
            unread.lines.drain(..over);
            unread.omitted += over;
        }
        let mut notice = format!(
            "<task-notification>\n<task-id>{}</task-id>\n<kind>monitor</kind>\n\
             <description>{}</description>\n",
            self.task.task_id, self.description
        );
        if !unread.lines.is_empty() {
            notice.push_str("<events>\n");
            if unread.omitted > 0 {
                notice.push_str(&format!(
                    "({} earlier lines — read them with TaskOutput)\n",
                    unread.omitted
                ));
            }
            for line in &unread.lines {
                notice.push_str(line);
                notice.push('\n');
            }
            notice.push_str("</events>\n");
        }
        if let Some(closing) = closing {
            notice.push_str(&format!("<ended>{closing}</ended>\n"));
        }
        notice.push_str("</task-notification>");
        self.session
            .post_notice(Some(self.task.task_id.clone()), notice);
    }
}

/// The events in the notice the agent has not read yet.
#[derive(Default)]
struct Unread {
    lines: Vec<String>,
    omitted: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn ctx_in(cwd: &std::path::Path) -> ToolContext {
        ToolContext::for_test(cwd.to_path_buf())
    }

    struct Holds;

    impl base::interface::exec::Sandbox for Holds {
        fn confine(
            &self,
            spec: base::interface::exec::ProcessSpec,
            _policy: &base::interface::exec::SandboxPolicy,
        ) -> base::interface::exec::Confined {
            base::interface::exec::Confined {
                spec,
                mode: base::interface::exec::SandboxMode::MacOSSandboxExec,
                enforcement: base::interface::exec::Enforcement::Full,
                unmet: Vec::new(),
            }
        }
    }

    #[tokio::test]
    async fn unattended_monitor_runs_when_the_sandbox_holds() {
        use base::context::config::NetworkModeConfig;
        let dir = TempDir::new().unwrap();
        let input = json!({"command": "npm run dev", "description": "dev server"});
        let unattended = |network, requires_network_policy| {
            let mut ctx = ctx_in(dir.path());
            let mut session = SessionState::new(dir.path().to_path_buf());
            session.unattended = true;
            ctx.session = Arc::new(session);
            ctx.dangerously_disable_sandbox = false;
            ctx.sandbox.network_mode = network;
            ctx.sandbox.unattended_requires_network_policy = requires_network_policy;
            ctx.exec.sandbox = Arc::new(Holds);
            ctx
        };
        let input = &input;
        let decide =
            move |ctx: ToolContext| async move { MonitorTool.check_permissions(input, &ctx).await };
        for (network, requires, allowed) in [
            (NetworkModeConfig::Unrestricted, false, true),
            (NetworkModeConfig::DenyAll, false, true),
            (NetworkModeConfig::Unrestricted, true, false),
            (NetworkModeConfig::DenyAll, true, true),
        ] {
            let d = decide(unattended(network, requires)).await;
            assert_eq!(
                matches!(d, PermissionDecision::Allow { .. }),
                allowed,
                "{network:?}, requires_network_policy={requires}: {d:?}"
            );
        }
        assert!(matches!(
            decide(ctx_in(dir.path())).await,
            PermissionDecision::Ask { .. }
        ));
    }

    #[tokio::test]
    async fn empty_command_rejected() {
        let tool = MonitorTool;
        let dir = TempDir::new().unwrap();
        let result = tool
            .validate_input(
                &json!({"command": "", "description": "test"}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(!result.is_ok(), "empty command should be rejected");
    }

    #[tokio::test]
    async fn empty_description_rejected() {
        let tool = MonitorTool;
        let dir = TempDir::new().unwrap();
        let result = tool
            .validate_input(
                &json!({"command": "echo hello", "description": ""}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(!result.is_ok(), "empty description should be rejected");
    }

    #[tokio::test]
    async fn timeout_exceeds_max_rejected() {
        let tool = MonitorTool;
        let dir = TempDir::new().unwrap();
        let result = tool
            .validate_input(
                &json!({"command": "echo hello", "description": "test", "timeout_ms": 3_700_000}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(!result.is_ok(), "timeout exceeding max should be rejected");
    }

    #[tokio::test]
    async fn persistent_skips_timeout_validation() {
        let tool = MonitorTool;
        let dir = TempDir::new().unwrap();
        let result = tool
            .validate_input(
                &json!({
                    "command": "echo hello",
                    "description": "test",
                    "timeout_ms": 3_700_000,
                    "persistent": true
                }),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(
            result.is_ok(),
            "persistent should skip timeout cap validation"
        );
    }

    #[tokio::test]
    async fn valid_input_accepted() {
        let tool = MonitorTool;
        let dir = TempDir::new().unwrap();
        let result = tool
            .validate_input(
                &json!({"command": "echo hello", "description": "test"}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(result.is_ok(), "valid input should be accepted");
    }

    #[tokio::test]
    async fn malformed_json_rejected() {
        let tool = MonitorTool;
        let dir = TempDir::new().unwrap();
        let result = tool
            .validate_input(&json!({"bad": "data"}), &ctx_in(dir.path()))
            .await;
        assert!(
            result.is_err(),
            "missing required fields should be rejected"
        );
    }

    #[test]
    fn schema_is_valid_object() {
        let tool = MonitorTool;
        let schema = tool.input_schema();
        assert!(schema.is_object(), "schema should be an object");
    }

    fn session_in(dir: &std::path::Path) -> (ToolContext, Arc<SessionState>) {
        let ctx = ctx_in(dir);
        let session = ctx.session.clone();
        (ctx, session)
    }

    async fn start(ctx: ToolContext, input: Value) -> ToolResult {
        MonitorTool
            .call(input, ctx, ProgressSender::noop("t"))
            .await
            .expect("the monitor starts")
    }

    /// The notices posted so far, waiting `up_to` for at least `n`.
    async fn notices(session: &SessionState, n: usize, up_to: Duration) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + up_to;
        let mut seen = Vec::new();
        while seen.len() < n && tokio::time::Instant::now() < deadline {
            seen.extend(session.take_notices());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        seen
    }

    fn monitor_task(session: &SessionState) -> Arc<RunningTask> {
        let id = session
            .task_infos()
            .into_iter()
            .find(|t| t["kind"] == "monitor")
            .and_then(|t| t["task_id"].as_str().map(str::to_string))
            .expect("the monitor is a task");
        session.find_running_task(&id).unwrap()
    }

    #[tokio::test]
    async fn it_returns_at_once_and_its_lines_arrive_as_notifications() {
        let dir = TempDir::new().unwrap();
        let (ctx, session) = session_in(dir.path());
        let started = std::time::Instant::now();
        let result = start(
            ctx,
            json!({"command": "echo first; sleep 0.6; echo second; sleep 30",
                   "description": "two events", "persistent": true}),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the call waited for the script"
        );
        let task = monitor_task(&session);
        assert!(format!("{:?}", result.content).contains(&task.task_id));

        let seen = notices(&session, 2, Duration::from_secs(10)).await;
        assert_eq!(
            seen.len(),
            2,
            "two events far apart are two notices: {seen:?}"
        );
        assert!(
            seen[0].contains("<events>\nfirst\n</events>"),
            "{}",
            seen[0]
        );
        assert!(seen[0].contains("<description>two events</description>"));
        assert!(seen[1].contains("second"));
        assert!(!task.has_ended(), "a persistent watch runs on");

        assert!(session.cancel_running_task(&task.task_id));
        task.wait_ended().await;
        assert_eq!(*task.status.lock().unwrap(), RunningStatus::Cancelled);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            session.take_notices().is_empty(),
            "a stopped watch ends silently"
        );
    }

    #[tokio::test]
    async fn lines_close_together_are_one_notification_and_the_exit_is_reported() {
        let dir = TempDir::new().unwrap();
        let (ctx, session) = session_in(dir.path());
        start(
            ctx,
            json!({"command": "printf 'a\\nb\\nc\\n'; sleep 0.5; exit 3", "description": "burst"}),
        )
        .await;
        let task = monitor_task(&session);
        task.wait_ended().await;
        let seen = notices(&session, 2, Duration::from_secs(5)).await;
        assert!(seen[0].contains("<events>\na\nb\nc\n</events>"), "{seen:?}");
        let last = seen.last().unwrap();
        assert!(last.contains("exited with code 3"), "{last}");
        assert!(matches!(
            &*task.status.lock().unwrap(),
            RunningStatus::Failed(_)
        ));
        assert_eq!(task.output_tail(1024).text, "a\nb\nc\n");
    }

    #[tokio::test]
    async fn a_clean_exit_completes_the_task() {
        let dir = TempDir::new().unwrap();
        let (ctx, session) = session_in(dir.path());
        start(ctx, json!({"command": "echo done", "description": "once"})).await;
        let task = monitor_task(&session);
        task.wait_ended().await;
        assert_eq!(*task.status.lock().unwrap(), RunningStatus::Completed);
        let seen = notices(&session, 1, Duration::from_secs(5)).await;
        assert!(
            seen.iter()
                .any(|n| n.contains("done") && n.contains("exited with code 0")),
            "{seen:?}"
        );
    }

    use base::interface::exec::{Confined, Enforcement, ProcessSpec, Sandbox, SandboxMode};

    /// Replaces every command with one that announces it ran confined.
    struct Confines(Enforcement);

    impl Sandbox for Confines {
        fn confine(
            &self,
            spec: ProcessSpec,
            _policy: &base::interface::exec::SandboxPolicy,
        ) -> Confined {
            Confined {
                spec: ProcessSpec {
                    args: vec!["-c".into(), "echo inside".into()],
                    ..spec
                },
                mode: SandboxMode::MacOSSandboxExec,
                enforcement: self.0,
                unmet: match self.0 {
                    Enforcement::Full => Vec::new(),
                    _ => vec!["the thing it could not do".into()],
                },
            }
        }
    }

    #[tokio::test]
    async fn the_script_runs_as_the_sandbox_confined_it() {
        let dir = TempDir::new().unwrap();
        let (mut ctx, session) = session_in(dir.path());
        ctx.exec.sandbox = Arc::new(Confines(Enforcement::Full));
        ctx.dangerously_disable_sandbox = false;
        start(
            ctx,
            json!({"command": "echo outside", "description": "where"}),
        )
        .await;
        let task = monitor_task(&session);
        task.wait_ended().await;
        let seen = notices(&session, 1, Duration::from_secs(5)).await;
        assert!(seen.iter().any(|n| n.contains("inside")), "{seen:?}");
        assert!(!seen.iter().any(|n| n.contains("outside")), "{seen:?}");
    }

    #[tokio::test]
    async fn a_sandbox_that_cannot_hold_the_script_refuses_it_when_enforcement_is_required() {
        let dir = TempDir::new().unwrap();
        let (mut ctx, session) = session_in(dir.path());
        ctx.exec.sandbox = Arc::new(Confines(Enforcement::Partial));
        ctx.dangerously_disable_sandbox = false;
        ctx.sandbox.require_enforcement = true;
        let result = start(ctx, json!({"command": "echo hi", "description": "refused"})).await;
        assert!(result.is_error);
        assert_eq!(
            result.structured_content.as_ref().unwrap()["refused"],
            "sandbox_unenforceable"
        );
        assert!(
            session.task_infos().is_empty(),
            "a refused script is no task"
        );
    }

    #[tokio::test]
    async fn events_nobody_has_read_yet_join_one_notification() {
        let dir = TempDir::new().unwrap();
        let (ctx, session) = session_in(dir.path());
        start(
            ctx,
            json!({"command": "for i in $(seq 1 80); do echo event-$i; sleep 0.25; done",
                   "description": "steady", "persistent": true}),
        )
        .await;
        let task = monitor_task(&session);
        tokio::time::timeout(Duration::from_secs(40), task.wait_ended())
            .await
            .expect("the script ends");
        let seen = session.take_notices();
        assert_eq!(
            seen.len(),
            1,
            "one unread notice, however long it ran: {seen:?}"
        );
        let notice = &seen[0];
        assert!(notice.contains("(30 earlier lines — read them with TaskOutput)\nevent-31\n"));
        assert!(notice.contains("event-80\n</events>"));
        assert!(notice.contains("exited with code 0"));
    }

    #[tokio::test]
    async fn a_timeout_stops_it_and_says_so() {
        let dir = TempDir::new().unwrap();
        let (ctx, session) = session_in(dir.path());
        start(
            ctx,
            json!({"command": "sleep 30", "description": "slow", "timeout_ms": 300}),
        )
        .await;
        let task = monitor_task(&session);
        tokio::time::timeout(Duration::from_secs(10), task.wait_ended())
            .await
            .expect("the timeout ends it");
        let seen = notices(&session, 1, Duration::from_secs(5)).await;
        assert!(seen[0].contains("Timed out"), "{seen:?}");
    }

    #[tokio::test]
    async fn a_flood_of_events_stops_it() {
        let dir = TempDir::new().unwrap();
        let (ctx, session) = session_in(dir.path());
        start(
            ctx,
            json!({"command": "while true; do echo tick; done", "description": "noisy", "persistent": true}),
        )
        .await;
        let task = monitor_task(&session);
        tokio::time::timeout(Duration::from_secs(10), task.wait_ended())
            .await
            .expect("the flood stops it");
        let seen = notices(&session, 1, Duration::from_secs(5)).await;
        let last = seen.last().unwrap();
        assert!(last.contains("tighter filter"), "{last}");
        assert!(
            last.contains("earlier lines — read them with TaskOutput"),
            "{last}"
        );
    }

    #[tokio::test]
    async fn the_turn_being_interrupted_stops_it() {
        let dir = TempDir::new().unwrap();
        let turn = CancellationToken::new();
        let ctx = ToolContext {
            cancel: turn.clone(),
            ..ctx_in(dir.path())
        };
        let session = ctx.session.clone();
        start(
            ctx,
            json!({"command": "sleep 60", "description": "x", "persistent": true}),
        )
        .await;
        let task = monitor_task(&session);
        turn.cancel();
        tokio::time::timeout(Duration::from_secs(10), task.wait_ended())
            .await
            .expect("the interrupt reaches it");
    }

    /// stderr is read and kept as output; only stdout lines are events.
    #[tokio::test]
    async fn stderr_is_output_but_not_an_event() {
        let dir = TempDir::new().unwrap();
        let (ctx, session) = session_in(dir.path());
        start(
            ctx,
            json!({"command": "head -c 400000 /dev/zero | tr '\\0' 'e' >&2; echo done",
                   "description": "stderr flood", "timeout_ms": 15000}),
        )
        .await;
        let task = monitor_task(&session);
        tokio::time::timeout(Duration::from_secs(20), task.wait_ended())
            .await
            .expect("an unread stderr pipe would wedge the child here");
        let seen = notices(&session, 1, Duration::from_secs(5)).await;
        let events: String = seen.concat();
        assert!(events.contains("<events>\ndone\n</events>"), "{events}");
        assert!(!events.contains("eeee"));
        assert!(task.output_tail(16).text.ends_with("done\n"));
    }

    #[tokio::test]
    async fn a_background_process_holding_stdout_does_not_hold_the_watch_open() {
        let dir = TempDir::new().unwrap();
        let (ctx, session) = session_in(dir.path());
        start(
            ctx,
            json!({"command": "sleep 30 & echo up", "description": "bg"}),
        )
        .await;
        let task = monitor_task(&session);
        tokio::time::timeout(Duration::from_secs(10), task.wait_ended())
            .await
            .expect("the watch waited on the background sleep");
    }
}
