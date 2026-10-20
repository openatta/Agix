//! FileEditTool —— "Edit"。
//!
//! 字符串字面替换：要么唯一匹配 + 单次替换，要么 `replace_all=true` 全量替换。
//! 不带 unified diff（简化输出）；接 `similar` 给 model 看 diff。

use crate::cancel::run_with_cancel;
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::{ExecError, FileSystem};
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileEditInput {
    /// The absolute path to the file to edit. Relative paths are resolved
    /// against the current working directory.
    pub file_path: String,

    /// **Q2 **: list of edits to apply atomically. If provided, the
    /// `old_string` / `new_string` / `replace_all` top-level fields are
    /// ignored. Edits are applied **in order** (each one runs against the
    /// in-memory result of the previous). If any edit fails (no match, or
    /// non-unique without replace_all) the whole call aborts and the file
    /// is left untouched.
    #[serde(default)]
    pub edits: Option<Vec<SingleEdit>>,

    /// Single-edit shorthand. Used when `edits` is None. The literal text to
    /// find — must match exactly (incl. whitespace).
    #[serde(default)]
    pub old_string: Option<String>,

    /// Single-edit shorthand: the text to replace `old_string` with. Must
    /// differ from `old_string`.
    #[serde(default)]
    pub new_string: Option<String>,

    /// Single-edit shorthand: when false (default), `old_string` must appear
    /// exactly once. When true, every occurrence is replaced at once.
    #[serde(default)]
    pub replace_all: Option<bool>,
}

/// **Q2 **: one entry in a multi-edit batch.
#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub struct SingleEdit {
    /// Literal text to find. Must match exactly.
    pub old_string: String,
    /// Replacement text. Must differ from `old_string`.
    pub new_string: String,
    /// `false` (default) → `old_string` must appear exactly once at the time
    /// this edit is applied. `true` → replace every occurrence.
    #[serde(default)]
    pub replace_all: Option<bool>,
}

