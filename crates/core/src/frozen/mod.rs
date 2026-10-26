//! Session-start frozen environment snapshot — the data sources for system prompt
//! segments [4] and [5].
//!
//! All git subcommands have a 1.5 s timeout; failures silently skip the
//! related field. **Not refreshed during a session** —
//! mid-session git commits are not updated.

pub mod frontmatter;
pub mod git;
pub mod import;
pub mod memory;
pub mod regex;
pub mod skill;
pub mod utils;

pub use self::frontmatter::split_frontmatter;
pub use self::import::{
    already_decided as import_already_decided, detect_import_sources, execute_import,
    mark_imported, mark_skipped, ImportError, ImportSource, ImportSourceKind, ImportSummary,
};
pub use self::memory::{find_relevant_memories, maybe_migrate_claude_to_agix, MemoryFileEntry};
pub use self::skill::{
    activate_conditional_skills, expand_skill_vars, load_session_skills,
    load_session_skills_with_bundled, load_skill_from_path, try_expand_skill_command, SkillEntry,
    SkillSource,
};

use self::memory::{collect_memory, collect_memory_files_with, load_all_memory_files};
use self::utils::truncate_chars;
use crate::paths::ConfigPaths;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Iso8601;

/// Git status output character limit.
const MAX_GIT_STATUS_CHARS: usize = 2000;

/// 会话级冻结的环境信息。一次会话只算一次；turn 之间不变。
#[derive(Debug, Clone, Default)]
pub struct FrozenContext {
    pub cwd: PathBuf,
    pub is_git: bool,
    pub is_worktree: bool,
    pub git_branch: Option<String>,
    pub git_main_branch: Option<String>,
    pub git_user_name: Option<String>,
    pub git_status: Option<String>,
    pub git_log: Option<String>,
    pub platform: String,
    pub shell: Option<String>,
    /// 这台机器上用户的 home 目录——和 `platform`/`shell` 一样是**环境事实**，
    /// 进 system prompt 给模型交代环境，不是 Agix 存东西的地方（那些根一律
    /// 由调用方注入，见 [`crate::paths::ConfigPaths`]）。收进快照而不是在拼
    /// prompt 时现读，是因为快照可以被测试替换，而 env 不能。
    pub home_dir: Option<String>,
    pub today: String,
    pub memory_blocks: Vec<MemoryFileEntry>,
    pub user_email: Option<String>,
    /// `~/.agix/scenes/<scope>/skills/<name>/SKILL.md` + `<cwd>/.agents/skills/<name>/SKILL.md`
    /// 的 metadata 索引（项目级技能挪到了 `.agents/skills/`——外部事实标准，Codex
    /// 也扫描这个路径；用户级技能仍在 Agix 私有的 `.agix/scenes/<scope>/` 下，因为
    /// 用户主目录不存在"被别的工具扫描"的场景）。只入 frontmatter
    /// （name/description/when_to_use）+ 不嵌入 body / 不做参数代换 / 不做 slash
    /// 调用。模型能"知道"skills 存在 + 在合适时主动引用。
    pub skills: Vec<SkillEntry>,
    /// 跨会话 memory 目录（每个 cwd 一个）。位于
    /// `~/.agix/scenes/<scope>/memory/<sha256(canonical_cwd)[..16]>/`。
    pub memory_dir: PathBuf,
    /// 上面目录里 MEMORY.md 的内容（仅当存在时加载）。注入 system prompt。
    pub memory_index: Option<String>,
    /// Topic memory files selected for the current user prompt. MEMORY.md is
    /// only an index; these files provide the matched details.
    pub relevant_memories: Vec<MemoryFileEntry>,
    /// Memory file paths already surfaced in prior turns. Used to deduplicate
    /// and avoid re-injecting the same memories every turn.
    pub already_surfaced: std::collections::HashSet<String>,
    /// **A-5 **: output style 已加载的内容（`name` 来自 EngineConfig，
    /// 由 `collect_output_style` 从 user/project 级别 `output-styles/<name>.md`
    /// 读取）。None = 没指定或文件不存在。
    pub output_style: Option<OutputStyle>,
}

