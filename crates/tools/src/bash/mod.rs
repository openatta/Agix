//! BashTool —— "Bash"。
//!
//! ：
//! - `bash -c <command>` 子进程，cwd = session.cwd
//! - **平台沙盒包装**（macOS sandbox-exec / Linux bwrap，受 `dangerously_disable_sandbox` 控制）
//! - stdout / stderr 行级流式喂给 `ProgressSender`
//! - 超时（默认 120s，上限 600s）+ cancel 通过停掉整个进程组实现；命令退出后仍占着输出管道的
//!   后台进程同样被停掉
//! - 命令分类用关键字白名单 / 黑名单（read-only / destructive）

pub mod classify;
mod output;
mod paths;
mod sandbox;
#[cfg(test)]
mod tests;

use output::{drain_output, Sink};
pub(crate) use paths::stays_where_the_session_may_read;

pub use base::interface::exec::SandboxMode;
pub use platform::sandbox::sandbox_status;

pub(crate) use sandbox::{confine_shell, unattended_sandbox_allows};
pub use sandbox::{engine_exec, engine_exec_for};

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Runs, Subject};
use base::tool::{
    CallTraits, PathStyle, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext,
    ToolResult, ToolSpec, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

/// 默认执行超时（120 秒）
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// 执行超时硬上限（10 分钟）
const MAX_TIMEOUT_MS: u64 = 600_000;
// No stall watchdog: stdin is null, so a command can't block on an
// interactive prompt (it reads EOF), and any genuine stall hits the
// per-command `timeout`.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashInput {
    /// The shell command to execute (run via `bash -c`).
    pub command: String,

    /// Optional execution timeout in milliseconds (default 120000, max 600000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// Brief one-sentence description of what the command does (for UI).
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct BashTool;

impl BashTool {
    fn reads_only_where_it_may(&self, input: &Value, ctx: &ToolContext) -> bool {
        self.call_traits(input).read_only
            && serde_json::from_value::<BashInput>(input.clone())
                .is_ok_and(|p| stays_where_the_session_may_read(&p.command, ctx))
    }
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .callable_from_code(true)
            .code_result("{stdout, stderr, exit_code}; a non-zero exit rejects with the output")
            .destructive(true)
            .permission_content(base::tool::PermissionContentKind::ShellCommand)
    }
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Run a shell command (sandboxed by default)."
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<BashInput>()
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("../prompts/coding/bash.prompt.md").to_string())
    }

    fn call_traits(&self, input: &Value) -> CallTraits {
        match parse_classification(input) {
            Some(c) => CallTraits::default()
                .read_only(c.read_only)
                .concurrency_safe(c.read_only)
                .destructive(c.destructive),
            // 解析不了的输入保守按 destructive 看
            None => CallTraits::default().destructive(true),
        }
    }

    fn accept_edits_allows(&self, input: &Value, ctx: &ToolContext) -> bool {
        self.reads_only_where_it_may(input, ctx)
    }

    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<BashInput>(input.clone())
            .ok()
            .map(|p| p.command)
    }

    async fn validate_input(&self, input: &Value, ctx: &ToolContext) -> ValidationResult {
        if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
            return ValidationResult::err(
                "run_in_background is not supported: Bash runs in the foreground. Use the \
                 Monitor tool for a long-running process",
                5,
            );
        }
        let parsed: Result<BashInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.command.trim().is_empty() => {
                ValidationResult::err("command must not be empty", 1)
            }
            Ok(p) if p.timeout_ms.unwrap_or(0) > MAX_TIMEOUT_MS => {
                ValidationResult::err(format!("timeout_ms exceeds {} ms cap", MAX_TIMEOUT_MS), 3)
            }
            Ok(ref p) if is_long_sleep(&p.command) => {
                let running = ctx.session.running_background_task_ids();
                if running.is_empty() {
                    ValidationResult::err(
                        "sleep >= 2s is blocked — use the Monitor tool for polling, or sleep 1 if \
                         you must",
                        4,
                    )
                } else {
                    ValidationResult::err(
                        format!("sleep >= 2s is blocked. {}", wait_with_task_wait(&running)),
                        4,
                    )
                }
            }
            Ok(ref p) if is_sleep_loop(&p.command) => {
                let running = ctx.session.running_background_task_ids();
                if running.is_empty() {
                    ValidationResult::Ok
                } else {
                    ValidationResult::err(
                        format!(
                            "a sleep loop cannot wait for background tasks. {}",
                            wait_with_task_wait(&running)
                        ),
                        6,
                    )
                }
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn check_permissions(&self, input: &Value, ctx: &ToolContext) -> PermissionDecision {
        // 已知只读命令（ls、cat、git status 等）自动允许，不弹权限窗。其余
        // 命令走 gate 模式分派（bypass → allow，其余模式 → ask；acceptEdits
        // 问 `accept_edits_allows`，条件与这里相同，所以也是 ask）。
        //
        // 这个 `Allow` 只是"待生效的快速通道"：gate 会先跑 deny 规则、
        // bypass-immune 路径与模式级硬拒绝，都放行了才兑现（见
        // `permissions::gate` 模块注释第 2 步）。相应地，`call_traits` 的 `read_only`
        // 必须真的意味着"不写、不执行别的程序、不读凭据" —— 见
        // `classify::classify` 的说明。只读也只在项目里只读：参数指向
        // cwd（及附加目录、read_directories）之外的路径时，和 Read 读项目外的
        // 文件一样要确认；git 只在会话自己所在的仓库里只读，项目带来的仓库的
        // 配置能让它拉起程序。
        if self.reads_only_where_it_may(input, ctx) {
            return PermissionDecision::Allow {
                decision_reason: Some("read_only".into()),
            };
        }
        // Nobody can answer the question below, and the OS sandbox already
        // holds the command to the directories it may write.
        if ctx.session.unattended && unattended_sandbox_allows(ctx) {
            return PermissionDecision::Allow {
                decision_reason: Some("sandboxed".into()),
            };
        }
        PermissionDecision::Ask {
            message: "Bash command requires confirmation".into(),
            decision_reason: None,
        }
    }

    async fn permission_facts(&self, input: &Value, ctx: &ToolContext) -> PermissionFacts {
        let subject = self
            .permission_match_content(input)
            .map_or(Subject::None, Subject::Shell);
        let facts = if self.reads_only_where_it_may(input, ctx) {
            PermissionFacts::new(subject, Reach::Inside)
        } else {
            PermissionFacts::new(subject, Reach::Unknown)
                .with_question("Bash command requires confirmation")
        };
        facts.with_runs(Runs::foreground())
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: BashInput = serde_json::from_value(input)?;
        let timeout = Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );

        let spec = match confine_shell("Bash", &input.command, &ctx) {
            Ok(spec) => spec,
            Err(refused) => return Ok(refused),
        };

        // The command runs under its own token, a child of the turn's: the
        // turn being interrupted still stops it, and so does stopping just
        // this task (`TaskStop`, or a host's `session.stopTask`).
        let run_cancel = ctx.cancel.child_token();
        let task_scope = base::context::TaskScope::new(
            ctx.session.clone(),
            format!("shell-{}", base::id::Id::new()),
            base::context::TaskType::Shell,
            run_cancel.clone(),
            serde_json::json!({"command": input.command, "tool_use_id": ctx.tool_use_id}),
        );
        let mut child = ctx
            .exec
            .process
            .spawn(spec, run_cancel.clone())
            .await
            .map_err(exec_error)?;
        #[allow(clippy::expect_used)]
        let output = child.output().expect("the stream is taken once");

        let stop_reading = tokio_util::sync::CancellationToken::new();
        let sink = Sink::new(progress.clone(), task_scope.task().clone());
        let mut drain = tokio::spawn(drain_output(output, sink, stop_reading.clone()));

        // A timeout is a result the model can read, not an error that throws
        // the output away: the command is killed and whatever it printed before
        // the deadline is still drained and returned, marked `[timed out …]`.
        let status = tokio::select! {
        biased;
        _ = run_cancel.cancelled() => {
            child.kill().await;
            stop_reading.cancel();
            return Err(ToolError::Cancelled);
        }
        _ = tokio::time::sleep(timeout) => {
            child.kill().await;
            None
        }
        r = child.wait() => Some(r.map_err(exec_error)?)};

        let (drained, left_running) = tokio::select! {
            biased;
            _ = run_cancel.cancelled() => {
                child.kill().await;
                stop_reading.cancel();
                return Err(ToolError::Cancelled);
            }
            r = &mut drain => (r, false),
            _ = tokio::time::sleep(base::interface::exec::OUTPUT_GRACE_AFTER_EXIT) => {
                child.kill().await;
                stop_reading.cancel();
                ((&mut drain).await, true)
            }
        };
        let (stdout_text, stderr_text) = drained.unwrap_or_default();
        let structured = serde_json::json!({
            "stdout": stdout_text,
            "stderr": stderr_text,
            "exit_code": status.as_ref().and_then(|s| s.code),
            "timed_out": status.is_none(),
        });
        let mut combined = stdout_text;
        if !stderr_text.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&stderr_text);
        }

        if combined.is_empty() {
            combined.push_str("(no output)");
        }
        if left_running {
            combined.push_str(
                "\n[the command exited but processes it started in the background were \
                 still holding its output open; they have been stopped. Run a long-lived \
                 process with the Monitor tool instead]",
            );
        }
        let is_error = status.as_ref().is_none_or(|s| !s.success);
        match &status {
            None => combined.push_str(&format!("\n[timed out after {}ms]", timeout.as_millis())),
            Some(s) if !s.success => match s.code {
                Some(code) => combined.push_str(&format!("\n[exit code: {code}]")),
                None => combined.push_str("\n[killed by signal]"),
            },
            Some(_) => {}
        }

        let mut result = if is_error {
            ToolResult::error_text(combined)
        } else {
            ToolResult::text(combined)
        };
        result.structured_content = Some(structured);
        Ok(result)
    }
}