impl FileEditInput {
    /// Normalise into the canonical edits list. Returns Err if neither shape
    /// is present, or if `edits` is empty.
    fn into_edits(self) -> Result<(String, Vec<SingleEdit>), ToolError> {
        if let Some(edits) = self.edits {
            if edits.is_empty() {
                return Err(ToolError::Validation(
                    "edits[] must contain at least one edit".into(),
                ));
            }
            return Ok((self.file_path, edits));
        }
        let old = self.old_string.ok_or_else(|| {
            ToolError::Validation(
                "either `edits` array or `old_string`/`new_string` must be provided".into(),
            )
        })?;
        let new = self.new_string.ok_or_else(|| {
            ToolError::Validation("new_string required when edits[] absent".into())
        })?;
        Ok((
            self.file_path,
            vec![SingleEdit {
                old_string: old,
                new_string: new,
                replace_all: self.replace_all,
            }],
        ))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FileEditTool;

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Find-and-replace edits on a single file (one or many)."
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(FileEditInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/file_edit.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    fn is_destructive(&self, _: &Value) -> bool {
        true
    }

    fn interrupt_behavior(&self, _input: &Value) -> base::tool::InterruptBehavior {
        base::tool::InterruptBehavior::Block
    }

    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<FileEditInput>(input.clone())
            .ok()
            .map(|i| i.file_path)
    }

    fn permission_content_kind(&self) -> base::tool::PermissionContentKind {
        base::tool::PermissionContentKind::WritePath
    }

    async fn validate_input(&self, input: &Value, ctx: &ToolContext) -> ValidationResult {
        let parsed: Result<FileEditInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.file_path.is_empty() => {
                ValidationResult::err("file_path must not be empty", 1)
            }
            Ok(p) => {
                // **P (read-before-edit)**: staleness check — file must have been
                // Read before it can be Edited.
                let resolved = if Path::new(&p.file_path).is_absolute() {
                    PathBuf::from(&p.file_path)
                } else {
                    ctx.cwd.join(&p.file_path)
                };
                let resolved = crate::security::normalize_path_lexically(&resolved);
                if let Some(msg) = ctx.session.check_read_staleness(&resolved) {
                    return ValidationResult::err(msg, 8);
                }

                // Accept either shape; check edits content if provided
                if let Some(edits) = &p.edits {
                    if edits.is_empty() {
                        return ValidationResult::err("edits[] must not be empty", 2);
                    }
                    for (i, e) in edits.iter().enumerate() {
                        if e.old_string.is_empty() {
                            return ValidationResult::err(
                                format!("edits[{i}].old_string must not be empty"),
                                3,
                            );
                        }
                        if e.old_string == e.new_string {
                            return ValidationResult::err(
                                format!("edits[{i}].old_string == new_string — nothing to change"),
                                4,
                            );
                        }
                    }
                    return ValidationResult::Ok;
                }
                // Single-edit shorthand
                let old = p.old_string.unwrap_or_default();
                let new = p.new_string.unwrap_or_default();
                if old.is_empty() {
                    ValidationResult::err(
                        "either `edits` array or `old_string` must be provided",
                        5,
                    )
                } else if old == new {
                    ValidationResult::err(
                        "old_string and new_string are identical — nothing to change",
                        6,
                    )
                } else {
                    ValidationResult::Ok
                }
            }
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 7),
        }
    }

    async fn check_permissions(&self, input: &Value, ctx: &ToolContext) -> PermissionDecision {
        // 1. 路径安全（与 Write 同款）
        if let Ok(parsed) = serde_json::from_value::<FileEditInput>(input.clone()) {
            let requested = PathBuf::from(&parsed.file_path);
            let path = if requested.is_absolute() {
                PathBuf::from(parsed.file_path)
            } else {
                ctx.cwd.join(parsed.file_path)
            };
            let path = crate::security::normalize_path_lexically(&path);
            let path = ctx.exec.filesystem.canonicalize_best_effort(&path).await;
            let policy = crate::security::write_policy(ctx).await;
            match permissions::path_safety::check_write_for(&requested, &path, &policy) {
                Ok(()) => {
                    return PermissionDecision::Allow {
                        decision_reason: Some("project_write".into()),
                    };
                }
                Err(permissions::path_safety::PathSafetyError::OutsideAllowedRoots { .. }) => {
                    return PermissionDecision::Ask {
                        message: "Edit outside the project requires confirmation".into(),
                        decision_reason: None,
                    };
                }
                Err(err) => {
                    return PermissionDecision::Deny {
                        reason: Some(format!("{err:?}")),
                        decision_reason: Some("path_safety".into()),
                    };
                }
            }
        }

        PermissionDecision::Ask {
            message: "File edit requires confirmation".into(),
            decision_reason: None,
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: FileEditInput = serde_json::from_value(input)?;
        let (file_path, edits) = input.into_edits()?;
        let path = resolve_path(&file_path, &ctx.cwd);

        // **P1 **: snapshot before mutate so /rewind can restore.
        if let Some(snapshot) = &ctx.snapshot_file {
            snapshot.record(&path, "Edit");
        }

        let outcome = run_with_cancel(
            &ctx.cancel,
            perform_edits_structured(&*ctx.exec.filesystem, &path, &edits),
        )
        .await??;

        // **S1-a **: emit structured diff payload alongside text. TUI
        // parses `structured_content.kind == "diff"` and renders inline; CLI
        // / headless ignores and shows text. is_error stays false.
        let structured = serde_json::to_value(&outcome.diff).ok();
        let mut result = ToolResult::text(outcome.summary);
        result.structured_content = structured;
        Ok(result)
    }
}

fn resolve_path(s: &str, cwd: &Path) -> PathBuf {
    let p = PathBuf::from(s);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

/// **S1-a **: structured diff payload — TUI deserializes from
/// `ToolResult.structured_content` to render inline. Wire-format-stable.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiffPayload {
    pub kind: String, // always "diff"
    pub file_path: String,
    pub before_bytes: usize,
    pub after_bytes: usize,
    pub hunks: Vec<DiffHunk>,
    /// `true` when the diff is too long and was truncated; UI may show "expand".
    pub truncated: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiffHunk {
    /// 1-based line number of the first change in the original file
    pub before_start: usize,
    pub before_count: usize,
    pub after_start: usize,
    pub after_count: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "tag", rename_all = "lowercase")]
pub enum DiffLine {
    /// Context line (unchanged)
    Context { text: String },
    /// Removed line
    Delete { text: String },
    /// Added line
    Insert { text: String },
}

struct PerformOutcome {
    summary: String,
    diff: DiffPayload,
}