/// **A-5 **: 一个 output-style 文件的加载结果。注入 system prompt 时用。
#[derive(Debug, Clone)]
pub struct OutputStyle {
    pub name: String,
    pub source: OutputStyleSource,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStyleSource {
    /// `~/.agix/scenes/<scope>/output-styles/<name>.md`
    User,
    /// `<cwd>/.agix/output-styles/<name>.md`
    Project,
}

/// FrozenContext 收集选项。
#[derive(Debug, Clone, Default)]
pub struct CollectOptions {
    /// 是否在 cwd 之上向上爬找 `AGENTS.md`（含 git root）。默认 true。
    /// monorepo 子目录不想吃父级 monorepo 上下文时设为 false。
    pub walk_up_claude_md: bool,

    /// **A-5 **: 启动时按名称加载 output style（先项目级再用户级）。
    /// None = 不加载。
    pub output_style: Option<String>,
}

impl CollectOptions {
    pub fn defaults() -> Self {
        Self {
            walk_up_claude_md: true,
            output_style: None,
        }
    }
}

/// The git-derived subset of `FrozenContext` — factored out so
/// `FrozenContext::refresh_git` can re-run just this part instead of the
/// full `collect_with_options` (which also does memory/skills/output-style
/// I/O that a git-status refresh has no business repeating).
struct GitFields {
    is_git: bool,
    is_worktree: bool,
    git_branch: Option<String>,
    git_main_branch: Option<String>,
    git_user_name: Option<String>,
    git_status: Option<String>,
    git_log: Option<String>,
}

async fn collect_git_fields(cwd: &Path) -> GitFields {
    let is_git = git::run_git_check(cwd).await;
    let is_worktree = if is_git {
        git::is_worktree(cwd).await
    } else {
        false
    };
    if !is_git {
        return GitFields {
            is_git,
            is_worktree,
            git_branch: None,
            git_main_branch: None,
            git_user_name: None,
            git_status: None,
            git_log: None,
        };
    }
    let git_branch = git::run_git_text(cwd, &["symbolic-ref", "--short", "HEAD"]).await;
    let git_main_branch = git::detect_main_branch(cwd).await;
    let git_user_name = git::run_git_text(cwd, &["config", "user.name"]).await;
    let git_status = git::run_git_text(cwd, &["--no-optional-locks", "status", "--short"])
        .await
        .map(|s| truncate_chars(&s, MAX_GIT_STATUS_CHARS, "\n... (truncated)"));
    let git_log =
        git::run_git_text(cwd, &["--no-optional-locks", "log", "--oneline", "-n", "5"]).await;
    GitFields {
        is_git,
        is_worktree,
        git_branch,
        git_main_branch,
        git_user_name,
        git_status,
        git_log,
    }
}

impl FrozenContext {
    /// 收集环境快照。所有 IO 错误都吞掉转 None / 默认值；这个函数不该 panic、
    /// 不该 fail -- 让上层 build_system_prompt 总能拿到一份合理的快照。
    ///
    /// `paths` 是这个实例的根目录集合——引擎层不去环境里找，调用方（daemon）
    /// 决定这个实例的状态放在哪。
    pub async fn collect(
        cwd: PathBuf,
        paths: &ConfigPaths,
        environment: &dyn crate::interface::environment::Environment,
    ) -> Self {
        Self::collect_with_options(cwd, CollectOptions::defaults(), paths, environment).await
    }

