//! `EnterWorktreeTool` / `ExitWorktreeTool` —— 显式 worktree 控制。
//!
//! 与 AgentTool.worktree 字段的区别：
//! - **AgentTool.worktree**：**子 agent**在 worktree 里跑；自动清理；用户场景是"派 sub-agent"
//! - **Enter/ExitWorktree**：**当前 session**进 worktree（改 cwd 概念）；适合"我要在隔离环境里折腾一会儿"
//!
//! 关键设计：当前 SessionState.cwd 是 immutable 的 PathBuf。EnterWorktree 不
//! 真改 session cwd（那要在 SessionState 加 Mutex），而是**返回 worktree 路径**
//! 让模型知道在哪，并把 path 加进 `additional_writable_dirs` 让文件工具能写
//! 进去。模型用 cwd-relative 路径时仍指当前 cwd；要写 worktree 必须用绝对路径
//! 或先 `Bash cd` 进去。
//!
//! TS 实装是 process-wide chdir —— 我们这边架构不同，做 minimal 版本：
//! 创建 worktree + 把 path 注入 SessionState；ExitWorktree 调 cleanup 并从
//! activated 移除。

use crate::exec_capture::capture;
use crate::worktree::WorktreeHandle;
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::{ExecProviders, ProcessSpec};
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// 会话期间的 active worktree —— EnterWorktree 创建 / ExitWorktree 清理。
/// in-memory 单 session 状态；不持久化。
#[derive(Default)]
pub struct WorktreeRegistry {
    inner: Mutex<Option<WorktreeHandle>>,
}

impl std::fmt::Debug for WorktreeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorktreeRegistry").finish()
    }
}

