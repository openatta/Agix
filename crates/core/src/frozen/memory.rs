//! Memory file management, cross-session memory search, and AGIX.md loading/migration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Iso8601;
use time::OffsetDateTime;

use super::frontmatter::{extract_yaml_field, split_frontmatter};
use super::utils::{sanitize_for_dir, truncate_chars, truncate_memory_entrypoint};

/// AGIX.md total length cap.
const MAX_CLAUDE_MD_CHARS: usize = 20_000;

/// 一段 AGIX.md 文件（路径 + 内容）。多条按"远到近"顺序排列。
#[derive(Debug, Clone)]
pub struct MemoryFileEntry {
    pub path: PathBuf,
    pub content: String,
}

/// 跨会话 memory 目录 ＋ MEMORY.md 加载。
///
/// 路径：`~/.agix/memory/<sha256(canonical_cwd)[..16]>/`——**2026-08-04 起不再
/// 挂 scope**：memory 跟 sessions/recordings 一样，只分"项目级"与"全局级"，不分
/// scene（不同 scene 的工具配置跟这份 memory 属于哪个项目无关）。
///
/// 行为：
/// - 目录不存在不主动创建（避免到处冒目录）；模型用 FileWrite 写时它会自动创建
/// - 目录里 `MEMORY.md` 存在则读出来注入 system prompt（同 AGENTS.md 玩法）
/// - MEMORY.md 长度截 8KB -- 它本就该是索引而非详情
///
/// 失败时返回 (默认 Path, None) -- 不阻塞 session 启动。
pub(crate) async fn collect_memory(cwd: &Path, global_root: &Path) -> (PathBuf, Option<String>) {
    use sha2::{Digest, Sha256};
    let canonical = tokio::fs::canonicalize(cwd)
        .await
        .unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    // 16 hex chars = 64 bits 命名空间，碰撞率 < 2^-32 即便 50k 项目也安全
    let hash_hex: String = hash[..8].iter().map(|b| format!("{:02x}", b)).collect();

    let dir = global_root.join("memory").join(&hash_hex);

    let index_path = dir.join("MEMORY.md");
    let index = tokio::fs::read_to_string(&index_path)
        .await
        .ok()
        .map(|s| truncate_memory_entrypoint(&s));

    (dir, index)
}

/// 项目级 memdir：`~/.agix/memory/<sanitize(cwd)>/*.md`，跨 session 持久知识
/// （如"项目 X 常用命令"、"曾经的设计决策"）。只是文件加载；
/// auto-extract / save tools 推迟。返回路径列表按文件名排序，让加载顺序稳定。
/// **2026-08-04 起不再挂 scope**，理由同 [`collect_memory`]。
pub(crate) async fn collect_memdir_files(global_root: &Path, cwd: &Path) -> Vec<PathBuf> {
    // Cap the count of memory files loaded, keeping the newest by mtime.
    const MAX_MEMORY_FILES: usize = 200;
    let sanitized = sanitize_for_dir(&cwd.display().to_string());
    let dir = global_root.join("memory").join(&sanitized);
    let mut files: Vec<PathBuf> = Vec::new();
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(e) => e,
        Err(_) => return files,
    };
    while let Ok(Some(e)) = entries.next_entry().await {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) == Some("md") {
            files.push(path);
        }
    }
    // Sort newest-first by mtime (un-statable files sort oldest), then cap.
    let mut keyed: Vec<(std::time::SystemTime, PathBuf)> = Vec::with_capacity(files.len());
    for f in files {
        let mtime = tokio::fs::metadata(&f)
            .await
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        keyed.push((mtime, f));
    }
    keyed.sort_by_key(|(mt, _)| std::cmp::Reverse(*mt));
    keyed
        .into_iter()
        .take(MAX_MEMORY_FILES)
        .map(|(_, p)| p)
        .collect()
}

