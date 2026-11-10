//! Memory file management, cross-session memory search, and AGIX.md loading/migration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Iso8601;
use time::OffsetDateTime;

/// AGIX.md total length cap.
const MAX_CLAUDE_MD_CHARS: usize = 20_000;

/// 一段 AGIX.md 文件（路径 + 内容）。多条按"远到近"顺序排列。
#[derive(Debug, Clone)]
pub struct MemoryFileEntry {
    pub path: PathBuf,
    pub content: String,
}

/// 简版 AGENTS.md 加载：从 cwd 向上找到 git root（或 $HOME），每一层看
/// `AGENTS.md`。顺序：repo root -> 子目录 -> cwd（远到近）。去重 by canonical path。
/// **总长上限**：20 KB；超了从最远那段截。
///
/// `walk_up=false` 时只读 cwd 级 `AGENTS.md`，跳过中间所有父目录。给 monorepo
/// 子目录想隔离父级上下文用。
///
/// `AGENTS.md` 是唯一权威指令文件，**不读** `.agix/AGIX.md`。
pub(crate) async fn collect_memory_files_with(
    cwd: &Path,
    do_walk_up: bool,
    files: &crate::paths::PromptFiles,
) -> Vec<MemoryFileEntry> {
    let mut visited = std::collections::HashSet::new();
    let mut walk_up: Vec<PathBuf> = Vec::new();

    if do_walk_up {
        // 从 cwd 向上爬到根
        let mut p = cwd.to_path_buf();
        loop {
            let candidate = p.join("AGENTS.md");
            if candidate.exists() {
                walk_up.push(candidate);
            }
            match p.parent() {
                Some(parent) if parent != p => p = parent.to_path_buf(),
                _ => break,
            }
        }
    } else {
        // 只 cwd 级
        let candidate = cwd.join("AGENTS.md");
        if candidate.exists() {
            walk_up.push(candidate);
        }
    }

    // 远到近：顶层 repo → cwd → 子（这里 walk_up 是 cwd 向上，所以反转）
    walk_up.reverse();

    let mut entries: Vec<MemoryFileEntry> = Vec::new();
    let mut total_len = 0usize;

    for path in walk_up {
        let Ok(canonical) = files.resolve(&path) else {
            continue;
        };
        if !visited.insert(canonical.clone()) {
            continue;
        }
        let Ok(content) = tokio::fs::read_to_string(&canonical).await else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        let block_len = content.len();
        // 总长达上限 -> 不再 push 后续段；保最近的（向后截留更近的内容）
        if total_len + block_len > MAX_CLAUDE_MD_CHARS {
            break;
        }
        total_len += block_len;
        entries.push(MemoryFileEntry {
            path: canonical,
            content,
        });
    }

    entries
}