    /// 带 options 的收集 -- `` 加 `walk_up_claude_md` 控制 monorepo 父级
    /// 上下文是否进 system prompt。
    pub async fn collect_with_options(
        cwd: PathBuf,
        opts: CollectOptions,
        paths: &ConfigPaths,
        environment: &dyn crate::interface::environment::Environment,
    ) -> Self {
        let cwd_clone = cwd.clone();

        // git 相关命令并发跑，每条独立超时 — 同一段逻辑也被 `refresh_git` 复用
        // （见下），所以拆成了独立的 `collect_git_fields`。
        let git = collect_git_fields(&cwd_clone).await;
        let is_git = git.is_git;
        let is_worktree = git.is_worktree;

        // 平台信息走 std + 环境变量，不阻塞
        let platform = std::env::consts::OS.to_string();
        let home_dir = std::env::var("HOME").ok();
        let shell = std::env::var("SHELL").ok().map(|p| {
            Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(p)
        });

        let today = environment
            .now()
            .format(&Iso8601::DATE)
            .unwrap_or_else(|_| "unknown".to_string());

        // user email 仅在已是 git repo 时尝试 git config
        let user_email = if is_git {
            git::run_git_text(&cwd_clone, &["config", "user.email"]).await
        } else {
            None
        };

        let (git_branch, git_main_branch, git_user_name, git_status, git_log) = (
            git.git_branch,
            git.git_main_branch,
            git.git_user_name,
            git.git_status,
            git.git_log,
        );

        let memory_blocks =
            collect_memory_files_with(&cwd, opts.walk_up_claude_md, &paths.global_data_dir).await;
        // P3c : 用 load_session_skills 而非 collect_skills -- 前者把 bundled
        // skills (simplify/verify/debug/batch/stuck) 也并入。否则
        // disk 没装 SKILL.md 时 system prompt 不暴露 bundled，模型不知道有这些
        // skill，/stuck /simplify 等 case 失败。
        let skills = load_session_skills(&cwd, paths).await;

        // memory_dir = <global_root>/memory/<sha256(canonical_cwd)[..16]>/（不分 scene）
        let (memory_dir, memory_index) = collect_memory(&cwd, &paths.global_data_dir).await;

        // P1.5: Pre-load all memory files from the project memory dir so they
        // are available for injection into the system prompt. Files are capped
        // at 8KB each, up to 5 files.
        let relevant_memories = if memory_dir.exists() {
            load_all_memory_files(&memory_dir, 5).await
        } else {
            Vec::new()
        };

        // A-5 : output style -- load by name from project then user dir.
        let output_style = match opts.output_style.as_deref() {
            Some(name) if !name.trim().is_empty() => collect_output_style(&cwd, name, paths).await,
            _ => None,
        };

        Self {
            cwd,
            is_git,
            is_worktree,
            git_branch,
            git_main_branch,
            git_user_name,
            git_status,
            git_log,
            platform,
            shell,
            home_dir,
            today,
            memory_blocks,
            user_email,
            skills,
            memory_dir,
            memory_index,
            relevant_memories,
            already_surfaced: std::collections::HashSet::new(),
            output_style,
        }
    }
}

impl FrozenContext {
    pub fn with_relevant_memories(mut self, memories: Vec<MemoryFileEntry>) -> Self {
        self.relevant_memories = memories;
        self
    }

    /// Re-run just the git-derived fields (branch/status/log/worktree-ness),
    /// leaving memory/skills/output-style untouched. Exists to partially
    /// relax this module's "not refreshed during a session" characteristic
    /// for the one field that goes stale fastest in a long-running session —
    /// callers are expected to invoke this only after detecting a
    /// git-mutating command in the turn just completed (see
    /// `runtime::turn`'s caller), not on every turn: a git status/log/branch
    /// re-derivation is real subprocess I/O (bounded by the same 1.5s
    /// per-subcommand timeout `collect_with_options` uses), not free.
    pub async fn refresh_git(&mut self) {
        let git = collect_git_fields(&self.cwd).await;
        self.is_git = git.is_git;
        self.is_worktree = git.is_worktree;
        self.git_branch = git.git_branch;
        self.git_main_branch = git.git_main_branch;
        self.git_user_name = git.git_user_name;
        self.git_status = git.git_status;
        self.git_log = git.git_log;
    }

