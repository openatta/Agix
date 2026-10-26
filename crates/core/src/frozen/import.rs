//! Cross-tool configuration import — detect and migrate Claude Code
//! (`CLAUDE.md`/`.claude/skills/`), Cursor (`.cursorrules`/`.cursor/rules/*.mdc`),
//! and Codex-style (`AGENTS.md` present but `.agents/skills/` missing) project
//! state into this engine's `AGENTS.md` + `.agents/skills/` + `.agix/rules/`
//! layout.
//!
//! Three layers, kept separate on purpose:
//! - This module: pure detection + execution, no UI, no callback awareness.
//! - `ImportCallback` (crate::interface::import_callback): automatic,
//!   process-level trigger for hosts that want to prompt a human.
//! - the bundled `import` skill: manual `/import`, does the file work with
//!   the ordinary file tools rather than through this module. It writes the
//!   same `IMPORTED_FROM_<TAG>_{BEGIN,END}` markers `merge_marked_section`
//!   does, so the two paths can re-run over each other's output.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Iso8601;
use time::OffsetDateTime;

use super::frontmatter::{extract_yaml_field, split_frontmatter};

/// Which external tool a detected import source came from. Single-select by
/// design (2026-08-03 decision) — importing from more than one source at
/// once is not supported, to avoid `AGENTS.md` append-ordering conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSourceKind {
    ClaudeCode,
    Codex,
    Cursor,
}

impl ImportSourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ImportSourceKind::ClaudeCode => "claude_code",
            ImportSourceKind::Codex => "codex",
            ImportSourceKind::Cursor => "cursor",
        }
    }

    pub fn try_parse(s: &str) -> Option<Self> {
        match s {
            "claude_code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "cursor" => Some(Self::Cursor),
            _ => None,
        }
    }
}

/// A detected candidate for import, carrying enough detail to execute
/// without re-scanning the filesystem.
#[derive(Debug, Clone)]
pub enum ImportSource {
    ClaudeCode {
        claude_md: Option<PathBuf>,
        skills_dir: Option<PathBuf>,
    },
    /// `AGENTS.md` already present (Codex-style project) but `.agents/skills/`
    /// missing. Not a content transformation — just scaffolding the missing
    /// directory.
    Codex,
    Cursor {
        legacy_rules: Option<PathBuf>,
        mdc_files: Vec<PathBuf>,
    },
}

impl ImportSource {
    pub fn kind(&self) -> ImportSourceKind {
        match self {
            ImportSource::ClaudeCode { .. } => ImportSourceKind::ClaudeCode,
            ImportSource::Codex => ImportSourceKind::Codex,
            ImportSource::Cursor { .. } => ImportSourceKind::Cursor,
        }
    }

    /// Human-readable one-line summary for presenting to a user.
    pub fn describe(&self) -> String {
        match self {
            ImportSource::ClaudeCode {
                claude_md,
                skills_dir,
            } => {
                let mut parts = Vec::new();
                if claude_md.is_some() {
                    parts.push("CLAUDE.md");
                }
                if skills_dir.is_some() {
                    parts.push(".claude/skills/");
                }
                format!("Claude Code ({})", parts.join(", "))
            }
            ImportSource::Codex => "Codex (AGENTS.md present, .agents/skills/ missing)".to_string(),
            ImportSource::Cursor {
                legacy_rules,
                mdc_files,
            } => {
                let mut parts = Vec::new();
                if legacy_rules.is_some() {
                    parts.push(".cursorrules".to_string());
                }
                if !mdc_files.is_empty() {
                    parts.push(format!(".cursor/rules/*.mdc ({} files)", mdc_files.len()));
                }
                format!("Cursor ({})", parts.join(", "))
            }
        }
    }
}