fn exec_error(e: base::interface::exec::ExecError) -> ToolError {
    e.into()
}

#[derive(Debug, Clone, Copy)]
pub struct CmdClassification {
    pub read_only: bool,
    pub destructive: bool,
}

/// Detect `sleep N` where N >= 2 (blocking poll patterns). Single
/// `sleep 1` is allowed; longer sleeps should use the Monitor tool.
fn is_long_sleep(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if !trimmed.starts_with("sleep ") && !trimmed.starts_with("sleep\t") {
        return false;
    }
    trimmed
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse::<f64>().ok())
        .map(|n| n >= 2.0)
        .unwrap_or(false)
}

/// A `for` / `while` / `until` loop and nothing else, whose rounds only
/// sleep (2s or more) and check or print something trivial — the shape a
/// model reaches for to wait out background work.
fn is_sleep_loop(cmd: &str) -> bool {
    let trimmed = cmd.trim().trim_end_matches(';').trim_end();
    let opens_a_loop = matches!(
        trimmed.split_whitespace().next(),
        Some("for" | "while" | "until")
    );
    if !opens_a_loop || !trimmed.ends_with("done") {
        return false;
    }
    let mut sleeps = false;
    for segment in permissions::shell::simple_commands(trimmed) {
        let words = permissions::shell::words(&segment);
        match words.first().map(String::as_str) {
            Some("sleep") if is_long_sleep(&segment) => sleeps = true,
            Some(
                "for" | ":" | "true" | "false" | "echo" | "printf" | "test" | "[" | "[[" | "seq"
                | "break" | "continue",
            ) => {}
            _ => return false,
        }
    }
    sleeps
}

/// Background tasks report their end as a notice delivered once the current
/// tool call returns, so a command that sleeps until they finish never sees
/// them finish.
fn wait_with_task_wait(running: &[String]) -> String {
    let ids = serde_json::to_string(running).unwrap_or_default();
    format!(
        "Background tasks are still running ({}); their completion notices arrive only after \
         this tool call returns, so sleeping here cannot see them end. Wait with TaskWait \
         {{\"task_ids\": {ids}, \"mode\": \"all\", \"timeout_ms\": 600000}} instead.",
        running.join(", ")
    )
}

fn parse_classification(input: &Value) -> Option<CmdClassification> {
    let parsed = serde_json::from_value::<BashInput>(input.clone()).ok()?;
    Some(classify::classify(&parsed.command))
}