/// 简版 AGENTS.md 加载：从 cwd 向上找到 git root（或 $HOME），每一层看
/// `AGENTS.md`。顺序：repo root -> 子目录 -> cwd（远到近）。去重 by canonical path。
/// **总长上限**：20 KB；超了从最远那段截。
///
/// `walk_up=false` 时只读 cwd 级 `AGENTS.md`，跳过中间所有父目录。给 monorepo
/// 子目录想隔离父级上下文用。
///
/// **不再读取** `.agix/AGIX.md`（旧的双轨指令文件）——`AGENTS.md` 是唯一权威
/// 指令文件。
pub(crate) async fn collect_memory_files_with(
    cwd: &Path,
    do_walk_up: bool,
    global_root: &Path,
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

    // memdir：在 repo 顶层之前插入
    // <global_root>/memory/<sanitized-cwd>/*.md
    let memdir_files = collect_memdir_files(global_root, cwd).await;
    let mut combined: Vec<PathBuf> = Vec::with_capacity(walk_up.len() + memdir_files.len());
    combined.extend(memdir_files);
    combined.extend(walk_up);

    let mut entries: Vec<MemoryFileEntry> = Vec::new();
    let mut total_len = 0usize;

    for path in combined {
        let canonical = tokio::fs::canonicalize(&path).await.unwrap_or(path.clone());
        if !visited.insert(canonical.clone()) {
            continue;
        }
        let Ok(content) = tokio::fs::read_to_string(&path).await else {
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

/// Pre-load all `.md` memory files from a project memory directory.
/// Capped at `max_files`, each truncated to 8KB.
pub(crate) async fn load_all_memory_files(
    memory_dir: &Path,
    max_files: usize,
) -> Vec<MemoryFileEntry> {
    let mut entries = match tokio::fs::read_dir(memory_dir).await {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut files: Vec<MemoryFileEntry> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if files.len() >= max_files {
            break;
        }
        let path = entry.path();
        if path.file_name().and_then(|s| s.to_str()) == Some("MEMORY.md")
            || path.extension().and_then(|s| s.to_str()) != Some("md")
        {
            continue;
        }
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            let truncated = truncate_chars(content.trim(), 8_000, "\n... (memory truncated)");
            files.push(MemoryFileEntry {
                path,
                content: truncated,
            });
        }
    }
    files
}

pub async fn find_relevant_memories(
    memory_dir: &Path,
    query: &str,
    max: usize,
) -> Vec<MemoryFileEntry> {
    let query_terms = query_terms(query);
    if query_terms.is_empty() || max == 0 {
        return Vec::new();
    }
    let mut entries = match tokio::fs::read_dir(memory_dir).await {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut scored = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.file_name().and_then(|s| s.to_str()) == Some("MEMORY.md")
            || path.extension().and_then(|s| s.to_str()) != Some("md")
        {
            continue;
        }
        let Ok(content) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        let haystack = content.to_lowercase();
        let body_score = query_terms
            .iter()
            .filter(|term| haystack.contains(term.as_str()))
            .count();

        // Title/slug weighting: frontmatter `name` and `description` fields
        // carry 3x weight because they encode the author's intent for when
        // this memory should be recalled.
        let (front, _) = split_frontmatter(&content);
        let title_score = if let Some(f) = front {
            let name = extract_yaml_field(f, "name");
            let desc = extract_yaml_field(f, "description");
            let title_text =
                [name.as_deref().unwrap_or(""), desc.as_deref().unwrap_or("")].join(" ");
            let title_lower = title_text.to_lowercase();
            query_terms
                .iter()
                .filter(|term| title_lower.contains(term.as_str()))
                .count()
                * 3
        } else {
            0
        };

        // Filename slug matching: `delete-stale-records.md` should match
        // "delete" and "stale" queries even if the body doesn't use those words.
        let slug_score = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|slug| {
                let slug_lower = slug.replace(['-', '_'], " ").to_lowercase();
                query_terms
                    .iter()
                    .filter(|term| slug_lower.contains(term.as_str()))
                    .count()
                    * 2
            })
            .unwrap_or(0);

        let score = body_score + title_score + slug_score;
        if score > 0 {
            scored.push((
                score,
                MemoryFileEntry {
                    path,
                    content: truncate_chars(content.trim(), 8_000, "\n... (memory truncated)"),
                },
            ));
        }
    }
    scored.sort_by_key(|(score, entry)| {
        (
            std::cmp::Reverse(*score),
            entry.path.file_name().map(|s| s.to_os_string()),
        )
    });
    scored
        .into_iter()
        .take(max)
        .map(|(_, entry)| entry)
        .collect()
}

/// Tokenize a query string into lowercase terms (>= 3 chars), expanded with
/// common synonyms.
fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| s.chars().count() >= 3)
        .take(24)
        .collect();
    // Expand with common synonyms so "delete stale sessions" also matches
    // "cleanup old conversations". Without this, pure keyword matching
    // misses semantically related memory entries.
    let expanded = expand_synonyms(&terms);
    terms.extend(expanded);
    terms.sort();
    terms.dedup();
    terms
}

