//! `VerifyPlanExecutionTool` —— gated behind `AGIX_VERIFY_PLAN=true` env var.
//!
//! Reads the current plan and compares it against the working-tree changes
//! (git diff) so the model can verify whether the implementation matches the plan.
//!
//! The tool itself does **not** evaluate correctness — it surfaces the plan + diff
//! for the model to reason about. The model's response to the tool result serves as
//! the verification step, which is recorded in the transcript for review.
//!
//! # Gating
//!
//! Only registered when `AGIX_VERIFY_PLAN=true` is set.

use crate::exec_capture::capture;
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::{ExecError, ExecProviders, ProcessSpec};
use base::tool::{
    PermissionDecision, ProgressSender, Tool, ToolContext, ToolResult, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifyPlanExecutionInput {
    /// Optional note about what aspect to verify (e.g. "check auth logic",
    /// "verify all planned files were created"). If empty, the full plan is checked.
    #[serde(default)]
    pub focus: Option<String>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct VerifyPlanExecutionTool;

#[async_trait]
impl Tool for VerifyPlanExecutionTool {
    fn description(&self) -> &str {
        "Verify that a plan implementation step was completed correctly"
    }
    fn name(&self) -> &str {
        "VerifyPlanExecution"
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(VerifyPlanExecutionInput))
            .expect("schemars output is valid JSON")
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    fn is_destructive(&self, _: &Value) -> bool {
        false
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        if let Err(e) = serde_json::from_value::<VerifyPlanExecutionInput>(input.clone()) {
            return ValidationResult::err(format!("invalid input: {e}"), 1);
        }
        ValidationResult::Ok
    }

    async fn prompt(&self, _: &base::tool::PromptContext) -> String {
        "- Verify that previously planned steps have been executed correctly\n\
         - Use after completing implementation to confirm all plan items are addressed\n\
         - Returns a checklist comparing planned vs. actual state\n\
         - If discrepancies are found, describe what remains to be done"
            .into()
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: VerifyPlanExecutionInput = serde_json::from_value(input)?;

        // 1. Read the plan text from plan mode state.
        let plan = crate::plan_mode::plan_text(&ctx).ok_or_else(|| {
            ToolError::exec("No active plan found. Call EnterPlanMode first.".to_string())
        })?;

        // 2. Run `git diff` in the session cwd to get working-tree changes.
        let diff_output = run_git_diff(&ctx.exec, &ctx.cwd, ctx.cancel.clone()).await;

        // 3. Build verification report.
        let focus_note = input
            .focus
            .as_ref()
            .map(|f| format!("\n\n## Verification Focus\n{f}"))
            .unwrap_or_default();

        let mut report = String::new();
        report.push_str("# Plan Verification Report\n\n");
        report.push_str(&format!("## Active Plan\n\n{plan}\n"));
        report.push_str(&format!(
            "\n## Working-Tree Changes (git diff)\n\n```diff\n{diff_output}\n```\n",
        ));
        report.push_str(&focus_note);
        report.push_str(
            "\n\n---\n\
             Review the plan against the changes above. Does the implementation match \
             the plan? If not, describe what still needs to be done.",
        );

        Ok(ToolResult::text(report))
    }
}

async fn run_git_diff(
    exec: &ExecProviders,
    cwd: &std::path::Path,
    cancel: tokio_util::sync::CancellationToken,
) -> String {
    let spec = ProcessSpec::new("git", cwd).args(["diff", "--no-color"]);

    match capture(exec, spec, cancel).await {
        Ok(o) if o.status.success => {
            let stdout = o.stdout_lossy().into_owned();
            if stdout.trim().is_empty() {
                "(no unstaged changes). ".to_string()
            } else {
                // Truncate very large diffs to avoid blowing the context.
                // Walk back from 16_384 to a UTF-8 char boundary (MSRV 1.85 compat).
                if stdout.len() > 16_384 {
                    let mut boundary = 16_384.min(stdout.len());
                    while !stdout.is_char_boundary(boundary) {
                        boundary -= 1;
                    }
                    format!("{}\n\n... (diff truncated at 16 KB)", &stdout[..boundary])
                } else {
                    stdout
                }
            }
        }
        Ok(o) => {
            format!("(git diff failed: {})", o.stderr_lossy())
        }
        // The three classes stay apart: telling the model "git is not
        // available" when the execution environment is the thing that is down
        // sends it looking for the wrong problem.
        Err(ExecError::Unavailable(e)) => {
            format!("(execution environment unavailable: {e})")
        }
        Err(ExecError::Denied(e)) => {
            format!("(git diff refused by policy: {e})")
        }
        Err(ExecError::Failed(e)) => {
            format!("(git not available: {e})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Mutex;

    fn ctx() -> ToolContext {
        ToolContext::for_test(PathBuf::from("/tmp"))
    }

    /// Serialize access to the AGIX_PLAN_STORE_DIR env var across tests
    /// in this binary that depend on plan_store::base_dir() resolution.
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn verify_requires_active_plan() {
        let tool = VerifyPlanExecutionTool;
        let c = ctx();
        let r = tool
            .call(serde_json::json!({}), c, ProgressSender::noop("t"))
            .await;
        assert!(r.is_err());
        let err = r.unwrap_err().to_string();
        assert!(
            err.contains("No active plan"),
            "expected plan-required error, got: {err}"
        );
    }

    #[tokio::test]
    async fn verify_with_active_plan_succeeds() {
        let c = ctx();
        // Plan state lives on this context's own `SessionState`.
        crate::plan_mode::plan_state_for_test(&c, "Implement login flow".to_string());
        let tool = VerifyPlanExecutionTool;
        let r = tool
            .call(
                serde_json::json!({"focus": "check auth"}),
                c,
                ProgressSender::noop("t"),
            )
            .await;
        // Should succeed and include plan text in the report.
        assert!(r.is_ok(), "expected ok, got: {:?}", r.err());
        let report = r.unwrap();
        match report.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(
                    t.contains("Implement login flow"),
                    "report should contain plan text"
                );
                assert!(
                    t.contains("Verification Focus"),
                    "report should include focus"
                );
            }
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn verify_safety_flags() {
        let tool = VerifyPlanExecutionTool;
        assert!(tool.is_read_only(&Value::Null));
        assert!(!tool.is_destructive(&Value::Null));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn verify_with_real_git_repo() {
        // Check git is available before creating any state.
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("SKIP: git not installed, skipping git-dependent test");
            return;
        }

        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let plan_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plan_dir).unwrap();

        // Init git repo with one committed file.
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["-c", "commit.gpgsign=false", "commit", "-m", "initial"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        // Make uncommitted changes.
        std::fs::write(
            dir.path().join("main.rs"),
            "fn main() { println!(\"hello\"); }",
        )
        .unwrap();

        // Create tool context with working dir = the temp git repo, then put
        // *its* session into plan mode.
        let c = ToolContext::for_test(dir.path().to_path_buf());
        crate::plan_mode::plan_state_for_test(
            &c,
            "Add hello world\nStep 1: modify main.rs".to_string(),
        );

        let tool = VerifyPlanExecutionTool;
        let r = tool
            .call(serde_json::json!({}), c, ProgressSender::noop("t"))
            .await;
        assert!(r.is_ok(), "expected ok, got: {:?}", r.err());
        let report = r.unwrap();
        match report.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(
                    t.contains("Add hello world"),
                    "report should contain plan text"
                );
                assert!(t.contains("println"), "report should contain git diff");
                assert!(t.contains("main.rs"), "report should mention changed file");
            }
            _ => panic!("expected Text content"),
        }

        // Cleanup env var.
        std::env::remove_var("AGIX_PLAN_STORE_DIR");
    }
}