    /// Re-derive only `git_status` and `git_branch` — the two fields that get
    /// re-shown to the model on *every* turn (`git_status` through the
    /// per-turn `<system-reminder>`, `git_branch` through the scene's `<env>`
    /// block) and therefore the two that must not be a session-start
    /// snapshot.
    ///
    /// A stale git status is worse than an absent one: the model does not
    /// treat it as a possibly-outdated hint, it treats it as current fact, so
    /// after a commit it keeps reasoning about files that are no longer dirty
    /// and re-suggesting work that is already done. The wider
    /// [`FrozenContext::refresh_git`] fixes that too, but it is called only
    /// after the *model itself* runs a git-mutating `Bash` command — which
    /// misses every ordinary cause of drift: the user committing in another
    /// terminal, an editor writing a file, and the model's own `Write`/`Edit`
    /// calls, none of which go through `Bash`.
    ///
    /// Two subcommands rather than [`refresh_git`](Self::refresh_git)'s six:
    /// `git log`/`main-branch detection`/`user.name` do not appear in
    /// anything injected per turn and change far more slowly, so paying for
    /// them every turn buys nothing. Skips all I/O when the session is not in
    /// a git repository at all — the common non-repo case stays free.
    pub async fn refresh_git_status(&mut self) {
        if !self.is_git {
            return;
        }
        self.git_branch = git::run_git_text(&self.cwd, &["symbolic-ref", "--short", "HEAD"]).await;
        self.git_status =
            git::run_git_text(&self.cwd, &["--no-optional-locks", "status", "--short"])
                .await
                .map(|s| truncate_chars(&s, MAX_GIT_STATUS_CHARS, "\n... (truncated)"));
    }
}

/// **A-5 **: load `<cwd>/.agix/output-styles/<name>.md` if present,
/// else `~/.agix/scenes/<scope>/output-styles/<name>.md`. Trims whitespace and caps body
/// at 8KB. Returns None if neither file is present or readable.
async fn collect_output_style(cwd: &Path, name: &str, paths: &ConfigPaths) -> Option<OutputStyle> {
    let safe = name.trim();
    if safe.is_empty() || safe.contains('/') || safe.contains('\\') || safe.starts_with('.') {
        return None;
    }
    let project = cwd
        .join(".agix")
        .join("output-styles")
        .join(format!("{safe}.md"));
    if let Ok(content) = tokio::fs::read_to_string(&project).await {
        if !content.trim().is_empty() {
            return Some(OutputStyle {
                name: safe.to_string(),
                source: OutputStyleSource::Project,
                content: truncate_chars(content.trim(), 8_000, "\n... (output style truncated)"),
            });
        }
    }
    {
        let user = paths
            .user_data_dir
            .join("output-styles")
            .join(format!("{safe}.md"));
        if let Ok(content) = tokio::fs::read_to_string(&user).await {
            if !content.trim().is_empty() {
                return Some(OutputStyle {
                    name: safe.to_string(),
                    source: OutputStyleSource::User,
                    content: truncate_chars(
                        content.trim(),
                        8_000,
                        "\n... (output style truncated)",
                    ),
                });
            }
        }
    }
    None
}

/// **A-5 **: list all output style names from user + project dirs (no
/// content read). Used by `/output-style` slash command.
pub async fn list_output_style_names(cwd: &Path, paths: &ConfigPaths) -> Vec<String> {
    let mut all: Vec<(String, OutputStyleSource)> = Vec::new();
    all.extend(
        scan_output_style_dir(&paths.user_data_dir.join("output-styles"))
            .await
            .into_iter()
            .map(|n| (n, OutputStyleSource::User)),
    );
    all.extend(
        scan_output_style_dir(&cwd.join(".agix").join("output-styles"))
            .await
            .into_iter()
            .map(|n| (n, OutputStyleSource::Project)),
    );
    // Dedup: keep last (project wins over user when names collide).
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for (name, _) in all.into_iter().rev() {
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out.reverse();
    out
}

async fn scan_output_style_dir(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return out,
    };
    while let Ok(Some(e)) = entries.next_entry().await {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("md") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roots under a tempdir. Tests never inherit the running user's home —
    /// that is the property this whole module was changed to guarantee.
    fn test_paths(root: &Path) -> ConfigPaths {
        ConfigPaths::new(root.join(".agix"), root.join(".agix"), "code")
    }

    use tempfile::TempDir;

    #[tokio::test]
    async fn collects_basic_fields_for_arbitrary_dir() {
        let dir = TempDir::new().unwrap();
        let ctx = FrozenContext::collect(
            dir.path().to_path_buf(),
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
        )
        .await;
        assert!(!ctx.platform.is_empty());
        assert!(ctx.today.len() == 10); // YYYY-MM-DD
                                        // 不在 git 仓库下，is_git=false 且 git_* 字段都 None
        assert!(!ctx.is_git);
        assert!(ctx.git_status.is_none());
    }

    #[tokio::test]
    async fn detects_git_repo_in_a_real_repo() {
        // 当前 cwd 是 agix 仓库本身，已经初始化了 git
        let pwd = std::env::current_dir().unwrap();
        let ctx = FrozenContext::collect(
            pwd.clone(),
            &test_paths(&pwd),
            &crate::interface::environment::SystemEnvironment,
        )
        .await;
        assert!(ctx.is_git, "expected agix workspace to be inside git");
        assert!(ctx.git_branch.is_some());
    }

    /// Runs a `git` command and fails loudly if it did not work.
    ///
    /// The `.output().unwrap()` this replaces only caught a spawn failure. A
    /// `git init` that ran and exited non-zero — which is what a machine out
    /// of file descriptors or process slots produces — left a directory that
    /// is not a repository, and the test then failed several lines later with
    /// `left: None, right: Some("original-branch")`: a true statement about
    /// the wrong thing, and nothing to act on. Checking here turns that into
    /// git's own words about what it could not do.
    fn run_git(cwd: &std::path::Path) -> impl Fn(&[&str]) + '_ {
        move |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap_or_else(|e| panic!("could not run `git {}`: {e}", args.join(" ")));
            assert!(
                out.status.success(),
                "`git {}` failed ({}): {}{}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout),
            );
        }
    }

