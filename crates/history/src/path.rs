//! 路径工具：`sanitize_path`、`canonicalize_cwd`、配置目录定位。
//!
//! 同一 cwd 产生同一项目目录名，jsonl 文件名也一致。

use crate::error::HistoryError;
use base::session::SessionId;
use std::path::{Path, PathBuf};

/// Delegate to shared implementation in `attacode-core`.
///
/// 把任意字符串变成跨平台安全的目录 / 文件名。
///
/// 规则（与 TS 端一致）：
/// 1. 把 `[^a-zA-Z0-9]` 全部替换成 `-`
/// 2. 长度 > 200 时，截到 200 + `-` + djb2(原串) 的 36 进制
///
/// > 注：使用 djb2 而非 wyhash；普通路径一致，超长路径不同。
pub fn sanitize_path(name: &str) -> String {
    base::path::sanitize_for_fs(name)
}

/// 会话状态的两个根，都从同一个全局根派生。
///
/// `projects` 与 `sessions` 是两个维度，不是两层：sidecar 只按 session id 索引
/// （`session_sidecar_paths` 拿不到项目信息），所以它进不了 `projects/<cwd>/`。
/// 这两个目录以前都叫 `sessions` 且都在用，导致 `<global>/sessions/` 下同时躺着
/// `<sanitized-cwd>/` 和 `<session-id>/` 两种命名的目录。
///
/// 从同一个 `global_root` 派生这件事本身是重点：这两个根曾经一个走
/// `ATTA_CONFIG_HOME`、一个走 `ATTA_DATA_DIR`，默认值相同但可以被配成不同的
/// 目录——session memory 因此写去过没人会读的地方。现在只有一个来源。
#[derive(Debug, Clone)]
pub struct HistoryRoots {
    /// 按项目分的状态（transcript）。
    pub projects: PathBuf,
    /// 按会话分的状态（sidecar：memory / metadata / 输入历史）。
    pub sessions: PathBuf,
}

impl HistoryRoots {
    pub fn under(global_root: &Path) -> Self {
        Self {
            projects: global_root.join("projects"),
            sessions: global_root.join("sessions"),
        }
    }
}

/// Project-local state directory (`<cwd>/.atta`) —— 与
/// `ConfigPaths::local_data_dir` 一致。
pub fn project_local_dir(canonical_cwd: &Path) -> PathBuf {
    canonical_cwd.join(".atta")
}

/// Project-local active session pointer.
pub fn project_session_state_file(canonical_cwd: &Path) -> PathBuf {
    project_local_dir(canonical_cwd).join("session.json")
}

/// Global sidecar directory for session-scoped private state.
pub fn session_sidecar_dir(sessions_root: &Path, session: &SessionId) -> PathBuf {
    sessions_root.join(session.to_string())
}

/// Session-scoped TUI input history. JSONL strings so multi-line prompts round-trip.
pub fn session_tui_input_history_file(sessions_root: &Path, session: &SessionId) -> PathBuf {
    session_sidecar_dir(sessions_root, session).join("tui_input_history.jsonl")
}

/// Session-scoped line-mode REPL input history. Native rustyline text format.
pub fn session_repl_input_history_file(sessions_root: &Path, session: &SessionId) -> PathBuf {
    session_sidecar_dir(sessions_root, session).join("repl_input_history.txt")
}

/// Session-scoped memory snapshot file for cross-session persistence.
pub fn session_memory_file(sessions_root: &Path, session: &SessionId) -> PathBuf {
    session_sidecar_dir(sessions_root, session).join("session_memory.md")
}

/// Session-scoped prompt baseline / previous-turn settings snapshot.
pub fn session_prompt_state_file(sessions_root: &Path, session: &SessionId) -> PathBuf {
    session_sidecar_dir(sessions_root, session).join("prompt_state.json")
}

/// Session metadata sidecar.
pub fn session_metadata_file(sessions_root: &Path, session: &SessionId) -> PathBuf {
    session_sidecar_dir(sessions_root, session).join("metadata.json")
}

