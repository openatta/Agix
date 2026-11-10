//! `EnterWorktreeTool` / `ExitWorktreeTool` —— 显式 worktree 控制。
//!
//! 与 AgentTool.worktree 字段的区别：
//! - **AgentTool.worktree**：**子 agent**在 worktree 里跑；自动清理；用户场景是"派 sub-agent"
//! - **Enter/ExitWorktree**：**当前 session**进 worktree（改 cwd 概念）；适合"我要在隔离环境里折腾一会儿"
//!
//! 会话的 cwd 不变：EnterWorktree 返回 worktree 的绝对路径，模型用绝对路径
//! 或先 `Bash cd` 进去才写得到那里（worktree 在仓库根的 `.agix/worktrees/`
//! 下，本来就在会话可写范围内）。会话同时只有一个 active worktree；
//! ExitWorktree `keep` 只让出它，`remove` 删目录和分支，拒绝删除时它仍是
//! active 的。会话结束时还 active 的 worktree 留在磁盘上。

use crate::worktree::WorktreeHandle;
use async_trait::async_trait;
use base::engine_exec::GitAccess;
use base::error::ToolError;
use base::tool::{
    PathStyle, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ToolSpec, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

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
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|h| {
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
    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<EnterWorktreeInput>()
    }
    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("prompts/coding/worktree_enter.prompt.md").to_string())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
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
        match crate::worktree::create_worktree(
            &ctx.exec,
            &crate::bash::engine_exec(&ctx),
            &ctx.cwd,
            &input.slug,
        )
        .await
        {
            Ok(handle) => {
                let path = handle.path().to_path_buf();
                let branch = handle.branch().to_string();
                // Store the handle directly — cleanup() takes &mut self,
                // so ExitWorktree can call it through the Mutex.
                *self
                    .registry
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
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
    /// "keep" = exit and leave the worktree on disk; "remove" = exit and delete it (default)
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
    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<ExitWorktreeInput>()
    }
    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("prompts/coding/worktree_exit.prompt.md").to_string())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().destructive(true)
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

        let Some(mut handle) = self.take() else {
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
            return Ok(ToolResult {
                content: base::tool::ToolResultContent::Text(format!(
                    "Exited the worktree session. The worktree stays on disk at {} (branch {}).",
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

        if !discard {
            let unsaved = unsaved_work(&ctx, &path, &branch).await;
            if !unsaved.is_empty() {
                self.put_back(handle);
                return Ok(ToolResult {
                    content: base::tool::ToolResultContent::Text(format!(
                        "Refused to remove the worktree: it has {}. The worktree session is \
                         still active. Use discard_changes: true to force removal.",
                        unsaved.join(" and ")
                    )),
                    is_error: true,
                    structured_content: None,
                    mcp_meta: None,
                    new_messages: Some(vec![]),
                });
            }
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
    }
}

impl ExitWorktreeTool {
    fn take(&self) -> Option<WorktreeHandle> {
        self.registry
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn put_back(&self, handle: WorktreeHandle) {
        *self
            .registry
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }
}

/// What removing the worktree would destroy: uncommitted changes, and
/// commits on its branch that no other branch or remote-tracking ref has.
///
/// A `git` that could not be run at all answers "nothing", because the
/// caller's only use for the answer is whether to refuse a removal the user
/// asked for, and refusing on an unanswerable question strands the worktree.
async fn unsaved_work(ctx: &ToolContext, path: &std::path::Path, branch: &str) -> Vec<String> {
    let git = crate::bash::engine_exec(ctx);
    let run = |args: Vec<String>| {
        let git = git.clone();
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = tokio::select! {
                out = git.git(path, GitAccess::ReadOnly, &args, GIT_TIMEOUT) => out.ok()?,
                () = ctx.cancel.cancelled() => return None,
            };
            out.status
                .success
                .then(|| out.stdout_lossy().trim().to_string())
        }
    };
    let mut unsaved = Vec::new();
    if run(vec!["status".into(), "--porcelain".into()])
        .await
        .is_some_and(|out| !out.is_empty())
    {
        unsaved.push("uncommitted changes".to_string());
    }
    let only_here = run(vec![
        "rev-list".into(),
        "--count".into(),
        "HEAD".into(),
        "--not".into(),
        format!("--exclude={branch}"),
        "--branches".into(),
        "--remotes".into(),
    ])
    .await
    .and_then(|n| n.parse::<u64>().ok())
    .unwrap_or(0);
    if only_here > 0 {
        unsaved.push(format!(
            "{only_here} commit(s) on {branch} that no other branch has"
        ));
    }
    unsaved
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
                &platform::tool_context_for_test("/tmp".into()),
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

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    struct Session {
        repo: tempfile::TempDir,
        enter: EnterWorktreeTool,
        exit: ExitWorktreeTool,
        registry: Arc<WorktreeRegistry>,
    }

    impl Session {
        fn new() -> Self {
            let repo = tempfile::TempDir::new().unwrap();
            git(repo.path(), &["init", "-q"]);
            std::fs::write(repo.path().join("README.md"), "# test").unwrap();
            git(repo.path(), &["add", "."]);
            git(repo.path(), &["commit", "-qm", "init"]);
            let registry = Arc::new(WorktreeRegistry::new());
            Self {
                repo,
                enter: EnterWorktreeTool::new(registry.clone()),
                exit: ExitWorktreeTool::new(registry.clone()),
                registry,
            }
        }

        fn ctx(&self) -> ToolContext {
            platform::tool_context_for_test(self.repo.path().to_path_buf())
        }

        async fn enter(&self, slug: &str) -> ToolResult {
            self.enter
                .call(
                    json!({ "slug": slug }),
                    self.ctx(),
                    ProgressSender::noop("t"),
                )
                .await
                .unwrap()
        }

        async fn exit(&self, input: Value) -> ToolResult {
            self.exit
                .call(input, self.ctx(), ProgressSender::noop("t"))
                .await
                .unwrap()
        }

        fn tree(&self, slug: &str) -> std::path::PathBuf {
            self.repo.path().join(".agix/worktrees").join(slug)
        }
    }

    fn text(r: &ToolResult) -> String {
        match &r.content {
            base::tool::ToolResultContent::Text(t) => t.clone(),
            other => format!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn keep_exits_the_session_and_leaves_the_worktree_on_disk() {
        let s = Session::new();
        assert!(!s.enter("keep-a").await.is_error);
        let kept = s.exit(json!({ "action": "keep" })).await;
        assert!(!kept.is_error, "{}", text(&kept));
        assert!(s.registry.current().is_none());
        assert!(s.tree("keep-a").exists());
        let next = s.enter("next-b").await;
        assert!(!next.is_error, "{}", text(&next));
        assert!(s.tree("next-b").exists());
    }

    #[tokio::test]
    async fn a_refused_removal_keeps_the_session_in_the_worktree() {
        let s = Session::new();
        assert!(!s.enter("dirty").await.is_error);
        std::fs::write(s.tree("dirty").join("new.txt"), "x").unwrap();
        let refused = s.exit(json!({ "action": "remove" })).await;
        assert!(refused.is_error);
        assert!(text(&refused).contains("uncommitted"), "{}", text(&refused));
        assert!(s.registry.current().is_some());
        let removed = s
            .exit(json!({ "action": "remove", "discard_changes": true }))
            .await;
        assert!(!removed.is_error, "{}", text(&removed));
        assert!(!s.tree("dirty").exists());
    }

    #[tokio::test]
    async fn commits_only_the_worktree_branch_has_refuse_a_removal() {
        let s = Session::new();
        assert!(!s.enter("committed").await.is_error);
        let tree = s.tree("committed");
        std::fs::write(tree.join("work.txt"), "x").unwrap();
        git(&tree, &["add", "."]);
        git(&tree, &["commit", "-qm", "work"]);
        let refused = s.exit(json!({ "action": "remove" })).await;
        assert!(refused.is_error);
        assert!(text(&refused).contains("1 commit"), "{}", text(&refused));
        assert!(tree.exists());
    }

    #[tokio::test]
    async fn a_clean_worktree_is_removed_without_discard() {
        let s = Session::new();
        assert!(!s.enter("clean").await.is_error);
        let removed = s.exit(json!({ "action": "remove" })).await;
        assert!(!removed.is_error, "{}", text(&removed));
        assert!(!s.tree("clean").exists());
        assert!(s.registry.current().is_none());
    }
}