/// Walk up from `cwd` to root, find `CLAUDE.md` at each level, and
/// create/update the sibling `AGIX.md` if `CLAUDE.md` has changed since the
/// last migration. Records per-path mtime in `<cwd>/.agix/migration.json`.
///
/// # Merge strategy
///
/// The first run wraps all `CLAUDE.md` content in HTML comment markers:
///
/// ```markdown
/// <!-- CLAUDE_MIGRATED_AT: 2026-05-18T12:34:56Z -->
/// <!-- CLAUDE_MIGRATED_BEGIN -->
/// [CLAUDE.md content]
/// <!-- CLAUDE_MIGRATED_END -->
/// ```
///
/// On subsequent runs, content between `BEGIN` and `END` markers is replaced
/// with the updated `CLAUDE.md` content. Any content **after** the `END`
/// marker (user edits) is preserved.
///
/// This runs at session start only (not on compact rebuilds).
///
/// **Not called anywhere in this codebase.** Kept as the foundation for a
/// user-confirmed import, which would replace the automatic behavior above
/// with an explicit confirmation step and target `AGENTS.md` instead of
/// `AGIX.md`, reusing the merge-marker logic.
pub async fn maybe_migrate_claude_to_agix(cwd: &Path) {
    // Walk up from cwd to root (same pattern as collect_memory_files_with)
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut p = cwd.to_path_buf();
    loop {
        dirs.push(p.clone());
        match p.parent() {
            Some(parent) if parent != p => p = parent.to_path_buf(),
            _ => break,
        }
    }

    // Collect candidate CLAUDE.md paths
    let claude_paths: Vec<PathBuf> = dirs
        .iter()
        .map(|d| d.join("CLAUDE.md"))
        .filter(|p| p.exists())
        .collect();

    if claude_paths.is_empty() {
        return;
    }

    // Load migration state from cwd/.agix/migration.json
    let mig_path = cwd.join(".agix").join("code").join("migration.json");
    let mut state: MigrationState = load_migration_state(&mig_path).await;

    let mut dirty = false;
    let now = OffsetDateTime::now_utc();

    for raw_path in &claude_paths {
        // Canonicalize before using as key to avoid duplicate records
        // from the same file reached through different symlink paths.
        let Ok(claude_path) = tokio::fs::canonicalize(raw_path).await else {
            continue;
        };
        let mtime_secs = match file_mtime_secs(&claude_path).await {
            Some(s) => s,
            None => continue,
        };
        let path_str = claude_path.to_string_lossy().to_string();
        let last = state.files.get(&path_str).copied().unwrap_or(0);

        if mtime_secs <= last {
            continue;
        }

        let claude_content = match tokio::fs::read_to_string(&claude_path).await {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Build AGIX.md path (same dir as CLAUDE.md)
        let agix_path = claude_path.with_file_name("AGIX.md");
        let existing = tokio::fs::read_to_string(&agix_path)
            .await
            .unwrap_or_default();
        let new_agix = merge_agix_content(&existing, &claude_content, &now);

        if let Some(parent) = agix_path.parent() {
            if tokio::fs::create_dir_all(parent).await.is_err() {
                continue;
            }
        }
        if tokio::fs::write(&agix_path, &new_agix).await.is_err() {
            continue;
        }

        state.files.insert(path_str, mtime_secs);
        dirty = true;
    }

    if dirty {
        state.migrated_at = Some(
            now.format(&Iso8601::DEFAULT)
                .unwrap_or_else(|_| "unknown".to_string()),
        );
        if let Some(parent) = mig_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = tokio::fs::write(&mig_path, &json).await;
        }
    }
}

/// Per-file migration state, persisted in `.agix/migration.json`.
#[derive(Debug, Serialize, Deserialize, Default)]
struct MigrationState {
    /// Human-readable ISO 8601 timestamp of the most recent migration run.
    #[serde(skip_serializing_if = "Option::is_none")]
    migrated_at: Option<String>,
    /// Map from canonical CLAUDE.md path -> mtime (seconds since epoch).
    files: HashMap<String, u64>,
}

async fn load_migration_state(path: &Path) -> MigrationState {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => MigrationState::default(),
    }
}