/// 把 cwd 解析到一个稳定的项目目录路径。
///
/// `realpath()` 解 symlink + 平台标准化（macOS `/tmp` → `/private/tmp`）；
/// 不存在就回退到原路径。
pub async fn canonicalize_cwd(p: &Path) -> Result<PathBuf, HistoryError> {
    match tokio::fs::canonicalize(p).await {
        Ok(c) => Ok(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(p.to_path_buf()),
        Err(e) => Err(HistoryError::Io(e)),
    }
}

/// 给定 projects_root 与 canonical cwd，返回该项目的会话目录。
pub fn project_dir(projects_root: &Path, canonical_cwd: &Path) -> PathBuf {
    let name = sanitize_path(&canonical_cwd.to_string_lossy());
    projects_root.join(name)
}

/// 无项目会话（`Meta.project_root` 为 `null`）的 transcript 目录名。
///
/// 不会与任何项目撞名：sanitize 之后的绝对路径必然以 `-` 开头（首字符是路径
/// 分隔符），而这个名字以 `_` 开头。
pub const NO_PROJECT_DIR: &str = "_no-project";

/// 一个会话的 transcript 目录：有项目的按项目分，无项目的进保留桶。
///
/// 无项目会话不能落在"daemon 当时的 cwd"那个项目下——它按定义不属于任何项目，
/// 而那个目录属于另一个真实项目，两者会混在一起。
pub fn project_dir_for(projects_root: &Path, home: Option<&Path>) -> PathBuf {
    match home {
        Some(cwd) => project_dir(projects_root, cwd),
        None => projects_root.join(NO_PROJECT_DIR),
    }
}

/// 给定 project_dir + session_id，返回对应的 jsonl 文件路径。
pub fn session_file(project_dir: &Path, session: &SessionId) -> PathBuf {
    project_dir.join(format!("{}.jsonl", session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_simple_path() {
        assert_eq!(
            sanitize_path("/Users/foo/my-project"),
            "-Users-foo-my-project"
        );
        assert_eq!(sanitize_path("a"), "a");
        assert_eq!(sanitize_path(""), "");
    }

    #[test]
    fn sanitize_replaces_non_ascii_alnum() {
        assert_eq!(sanitize_path("a/b\\c:d e_f"), "a-b-c-d-e-f");
        // unicode 字符也变成 -
        assert_eq!(sanitize_path("héllo"), "h-llo");
    }

    #[test]
    fn sanitize_handles_overlong_with_hash() {
        let long = "/".repeat(300);
        let s = sanitize_path(&long);
        // Our sanitize_for_fs keeps total length exactly at MAX_SANITIZED_LENGTH
        // by truncating the prefix before appending the hash suffix.
        assert_eq!(s.len(), base::path::MAX_SANITIZED_LENGTH);
        // Contains the hash separator.
        let dash_pos = s.rfind('-').unwrap();
        // Suffix after the last dash is the radix-36 hash (non-empty).
        assert!(!s[dash_pos + 1..].is_empty());
    }

    #[test]
    fn project_dir_layout() {
        let root = PathBuf::from("/tmp/projects");
        let cwd = PathBuf::from("/Users/me/work");
        let d = project_dir(&root, &cwd);
        assert_eq!(d, PathBuf::from("/tmp/projects/-Users-me-work"));
    }

    #[test]
    fn session_file_layout() {
        let dir = PathBuf::from("/tmp/projects/-foo");
        let id = SessionId::new();
        let f = session_file(&dir, &id);
        assert_eq!(f.parent(), Some(dir.as_path()));
        assert_eq!(f.extension().and_then(|e| e.to_str()), Some("jsonl"));
        let stem = f.file_stem().and_then(|s| s.to_str()).unwrap();
        assert_eq!(stem, id.to_string());
    }

    #[test]
    fn project_local_state_layout() {
        let cwd = PathBuf::from("/Users/me/work");
        assert_eq!(
            project_session_state_file(&cwd),
            PathBuf::from("/Users/me/work/.atta/session.json")
        );
    }

    #[test]
    fn session_sidecar_layout() {
        let root = PathBuf::from("/tmp/atta/code/sessions");
        let id = SessionId::new();
        assert_eq!(session_sidecar_dir(&root, &id), root.join(id.to_string()));
        assert_eq!(
            session_tui_input_history_file(&root, &id),
            root.join(id.to_string()).join("tui_input_history.jsonl")
        );
        assert_eq!(
            session_repl_input_history_file(&root, &id),
            root.join(id.to_string()).join("repl_input_history.txt")
        );
        assert_eq!(
            session_memory_file(&root, &id),
            root.join(id.to_string()).join("session_memory.md")
        );
        assert_eq!(
            session_prompt_state_file(&root, &id),
            root.join(id.to_string()).join("prompt_state.json")
        );
        assert_eq!(
            session_metadata_file(&root, &id),
            root.join(id.to_string()).join("metadata.json")
        );
    }

    #[tokio::test]
    async fn canonicalize_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().to_path_buf();
        let c = canonicalize_cwd(&p).await.unwrap();
        // 实际上 canonicalize 应该等于 p（或其 realpath 后的形式）
        assert!(c.exists());
    }

    #[tokio::test]
    async fn canonicalize_nonexistent_path_falls_back() {
        let p = PathBuf::from("/this/path/does/not/exist/anywhere/i/hope");
        let c = canonicalize_cwd(&p).await.unwrap();
        assert_eq!(c, p);
    }

    /// 两个根同源——它们曾经各读各的 env 变量，默认相同但可以被配散，
    /// session memory 因此写去过没人会读的地方。
    #[test]
    fn both_roots_come_from_the_one_global_root() {
        let roots = HistoryRoots::under(Path::new("/state/.atta"));
        assert_eq!(roots.projects, Path::new("/state/.atta/projects"));
        assert_eq!(roots.sessions, Path::new("/state/.atta/sessions"));
    }
}