/// Common synonym pairs for cross-session memory matching.
/// Only expands single-word terms (not phrases) to keep noise low.
fn expand_synonyms(terms: &[String]) -> Vec<String> {
    let pairs: &[(&str, &[&str])] = &[
        (
            "delete",
            &["remove", "cleanup", "purge", "clear", "destroy"],
        ),
        ("remove", &["delete", "cleanup", "purge", "clear"]),
        ("cleanup", &["purge", "clear", "remove", "delete"]),
        ("session", &["turn", "conversation", "chat"]),
        ("conversation", &["session", "chat", "turn", "dialog"]),
        ("kill", &["stop", "cancel", "terminate", "abort"]),
        ("stop", &["kill", "cancel", "terminate", "abort"]),
        ("compile", &["build", "make"]),
        ("build", &["compile", "make", "create"]),
        ("error", &["failure", "bug", "panic", "crash"]),
        ("bug", &["error", "defect", "issue", "failure"]),
        ("config", &["setting", "configuration", "preference"]),
        ("setting", &["config", "configuration", "preference"]),
        ("test", &["verify", "check", "validate"]),
        ("verify", &["test", "check", "validate", "confirm"]),
        ("memory", &["context", "history", "recall"]),
        ("slow", &["performance", "latency", "lag", "sluggish"]),
        ("fast", &["quick", "rapid", "speed"]),
        ("auth", &["login", "credential", "token", "oauth"]),
        ("token", &["auth", "credential", "key"]),
    ];
    let mut out = Vec::new();
    for term in terms {
        if let Some((_, syns)) = pairs.iter().find(|(k, _)| *k == term.as_str()) {
            for &syn in *syns {
                let syn_s = syn.to_string();
                if !terms.contains(&syn_s) {
                    out.push(syn_s);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// AGIX.md migration from CLAUDE.md
// ---------------------------------------------------------------------------

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
/// **Not currently called anywhere in this codebase** — verified by
/// full-repo grep during the 2026-08-03 AGENTS.md/.agents migration. This is
/// meant as the foundation for the future user-confirmed import feature
/// (migration doc Phase 3), which will supersede the automatic/silent
/// behavior described above with an explicit user-confirmation step and
/// retarget the output to `AGENTS.md` instead of `AGIX.md`. Left as-is (not
/// deleted, not wired up) so that future work can reuse the merge-marker
/// logic.
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

    // -----------------------------------------------------------------------
    // collect_memory_files_with / walk_up
    // -----------------------------------------------------------------------

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
        let with_walk = collect_memory_files_with(&child, true, &dir.path().join("state")).await;
        assert!(with_walk
            .iter()
            .any(|e| e.content.contains("PARENT-MONOREPO")));
        assert!(with_walk.iter().any(|e| e.content.contains("CHILD-LOCAL")));

        // walk_up = false -- 只 child
        let no_walk = collect_memory_files_with(&child, false, &dir.path().join("state")).await;
        assert!(!no_walk
            .iter()
            .any(|e| e.content.contains("PARENT-MONOREPO")));
        assert!(no_walk.iter().any(|e| e.content.contains("CHILD-LOCAL")));
    }

    // -----------------------------------------------------------------------
    // memdir
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn memdir_loads_md_files_for_project() {
        // 假装的状态根 + cwd
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let sanitized = sanitize_for_dir(&cwd.path().display().to_string());
        let memdir = home.path().join(".agix").join("memory").join(&sanitized);
        tokio::fs::create_dir_all(&memdir).await.unwrap();
        tokio::fs::write(memdir.join("commands.md"), "# Common commands\nfoo\nbar")
            .await
            .unwrap();
        tokio::fs::write(memdir.join("decisions.md"), "# Past decisions")
            .await
            .unwrap();
        // 非 md 应当不收
        tokio::fs::write(memdir.join("note.txt"), "ignore me")
            .await
            .unwrap();

        let files = collect_memdir_files(&home.path().join(".agix"), cwd.path()).await;
        assert_eq!(files.len(), 2);
        // Order is mtime-newest-first; assert by membership, not position.
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.contains("commands")));
        assert!(names.iter().any(|n| n.contains("decisions")));
        assert!(!names.iter().any(|n| n.contains("note.txt")));
    }

    #[tokio::test]
    async fn memdir_returns_empty_when_dir_missing() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let files = collect_memdir_files(&home.path().join(".agix"), cwd.path()).await;
        assert!(files.is_empty());
    }

    // -----------------------------------------------------------------------
    // find_relevant_memories
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn find_relevant_memories_scores_and_ranks_by_query_match() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("delete-sessions.md"),
            "---\nname: delete-sessions\ndescription: How to delete stale sessions\n---\n# Delete Sessions\nUse the purge command to clean old sessions.",
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.path().join("build-config.md"),
            "---\nname: build-config\ndescription: Build configuration tips\n---\n# Build Config\nSet up your build pipeline.",
        )
        .await
        .unwrap();
        // Non-md file -- should be skipped
        tokio::fs::write(dir.path().join("notes.txt"), "delete all the things")
            .await
            .unwrap();

        let results = find_relevant_memories(dir.path(), "delete old sessions", 5).await;
        assert!(!results.is_empty());
        let first = &results[0];
        assert!(first
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("delete-sessions"));
    }

    #[tokio::test]
    async fn find_relevant_memories_skips_memory_md_index() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("MEMORY.md"),
            "# Index\n- [delete](delete.md)",
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.path().join("topic.md"),
            "---\nname: topic\ndescription: A topic\n---\nbody text",
        )
        .await
        .unwrap();

        let results = find_relevant_memories(dir.path(), "topic", 5).await;
        assert_eq!(results.len(), 1);
        assert!(results[0]
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("topic"));
    }

    #[tokio::test]
    async fn find_relevant_memories_returns_empty_for_no_match() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("topic.md"),
            "---\nname: topic\n---\ncompletely unrelated text",
        )
        .await
        .unwrap();

        let results = find_relevant_memories(dir.path(), "zzzxyqnotfound", 5).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn find_relevant_memories_respects_max_limit() {
        let dir = TempDir::new().unwrap();
        for i in 0..5 {
            tokio::fs::write(
                dir.path().join(format!("m{i}.md")),
                format!("---\nname: m{i}\ndescription: common\n---\ncommon query text"),
            )
            .await
            .unwrap();
        }
        let results = find_relevant_memories(dir.path(), "common", 2).await;
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn find_relevant_memories_title_slug_weighting() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("delete-records.md"),
            "---\nname: delete-records\ndescription: How to delete records\n---\n# Delete\nUse delete to remove records.",
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.path().join("other.md"),
            "---\nname: unrelated\ndescription: something else\n---\nYou might want to delete things.",
        )
        .await
        .unwrap();

        let results = find_relevant_memories(dir.path(), "delete records", 5).await;
        assert_eq!(results.len(), 2);
        assert!(results[0]
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("delete-records"));
    }

    // -----------------------------------------------------------------------
    // query_terms & expand_synonyms
    // -----------------------------------------------------------------------

    #[test]
    fn query_terms_splits_and_filters_short_tokens() {
        let terms = query_terms("fix the auth bug in login");
        // "in" (2 chars) filtered; "fix" "the" "auth" "bug" "login" kept (>= 3 chars)
        assert!(terms.contains(&"fix".to_string()));
        assert!(terms.contains(&"auth".to_string()));
        assert!(terms.contains(&"bug".to_string()));
        assert!(terms.contains(&"login".to_string()));
        assert!(terms.contains(&"the".to_string()));
        assert!(!terms.contains(&"in".to_string()));
    }

    #[test]
    fn query_terms_expands_synonyms() {
        let terms = query_terms("delete stale sessions");
        // "delete" -> "remove", "cleanup", "purge", "clear", "destroy"
        assert!(terms.contains(&"remove".to_string()));
        assert!(terms.contains(&"cleanup".to_string()));
        assert!(terms.contains(&"purge".to_string()));
        assert!(terms.contains(&"delete".to_string()));
    }

    #[test]
    fn query_terms_deduplicates_after_expansion() {
        let terms = query_terms("delete remove cleanup");
        let unique: std::collections::HashSet<_> = terms.iter().collect();
        assert_eq!(terms.len(), unique.len(), "no duplicates");
    }

    #[test]
    fn query_terms_empty_yields_empty() {
        assert!(query_terms("").is_empty());
        assert!(query_terms("a b c").is_empty());
    }

    #[test]
    fn expand_synonyms_adds_known_pairs() {
        let input: Vec<String> = vec!["compile".into(), "fast".into()];
        let expanded = expand_synonyms(&input);
        assert!(expanded.contains(&"build".to_string()));
        assert!(expanded.contains(&"make".to_string()));
        assert!(expanded.contains(&"quick".to_string()));
        assert!(expanded.contains(&"rapid".to_string()));
    }

    #[test]
    fn expand_synonyms_skips_unknown_terms() {
        let input: Vec<String> = vec!["fizzbuzz".into()];
        let expanded = expand_synonyms(&input);
        assert!(expanded.is_empty());
    }

    #[test]
    fn expand_synonyms_avoids_self_duplication() {
        let input: Vec<String> = vec!["delete".into(), "remove".into()];
        let expanded = expand_synonyms(&input);
        // "delete" -> "remove" but "remove" is already in input
        assert!(!expanded.contains(&"remove".to_string()));
    }

    // -----------------------------------------------------------------------
    // merge_agix_content
    // -----------------------------------------------------------------------

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

    #[tokio::test]
    async fn memdir_files_capped_at_200() {
        let tmp = tempfile::TempDir::new().unwrap();
        let global_root = tmp.path().join(".agix");
        let cwd = std::path::Path::new("/fake/project/abc");
        let sanitized = sanitize_for_dir(&cwd.display().to_string());
        let dir = global_root.join("memory").join(&sanitized);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        for i in 0..250u32 {
            tokio::fs::write(dir.join(format!("m{i}.md")), format!("body {i}"))
                .await
                .unwrap();
        }
        let got = collect_memdir_files(&global_root, cwd).await;
        assert_eq!(got.len(), 200);
    }
}