/// Detect all importable sources in `cwd`. Pure read-only filesystem check —
/// does not consult or write the `.imported.json` marker. Callers that care
/// about "have we already asked" should check `already_decided()` first.
pub async fn detect_import_sources(cwd: &Path) -> Vec<ImportSource> {
    let mut sources = Vec::new();

    // Claude Code
    let claude_md_path = cwd.join("CLAUDE.md");
    let claude_md = path_exists(&claude_md_path).await.then_some(claude_md_path);
    let claude_skills_dir_path = cwd.join(".claude").join("skills");
    let claude_skills_dir = dir_has_any_skill_md(&claude_skills_dir_path)
        .await
        .then_some(claude_skills_dir_path);
    if claude_md.is_some() || claude_skills_dir.is_some() {
        sources.push(ImportSource::ClaudeCode {
            claude_md,
            skills_dir: claude_skills_dir,
        });
    }

    // Codex: AGENTS.md present but .agents/skills/ missing.
    let agents_md = cwd.join("AGENTS.md");
    let agents_skills = cwd.join(".agents").join("skills");
    if path_exists(&agents_md).await && !path_exists(&agents_skills).await {
        sources.push(ImportSource::Codex);
    }

    // Cursor
    let cursorrules_path = cwd.join(".cursorrules");
    let legacy_rules = path_exists(&cursorrules_path)
        .await
        .then_some(cursorrules_path);
    let mdc_files = scan_cursor_mdc_files(&cwd.join(".cursor").join("rules")).await;
    if legacy_rules.is_some() || !mdc_files.is_empty() {
        sources.push(ImportSource::Cursor {
            legacy_rules,
            mdc_files,
        });
    }

    sources
}

async fn path_exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

async fn dir_has_any_skill_md(dir: &Path) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return false;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let p = entry.path();
        if p.is_dir() && path_exists(&p.join("SKILL.md")).await {
            return true;
        }
    }
    false
}

async fn scan_cursor_mdc_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("mdc") {
            out.push(p);
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// `.agix/.imported.json` marker
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default)]
struct ImportedMarker {
    #[serde(skip_serializing_if = "Option::is_none")]
    decided_at: Option<String>,
    /// "imported" | "skipped". Absent (file missing, or this field missing)
    /// is treated as "deferred" — ask again next time.
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default)]
    sources_seen: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped_reason: Option<String>,
}

fn marker_path(cwd: &Path) -> PathBuf {
    cwd.join(".agix").join(".imported.json")
}

async fn load_marker(cwd: &Path) -> ImportedMarker {
    match tokio::fs::read_to_string(marker_path(cwd)).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => ImportedMarker::default(),
    }
}

async fn write_marker(cwd: &Path, marker: &ImportedMarker) -> std::io::Result<()> {
    let path = marker_path(cwd);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string_pretty(marker).unwrap_or_default();
    tokio::fs::write(path, json).await
}

