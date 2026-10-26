//! Git worktree 管理器 —— 给 sub-agent 提供独立 checkout 的隔离运行环境。
//!
//! 本模块不引入 git 库依赖，全靠 `git` 子命令。
//!
//! ## 用法
//!
//! ```ignore
//! let mut handle = create_worktree(repo_root, "probe").await?;
//! // sub-agent 在 handle.path() 跑
//! handle.cleanup().await?;
//! ```
//!
//! ## 边界
//!
//! - 非 git 仓库 → 立即 `NotAGitRepo` 错
//! - slug 必须通过 [`validate_slug`]（防 path traversal）
//! - 同一 slug 路径已存在 → `AlreadyExists`（首层不做 resume；交给 phase 1c
//!   决定要不要做）
//! - cleanup 失败仅 warn，不传播 —— sub-agent 主要工作已完成

use crate::exec_capture::capture;
use base::interface::exec::{ExecError, ExecProviders, ProcessSpec};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

const SLUG_MAX_LEN: usize = 64;
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const WORKTREES_SUBDIR: &str = ".agix/worktrees";
const BRANCH_PREFIX: &str = "agix/worktree-";

/// `WorktreeHandle` —— 创建后持有；drop 时不自动清（避免 sync drop 起 tokio
/// runtime）。caller 必须显式 await `cleanup()`。
#[derive(Debug)]
pub struct WorktreeHandle {
    path: PathBuf,
    branch: String,
    repo_root: PathBuf,
    exec: ExecProviders,
    /// 若已经被 cleanup 过则 false，二次调用变 no-op
    pending: bool,
}