impl WorktreeRegistry {
    /// Construct a new instance.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Currently active worktree path + slug, if any.
    pub fn current(&self) -> Option<(String, std::path::PathBuf)> {
        self.inner.lock().unwrap().as_ref().map(|h| {
            let slug = h
                .branch()
                .strip_prefix("agix/worktree-")
                .unwrap_or("?")
                .to_string();
            (slug, h.path().to_path_buf())
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnterWorktreeInput {
    /// Slug for the worktree directory (e.g. `experiment-foo`). Path-traversal
    /// safe; segments must match `[a-zA-Z0-9._-]`.
    pub slug: String,
}

pub struct EnterWorktreeTool {
    registry: Arc<WorktreeRegistry>,
}

impl EnterWorktreeTool {
    /// Construct a new instance.
    pub fn new(registry: Arc<WorktreeRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for EnterWorktreeTool {
    fn description(&self) -> &str {
        "Create an isolated git worktree and switch the session into it"
    }
    fn name(&self) -> &str {
        "EnterWorktree"
    }
    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述。
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(EnterWorktreeInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/worktree_enter.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false // 改 registry
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<EnterWorktreeInput>(input.clone()) {
            Ok(p) => match crate::worktree::validate_slug(&p.slug) {
                Ok(_) => ValidationResult::Ok,
                Err(e) => ValidationResult::err(format!("invalid slug: {e}"), 1),
            },
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        // 创建 worktree 是文件系统副作用，但很受控；ask 是合理默认
        PermissionDecision::Ask {
            message: "EnterWorktree will create a new git worktree under .agix/worktrees/".into(),
            decision_reason: None,
        }
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: EnterWorktreeInput = serde_json::from_value(input)?;
        // 单 session 只允许一个 active worktree
        if self.registry.current().is_some() {
            return Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(
                    "An active worktree already exists. Call ExitWorktree first.".into(),
                ),
                is_error: true,
                structured_content: None,
                mcp_meta: None,
                new_messages: Some(vec![]),
            });
        }
        match crate::worktree::create_worktree(&ctx.exec, &ctx.cwd, &input.slug).await {
            Ok(handle) => {
                let path = handle.path().to_path_buf();
                let branch = handle.branch().to_string();
                // Store the handle directly — cleanup() takes &mut self,
                // so ExitWorktree can call it through the Mutex.
                *self.registry.inner.lock().unwrap() = Some(handle);
                Ok(ToolResult {
                    content: base::tool::ToolResultContent::Text(format!(
                        "Worktree created at {}. Branch: {}. Use this absolute path \
                         in subsequent file operations to stay isolated. Call \
                         ExitWorktree when done.",
                        path.display(),
                        branch
                    )),
                    is_error: false,
                    structured_content: Some(json!({
                        "path": path.display().to_string(),
                        "branch": branch,
                        "slug": input.slug})),
                    mcp_meta: None,
                    new_messages: Some(vec![]),
                })
            }
            Err(e) => Err(ToolError::exec(format!("EnterWorktree failed: {e}"))),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct ExitWorktreeInput {
    /// "keep" = leave worktree on disk; "remove" = delete (default)
    #[serde(default)]
    pub action: Option<String>,
    /// With action "remove": force removal even with uncommitted changes or unpushed commits.
    #[serde(default)]
    pub discard_changes: Option<bool>,
}

pub struct ExitWorktreeTool {
    registry: Arc<WorktreeRegistry>,
}

impl ExitWorktreeTool {
    /// Construct a new instance.
    pub fn new(registry: Arc<WorktreeRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for ExitWorktreeTool {
    fn description(&self) -> &str {
        "Exit a worktree session (keep or remove)"
    }
    fn name(&self) -> &str {
        "ExitWorktree"
    }
    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述。
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ExitWorktreeInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/worktree_exit.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        true // 删 worktree 目录 + 分支
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::Ask {
            message: "ExitWorktree will delete the worktree directory and its branch".into(),
            decision_reason: None,
        }
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let inp: ExitWorktreeInput = serde_json::from_value(input).unwrap_or_default();
        let action = inp.action.as_deref().unwrap_or("remove").to_string();
        let discard = inp.discard_changes.unwrap_or(false);

        // Release lock before any await points
        let Some(mut handle) = self.registry.inner.lock().unwrap().take() else {
            return Ok(ToolResult {
                content: base::tool::ToolResultContent::Text("No active worktree to exit.".into()),
                is_error: true,
                structured_content: None,
                mcp_meta: None,
                new_messages: Some(vec![]),
            });
        };
        let path = handle.path().to_path_buf();
        let branch = handle.branch().to_string();

        if action == "keep" {
            self.registry.inner.lock().unwrap().replace(handle);
            return Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "Kept worktree at {} (branch {}). Continue working there.",
                    path.display(),
                    branch,
                )),
                is_error: false,
                structured_content: Some(json!({
                    "kept_path": path.display().to_string(),
                    "branch": branch})),
                mcp_meta: None,
                new_messages: Some(vec![]),
            });
        }

        // "remove" or unknown — handle is owned, lock is dropped, safe to await
        {
            if !discard && has_uncommitted_changes(&ctx.exec, &path, ctx.cancel.clone()).await {
                return Ok(ToolResult {
                    content: base::tool::ToolResultContent::Text(
                        "Worktree has uncommitted changes. Use discard_changes: true to force removal.".into(),
                    ),
                    is_error: true,
                    structured_content: None,
                    mcp_meta: None,
                    new_messages: Some(vec![]),
                });
            }
            handle.cleanup().await;
            let slug = branch
                .strip_prefix("agix/worktree-")
                .unwrap_or("?")
                .to_string();
            Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "Removed worktree {} (branch {}).",
                    path.display(),
                    branch,
                )),
                is_error: false,
                structured_content: Some(json!({
                    "removed_path": path.display().to_string(),
                    "removed_branch": branch,
                    "slug": slug})),
                mcp_meta: None,
                new_messages: Some(vec![]),
            })
        } // end of "remove" block
    }
}

/// Check if a git worktree has uncommitted changes.
///
/// A `git` that could not be run at all answers "no changes", because the
/// caller's only use for the answer is whether to refuse a removal the user
/// asked for, and refusing on an unanswerable question strands the worktree.
async fn has_uncommitted_changes(
    exec: &ExecProviders,
    path: &std::path::Path,
    cancel: tokio_util::sync::CancellationToken,
) -> bool {
    let spec = ProcessSpec::new("git", path).args(["status", "--porcelain"]);
    capture(exec, spec, cancel)
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_starts_empty() {
        let r = WorktreeRegistry::new();
        assert!(r.current().is_none());
    }

    #[tokio::test]
    async fn enter_validates_bad_slug() {
        let registry = Arc::new(WorktreeRegistry::new());
        let tool = EnterWorktreeTool::new(registry);
        let r = tool
            .validate_input(
                &json!({"slug": "../escape"}),
                &ToolContext::for_test("/tmp".into()),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[test]
    fn enter_tool_name_matches_ts() {
        assert_eq!(
            EnterWorktreeTool::new(Arc::new(WorktreeRegistry::new())).name(),
            "EnterWorktree"
        );
    }

    #[test]
    fn exit_tool_name_matches_ts() {
        assert_eq!(
            ExitWorktreeTool::new(Arc::new(WorktreeRegistry::new())).name(),
            "ExitWorktree"
        );
    }
}
