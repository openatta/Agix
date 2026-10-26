//! MonitorTool — "Monitor".
//!
//! Start a background script that streams stdout lines as events.
//! Each stdout line becomes a notification. Exit ends the watch.
//!
//! Supports:
//! - Timeout (default 300s, max 3600s)
//! - Persistent mode (ignores timeout, runs until cancelled via TaskStop or session end)
//! - Cancellation via context cancel token
//! - Line-by-line streaming via ProgressSender

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::{OutputStream, ProcessSpec};
use base::tool::{
    InterruptBehavior, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext,
    ToolResult, ValidationResult,
};
use futures::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tokio_util::codec::{FramedRead, LinesCodec};

/// Default monitor timeout (300 seconds).
const DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Maximum allowed timeout (3600 seconds = 1 hour).
const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// Maximum line length accepted from the FramedRead LinesCodec.
const MAX_LINE_LENGTH: usize = 64 * 1024;

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
        serde_json::to_value(schemars::schema_for!(MonitorInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/monitor.prompt.md").to_string()
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
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

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolContext) -> PermissionDecision {
        PermissionDecision::ask("Monitor command requires confirmation")
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: MonitorInput = serde_json::from_value(input)?;
        let timeout = Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );

        // Spawn the child process via bash -c
        let spec =
            ProcessSpec::new("bash", &ctx.cwd).args(["-c".to_string(), input.command.clone()]);
        let run_cancel = ctx.cancel.child_token();
        let _task = base::context::TaskScope::new(
            ctx.session.clone(),
            format!("monitor-{}", base::id::Id::new()),
            base::context::TaskType::Monitor,
            run_cancel.clone(),
            serde_json::json!({
                "command": input.command,
                "description": input.description,
                "tool_use_id": ctx.tool_use_id,
            }),
        );
        let mut child = ctx
            .exec
            .process
            .spawn(spec, run_cancel.clone())
            .await
            .map_err(ToolError::from)?;
        let chunks = child.output().expect("the first call owns the pipes");

        // Spawn a background task to drain stdout lines and send them as progress events.
        let progress_clone = progress.clone();
        let stop_reading = tokio_util::sync::CancellationToken::new();
        let stop = stop_reading.clone();
        let mut drain_handle = tokio::spawn(async move {
            let stdout = tokio_util::io::StreamReader::new(chunks.map(|chunk| match chunk {
                // Only stdout becomes a notification. The stderr chunks are
                // read and thrown away rather than left unread: a provider
                // pipes both, and a pipe nobody drains stalls the child.
                Ok(c) if c.stream == OutputStream::Stdout => Ok(std::io::Cursor::new(c.bytes)),
                Ok(_) => Ok(std::io::Cursor::new(Vec::new())),
                Err(e) => Err(std::io::Error::other(e.to_string())),
            }));
            let mut framed =
                FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_LENGTH));
            let mut line_count: u64 = 0;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = stop.cancelled() => None,
                    n = framed.next() => n,
                };
                let Some(line_res) = next else { break };
                match line_res {
                    Ok(line) => {
                        line_count += 1;
                        progress_clone.send(&format!("{line}\n"));
                    }
                    Err(_) => break,
                }
            }
            line_count
        });

        // Wait for child exit, respecting cancel and optionally timeout.
        let wait_result = if input.persistent {
            // Persistent: only cancel can stop us.
            tokio::select! {
                biased;
                _ = run_cancel.cancelled() => {
                    child.kill().await;
                    stop_reading.cancel();
                    return Err(ToolError::Cancelled);
                }
                r = child.wait() => r,
            }
        } else {
            // Non-persistent: cancel or timeout can stop us.
            tokio::select! {
                biased;
                _ = run_cancel.cancelled() => {
                    child.kill().await;
                    stop_reading.cancel();
                    return Err(ToolError::Cancelled);
                }
                _ = tokio::time::sleep(timeout) => {
                    child.kill().await;
                    stop_reading.cancel();
                    return Err(ToolError::Timeout(timeout));
                }
                r = child.wait() => r,
            }
        };

        let status = wait_result.map_err(ToolError::from)?;
        let line_count = tokio::select! {
            r = &mut drain_handle => r.unwrap_or(0),
            _ = tokio::time::sleep(base::interface::exec::OUTPUT_GRACE_AFTER_EXIT) => {
                child.kill().await;
                stop_reading.cancel();
                (&mut drain_handle).await.unwrap_or(0)
            }
        };

        let summary = if status.success {
            format!(
                "Command completed successfully (exit code 0). {} lines streamed.",
                line_count
            )
        } else {
            let code = status.code.unwrap_or(-1);
            format!(
                "Command exited with code {}. {} lines streamed.",
                code, line_count
            )
        };

        if status.success {
            Ok(ToolResult::text(summary))
        } else {
            Ok(ToolResult::error_text(summary))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn ctx_in(cwd: &std::path::Path) -> ToolContext {
        ToolContext::for_test(cwd.to_path_buf())
    }

    // ── Validation tests ──

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

    // ── Schema ──

    #[test]
    fn schema_is_valid_object() {
        let tool = MonitorTool;
        let schema = tool.input_schema();
        assert!(schema.is_object(), "schema should be an object");
    }

    // ── Execution tests ──

    #[tokio::test]
    async fn basic_command_succeeds() {
        let dir = TempDir::new().unwrap();
        let tool = MonitorTool;
        let result = tool
            .call(
                json!({"command": "echo 'hello world'", "description": "basic test", "timeout_ms": 5000}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "successful exit should not be error");
    }

    /// stderr used to go to `/dev/null`; the contract pipes both streams. It
    /// has to be read — an undrained pipe stops the child at the kernel's
    /// buffer — and then dropped, because only stdout is a notification.
    #[tokio::test]
    async fn a_flood_of_stderr_is_swallowed_without_wedging_the_child() {
        let dir = TempDir::new().unwrap();
        let tool = MonitorTool;
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let callback = base::tool::ProgressSender::with_callback(
            "test",
            std::sync::Arc::new(collector::Collector(captured.clone())),
        );

        let result = tokio::time::timeout(
            Duration::from_secs(20),
            tool.call(
                json!({
                    "command": "head -c 400000 /dev/zero | tr '\\0' 'e' >&2; echo done",
                    "description": "stderr flood",
                    "timeout_ms": 15000
                }),
                ctx_in(dir.path()),
                callback,
            ),
        )
        .await
        .expect("an unread stderr pipe would wedge the child here")
        .unwrap();
        assert!(!result.is_error, "the command exits zero: {result:?}");

        let lines = captured.lock().unwrap().clone();
        assert_eq!(
            lines,
            vec!["done\n".to_string()],
            "stderr must not become notifications"
        );
    }

    #[tokio::test]
    async fn failing_command_returns_error() {
        let dir = TempDir::new().unwrap();
        let tool = MonitorTool;
        let result = tool
            .call(
                json!({"command": "exit 42", "description": "failure test", "timeout_ms": 5000}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(result.is_error, "non-zero exit should be marked as error");
    }

    #[tokio::test]
    async fn cancellation_returns_cancelled_error() {
        let dir = TempDir::new().unwrap();
        let tool = MonitorTool;

        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        // Cancel after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        // Build a custom ToolContext with our CancellationToken
        let ctx = ToolContext {
            cwd: dir.path().to_path_buf(),
            cancel,
            ..ToolContext::for_test(dir.path().to_path_buf())
        };

        let result = tool
            .call(
                json!({"command": "sleep 60 && echo done", "description": "cancel test", "timeout_ms": 30000}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await;

        match result {
            Err(ToolError::Cancelled) => {} // expected
            Err(e) => panic!("expected Cancelled, got: {e:?}"),
            Ok(_) => panic!("expected cancellation error, got Ok"),
        }
    }

    #[tokio::test]
    async fn timeout_returns_timeout_error() {
        let dir = TempDir::new().unwrap();
        let tool = MonitorTool;
        let result = tool
            .call(
                json!({"command": "sleep 30", "description": "timeout test", "timeout_ms": 100}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await;

        match result {
            Err(ToolError::Timeout(_)) => {} // expected
            Err(e) => panic!("expected Timeout, got: {e:?}"),
            Ok(_) => panic!("expected timeout error, got Ok"),
        }
    }

    #[tokio::test]
    async fn a_background_process_holding_stdout_does_not_hold_the_watch_open() {
        let dir = TempDir::new().unwrap();
        let started = std::time::Instant::now();
        let result = MonitorTool
            .call(
                json!({"command": "sleep 30 & echo up", "description": "bg", "timeout_ms": 20000}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the watch waited on the background sleep: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn persistent_command_not_killed_by_timeout() {
        let dir = TempDir::new().unwrap();
        let tool = MonitorTool;
        // persistent=true with a very small timeout_ms — should NOT kill the process.
        // The command echoes quickly and exits, so it completes before any cancel.
        let result = tool
            .call(
                json!({
                    "command": "echo 'persistent works'",
                    "description": "persistent test",
                    "timeout_ms": 1,
                    "persistent": true
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "persistent command should complete");
    }

    #[tokio::test]
    async fn lines_are_streamed_via_progress() {
        let dir = TempDir::new().unwrap();
        let tool = MonitorTool;

        // Use a progress sender that captures lines
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let callback = base::tool::ProgressSender::with_callback(
            "test",
            std::sync::Arc::new(collector::Collector(captured_clone)),
        );

        let result = tool
            .call(
                json!({"command": "printf 'line1\\nline2\\nline3\\n'", "description": "stream test", "timeout_ms": 5000}),
                ctx_in(dir.path()),
                callback,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "streaming command should succeed");

        let lines = captured.lock().unwrap();
        assert!(!lines.is_empty(), "should have received progress lines");
    }
}

/// Helper module for progress callback test.
#[cfg(test)]
mod collector {
    use base::tool::ProgressCallback;

    pub struct Collector(pub std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl ProgressCallback for Collector {
        fn on_progress(&self, _tool_use_id: &str, data: &str) {
            self.0.lock().unwrap().push(data.to_string());
        }
    }
}
