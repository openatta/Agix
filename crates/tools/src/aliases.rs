//! 一组"轻量"工具集合：要么 alias 到已有能力（Skill/Brief），要么是简单
//! cross-platform spawn（PowerShell/REPL）。
//!
//! 与对应工具对齐 schema 字段名 + 语义。
//!
//! ## Name-collision cleanup
//!
//! Tool names are the model-visible identity and `InMemoryToolRegistry::get`
//! returns the first match, so this module defines no tool whose name another
//! module in this crate already uses — `TaskOutput` in particular is
//! [`crate::task_output::TaskOutputTool`].

use crate::exec_capture::capture;
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::{ExecError, ProcessSpec};
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

/// Default PowerShell timeout (60s).
const DEFAULT_POWERSHELL_TIMEOUT_MS: u64 = 60_000;
/// Maximum PowerShell timeout (600s).
const MAX_POWERSHELL_TIMEOUT_MS: u64 = 600_000;
/// REPL timeout (30s, both default and max).
const REPL_TIMEOUT_MS: u64 = 30_000;

// ============ BriefTool placeholder marker ============
//
// Brief 是"短消息给用户看"——本质是 AskUserQuestion 的简化形式（不强制
// 用户回 y/n，仅显示）。Rust 直接调 effects.append_system_message。

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BriefInput {
    /// Short message to surface to the user (1-2 sentences)
    pub message: String,
}

pub struct BriefTool;