impl WorktreeHandle {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn branch(&self) -> &str {
        &self.branch
    }
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// 删除 worktree 目录 + 关联分支。失败 warn 不报错。
    /// Takes `&mut self` so callers can store the handle in an `Option` and
    /// clean up without consuming ownership.
    pub async fn cleanup(&mut self) {
        if !self.pending {
            return;
        }
        self.pending = false;

        // git worktree remove --force <path>
        if let Err(e) = run_git(
            &self.exec,
            &self.repo_root,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        )
        .await
        {
            warn!(
                path = %self.path.display(),
                error = %e,
                "git worktree remove failed; manual cleanup may be needed (run `git worktree prune`)"
            );
        }

        // git branch -D <branch>
        if let Err(e) = run_git(&self.exec, &self.repo_root, &["branch", "-D", &self.branch]).await
        {
            warn!(
                branch = %self.branch,
                error = %e,
                "git branch -D failed; remove manually if not wanted"
            );
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum WorktreeError {
    #[error("not a git repository: {0}")]
    NotAGitRepo(PathBuf),

    #[error("invalid worktree slug: {0}")]
    InvalidSlug(String),

    #[error("worktree path already exists: {0}")]
    AlreadyExists(PathBuf),

    #[error("git command failed: {0}")]
    GitFailed(String),

    #[error("{0}")]
    Exec(#[from] ExecError),
}

/// 创建 worktree。会:
/// 1. 验 slug
/// 2. 找 repo root（slug 错或非 git 都 fail-fast）
/// 3. 写 `.gitignore` 加 `.agix/`（若未存在）
/// 4. `git worktree add -b <branch> <path> HEAD`
pub async fn create_worktree(
    exec: &ExecProviders,
    cwd: &Path,
    slug: &str,
) -> Result<WorktreeHandle, WorktreeError> {
    validate_slug(slug)?;
    let repo_root = find_git_root(exec, cwd).await?;
    let flat = flatten_slug(slug);
    let path = repo_root.join(WORKTREES_SUBDIR).join(&flat);
    if exec.filesystem.metadata(&path).await.is_ok() {
        return Err(WorktreeError::AlreadyExists(path));
    }
    let branch = format!("{BRANCH_PREFIX}{flat}");

    // 父目录得先存在 git worktree add 才不报错
    if let Some(parent) = path.parent() {
        exec.filesystem.create_dir_all(parent).await?;
    }

    ensure_gitignore_entry(exec, &repo_root, ".agix/").await;

    run_git(
        exec,
        &repo_root,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &path.to_string_lossy(),
            "HEAD",
        ],
    )
    .await?;

    debug!(
        path = %path.display(),
        branch = %branch,
        "worktree created"
    );

    Ok(WorktreeHandle {
        path,
        branch,
        repo_root,
        exec: exec.clone(),
        pending: true,
    })
}

/// 启动时清扫上次会话 / 进程 crash 留下的孤儿 worktree。
///
/// **行为**：
/// - 找当前 cwd 所在 git repo（非 git 仓库时 silently no-op）
/// - 跑 `git worktree prune` —— 清掉 git 元数据里"目录已经不存在"的 worktree
///   引用（这是 git 自带操作；安全）
/// - 扫 `.agix/worktrees/<*>/` 下每个目录：如果对应分支
///   `agix/worktree-<flat>` 还在但 git worktree list 不见这个 worktree
///   注册，就把整个目录 + 分支删了（恢复磁盘空间）
///
/// 设计权衡：**我们只清以 `.agix/worktrees/` 为前缀的目录**。
/// 其它工具的 worktree 不动。
///
/// 失败仅 warn，不阻塞启动 —— 用户体验上 "agix 启动忽然卡住" 比 "孤儿没清"
/// 更糟。
pub async fn prune_orphan_worktrees(exec: &ExecProviders, cwd: &Path) {
    let repo_root = match find_git_root(exec, cwd).await {
        Ok(r) => r,
        Err(_) => return, // 非 git repo，没什么可清的
    };

    // 1. git worktree prune —— 清 git 内部状态
    if let Err(e) = run_git(exec, &repo_root, &["worktree", "prune"]).await {
        debug!(error = %e, "git worktree prune failed; ignoring");
    }

    // 2. 扫 `.agix/worktrees/` 残留
    let worktrees_root = repo_root.join(WORKTREES_SUBDIR);
    let entries = match exec.filesystem.read_dir(&worktrees_root).await {
        Ok(e) => e,
        Err(_) => return, // 目录不存在 → 没有孤儿
    };

    // git worktree list --porcelain 拿活跃列表，比对找孤儿
    let active = run_git_capture(exec, &repo_root, &["worktree", "list", "--porcelain"])
        .await
        .unwrap_or_default();

    let mut pruned = 0usize;
    for entry in entries {
        if !entry.is_dir {
            continue;
        }
        let path = entry.path;
        let path_str = path.to_string_lossy();
        // 在 git worktree list 输出里找这个路径 —— 出现就是活跃的，跳
        if active.contains(path_str.as_ref()) {
            continue;
        }
        // 不活跃 → 是孤儿。删目录 + 删分支
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let branch = format!("{BRANCH_PREFIX}{dir_name}");
        if let Err(e) = exec.filesystem.remove_dir_all(&path).await {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to remove orphan worktree dir"
            );
            continue;
        }
        // 分支可能不存在（创建失败时未必有），失败也 OK
        let _ = run_git(exec, &repo_root, &["branch", "-D", &branch]).await;
        pruned += 1;
    }

    if pruned > 0 {
        debug!(
            count = pruned,
            "pruned orphan worktrees from previous sessions"
        );
    }
}

/// Slug 校验：长度上限 + segment 正则 + 拒 `.` `..`
///
/// 安全性：path traversal 通过 `Path::join` 时，`..` 段会逃出根 —— 这里硬拒。
pub fn validate_slug(s: &str) -> Result<(), WorktreeError> {
    if s.is_empty() {
        return Err(WorktreeError::InvalidSlug("empty".into()));
    }
    if s.len() > SLUG_MAX_LEN {
        return Err(WorktreeError::InvalidSlug(format!(
            "must be {SLUG_MAX_LEN} chars or fewer (got {})",
            s.len()
        )));
    }
    for seg in s.split('/') {
        if seg.is_empty() {
            return Err(WorktreeError::InvalidSlug(
                "empty segment (leading / trailing / consecutive `/`)".into(),
            ));
        }
        if seg == "." || seg == ".." {
            return Err(WorktreeError::InvalidSlug(format!(
                "segment cannot be `.` or `..`: {s}"
            )));
        }
        if !seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(WorktreeError::InvalidSlug(format!(
                "segment must match [a-zA-Z0-9._-]+: {seg}"
            )));
        }
    }
    Ok(())
}

/// `/` → `+` —— 让嵌套 slug 在文件系统 + git 分支名里都安全。
/// `+` 不在 SLUG_VALID 字符集里，所以映射可逆（虽然我们不需要逆向）。
pub(crate) fn flatten_slug(s: &str) -> String {
    s.replace('/', "+")
}

async fn find_git_root(exec: &ExecProviders, cwd: &Path) -> Result<PathBuf, WorktreeError> {
    let out = run_git_capture(exec, cwd, &["rev-parse", "--show-toplevel"]).await?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err(WorktreeError::NotAGitRepo(cwd.to_path_buf()));
    }
    Ok(PathBuf::from(trimmed))
}