    /// `refresh_git` exists precisely to relax the module's "not refreshed
    /// during a session" characteristic for the git-derived fields — this
    /// pins down that calling it after the branch actually changes on disk
    /// updates `git_branch` in place, without needing a full re-`collect()`.
    #[tokio::test]
    async fn refresh_git_picks_up_a_branch_change() {
        let dir = TempDir::new().unwrap();
        let run = run_git(dir.path());
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--allow-empty", "-q", "-m", "init"]);
        run(&["checkout", "-q", "-b", "original-branch"]);

        let mut ctx = FrozenContext::collect(
            dir.path().to_path_buf(),
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
        )
        .await;
        assert_eq!(ctx.git_branch.as_deref(), Some("original-branch"));

        run(&["checkout", "-q", "-b", "new-branch"]);
        ctx.refresh_git().await;
        assert_eq!(
            ctx.git_branch.as_deref(),
            Some("new-branch"),
            "refresh_git should have picked up the branch switch made after collect()"
        );
    }

    /// A-2: the per-turn `<system-reminder>` re-shows `gitStatus` on every
    /// turn, so a session-start snapshot of it is not merely stale, it is
    /// actively misleading — the model reads it as current fact and keeps
    /// citing files that are no longer dirty. Same shape as
    /// `refresh_git_picks_up_a_branch_change`, but for the field that
    /// actually changes several times per turn, and driven by a change made
    /// *outside* the agent (the user committing) rather than by a git command
    /// the model ran — which is the case `refresh_git`'s Bash-sniffing caller
    /// cannot see.
    #[tokio::test]
    async fn refresh_git_status_picks_up_a_commit_made_mid_session() {
        let dir = TempDir::new().unwrap();
        let run = run_git(dir.path());
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--allow-empty", "-q", "-m", "init"]);
        tokio::fs::write(dir.path().join("dirty.txt"), "wip")
            .await
            .unwrap();
        run(&["add", "dirty.txt"]);

        let mut ctx = FrozenContext::collect(
            dir.path().to_path_buf(),
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
        )
        .await;
        assert!(
            ctx.git_status
                .as_deref()
                .unwrap_or_default()
                .contains("dirty.txt"),
            "sanity check: the staged file should show up at collect() time"
        );

        // The user commits from another terminal — no Bash tool call, so the
        // heuristic that gates the wider `refresh_git` never fires.
        run(&["commit", "-q", "-m", "done"]);
        ctx.refresh_git_status().await;

        assert!(
            !ctx.git_status
                .as_deref()
                .unwrap_or_default()
                .contains("dirty.txt"),
            "git status must not keep reporting a file that was committed \
             mid-session, got: {:?}",
            ctx.git_status
        );
    }

    /// Outside a git repository the refresh must cost nothing and invent
    /// nothing — the common non-repo session pays no subprocess per turn.
    #[tokio::test]
    async fn refresh_git_status_is_a_noop_outside_a_repo() {
        let dir = TempDir::new().unwrap();
        let mut ctx = FrozenContext::collect(
            dir.path().to_path_buf(),
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
        )
        .await;
        assert!(!ctx.is_git);
        ctx.refresh_git_status().await;
        assert!(ctx.git_status.is_none());
        assert!(ctx.git_branch.is_none());
    }