/// **Q2 **: apply N edits atomically. Each edit runs against the
/// in-memory result of the previous one. If any edit fails (no match, or
/// non-unique without replace_all), the file is **not** written and an
/// error is returned describing which edit failed and why.
async fn perform_edits_structured(
    fs: &dyn FileSystem,
    path: &Path,
    edits: &[SingleEdit],
) -> Result<PerformOutcome, ToolError> {
    let metadata = fs
        .metadata_following_symlinks(path)
        .await
        .map_err(|e| match e {
            ExecError::Failed(_) => {
                ToolError::Validation(format!("file not found: {}", path.display()))
            }
            other => other.into(),
        })?;
    if metadata.is_dir {
        return Err(ToolError::Validation(format!(
            "path is a directory, not a file: {}",
            path.display()
        )));
    }

    // File size limit (1 GiB).
    const MAX_EDIT_BYTES: u64 = 1024 * 1024 * 1024;
    if metadata.len > MAX_EDIT_BYTES {
        return Err(ToolError::Validation(format!(
            "file is {} bytes; max edit size is {} bytes",
            metadata.len, MAX_EDIT_BYTES
        )));
    }

    let original = String::from_utf8(fs.read(path).await?).map_err(|_| {
        ToolError::Validation(format!("file is not valid UTF-8: {}", path.display()))
    })?;

    let mut current = original.clone();
    let mut total_replacements: usize = 0;
    for (idx, edit) in edits.iter().enumerate() {
        let count = current.matches(&edit.old_string).count();
        let replace_all = edit.replace_all.unwrap_or(false);
        if count == 0 {
            return Err(ToolError::Validation(format!(
                "edit[{idx}]: old_string not found in {} (after applying previous edits) — \
                 make sure it matches exactly, including whitespace",
                path.display()
            )));
        }
        if count > 1 && !replace_all {
            return Err(ToolError::Validation(format!(
                "edit[{idx}]: old_string appears {count} times in {} (after previous edits); \
                 pass replace_all=true or include more context to make it unique",
                path.display()
            )));
        }
        current = if replace_all {
            current.replace(&edit.old_string, &edit.new_string)
        } else {
            current.replacen(&edit.old_string, &edit.new_string, 1)
        };
        total_replacements += if replace_all { count } else { 1 };
    }

    fs.write_str(path, &current).await?;

    let path_str = path.display().to_string();
    let (diff_text, diff_payload) = render_diff(&original, &current, &path_str);
    let edit_count = edits.len();
    let summary = format!(
        "Applied {} edit{} ({} replacement{}) to {} ({} → {} bytes)\n\n{}",
        edit_count,
        if edit_count == 1 { "" } else { "s" },
        total_replacements,
        if total_replacements == 1 { "" } else { "s" },
        path.display(),
        original.len(),
        current.len(),
        diff_text,
    );
    Ok(PerformOutcome {
        summary,
        diff: diff_payload,
    })
}

/// **S1-a **: generate both unified-text (for headless / model) AND
/// structured payload (for TUI inline render). One pass — same hunks.
/// Max 100 changed lines; overflow → truncated=true on the structured side
/// + `[diff truncated]` marker on the text side.
fn render_diff(before: &str, after: &str, path: &str) -> (String, DiffPayload) {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(before, after);

    let mut text = String::new();
    text.push_str(&format!("--- {path} (before)\n+++ {path} (after)\n"));

    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut hunk_count = 0usize;
    let max_lines = 100usize;
    let mut total_lines = 0usize;
    let mut truncated = false;

    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        if total_lines >= max_lines {
            text.push_str("\n[diff truncated — too many changes]\n");
            truncated = true;
            break;
        }
        text.push_str(&format!("{}\n", hunk.header()));

        // similar's UnifiedHunk doesn't directly expose start/count for
        // before/after — but its Display ("@@ -X,Y +Z,W @@") does. Parse it.
        let header = hunk.header().to_string();
        let (b_start, b_count, a_start, a_count) =
            parse_hunk_header(&header).unwrap_or((0, 0, 0, 0));

        let mut hunk_lines: Vec<DiffLine> = Vec::new();
        for change in hunk.iter_changes() {
            if total_lines >= max_lines {
                truncated = true;
                break;
            }
            let sign = match change.tag() {
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
                ChangeTag::Equal => ' ',
            };
            let s = change.to_string();
            let s = s.strip_suffix('\n').unwrap_or(&s).to_string();
            text.push_str(&format!("{sign}{s}\n"));
            hunk_lines.push(match change.tag() {
                ChangeTag::Delete => DiffLine::Delete { text: s },
                ChangeTag::Insert => DiffLine::Insert { text: s },
                ChangeTag::Equal => DiffLine::Context { text: s },
            });
            total_lines += 1;
        }

        hunks.push(DiffHunk {
            before_start: b_start,
            before_count: b_count,
            after_start: a_start,
            after_count: a_count,
            lines: hunk_lines,
        });
        hunk_count += 1;
    }

    if hunk_count == 0 {
        text.push_str(
            "(no textual difference detected — strings may differ only in trailing whitespace)\n",
        );
    }

    let payload = DiffPayload {
        kind: "diff".into(),
        file_path: path.to_string(),
        before_bytes: before.len(),
        after_bytes: after.len(),
        hunks,
        truncated,
    };
    (text, payload)
}