/// 在 repo root 的 `.gitignore` 末尾追加 `entry`（带换行）；若文件不存在创建；
/// 若 entry 已经在文件里出现一次，no-op。失败仅 warn。
async fn ensure_gitignore_entry(exec: &ExecProviders, repo_root: &Path, entry: &str) {
    let path = repo_root.join(".gitignore");
    let existing = exec
        .filesystem
        .read_to_string(&path)
        .await
        .unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.trim() == entry.trim_end_matches('/') || line.trim() == entry)
    {
        return;
    }
    let mut new_content = existing;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str("# agix worktrees (auto-added)\n");
    new_content.push_str(entry);
    if !entry.ends_with('\n') {
        new_content.push('\n');
    }
    if let Err(e) = exec.filesystem.write_str(&path, &new_content).await {
        warn!(
            path = %path.display(),
            error = %e,
            "failed to update .gitignore for worktree directory; user may want to add it manually"
        );
    }
}

/// 跑一个 git 命令；非零退出转 GitFailed。stdout/stderr 一并附进 error。
async fn run_git(exec: &ExecProviders, cwd: &Path, args: &[&str]) -> Result<(), WorktreeError> {
    let _ = run_git_capture(exec, cwd, args).await?;
    Ok(())
}

/// 跑 git，返回 stdout（trimmed）。带 15s 超时。
async fn run_git_capture(
    exec: &ExecProviders,
    cwd: &Path,
    args: &[&str],
) -> Result<String, WorktreeError> {
    let mut spec = ProcessSpec::new("git", cwd).args(args.iter().copied());
    spec.env = vec![
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ("GIT_ASKPASS".to_string(), String::new()),
    ];

    let out = timeout(GIT_TIMEOUT, capture(exec, spec, CancellationToken::new()))
        .await
        .map_err(|_| {
            WorktreeError::GitFailed(format!("git {} timed out after 15s", args.join(" ")))
        })??;

    if !out.status.success {
        return Err(WorktreeError::GitFailed(format!(
            "git {}: {}",
            args.join(" "),
            out.stderr_lossy().trim()
        )));
    }
    Ok(out.stdout_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ----- slug validation -----

    #[test]
    fn validate_slug_accepts_simple() {
        assert!(validate_slug("foo").is_ok());
        assert!(validate_slug("foo-bar_baz.42").is_ok());
    }

    #[test]
    fn validate_slug_accepts_nested() {
        assert!(validate_slug("user/feature").is_ok());
        assert!(validate_slug("a/b/c").is_ok());
    }

    #[test]
    fn validate_slug_rejects_empty() {
        assert!(matches!(
            validate_slug(""),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn validate_slug_rejects_dotdot() {
        assert!(matches!(
            validate_slug(".."),
            Err(WorktreeError::InvalidSlug(_))
        ));
        assert!(matches!(
            validate_slug("foo/../bar"),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn validate_slug_rejects_dot() {
        assert!(matches!(
            validate_slug("."),
            Err(WorktreeError::InvalidSlug(_))
        ));
        assert!(matches!(
            validate_slug("foo/./bar"),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn validate_slug_rejects_too_long() {
        let s = "a".repeat(SLUG_MAX_LEN + 1);
        assert!(matches!(
            validate_slug(&s),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn validate_slug_rejects_special_chars() {
        for bad in &["foo bar", "foo*", "foo$", "foo:bar", "foo\\bar", "foo;rm"] {
            assert!(
                matches!(validate_slug(bad), Err(WorktreeError::InvalidSlug(_))),
                "expected reject for {bad}"
            );
        }
    }

    #[test]
    fn validate_slug_rejects_leading_or_trailing_slash() {
        assert!(matches!(
            validate_slug("/foo"),
            Err(WorktreeError::InvalidSlug(_))
        ));
        assert!(matches!(
            validate_slug("foo/"),
            Err(WorktreeError::InvalidSlug(_))
        ));
        assert!(matches!(
            validate_slug("foo//bar"),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn flatten_slug_replaces_slash() {
        assert_eq!(flatten_slug("foo"), "foo");
        assert_eq!(flatten_slug("user/feature"), "user+feature");
        assert_eq!(flatten_slug("a/b/c"), "a+b+c");
    }

    // ----- end-to-end worktree create + cleanup -----

    fn exec() -> ExecProviders {
        ExecProviders::local()
    }

    use base::interface::exec::local::{LocalFileSystem, LocalProcess};
    use base::interface::exec::{DirEntry, FileSystem, Metadata, Process, ProcessHandle};

    /// The local providers, with a note of everything that went through them.
    ///
    /// This is what makes "the git subprocess goes through the contract" a
    /// testable claim rather than a code-reading exercise: a call that went
    /// straight to `tokio::process` would leave no entry here and every other
    /// assertion in these tests would still pass.
    #[derive(Default)]
    struct Log {
        argv: std::sync::Mutex<Vec<Vec<String>>>,
        written: std::sync::Mutex<Vec<PathBuf>>,
    }

    struct WatchedProcess(std::sync::Arc<Log>);

    #[async_trait::async_trait]
    impl Process for WatchedProcess {
        async fn spawn(
            &self,
            spec: ProcessSpec,
            cancel: CancellationToken,
        ) -> Result<Box<dyn ProcessHandle>, ExecError> {
            let mut argv = vec![spec.program.clone()];
            argv.extend(spec.args.iter().cloned());
            self.0.argv.lock().unwrap().push(argv);
            LocalProcess::default().spawn(spec, cancel).await
        }
    }

    struct WatchedFileSystem(std::sync::Arc<Log>);

    #[async_trait::async_trait]
    impl FileSystem for WatchedFileSystem {
        async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
            LocalFileSystem.read(path).await
        }
        async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
            self.0.written.lock().unwrap().push(path.to_path_buf());
            LocalFileSystem.write(path, bytes).await
        }
        async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError> {
            LocalFileSystem.create_dir_all(path).await
        }
        async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError> {
            LocalFileSystem.read_dir(path).await
        }
        async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError> {
            LocalFileSystem.remove_dir_all(path).await
        }
        async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
            LocalFileSystem.metadata(path).await
        }
        async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
            LocalFileSystem.canonicalize(path).await
        }
    }

    fn watched() -> (ExecProviders, std::sync::Arc<Log>) {
        let log = std::sync::Arc::new(Log::default());
        let mut providers = ExecProviders::local();
        providers.process = std::sync::Arc::new(WatchedProcess(log.clone()));
        providers.filesystem = std::sync::Arc::new(WatchedFileSystem(log.clone()));
        (providers, log)
    }

    /// 在 TempDir 起一个最小 git repo，里面有一次提交（worktree add 需要 HEAD）
    async fn make_minimal_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        let init = run_git_capture(&exec(), p, &["init"]).await;
        assert!(init.is_ok(), "git init: {init:?}");
        // 配置 user 让 commit 不报错（CI 可能没全局 config）
        run_git_capture(&exec(), p, &["config", "user.email", "test@example.com"])
            .await
            .unwrap();
        run_git_capture(&exec(), p, &["config", "user.name", "test"])
            .await
            .unwrap();
        tokio::fs::write(p.join("README.md"), "# test")
            .await
            .unwrap();
        run_git_capture(&exec(), p, &["add", "."]).await.unwrap();
        run_git_capture(&exec(), p, &["commit", "-m", "init"])
            .await
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn create_worktree_succeeds_in_real_repo() {
        let repo = make_minimal_repo().await;
        let mut handle = create_worktree(&exec(), repo.path(), "probe")
            .await
            .unwrap();

        // worktree 路径应当存在且包含一份 README
        assert!(handle.path().exists(), "worktree path missing");
        assert!(
            handle.path().join("README.md").exists(),
            "worktree should have README"
        );
        assert_eq!(handle.branch(), "agix/worktree-probe");

        // .gitignore 应当多出 .agix/
        let gi = tokio::fs::read_to_string(repo.path().join(".gitignore"))
            .await
            .unwrap_or_default();
        assert!(
            gi.contains(".agix/"),
            "expected .gitignore entry; got {gi:?}"
        );

        // cleanup 后路径 + 分支都应当消失
        handle.cleanup().await;
        assert!(!repo.path().join(".agix/worktrees/probe").exists());
        let branches = run_git_capture(&exec(), repo.path(), &["branch", "--list"])
            .await
            .unwrap();
        assert!(
            !branches.contains("agix/worktree-probe"),
            "branch should be deleted; got {branches:?}"
        );
    }

    #[tokio::test]
    async fn create_worktree_flattens_nested_slug() {
        let repo = make_minimal_repo().await;
        let mut handle = create_worktree(&exec(), repo.path(), "team/feature-a")
            .await
            .unwrap();
        // 嵌套 slug 应当 flatten 成 +
        assert!(
            handle.path().to_string_lossy().contains("team+feature-a"),
            "expected flattened path; got {}",
            handle.path().display()
        );
        assert_eq!(handle.branch(), "agix/worktree-team+feature-a");
        handle.cleanup().await;
    }

    #[tokio::test]
    async fn create_worktree_fails_on_existing_path() {
        let repo = make_minimal_repo().await;
        let mut h1 = create_worktree(&exec(), repo.path(), "dup").await.unwrap();
        // 第二次同名 slug 应当 AlreadyExists
        let r2 = create_worktree(&exec(), repo.path(), "dup").await;
        assert!(matches!(r2, Err(WorktreeError::AlreadyExists(_))));
        h1.cleanup().await;
    }

    #[tokio::test]
    async fn create_worktree_fails_on_non_git_dir() {
        let plain = TempDir::new().unwrap();
        let r = create_worktree(&exec(), plain.path(), "probe").await;
        assert!(
            matches!(
                r,
                Err(WorktreeError::NotAGitRepo(_)) | Err(WorktreeError::GitFailed(_))
            ),
            "expected NotAGitRepo or GitFailed, got {r:?}"
        );
    }

    #[tokio::test]
    async fn create_worktree_rejects_invalid_slug_before_touching_disk() {
        let repo = make_minimal_repo().await;
        let r = create_worktree(&exec(), repo.path(), "../escape").await;
        assert!(matches!(r, Err(WorktreeError::InvalidSlug(_))));
        // 路径不应当被创建
        assert!(!repo.path().join(".agix").exists());
    }

    #[tokio::test]
    async fn ensure_gitignore_entry_is_idempotent() {
        let dir = TempDir::new().unwrap();
        // 第一次：写入
        ensure_gitignore_entry(&exec(), dir.path(), ".agix/").await;
        let v1 = tokio::fs::read_to_string(dir.path().join(".gitignore"))
            .await
            .unwrap();
        assert!(v1.contains(".agix/"));
        // 第二次：no-op，文件不变
        ensure_gitignore_entry(&exec(), dir.path(), ".agix/").await;
        let v2 = tokio::fs::read_to_string(dir.path().join(".gitignore"))
            .await
            .unwrap();
        assert_eq!(v1, v2, "second call should not change file");
    }

    #[tokio::test]
    async fn prune_removes_orphan_worktree_dir() {
        let repo = make_minimal_repo().await;
        // 模拟孤儿：手动 mkdir .agix/worktrees/orphan/ —— 没经过 git worktree add
        let orphan = repo.path().join(".agix/worktrees/orphan");
        tokio::fs::create_dir_all(&orphan).await.unwrap();
        tokio::fs::write(orphan.join("leftover.txt"), "stale")
            .await
            .unwrap();
        assert!(orphan.exists());

        prune_orphan_worktrees(&exec(), repo.path()).await;
        assert!(
            !orphan.exists(),
            "orphan dir should be removed; still at {}",
            orphan.display()
        );
    }

    #[tokio::test]
    async fn prune_keeps_active_worktrees() {
        let repo = make_minimal_repo().await;
        // 一个真活跃的 worktree
        let mut active = create_worktree(&exec(), repo.path(), "active")
            .await
            .unwrap();
        let active_path = active.path().to_path_buf();
        // 一个孤儿（不经 git worktree add）
        let orphan = repo.path().join(".agix/worktrees/orphan");
        tokio::fs::create_dir_all(&orphan).await.unwrap();

        prune_orphan_worktrees(&exec(), repo.path()).await;

        // 活跃的应当还在
        assert!(active_path.exists(), "active worktree should be preserved");
        // 孤儿应当清了
        assert!(!orphan.exists(), "orphan should be pruned");

        // cleanup 真活跃的
        active.cleanup().await;
    }

    #[tokio::test]
    async fn prune_in_non_git_dir_is_silent_noop() {
        let plain = TempDir::new().unwrap();
        // 不应当 panic 或 error
        prune_orphan_worktrees(&exec(), plain.path()).await;
    }

    #[tokio::test]
    async fn prune_with_no_agix_dir_is_silent_noop() {
        let repo = make_minimal_repo().await;
        // 全新 repo 没 .agix/worktrees/ 目录
        prune_orphan_worktrees(&exec(), repo.path()).await;
        // git 还是好的
        let r = run_git_capture(&exec(), repo.path(), &["status"]).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn ensure_gitignore_entry_appends_to_existing() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(".gitignore"), "node_modules/\n*.log\n")
            .await
            .unwrap();
        ensure_gitignore_entry(&exec(), dir.path(), ".agix/").await;
        let after = tokio::fs::read_to_string(dir.path().join(".gitignore"))
            .await
            .unwrap();
        assert!(after.contains("node_modules/"), "preserved existing");
        assert!(after.contains("*.log"), "preserved existing");
        assert!(after.contains(".agix/"), "added new");
    }

    #[tokio::test]
    async fn every_git_call_and_every_write_reaches_the_providers() {
        let repo = make_minimal_repo().await;
        let (providers, log) = watched();

        let mut handle = create_worktree(&providers, repo.path(), "watched")
            .await
            .unwrap();
        handle.cleanup().await;

        let argv = log.argv.lock().unwrap().clone();
        assert!(
            argv.iter().all(|a| a[0] == "git"),
            "nothing but git should be spawned: {argv:?}"
        );
        for expected in [
            vec!["rev-parse", "--show-toplevel"],
            vec!["worktree", "add"],
            vec!["worktree", "remove"],
            vec!["branch", "-D"],
        ] {
            assert!(
                argv.iter().any(
                    |a| a[1..].iter().zip(&expected).all(|(got, want)| got == want)
                        && a.len() > expected.len()
                ),
                "`git {}` never reached the Process contract; got {argv:?}",
                expected.join(" ")
            );
        }

        let written = log.written.lock().unwrap().clone();
        assert_eq!(
            written
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![".gitignore".to_string()],
            "the .gitignore write is the one write this path makes, and it has \
             to go through the FileSystem contract"
        );
    }
}