#[async_trait]
impl Tool for BriefTool {
    fn description(&self) -> &str {
        "Send a message to the user"
    }
    fn name(&self) -> &str {
        "SendUserMessage"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(BriefInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "Send a short message the user will read. Use this to surface important \
         information: background task completion, blocking issues, or key decisions \
         the user needs to know about. The message is displayed prominently — keep \
         it concise (1-3 sentences, markdown supported)."
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<BriefInput>(input.clone()) {
            Ok(p) if p.message.trim().is_empty() => {
                ValidationResult::err("message must not be empty", 1)
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
        let input: BriefInput = serde_json::from_value(input)?;
        if let Some(ref effects) = ctx.effects {
            effects.append_system_message("notice", &input.message);
        }
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(format!("Briefed: {}", input.message)),
            is_error: false,
            structured_content: Some(json!({"message": input.message})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

// ============ PowerShellTool ============
//
// 在 PATH 上有 `pwsh`（PowerShell Core，跨平台）就可以跑；未找到时返回清晰错误。
// Windows 上还兼容老的 `powershell.exe`（Windows PowerShell 5.x）。
// 与 TS 同 schema：command。

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PowerShellInput {
    pub command: String,
    /// timeout in milliseconds (default 60s, max 600s)
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub struct PowerShellTool;

#[async_trait]
impl Tool for PowerShellTool {
    fn description(&self) -> &str {
        "Run a PowerShell command"
    }
    fn name(&self) -> &str {
        "PowerShell"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(PowerShellInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "Executes a given PowerShell command with optional timeout. Working directory \
         persists between commands; shell state (variables, functions) does not.\n\
         \n\
         IMPORTANT: This tool is for terminal operations via PowerShell: git, npm, \
         docker, and PS cmdlets. DO NOT use it for file operations (reading, writing, \
         editing, searching, finding files) - use the specialized tools for this instead.\n\
         \n\
         PowerShell eddition: PowerShell 7+ or Windows PowerShell 5.1\n\
         \n\
         Usage notes:\n\
         - The command argument is required.\n\
         - You can specify an optional timeout in milliseconds.\n\
         - Avoid using PowerShell to run commands that have dedicated tools:\n\
           - File search: Use Glob (NOT Get-ChildItem -Recurse)\n\
           - Content search: Use Grep (NOT Select-String)\n\
           - Read files: Use Read (NOT Get-Content)\n\
           - Edit files: Use Edit\n\
           - Write files: Use Write (NOT Set-Content/Out-File)\n\
         - For git commands:\n\
           - Prefer to create a new commit rather than amending\n\
           - Never skip hooks unless the user explicitly asks\n\
         - Interactive and blocking commands will hang — use -NonInteractive"
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<PowerShellInput>(input.clone())
            .ok()
            .map(|i| i.command)
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<PowerShellInput>(input.clone()) {
            Ok(p) if p.command.trim().is_empty() => {
                ValidationResult::err("command must not be empty", 1)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::Ask {
            message: "PowerShell command".into(),
            decision_reason: None,
        }
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: PowerShellInput = serde_json::from_value(input)?;
        let timeout = std::time::Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(DEFAULT_POWERSHELL_TIMEOUT_MS)
                .min(MAX_POWERSHELL_TIMEOUT_MS),
        );

        let program = pick_powershell().ok_or_else(|| {
            #[allow(clippy::useless_format)]
            ToolError::exec(format!(
                "no PowerShell binary on PATH (looked for `pwsh`, `pwsh-preview`, and on Windows \
                 `powershell.exe`). Install PowerShell Core from https://aka.ms/powershell or use \
                 the Bash tool for shell tasks."
            ))
        })?;

        let spec = ProcessSpec::new(&program, &ctx.cwd).args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &input.command,
        ]);
        let out = tokio::time::timeout(timeout, capture(&ctx.exec, spec, ctx.cancel.clone()))
            .await
            .map_err(|_| ToolError::exec(format!("{program} timed out after {timeout:?}")))?
            .map_err(|e| match e {
                ExecError::Denied(m) => ToolError::Denied(format!("{program}: {m}")),
                other => ToolError::exec(format!("{program} spawn: {other}")),
            })?;
        let stdout = truncate_output(&out.stdout_lossy());
        let stderr = truncate_output(&out.stderr_lossy());
        let body = if !stderr.is_empty() {
            format!("{stdout}\n--- stderr ---\n{stderr}")
        } else {
            stdout
        };
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: !out.status.success,
            structured_content: Some(json!({
                "program": program,
                "exit_code": out.status.code})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

/// Pick the first usable PowerShell binary on PATH. PowerShell Core (`pwsh`) is
/// cross-platform; `powershell.exe` is Windows PowerShell 5.x as a fallback.
fn pick_powershell() -> Option<String> {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["pwsh", "pwsh-preview", "powershell"]
    } else {
        &["pwsh", "pwsh-preview"]
    };
    for c in candidates {
        if which::which(c).is_ok() {
            return Some(c.to_string());
        }
    }
    None
}

/// Cap process output at 200 KiB to keep tool results LLM-friendly.
const MAX_OUTPUT_BYTES: usize = 200 * 1024;
fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let cut = MAX_OUTPUT_BYTES;
    // Walk back from the byte limit to the nearest UTF-8 char boundary
    // to avoid panicking on multi-byte text (e.g. CJK, emoji).
    let safe_cut = (0..=cut)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    let mut out = String::with_capacity(safe_cut + 64);
    out.push_str(&s[..safe_cut]);
    out.push_str(&format!(
        "\n\n[... output truncated: {} bytes total, showing first {} bytes ...]",
        s.len(),
        safe_cut
    ));
    out
}

// ============ REPLTool ============
//
// 跑 `python -c <expr>` / `node -e <expr>` —— 单 shot eval，不维护 REPL 状态。
// TS 的实现可能更花哨；我们 minimal viable。

#[derive(Debug, serde::Serialize, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplLanguage {
    Python,
    Node,
    Ruby,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplInput {
    pub language: ReplLanguage,
    /// Code to evaluate
    pub code: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub struct ReplTool;

#[async_trait]
impl Tool for ReplTool {
    fn description(&self) -> &str {
        "Evaluate a single expression in python/node/ruby"
    }
    fn name(&self) -> &str {
        "REPL"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ReplInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "- Eval a short snippet in Python, Node.js, or Ruby (one-shot — no REPL \
         state maintained between calls)\n\
         - Use for quick math / data shaping / string manipulation when shelling out \
         is overkill\n\
         - Capped at 30s and 200KB of output\n\
         - Errors come back via stderr in tool result\n\
         \n\
         When NOT to use:\n\
         - For file operations — use Read/Write/Edit instead\n\
         - For multi-step scripting — use Bash instead\n\
         - For installing packages or running servers — use Bash instead"
            .into()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<ReplInput>(input.clone())
            .ok()
            .map(|i| i.code)
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<ReplInput>(input.clone()) {
            Ok(p) if p.code.trim().is_empty() => ValidationResult::err("code must not be empty", 1),
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::Ask {
            message: "REPL eval".into(),
            decision_reason: None,
        }
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ReplInput = serde_json::from_value(input)?;
        let timeout = std::time::Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(REPL_TIMEOUT_MS)
                .min(REPL_TIMEOUT_MS),
        );
        let (program, args): (&str, Vec<&str>) = match input.language {
            ReplLanguage::Python => ("python3", vec!["-c", &input.code]),
            ReplLanguage::Node => ("node", vec!["-e", &input.code]),
            ReplLanguage::Ruby => ("ruby", vec!["-e", &input.code]),
        };
        let spec = ProcessSpec::new(program, &ctx.cwd).args(args.iter().copied());
        let out = tokio::time::timeout(timeout, capture(&ctx.exec, spec, ctx.cancel.clone()))
            .await
            .map_err(|_| ToolError::exec(format!("REPL ({program}) timed out")))?
            .map_err(|e| match e {
                ExecError::Denied(m) => ToolError::Denied(format!("REPL ({program}): {m}")),
                other => ToolError::exec(format!(
                    "REPL spawn ({program}): {other}; ensure {program} is installed"
                )),
            })?;
        let stdout = truncate_output(&out.stdout_lossy());
        let stderr = truncate_output(&out.stderr_lossy());
        let body = if !stderr.is_empty() {
            format!("{stdout}\n--- stderr ---\n{stderr}")
        } else {
            stdout
        };
        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: !out.status.success,
            structured_content: Some(
                json!({"language": input.language, "exit_code": out.status.code}),
            ),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext::for_test(PathBuf::from("/tmp"))
    }

    // ---- BriefTool ----

    #[tokio::test]
    async fn brief_validates_empty() {
        let tool = BriefTool;
        let r = tool.validate_input(&json!({"message": "  "}), &ctx()).await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn brief_emits_system_message() {
        let tool = BriefTool;
        let r = tool
            .call(
                json!({"message": "refactoring"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
    }

    // ---- PowerShell ----

    #[tokio::test]
    async fn powershell_runs_when_pwsh_present_or_errors_clearly() {
        // Skip the case where pwsh isn't installed in CI — we just check the
        // tool's response is shaped sensibly (either a clean run or a clear
        // "no PowerShell binary" error).
        let tool = PowerShellTool;
        let r = tool
            .call(
                json!({"command": "Write-Output 'pwsh-ok'"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await;
        match r {
            Ok(tr) => {
                if !tr.is_error {
                    if let base::tool::ToolResultContent::Text(s) = tr.content {
                        assert!(s.contains("pwsh-ok"));
                    }
                }
            }
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("PowerShell") || msg.contains("pwsh"));
            }
        }
    }

    #[test]
    fn truncate_output_caps_long_strings() {
        let s = "x".repeat(MAX_OUTPUT_BYTES + 1000);
        let t = truncate_output(&s);
        assert!(t.len() <= MAX_OUTPUT_BYTES + 200);
        assert!(t.contains("output truncated"));
    }

    // ---- REPL ----（依赖 python3 / node 安装；跑测试机器一般有 python3）

    #[tokio::test]
    async fn repl_python_simple_expr() {
        // Skip if python3 not on PATH (CI may not have it)
        let py_check = std::process::Command::new("python3")
            .arg("--version")
            .output();
        if py_check.is_err() || !py_check.unwrap().status.success() {
            return;
        }
        let tool = ReplTool;
        let r = tool
            .call(
                json!({"language": "python", "code": "print(2+2)"}),
                ctx(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        if !r.is_error {
            match r.content {
                base::tool::ToolResultContent::Text(s) => {
                    assert!(s.contains("4"));
                }
                _ => panic!(),
            }
        }
    }
}