/// Parse `@@ -B_START,B_COUNT +A_START,A_COUNT @@` hunk header.
/// Tolerant: returns None on malformed; counts default to 1 when omitted
/// (`@@ -B_START +A_START @@` shape).
fn parse_hunk_header(s: &str) -> Option<(usize, usize, usize, usize)> {
    // Find "-X,Y" and "+X,Y"
    let trimmed = s.trim_start_matches('@').trim_end_matches('@').trim();
    let mut parts = trimmed.split_whitespace();
    let before_part = parts.next()?;
    let after_part = parts.next()?;
    let (b_start, b_count) = parse_pair(before_part.trim_start_matches('-'))?;
    let (a_start, a_count) = parse_pair(after_part.trim_start_matches('+'))?;
    Some((b_start, b_count, a_start, a_count))
}

fn parse_pair(s: &str) -> Option<(usize, usize)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::exec::local::LocalFileSystem;
    use base::interface::exec::{DirEntry, Metadata};
    use base::tool::ToolResultContent;
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx_in(cwd: &Path) -> ToolContext {
        ToolContext::for_test(cwd.to_path_buf())
    }

    #[tokio::test]
    async fn replaces_unique_occurrence() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "hello world\n").await.unwrap();
        let tool = FileEditTool;
        let r = tool
            .call(
                json!({
                    "file_path": p.to_string_lossy(),
                    "old_string": "world",
                    "new_string": "AttaCode"
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let after = tokio::fs::read_to_string(&p).await.unwrap();
        assert_eq!(after, "hello AttaCode\n");
        match r.content {
            ToolResultContent::Text(s) => {
                assert!(s.contains("1 edit") && s.contains("1 replacement"));
                // 应当带 unified diff
                assert!(s.contains("---"));
                assert!(s.contains("+++"));
                assert!(s.contains("-hello world"));
                assert!(s.contains("+hello AttaCode"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn ambiguous_match_errors() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "x\nx\nx\n").await.unwrap();
        let tool = FileEditTool;
        let err = tool
            .call(
                json!({
                    "file_path": p.to_string_lossy(),
                    "old_string": "x",
                    "new_string": "y"
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("3 times"));
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        // 文件未被改
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "x\nx\nx\n");
    }

    #[tokio::test]
    async fn replace_all_accepts_multiple() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "x\nx\nx\n").await.unwrap();
        let tool = FileEditTool;
        let r = tool
            .call(
                json!({
                    "file_path": p.to_string_lossy(),
                    "old_string": "x",
                    "new_string": "yy",
                    "replace_all": true
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "yy\nyy\nyy\n");
        match r.content {
            ToolResultContent::Text(s) => assert!(s.contains("3 replacements")),
            _ => panic!(),
        }
    }

    // ---- S1-a : structured diff payload ----

    #[tokio::test]
    async fn structured_content_carries_diff_payload() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "alpha\nbeta\ngamma\n").await.unwrap();
        let tool = FileEditTool;
        let r = tool
            .call(
                json!({
                    "file_path": p.to_string_lossy(),
                    "old_string": "beta",
                    "new_string": "BETA"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let sc = r.structured_content.expect("structured_content present");
        let payload: super::DiffPayload = serde_json::from_value(sc).unwrap();
        assert_eq!(payload.kind, "diff");
        assert!(payload.file_path.ends_with("a.txt"));
        assert_eq!(payload.hunks.len(), 1);
        let hunk = &payload.hunks[0];
        // Should contain delete "beta" + insert "BETA" + maybe context
        let has_delete_beta = hunk
            .lines
            .iter()
            .any(|l| matches!(l, super::DiffLine::Delete { text } if text == "beta"));
        let has_insert_beta = hunk
            .lines
            .iter()
            .any(|l| matches!(l, super::DiffLine::Insert { text } if text == "BETA"));
        assert!(has_delete_beta);
        assert!(has_insert_beta);
        assert!(!payload.truncated);
    }

    #[test]
    fn parse_hunk_header_full_form() {
        assert_eq!(
            super::parse_hunk_header("@@ -10,5 +12,7 @@"),
            Some((10, 5, 12, 7))
        );
    }

    #[test]
    fn parse_hunk_header_omitted_count() {
        // "@@ -X +Y @@" form (count defaults to 1)
        assert_eq!(super::parse_hunk_header("@@ -3 +5 @@"), Some((3, 1, 5, 1)));
    }

    #[test]
    fn parse_hunk_header_garbage_returns_none() {
        assert!(super::parse_hunk_header("not a header").is_none());
    }

    // ---- Q2 : multi-edit ----

    #[tokio::test]
    async fn multi_edit_applies_all_in_order() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "alpha beta gamma\n").await.unwrap();
        let tool = FileEditTool;
        let r = tool
            .call(
                json!({
                    "file_path": p.to_string_lossy(),
                    "edits": [
                        {"old_string": "alpha", "new_string": "ALPHA"},
                        {"old_string": "gamma", "new_string": "GAMMA"},
                    ]
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "ALPHA beta GAMMA\n"
        );
        match r.content {
            ToolResultContent::Text(s) => {
                assert!(s.contains("2 edits"));
                assert!(s.contains("2 replacements"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn multi_edit_failing_one_aborts_all() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "alpha beta gamma\n").await.unwrap();
        let tool = FileEditTool;
        let err = tool
            .call(
                json!({
                    "file_path": p.to_string_lossy(),
                    "edits": [
                        {"old_string": "alpha", "new_string": "ALPHA"},
                        {"old_string": "DOES_NOT_EXIST", "new_string": "X"},
                    ]
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        // file untouched: first edit must NOT have been written
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "alpha beta gamma\n"
        );
    }

    #[tokio::test]
    async fn multi_edit_in_order_chained_substitutions() {
        // edit B operates on the *output* of edit A
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "old1\n").await.unwrap();
        let tool = FileEditTool;
        let r = tool
            .call(
                json!({
                    "file_path": p.to_string_lossy(),
                    "edits": [
                        {"old_string": "old1", "new_string": "old2"},
                        {"old_string": "old2", "new_string": "final"},
                    ]
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "final\n");
    }

    #[tokio::test]
    async fn multi_edit_empty_array_errors() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "hi").await.unwrap();
        let tool = FileEditTool;
        let ctx = ctx_in(dir.path());
        // Pre-populate read cache so staleness check passes
        ctx.session.record_read(&p);
        let r = tool
            .validate_input(
                &json!({"file_path": p.to_string_lossy(), "edits": []}),
                &ctx,
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn old_string_not_found_errors() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "hello").await.unwrap();
        let tool = FileEditTool;
        let err = tool
            .call(
                json!({
                    "file_path": p.to_string_lossy(),
                    "old_string": "nope",
                    "new_string": "yes"
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn file_not_found_errors() {
        let dir = TempDir::new().unwrap();
        let tool = FileEditTool;
        let err = tool
            .call(
                json!({
                    "file_path": dir.path().join("ghost.txt").to_string_lossy(),
                    "old_string": "x",
                    "new_string": "y"
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(msg) if msg.contains("not found")));
    }

    #[tokio::test]
    async fn directory_errors() {
        let dir = TempDir::new().unwrap();
        let tool = FileEditTool;
        let err = tool
            .call(
                json!({
                    "file_path": dir.path().to_string_lossy(),
                    "old_string": "x",
                    "new_string": "y"
                }),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn empty_old_string_validates_err() {
        let dir = TempDir::new().unwrap();
        let tool = FileEditTool;
        let v = tool
            .validate_input(
                &json!({"file_path": "/x", "old_string": "", "new_string": "y"}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(!matches!(v, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn identical_strings_validate_err() {
        let dir = TempDir::new().unwrap();
        let tool = FileEditTool;
        let v = tool
            .validate_input(
                &json!({"file_path": "/x", "old_string": "y", "new_string": "y"}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(!matches!(v, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn relative_path_resolves_against_cwd() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("rel.txt");
        tokio::fs::write(&p, "abc").await.unwrap();
        let tool = FileEditTool;
        tool.call(
            json!({"file_path": "rel.txt", "old_string": "b", "new_string": "B"}),
            ctx_in(dir.path()),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "aBc");
    }

    #[tokio::test]
    async fn permissions_default_ask() {
        let tool = FileEditTool;
        let dir = TempDir::new().unwrap();
        let d = tool
            .check_permissions(&json!({}), &ctx_in(dir.path()))
            .await;
        assert!(matches!(d, PermissionDecision::Ask { .. }));
    }

    #[tokio::test]
    async fn project_edit_allows_inside_asks_outside_and_denies_sensitive() {
        let tool = FileEditTool;
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        let inside = tool
            .check_permissions(
                &json!({"file_path": "a.txt", "old_string": "a", "new_string": "b"}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(matches!(inside, PermissionDecision::Allow { .. }));

        let outside = tool
            .check_permissions(
                &json!({
                    "file_path": outside.path().join("a.txt").to_string_lossy(),
                    "old_string": "a",
                    "new_string": "b"
                }),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(matches!(outside, PermissionDecision::Ask { .. }));

        let sensitive = tool
            .check_permissions(
                &json!({"file_path": ".env", "old_string": "a", "new_string": "b"}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(matches!(sensitive, PermissionDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn flags_are_destructive_and_not_readonly() {
        let tool = FileEditTool;
        assert!(!tool.is_read_only(&Value::Null));
        assert!(tool.is_destructive(&Value::Null));
        assert!(!tool.is_concurrency_safe(&Value::Null));
        assert_eq!(tool.name(), "Edit");
    }

    #[tokio::test]
    async fn cancel_aborts_call() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "abc").await.unwrap();
        let tool = FileEditTool;
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        ctx.cancel.cancel();
        let err = tool
            .call(
                json!({"file_path": p.to_string_lossy(), "old_string": "b", "new_string": "B"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    /// Serves one body for every read and collects every write, so what the
    /// edit worked on and where the result went can only be the provider.
    struct Interposed {
        body: &'static str,
        written: std::sync::Mutex<Vec<(PathBuf, String)>>,
    }

    #[async_trait]
    impl FileSystem for Interposed {
        async fn read(&self, _: &Path) -> Result<Vec<u8>, ExecError> {
            Ok(self.body.as_bytes().to_vec())
        }
        async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
            self.written.lock().unwrap().push((
                path.to_path_buf(),
                String::from_utf8_lossy(bytes).into_owned(),
            ));
            Ok(())
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
        async fn metadata(&self, _: &Path) -> Result<Metadata, ExecError> {
            Ok(Metadata {
                is_dir: false,
                is_file: true,
                is_symlink: false,
                len: self.body.len() as u64,
            })
        }
        async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
            Ok(path.to_path_buf())
        }
    }

    #[tokio::test]
    async fn the_edit_reads_and_writes_through_the_provider() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("code.rs");
        tokio::fs::write(&p, "on the disk\n").await.unwrap();

        let provider = Arc::new(Interposed {
            body: "in the provider\n",
            written: std::sync::Mutex::new(Vec::new()),
        });
        let mut ctx = ctx_in(dir.path());
        ctx.exec.filesystem = provider.clone();

        FileEditTool
            .call(
                json!({
                    "file_path": p.to_string_lossy(),
                    "old_string": "in the provider",
                    "new_string": "edited",
                }),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .expect("the old_string only exists in the provider's copy");

        let written = provider.written.lock().unwrap().clone();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].0, p);
        assert_eq!(written[0].1, "edited\n");
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "on the disk\n",
            "the disk must not have been touched"
        );
    }
}