/// Whether the **automatic** import-detection path should skip this project
/// entirely: either it's already an Agix project (`.agents/` exists) or
/// a previous decision (`imported`/`skipped`) was already recorded. Timeouts
/// and "defer" decisions leave no marker, so they don't count here — the
/// automatic path will ask again next process start.
///
/// The **manual** `/import` path (the bundled `import` skill) intentionally
/// does not consult this — a user explicitly asking to import should always
/// get a fresh detection, regardless of prior decisions.
pub async fn already_decided(cwd: &Path) -> bool {
    if path_exists(&cwd.join(".agents")).await {
        return true;
    }
    let marker = load_marker(cwd).await;
    matches!(
        marker.decision.as_deref(),
        Some("imported") | Some("skipped")
    )
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Record that `chosen` was imported. Called after a successful
/// `execute_import()`.
pub async fn mark_imported(
    cwd: &Path,
    sources_seen: &[ImportSource],
    chosen: ImportSourceKind,
) -> std::io::Result<()> {
    let marker = ImportedMarker {
        decided_at: Some(now_iso()),
        decision: Some("imported".to_string()),
        source: Some(chosen.as_str().to_string()),
        sources_seen: sources_seen
            .iter()
            .map(|s| s.kind().as_str().to_string())
            .collect(),
        skipped_reason: None,
    };
    write_marker(cwd, &marker).await
}

/// Record that the user explicitly declined to import. Only call this for a
/// real "no" — timeouts and "defer" should leave the marker untouched (see
/// `already_decided` docs).
pub async fn mark_skipped(
    cwd: &Path,
    sources_seen: &[ImportSource],
    reason: Option<String>,
) -> std::io::Result<()> {
    let marker = ImportedMarker {
        decided_at: Some(now_iso()),
        decision: Some("skipped".to_string()),
        source: None,
        sources_seen: sources_seen
            .iter()
            .map(|s| s.kind().as_str().to_string())
            .collect(),
        skipped_reason: reason,
    };
    write_marker(cwd, &marker).await
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ImportSummary {
    pub kind: ImportSourceKind,
    /// Human-readable log of what was written, in order.
    pub actions: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Execute the import for a single chosen source. Does **not** write the
/// `.imported.json` marker — callers do that via `mark_imported` after this
/// returns successfully, since the marker's `sources_seen` needs the full
/// detected list, not just the chosen one.
pub async fn execute_import(
    cwd: &Path,
    source: &ImportSource,
) -> Result<ImportSummary, ImportError> {
    let mut actions = Vec::new();
    match source {
        ImportSource::ClaudeCode {
            claude_md,
            skills_dir,
        } => {
            if let Some(claude_md_path) = claude_md {
                if let Ok(content) = tokio::fs::read_to_string(claude_md_path).await {
                    merge_into_agents_md(cwd, &content, "CLAUDE_CODE").await?;
                    actions.push(format!(
                        "merged {} into AGENTS.md",
                        claude_md_path.display()
                    ));
                }
            }
            if let Some(src_dir) = skills_dir {
                let dest_dir = cwd.join(".agents").join("skills");
                let copied = copy_skill_dirs(src_dir, &dest_dir).await?;
                actions.push(format!(
                    "copied {copied} skill dir(s) from .claude/skills/ to .agents/skills/"
                ));
            }
        }
        ImportSource::Codex => {
            let dest = cwd.join(".agents").join("skills");
            tokio::fs::create_dir_all(&dest).await?;
            actions.push("created empty .agents/skills/ directory".to_string());
        }
        ImportSource::Cursor {
            legacy_rules,
            mdc_files,
        } => {
            if let Some(rules_path) = legacy_rules {
                if let Ok(content) = tokio::fs::read_to_string(rules_path).await {
                    merge_into_agents_md(cwd, &content, "CURSOR_RULES").await?;
                    actions.push(format!("merged {} into AGENTS.md", rules_path.display()));
                }
            }
            for mdc_path in mdc_files {
                if let Ok(content) = tokio::fs::read_to_string(mdc_path).await {
                    if let Some(slug) = mdc_path.file_stem().and_then(|s| s.to_str()) {
                        let (rule_body, always_apply) = convert_mdc_to_rule(&content);
                        let dest = cwd.join(".agix").join("rules").join(format!("{slug}.md"));
                        if let Some(parent) = dest.parent() {
                            tokio::fs::create_dir_all(parent).await?;
                        }
                        tokio::fs::write(&dest, rule_body).await?;
                        actions.push(format!("wrote .agix/rules/{slug}.md"));
                        if always_apply {
                            append_rule_reference(cwd, &format!(".agix/rules/{slug}.md")).await?;
                            actions.push(format!(
                                "referenced always-apply rule {slug} from AGENTS.md"
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(ImportSummary {
        kind: source.kind(),
        actions,
    })
}

async fn merge_into_agents_md(
    cwd: &Path,
    content: &str,
    marker_tag: &str,
) -> Result<(), ImportError> {
    let agents_path = cwd.join("AGENTS.md");
    let existing = tokio::fs::read_to_string(&agents_path)
        .await
        .unwrap_or_default();
    let merged = merge_marked_section(&existing, content, marker_tag);
    tokio::fs::write(&agents_path, merged).await?;
    Ok(())
}

/// Wrap `new_content` in HTML-comment markers tagged with `tag` and merge it
/// into `existing`. On repeat calls with the same `tag`, replaces only the
/// content between the markers, preserving anything after `END` (user edits).
/// Generalizes the marker-wrap strategy `merge_agix_content()` in `memory.rs`
/// used for the (unwired) `CLAUDE.md -> AGIX.md` auto-migration, parameterized
/// by `tag` so different import sources don't collide with each other's
/// sections in the same `AGENTS.md`.
fn merge_marked_section(existing: &str, new_content: &str, tag: &str) -> String {
    let ts = now_iso();
    let begin = format!("<!-- IMPORTED_FROM_{tag}_BEGIN -->");
    let end = format!("<!-- IMPORTED_FROM_{tag}_END -->");
    let header = format!("<!-- IMPORTED_FROM_{tag}_AT: {ts} -->\n{begin}\n{new_content}");

    if existing.is_empty() {
        return format!("{header}\n{end}\n");
    }

    if let (Some(_bp), Some(ep)) = (existing.find(&begin), existing.find(&end)) {
        let after_end = ep + end.len();
        let tail = existing[after_end..].trim();
        let mut result = format!("{header}\n{end}");
        if !tail.is_empty() {
            result.push_str("\n\n");
            result.push_str(tail);
        }
        result.push('\n');
        return result;
    }

    format!("{header}\n{end}\n\n{}\n", existing.trim())
}

/// Copy each `<name>/SKILL.md` subdirectory from `src_dir` into `dest_dir`,
/// skipping names that already exist at the destination (new/manually-created
/// skills win — this is a one-time import, not a sync).
async fn copy_skill_dirs(src_dir: &Path, dest_dir: &Path) -> std::io::Result<usize> {
    let mut count = 0;
    let mut entries = match tokio::fs::read_dir(src_dir).await {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let src = entry.path();
        if !src.is_dir() {
            continue;
        }
        let skill_md = src.join("SKILL.md");
        if tokio::fs::metadata(&skill_md).await.is_err() {
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        let dest = dest_dir.join(name);
        if tokio::fs::metadata(&dest).await.is_ok() {
            continue; // don't clobber an existing project skill of the same name
        }
        tokio::fs::create_dir_all(&dest).await?;
        tokio::fs::copy(&skill_md, dest.join("SKILL.md")).await?;
        count += 1;
    }
    Ok(count)
}

/// Parse a Cursor `.mdc` file's YAML frontmatter (`description`/`globs`/
/// `alwaysApply`) and produce the `.agix/rules/<slug>.md` body. `globs` is
/// kept as a documentation comment only — Agix's `.agix/rules/` is
/// lazily loaded (via `AGENTS.md` references), not auto-matched by path, so
/// there's no live glob-matching behavior to wire it into. Returns
/// `(body, always_apply)`.
fn convert_mdc_to_rule(content: &str) -> (String, bool) {
    let (front, body) = split_frontmatter(content);
    let mut always_apply = false;
    let mut header = String::new();
    if let Some(yaml) = front {
        if let Some(desc) = extract_yaml_field(yaml, "description") {
            header.push_str(&format!("<!-- description: {desc} -->\n"));
        }
        if let Some(globs) = extract_yaml_field(yaml, "globs") {
            header.push_str(&format!(
                "<!-- globs: {globs} (imported from Cursor; not auto-matched by Agix) -->\n"
            ));
        }
        if let Some(aa) = extract_yaml_field(yaml, "alwaysApply") {
            always_apply = aa.eq_ignore_ascii_case("true");
        }
    }
    (format!("{header}{body}"), always_apply)
}

async fn append_rule_reference(cwd: &Path, rule_path: &str) -> Result<(), ImportError> {
    let agents_path = cwd.join("AGENTS.md");
    let mut existing = tokio::fs::read_to_string(&agents_path)
        .await
        .unwrap_or_default();
    let line = format!("- `{rule_path}` (always-apply, imported from Cursor)");
    if !existing.contains(&line) {
        if !existing.is_empty() && !existing.ends_with('\n') {
            existing.push('\n');
        }
        existing.push_str(&line);
        existing.push('\n');
        tokio::fs::write(&agents_path, existing).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn detects_nothing_in_empty_project() {
        let dir = TempDir::new().unwrap();
        let sources = detect_import_sources(dir.path()).await;
        assert!(sources.is_empty());
    }

    #[tokio::test]
    async fn detects_claude_md() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "# instructions")
            .await
            .unwrap();
        let sources = detect_import_sources(dir.path()).await;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind(), ImportSourceKind::ClaudeCode);
    }

    #[tokio::test]
    async fn detects_claude_skills_dir() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join(".claude/skills/demo");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        tokio::fs::write(skill_dir.join("SKILL.md"), "---\ndescription: x\n---\nbody")
            .await
            .unwrap();
        let sources = detect_import_sources(dir.path()).await;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind(), ImportSourceKind::ClaudeCode);
    }

    #[tokio::test]
    async fn detects_codex_gap() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("AGENTS.md"), "# instructions")
            .await
            .unwrap();
        let sources = detect_import_sources(dir.path()).await;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind(), ImportSourceKind::Codex);
    }

    #[tokio::test]
    async fn no_codex_gap_when_agents_skills_present() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("AGENTS.md"), "# instructions")
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.path().join(".agents/skills"))
            .await
            .unwrap();
        let sources = detect_import_sources(dir.path()).await;
        assert!(sources.is_empty());
    }

    #[tokio::test]
    async fn detects_cursor_legacy_rules_and_mdc() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(".cursorrules"), "be nice")
            .await
            .unwrap();
        let rules_dir = dir.path().join(".cursor/rules");
        tokio::fs::create_dir_all(&rules_dir).await.unwrap();
        tokio::fs::write(
            rules_dir.join("style.mdc"),
            "---\ndescription: style\n---\nbody",
        )
        .await
        .unwrap();
        let sources = detect_import_sources(dir.path()).await;
        assert_eq!(sources.len(), 1);
        match &sources[0] {
            ImportSource::Cursor {
                legacy_rules,
                mdc_files,
            } => {
                assert!(legacy_rules.is_some());
                assert_eq!(mdc_files.len(), 1);
            }
            _ => panic!("expected Cursor source"),
        }
    }

    #[tokio::test]
    async fn detects_multiple_sources_simultaneously() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "x")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join(".cursorrules"), "y")
            .await
            .unwrap();
        let sources = detect_import_sources(dir.path()).await;
        assert_eq!(sources.len(), 2);
    }

    #[tokio::test]
    async fn already_decided_true_when_agents_dir_exists() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".agents"))
            .await
            .unwrap();
        assert!(already_decided(dir.path()).await);
    }

    #[tokio::test]
    async fn already_decided_false_for_fresh_project() {
        let dir = TempDir::new().unwrap();
        assert!(!already_decided(dir.path()).await);
    }

    #[tokio::test]
    async fn already_decided_true_after_mark_imported() {
        let dir = TempDir::new().unwrap();
        mark_imported(dir.path(), &[], ImportSourceKind::ClaudeCode)
            .await
            .unwrap();
        assert!(already_decided(dir.path()).await);
    }

    #[tokio::test]
    async fn already_decided_true_after_mark_skipped() {
        let dir = TempDir::new().unwrap();
        mark_skipped(dir.path(), &[], Some("user said no".into()))
            .await
            .unwrap();
        assert!(already_decided(dir.path()).await);
    }

    #[tokio::test]
    async fn execute_claude_code_merges_agents_md_and_copies_skills() {
        let dir = TempDir::new().unwrap();
        let claude_md = dir.path().join("CLAUDE.md");
        tokio::fs::write(&claude_md, "Be concise.").await.unwrap();
        let skill_src = dir.path().join(".claude/skills/demo");
        tokio::fs::create_dir_all(&skill_src).await.unwrap();
        tokio::fs::write(
            skill_src.join("SKILL.md"),
            "---\ndescription: demo\n---\nbody",
        )
        .await
        .unwrap();

        let source = ImportSource::ClaudeCode {
            claude_md: Some(claude_md),
            skills_dir: Some(dir.path().join(".claude/skills")),
        };
        let summary = execute_import(dir.path(), &source).await.unwrap();
        assert_eq!(summary.kind, ImportSourceKind::ClaudeCode);
        assert_eq!(summary.actions.len(), 2);

        let agents_md = tokio::fs::read_to_string(dir.path().join("AGENTS.md"))
            .await
            .unwrap();
        assert!(agents_md.contains("Be concise."));
        assert!(agents_md.contains("IMPORTED_FROM_CLAUDE_CODE_BEGIN"));

        let copied_skill = dir.path().join(".agents/skills/demo/SKILL.md");
        assert!(tokio::fs::metadata(&copied_skill).await.is_ok());
    }

    #[tokio::test]
    async fn execute_claude_code_does_not_clobber_existing_project_skill() {
        let dir = TempDir::new().unwrap();
        let skill_src = dir.path().join(".claude/skills/demo");
        tokio::fs::create_dir_all(&skill_src).await.unwrap();
        tokio::fs::write(
            skill_src.join("SKILL.md"),
            "---\ndescription: old\n---\nbody",
        )
        .await
        .unwrap();
        let existing_dest = dir.path().join(".agents/skills/demo");
        tokio::fs::create_dir_all(&existing_dest).await.unwrap();
        tokio::fs::write(
            existing_dest.join("SKILL.md"),
            "---\ndescription: NEW\n---\nbody",
        )
        .await
        .unwrap();

        let source = ImportSource::ClaudeCode {
            claude_md: None,
            skills_dir: Some(dir.path().join(".claude/skills")),
        };
        execute_import(dir.path(), &source).await.unwrap();

        let content = tokio::fs::read_to_string(existing_dest.join("SKILL.md"))
            .await
            .unwrap();
        assert!(
            content.contains("NEW"),
            "existing project skill must not be overwritten"
        );
    }

    #[tokio::test]
    async fn execute_codex_creates_empty_skills_dir() {
        let dir = TempDir::new().unwrap();
        let summary = execute_import(dir.path(), &ImportSource::Codex)
            .await
            .unwrap();
        assert_eq!(summary.kind, ImportSourceKind::Codex);
        assert!(tokio::fs::metadata(dir.path().join(".agents/skills"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn execute_cursor_merges_rules_and_converts_mdc() {
        let dir = TempDir::new().unwrap();
        let cursorrules = dir.path().join(".cursorrules");
        tokio::fs::write(&cursorrules, "always use tabs")
            .await
            .unwrap();
        let mdc_path = dir.path().join(".cursor/rules/style.mdc");
        tokio::fs::create_dir_all(mdc_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &mdc_path,
            "---\ndescription: style guide\nglobs: [\"*.rs\"]\nalwaysApply: true\n---\nUse rustfmt.",
        )
        .await
        .unwrap();

        let source = ImportSource::Cursor {
            legacy_rules: Some(cursorrules),
            mdc_files: vec![mdc_path],
        };
        let summary = execute_import(dir.path(), &source).await.unwrap();
        assert_eq!(summary.kind, ImportSourceKind::Cursor);

        let agents_md = tokio::fs::read_to_string(dir.path().join("AGENTS.md"))
            .await
            .unwrap();
        assert!(agents_md.contains("always use tabs"));
        assert!(agents_md.contains("IMPORTED_FROM_CURSOR_RULES_BEGIN"));
        assert!(
            agents_md.contains(".agix/rules/style.md"),
            "always-apply rule must be referenced"
        );

        let rule_content = tokio::fs::read_to_string(dir.path().join(".agix/rules/style.md"))
            .await
            .unwrap();
        assert!(rule_content.contains("Use rustfmt."));
        assert!(rule_content.contains("style guide"));
    }

    #[tokio::test]
    async fn merge_marked_section_replaces_on_repeat_call_preserving_user_tail() {
        let first = merge_marked_section("", "old content", "TEST");
        assert!(first.contains("old content"));

        let with_tail = format!("{first}\n# User notes\ncustom stuff\n");
        let second = merge_marked_section(&with_tail, "new content", "TEST");
        assert!(second.contains("new content"));
        assert!(!second.contains("old content"));
        assert!(second.contains("# User notes"));
        assert!(second.contains("custom stuff"));
    }

    #[tokio::test]
    async fn kind_str_roundtrip() {
        for kind in [
            ImportSourceKind::ClaudeCode,
            ImportSourceKind::Codex,
            ImportSourceKind::Cursor,
        ] {
            assert_eq!(ImportSourceKind::try_parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ImportSourceKind::try_parse("bogus"), None);
    }
}