/// Returns the mtime of `path` as seconds since Unix epoch.
async fn file_mtime_secs(path: &Path) -> Option<u64> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let modified = metadata.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Merge CLAUDE.md content into existing AGIX.md content.
///
/// - If AGIX.md doesn't exist: wrap CLAUDE.md content in comment markers.
/// - If markers exist: replace content between begin/end, preserve user edits after end.
/// - If no markers: prepend the marker section before existing content.
fn merge_agix_content(existing: &str, claude_content: &str, now: &OffsetDateTime) -> String {
    let ts = now
        .format(&Iso8601::DEFAULT)
        .unwrap_or_else(|_| "unknown".to_string());
    const BEGIN_MARKER: &str = "<!-- CLAUDE_MIGRATED_BEGIN -->";
    const END_MARKER: &str = "<!-- CLAUDE_MIGRATED_END -->";

    let header = format!("<!-- CLAUDE_MIGRATED_AT: {ts} -->\n{BEGIN_MARKER}\n{claude_content}");

    if existing.is_empty() {
        return format!("{header}\n{END_MARKER}\n");
    }

    // Check if AGIX.md already has migration markers
    let begin_pos = existing.find("<!-- CLAUDE_MIGRATED_BEGIN");
    let end_pos = existing.find("<!-- CLAUDE_MIGRATED_END");

    if let (Some(_bp), Some(ep)) = (begin_pos, end_pos) {
        // Replace content between markers, preserve trailing user content
        let after_end = ep + END_MARKER.len();
        let user_tail = existing[after_end..].trim();
        let cap = header.len() + END_MARKER.len() + user_tail.len() + 4;
        let mut result = String::with_capacity(cap);
        result.push_str(&header);
        result.push('\n');
        result.push_str(END_MARKER);
        if !user_tail.is_empty() {
            result.push_str("\n\n");
            result.push_str(user_tail);
        }
        result.push('\n');
        return result;
    }

    // No markers: prepend migrated header to preserve existing content
    format!("{header}\n{END_MARKER}\n\n{}\n", existing.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn walk_up_false_skips_parent_claude_md() {
        // parent 有 AGENTS.md / cwd 有 AGENTS.md；walk_up=false 应只读 cwd
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        tokio::fs::create_dir_all(&child).await.unwrap();
        tokio::fs::write(parent.join("AGENTS.md"), "PARENT-MONOREPO")
            .await
            .unwrap();
        tokio::fs::write(child.join("AGENTS.md"), "CHILD-LOCAL")
            .await
            .unwrap();

        // walk_up = true（默认）-- parent 应该出现
        let with_walk =
            collect_memory_files_with(&child, true, &crate::paths::PromptFiles::unscoped()).await;
        assert!(with_walk
            .iter()
            .any(|e| e.content.contains("PARENT-MONOREPO")));
        assert!(with_walk.iter().any(|e| e.content.contains("CHILD-LOCAL")));

        // walk_up = false -- 只 child
        let no_walk =
            collect_memory_files_with(&child, false, &crate::paths::PromptFiles::unscoped()).await;
        assert!(!no_walk
            .iter()
            .any(|e| e.content.contains("PARENT-MONOREPO")));
        assert!(no_walk.iter().any(|e| e.content.contains("CHILD-LOCAL")));
    }

    #[test]
    fn merge_agix_empty_creates_wrapped_content() {
        let now = OffsetDateTime::now_utc();
        let result = merge_agix_content("", "# Hello\n\nSome CLAUDE.md", &now);
        assert!(result.contains("<!-- CLAUDE_MIGRATED_AT:"));
        assert!(result.contains("<!-- CLAUDE_MIGRATED_BEGIN -->"));
        assert!(result.contains("Some CLAUDE.md"));
        assert!(result.contains("<!-- CLAUDE_MIGRATED_END -->"));
    }

    #[test]
    fn merge_agix_with_existing_markers_replaces_content() {
        let now = OffsetDateTime::now_utc();
        let existing = "<!-- CLAUDE_MIGRATED_AT: 2024-01-01T00:00:00Z -->\n<!-- CLAUDE_MIGRATED_BEGIN -->\nold content\n<!-- CLAUDE_MIGRATED_END -->\n\n# User additions\ncustom notes\n";
        let result = merge_agix_content(existing, "NEW content", &now);
        assert!(result.contains("NEW content"));
        assert!(!result.contains("old content"));
        assert!(result.contains("# User additions"));
        assert!(result.contains("custom notes"));
    }

    #[test]
    fn merge_agix_no_markers_prepends() {
        let now = OffsetDateTime::now_utc();
        let result = merge_agix_content("# My AGIX\n\nsome content", "CLAUDE content", &now);
        assert!(result.contains("<!-- CLAUDE_MIGRATED_BEGIN -->"));
        assert!(result.contains("CLAUDE content"));
        assert!(result.contains("# My AGIX"));
    }
}