    #[tokio::test]
    async fn loads_claude_md_from_cwd() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("AGENTS.md");
        tokio::fs::write(&p, "# Test instructions\nbe concise.")
            .await
            .unwrap();
        let ctx = FrozenContext::collect(
            dir.path().to_path_buf(),
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
        )
        .await;
        assert_eq!(ctx.memory_blocks.len(), 1, "expected one AGENTS.md");
        assert!(ctx.memory_blocks[0].content.contains("be concise"));
    }

    #[tokio::test]
    async fn loads_nested_claude_md_in_walk_up_order() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("p");
        let child = parent.join("c");
        tokio::fs::create_dir_all(&child).await.unwrap();
        tokio::fs::write(parent.join("AGENTS.md"), "PARENT")
            .await
            .unwrap();
        tokio::fs::write(child.join("AGENTS.md"), "CHILD")
            .await
            .unwrap();

        let ctx = FrozenContext::collect(
            child,
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
        )
        .await;
        // 应该顺序：parent 在前，child 在后（远到近）
        assert!(ctx.memory_blocks.len() >= 2);
        let parent_idx = ctx
            .memory_blocks
            .iter()
            .position(|e| e.content == "PARENT")
            .unwrap();
        let child_idx = ctx
            .memory_blocks
            .iter()
            .position(|e| e.content == "CHILD")
            .unwrap();
        assert!(parent_idx < child_idx, "parent must come before child");
    }

    // -----------------------------------------------------------------------
    // output_style
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn output_style_project_takes_precedence_over_user() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let user_dir = test_paths(home.path()).user_data_dir.join("output-styles");
        let proj_dir = cwd.path().join(".agix/output-styles");
        tokio::fs::create_dir_all(&user_dir).await.unwrap();
        tokio::fs::create_dir_all(&proj_dir).await.unwrap();
        tokio::fs::write(user_dir.join("terse.md"), "USER VERSION")
            .await
            .unwrap();
        tokio::fs::write(proj_dir.join("terse.md"), "PROJECT VERSION")
            .await
            .unwrap();

        let style = collect_output_style(cwd.path(), "terse", &test_paths(home.path()))
            .await
            .unwrap();
        assert_eq!(style.name, "terse");
        assert_eq!(style.source, OutputStyleSource::Project);
        assert!(style.content.contains("PROJECT VERSION"));
    }

    #[tokio::test]
    async fn output_style_falls_back_to_user_when_no_project() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let user_dir = test_paths(home.path()).user_data_dir.join("output-styles");
        tokio::fs::create_dir_all(&user_dir).await.unwrap();
        tokio::fs::write(user_dir.join("verbose.md"), "explain everything")
            .await
            .unwrap();

        let style = collect_output_style(cwd.path(), "verbose", &test_paths(home.path()))
            .await
            .unwrap();
        assert_eq!(style.source, OutputStyleSource::User);
        assert!(style.content.contains("explain everything"));
    }

    #[tokio::test]
    async fn output_style_returns_none_for_path_traversal_attempt() {
        let cwd = TempDir::new().unwrap();
        // names with slashes / leading dot must not load anything
        assert!(
            collect_output_style(cwd.path(), "../etc/passwd", &test_paths(cwd.path()))
                .await
                .is_none()
        );
        assert!(
            collect_output_style(cwd.path(), ".hidden", &test_paths(cwd.path()))
                .await
                .is_none()
        );
        assert!(
            collect_output_style(cwd.path(), "", &test_paths(cwd.path()))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_output_style_names_dedups_user_and_project() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let user_dir = test_paths(home.path()).user_data_dir.join("output-styles");
        let proj_dir = cwd.path().join(".agix/output-styles");
        tokio::fs::create_dir_all(&user_dir).await.unwrap();
        tokio::fs::create_dir_all(&proj_dir).await.unwrap();
        tokio::fs::write(user_dir.join("terse.md"), "x")
            .await
            .unwrap();
        tokio::fs::write(user_dir.join("verbose.md"), "x")
            .await
            .unwrap();
        tokio::fs::write(proj_dir.join("terse.md"), "y")
            .await
            .unwrap();
        tokio::fs::write(proj_dir.join("local.md"), "y")
            .await
            .unwrap();

        let names = list_output_style_names(cwd.path(), &test_paths(home.path())).await;
        // expected: terse (deduped), verbose (user), local (project)
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["local", "terse", "verbose"]);
    }
}
