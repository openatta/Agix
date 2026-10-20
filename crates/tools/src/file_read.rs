//! FileReadTool —— "Read"。
//!
//! 第一个具体工具。只读、并发安全、不需要用户确认。

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::{ExecError, FileSystem, Metadata};
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ToolResultBlock, ToolResultContent, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// `Read` 工具的输入 schema。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileReadInput {
    /// The absolute path to the file to read. Relative paths are resolved
    /// against the current working directory.
    pub file_path: String,

    /// Line number to start reading from (1-indexed). Default 1.
    #[serde(default)]
    pub offset: Option<usize>,

    /// Maximum number of lines to read. Default 2000 (configurable via
    /// EngineConfig::default_read_lines).
    #[serde(default)]
    pub limit: Option<usize>,

    /// PDF page range (e.g., "1-5", "3"). Max 20 pages per request.
    /// Only applicable to PDF files.
    #[serde(default)]
    #[serde(alias = "pageRange")]
    pub pages: Option<String>,
}

/// 单元 struct —— 工具实例无状态，复用同一份。
#[derive(Debug, Default, Clone, Copy)]
pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file (text/image/PDF) with optional line range."
    }

    fn input_schema(&self) -> Value {
        // schemars::schema_for! 给的 RootSchema 序列化是合法 JSON Schema draft-07
        serde_json::to_value(schemars::schema_for!(FileReadInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/file_read.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<FileReadInput>(input.clone())
            .ok()
            .map(|i| i.file_path)
    }

    fn permission_content_kind(&self) -> base::tool::PermissionContentKind {
        base::tool::PermissionContentKind::ReadPath
    }

    async fn validate_input(&self, input: &Value, _ctx: &ToolContext) -> ValidationResult {
        let parsed: Result<FileReadInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.file_path.is_empty() => {
                ValidationResult::err("file_path must not be empty", 1)
            }
            // Block device paths and binary extensions
            Ok(ref p) => {
                let lowered = p.file_path.to_lowercase();
                if lowered.starts_with("/dev/") {
                    return ValidationResult::err("cannot read from /dev/ paths", 5);
                }
                for ext in &[
                    ".exe", ".dll", ".so", ".dylib", ".class", ".o", ".a", ".bin", ".wasm",
                ] {
                    if lowered.ends_with(ext) {
                        return ValidationResult::err(
                            format!(
                                "cannot read binary files ({ext}); use Bash to inspect if needed"
                            ),
                            6,
                        );
                    }
                }
                ValidationResult::Ok
            }
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn check_permissions(&self, input: &Value, ctx: &ToolContext) -> PermissionDecision {
        if let Ok(parsed) = serde_json::from_value::<FileReadInput>(input.clone()) {
            let path = crate::security::normalize_path_lexically(&resolve_path(
                &parsed.file_path,
                &ctx.cwd,
            ));
            let cwd = crate::security::normalize_path_lexically(&ctx.cwd);
            if crate::security::is_path_within_root(&path, &cwd) {
                return PermissionDecision::allow();
            }
        }

        PermissionDecision::Ask {
            message: "Read outside the project requires confirmation".into(),
            decision_reason: None,
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: FileReadInput = serde_json::from_value(input)?;

        let path = resolve_path(&input.file_path, &ctx.cwd);

        // **P (read dedup)**: 文件未变化且相同范围时跳过重新读取。
        if ctx
            .session
            .check_read_dedup_with_range(&path, input.offset, input.limit)
        {
            let msg = if ctx.session.is_consecutive_read(&path) {
                "[file unchanged] Use the previous content."
            } else {
                "[file unchanged] File unchanged since last read. Use the previous content."
            };
            return Ok(ToolResult::text(msg.to_string()));
        }

        // 全程包在 select! 里以 honor cancel
        let max_bytes = ctx.max_file_read_bytes.max(1024 * 1024); // at least 1MB
        let result = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        r = read_file_to_text(
            &*ctx.exec.filesystem,
            &path,
            &input,
            max_bytes as u64,
            2000,  // default_read_lines
            2000,  // max_line_chars
        ) => r};

        match result {
            Ok(tool_result) => {
                // **P (read-before-edit)**: 记录本次读取(含偏移范围)，供去重与 Edit/Write 做 staleness 检测。
                ctx.session
                    .record_read_with_range(&path, input.offset, input.limit);
                Ok(tool_result)
            }
            Err(ToolError::Execution(e)) if e.to_string().contains("not found") => Ok(
                ToolResult::text(format!("(no such file) {path}", path = path.display())),
            ),
            Err(e) => Err(e),
        }
    }
}

/// 把相对路径解到 cwd。绝对路径原样返回。
fn resolve_path(s: &str, cwd: &Path) -> PathBuf {
    let p = PathBuf::from(s);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

async fn read_file_to_text(
    fs: &dyn FileSystem,
    path: &Path,
    input: &FileReadInput,
    max_bytes: u64,
    default_lines: usize,
    max_line_chars: usize,
) -> Result<ToolResult, ToolError> {
    // Resolution doubles as the existence probe: a model naming a file that
    // is not there is the one failure this tool answers with prose.
    let metadata = fs
        .metadata_following_symlinks(path)
        .await
        .map_err(|e| match e {
            ExecError::Failed(_) => ToolError::exec(format!("file not found: {}", path.display())),
            other => other.into(),
        })?;

    if metadata.is_dir {
        return Err(ToolError::Validation(format!(
            "path is a directory, not a file: {}",
            path.display()
        )));
    }

    // T1.1: Multimedia detection — check extension before reading as text
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Image handling
    if matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    ) {
        return read_image(fs, path, &metadata, max_bytes).await;
    }

    // PDF handling
    if ext == "pdf" {
        return read_pdf_text(path, &metadata, max_bytes, input.pages.as_deref())
            .await
            .map(ToolResult::text);
    }

    // Jupyter notebook handling
    if ext == "ipynb" {
        return read_notebook(
            fs,
            path,
            input.offset,
            input.limit,
            default_lines,
            max_line_chars,
        )
        .await
        .map(ToolResult::text);
    }

    if metadata.len > max_bytes {
        return Err(ToolError::Validation(format!(
            "file too large ({} bytes); cap is {} bytes. Use Grep / Glob or pass a smaller window.",
            metadata.len, max_bytes
        )));
    }

    let content = String::from_utf8(fs.read(path).await?).map_err(|_| {
        ToolError::Validation(format!(
            "file is not valid UTF-8 (possible binary file): {}. \
             Use Bash with `file {}` to inspect the type.",
            path.display(),
            path.display()
        ))
    })?;

    // Empty file warning.
    if content.trim().is_empty() {
        return Ok(ToolResult::text(
            "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>",
        ));
    }

    Ok(ToolResult::text(format_with_line_numbers(
        &content,
        input.offset.unwrap_or(1).max(1),
        input.limit.unwrap_or(default_lines),
        max_line_chars,
    )))
}

// ── T1.1: Multimedia readers ──

/// Read an image file and return base64-encoded content with MIME type info.
/// Read an image as a real image block plus a cheap human-readable header.
///
/// This used to return a single `String` containing the whole base64 payload
/// inline. That was strictly worse than not supporting images at all: a 1 MB PNG
/// became ~1.4 MB of base64 *text* in the message history — roughly 350k tokens
/// by a chars/4 estimate — and the model still could not see the image, because
/// text is text. (`enforce_tool_result_budget` then truncated it to 50 KB, i.e.
/// ~12k tokens of meaningless base64 noise plus a mutilated payload.)
///
/// Now the payload rides in a `ToolResultBlock::image`, which the runtime lifts
/// into a `ModelContentBlock::Image` — a real image on the wire, billed at the
/// flat per-image rate rather than by base64 length. The header line stays as a
/// text block: name/mime/size/dimensions are genuinely useful to the model and
/// cost a handful of tokens.
async fn read_image(
    fs: &dyn FileSystem,
    path: &Path,
    metadata: &Metadata,
    max_bytes: u64,
) -> Result<ToolResult, ToolError> {
    if metadata.len > max_bytes {
        return Err(ToolError::Validation(format!(
            "image too large ({} bytes); cap is {} bytes",
            metadata.len, max_bytes
        )));
    }
    let bytes = fs.read(path).await?;
    let mime = mime_from_ext(path);
    let b64 = base64_encode(&bytes);
    let dims = estimate_image_dimensions(&bytes);
    let dims_str = dims
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_else(|| "unknown".to_string());

    let header = format!(
        "[Image: {} — {} — {} bytes, {} pixels. Rendered below.]",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        mime,
        bytes.len(),
        dims_str,
    );

    Ok(ToolResult {
        content: ToolResultContent::Blocks(vec![
            ToolResultBlock::text(header),
            ToolResultBlock::image(mime, b64),
        ]),
        is_error: false,
        structured_content: None,
        mcp_meta: None,
        new_messages: None,
    })
}

/// Read a PDF file — returns basic text extraction via the `pdftotext` CLI tool.
async fn read_pdf_text(
    path: &Path,
    metadata: &Metadata,
    max_bytes: u64,
    pages: Option<&str>,
) -> Result<String, ToolError> {
    if metadata.len > max_bytes.max(50 * 1024 * 1024) {
        return Err(ToolError::Validation(format!(
            "PDF too large ({} bytes); cap is {} bytes",
            metadata.len,
            max_bytes.max(50 * 1024 * 1024)
        )));
    }
    let page_range = pages.unwrap_or("1-20");
    // Try pdftotext first, then fall back to raw bytes
    let output = tokio::process::Command::new("pdftotext")
        .arg("-f")
        .arg(page_range.split('-').next().unwrap_or("1"))
        .arg("-l")
        .arg(page_range.split('-').nth(1).unwrap_or("20"))
        .arg(path)
        .arg("-") // stdout
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            let truncated = if text.len() > max_bytes as usize {
                // Character-boundary aware: extracted PDF text is arbitrary
                // UTF-8, so a raw `&text[..max_bytes]` panics whenever the cap
                // lands inside a multi-byte character.
                format!(
                    "{}...\n[text truncated at {} bytes]",
                    base::text::truncate_at_char_boundary(&text, max_bytes as usize),
                    max_bytes
                )
            } else {
                text
            };
            Ok(format!(
                "[PDF: {} — {} bytes, pages {page_range}]\n\n{truncated}",
                path.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default(),
                metadata.len,
            ))
        }
        _ => {
            // No probe read here. It used to open the file so that an
            // unreadable PDF reported that rather than a cheerful "install
            // poppler" — but the contract reads whole values, so the probe
            // meant pulling up to fifty megabytes into memory and dropping
            // them to sharpen one error message. `metadata` above already
            // establishes the file is there.
            Ok(format!(
                "[PDF: {} — {} bytes. Install pdftotext (poppler-utils) for text extraction.]",
                path.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default(),
                metadata.len,
            ))
        }
    }
}

/// Read a Jupyter notebook and format cells as readable text.
async fn read_notebook(
    fs: &dyn FileSystem,
    path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
    default_lines: usize,
    max_line_chars: usize,
) -> Result<String, ToolError> {
    let content = fs.read_to_string(path).await?;
    let notebook: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| ToolError::Validation(format!("invalid notebook JSON: {e}")))?;

    let cells = notebook["cells"]
        .as_array()
        .ok_or_else(|| ToolError::Validation("notebook has no cells array".to_string()))?;

    let mut out = format!(
        "[Notebook: {} — {} cells]\n\n",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        cells.len(),
    );

    for (i, cell) in cells.iter().enumerate() {
        let cell_type = cell["cell_type"].as_str().unwrap_or("unknown");
        let source = cell["source"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .or_else(|| cell["source"].as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let header = format!("<cell id=\"{}\" type=\"{cell_type}\">", i);
        out.push_str(&header);
        out.push('\n');
        if cell_type == "code" {
            for line in source.lines() {
                out.push_str(&format!("        {line}\n"));
            }
            // Show outputs if present
            if let Some(outputs) = cell["outputs"].as_array() {
                for output in outputs {
                    if let Some(text) = output["text"].as_array() {
                        let joined: String = text.iter().filter_map(|v| v.as_str()).collect();
                        let prefix = if joined.contains('\n') { "\n" } else { "" };
                        out.push_str(&format!("Output:\n{prefix}{joined}\n"));
                    } else if let Some(text_str) = output["text/plain"].as_str() {
                        out.push_str(&format!("Output:\n{text_str}\n"));
                    }
                }
            }
        } else {
            // Markdown cell — strip to plain text
            out.push_str(&format!(
                "        {}\n",
                source.lines().next().unwrap_or("")
            ));
        }
        out.push_str(&format!("</cell id=\"{i}\">\n\n"));
    }

    // Apply offset/limit for large notebooks
    let result = if offset.is_some() || limit.is_some() {
        format_with_line_numbers(
            &out,
            offset.unwrap_or(1).max(1),
            limit.unwrap_or(default_lines),
            max_line_chars,
        )
    } else {
        out
    };

    Ok(result)
}

fn mime_from_ext(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Crude image dimension estimation from PNG/JPEG/GIF headers.
fn estimate_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 {
        return None;
    }
    // PNG: width at offset 16, height at offset 20 (big-endian u32)
    if &bytes[1..4] == b"PNG" {
        let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        return Some((w.min(10000), h.min(10000)));
    }
    // JPEG: scan for SOF0 marker (0xFF 0xC0)
    if bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < bytes.len() {
            if bytes[i] == 0xFF {
                let marker = bytes[i + 1];
                if marker == 0xC0 || marker == 0xC2 {
                    let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                    let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                    return Some((w.min(10000), h.min(10000)));
                }
                i += 2 + u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            } else {
                i += 1;
            }
        }
    }
    // GIF: width at offset 6, height at offset 8 (little-endian u16)
    if bytes.len() >= 10 && (&bytes[0..3] == b"GIF" || &bytes[0..4] == b"GIF8") {
        let w = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
        let h = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
        return Some((w.min(10000), h.min(10000)));
    }
    None
}

fn format_with_line_numbers(
    content: &str,
    offset: usize,
    limit: usize,
    max_line_chars: usize,
) -> String {
    let total = content.lines().count();
    let mut out = String::new();
    let skip = offset - 1;
    let actually_read = content.lines().count().saturating_sub(skip).min(limit);

    for (i, line) in content.lines().enumerate().skip(skip).take(limit) {
        let truncated = if line.chars().count() > max_line_chars {
            let mut buf: String = line.chars().take(max_line_chars).collect();
            buf.push_str("…[truncated]");
            buf
        } else {
            line.to_string()
        };
        // `cat -n` 风格：右对齐 6 位行号 + 制表符 + 行内容
        out.push_str(&format!("{:>6}\t{}\n", i + 1, truncated));
    }

    if actually_read == 0 && total > 0 && skip >= total {
        out.push_str(&format!(
            "[no lines: file has {total} lines; offset={offset} skipped past end]\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::exec::DirEntry;
    use base::tool::ToolResultContent;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx_in(cwd: &Path) -> ToolContext {
        ToolContext::for_test(cwd.to_path_buf())
    }

    #[tokio::test]
    async fn reads_simple_file_with_line_numbers() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("hello.txt");
        tokio::fs::write(&p, "line1\nline2\nline3\n").await.unwrap();

        let tool = FileReadTool;
        let r = tool
            .call(
                json!({"file_path": p.to_string_lossy()}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("\tline1"));
                assert!(t.contains("\tline2"));
                assert!(t.contains("\tline3"));
                assert!(t.starts_with("     1\t"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn relative_path_resolves_against_cwd() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "hi\n").await.unwrap();

        let tool = FileReadTool;
        let r = tool
            .call(
                json!({"file_path": "a.txt"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Text(t) => assert!(t.contains("\thi")),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn offset_and_limit_window() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nums.txt");
        let body: String = (1..=10).map(|i| format!("L{i}\n")).collect();
        tokio::fs::write(&p, body).await.unwrap();

        let tool = FileReadTool;
        let r = tool
            .call(
                json!({"file_path": p.to_string_lossy(), "offset": 3, "limit": 4}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let txt = match r.content {
            ToolResultContent::Text(t) => t,
            _ => panic!(),
        };
        // 应该看到 L3..L6，4 行
        assert!(txt.contains("\tL3"));
        assert!(txt.contains("\tL4"));
        assert!(txt.contains("\tL5"));
        assert!(txt.contains("\tL6"));
        assert!(!txt.contains("\tL2"));
        assert!(!txt.contains("\tL7"));
        assert_eq!(txt.lines().count(), 4);
    }

    #[tokio::test]
    async fn truncates_overlong_line() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("long.txt");
        let line: String = "x".repeat(5000);
        tokio::fs::write(&p, &line).await.unwrap();

        let tool = FileReadTool;
        let r = tool
            .call(
                json!({"file_path": p.to_string_lossy()}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let txt = match r.content {
            ToolResultContent::Text(t) => t,
            _ => panic!(),
        };
        assert!(txt.contains("…[truncated]"));
        // 行号 + 制表符 + 前 2000 char + 标记 + 换行 ≈ 7 + 1 + 2000 + 12 + 1
        // 真实长度小于 5000
        assert!(txt.chars().count() < 5000);
    }

    #[tokio::test]
    async fn file_not_found_returns_text_marker() {
        // .3: not-found 走 success+text 而非 Validation Err。
        let dir = TempDir::new().unwrap();
        let tool = FileReadTool;
        let result = tool
            .call(
                json!({"file_path": dir.path().join("ghost.txt").to_string_lossy()}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .expect("not-found should be Ok with text marker");
        let text = match &result.content {
            base::tool::ToolResultContent::Text(s) => s.clone(),
            _ => panic!("expected text content"),
        };
        assert!(text.contains("no such file"), "got: {text}");
    }

    #[tokio::test]
    async fn directory_yields_validation_error() {
        let dir = TempDir::new().unwrap();
        let tool = FileReadTool;
        let err = tool
            .call(
                json!({"file_path": dir.path().to_string_lossy()}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn empty_file_path_validates_to_err() {
        let dir = TempDir::new().unwrap();
        let tool = FileReadTool;
        let res = tool
            .validate_input(&json!({"file_path": ""}), &ctx_in(dir.path()))
            .await;
        assert!(!matches!(res, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn flags_say_read_only_and_concurrent() {
        let tool = FileReadTool;
        assert!(tool.is_read_only(&Value::Null));
        assert!(tool.is_concurrency_safe(&Value::Null));
        assert!(tool.is_enabled());
        assert_eq!(tool.name(), "Read");
    }

    #[tokio::test]
    async fn project_read_allows_inside_and_asks_outside() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let tool = FileReadTool;

        let inside = tool
            .check_permissions(&json!({"file_path": "a.txt"}), &ctx_in(dir.path()))
            .await;
        assert!(matches!(inside, PermissionDecision::Allow { .. }));

        let outside = tool
            .check_permissions(
                &json!({"file_path": outside.path().join("secret.txt")}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(matches!(outside, PermissionDecision::Ask { .. }));
    }

    #[tokio::test]
    async fn cancel_aborts_call() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "x").await.unwrap();
        let tool = FileReadTool;

        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        ctx.cancel.cancel(); // 提前取消
        let err = tool
            .call(
                json!({"file_path": p.to_string_lossy()}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn input_schema_is_object_with_file_path() {
        let tool = FileReadTool;
        let s = tool.input_schema();
        // schemars 的 RootSchema 顶层有 type/properties
        assert!(s.is_object(), "schema must be a JSON object");
        let body = if let Some(p) = s.get("properties") {
            p
        } else if let Some(s) = s.get("schema") {
            s.get("properties").expect("nested properties present")
        } else {
            panic!("expected properties on schema: {s}");
        };
        assert!(body.get("file_path").is_some(), "schema lacks file_path");
    }

    #[test]
    fn resolve_path_relative_and_absolute() {
        let cwd = PathBuf::from("/tmp/work");
        assert_eq!(
            resolve_path("a.txt", &cwd),
            PathBuf::from("/tmp/work/a.txt")
        );
        assert_eq!(
            resolve_path("/etc/hosts", &cwd),
            PathBuf::from("/etc/hosts")
        );
    }

    #[tokio::test]
    async fn consecutive_read_returns_unchanged() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("dedup.txt");
        tokio::fs::write(&p, "hello\n").await.unwrap();
        let tool = FileReadTool;
        // Create one context and clone for second call to share the Arc'd session
        let ctx = ctx_in(dir.path());

        // First read — should succeed normally
        let r1 = tool
            .call(
                json!({"file_path": p.to_string_lossy()}),
                ctx.clone(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let t1 = match r1.content {
            ToolResultContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(
            t1.contains("hello"),
            "first read should return content, got: {t1}"
        );

        // Second read — same file, unchanged -> dedup
        let r2 = tool
            .call(
                json!({"file_path": p.to_string_lossy()}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let t2 = match r2.content {
            ToolResultContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(
            t2.contains("file unchanged"),
            "dedup should trigger, got: {t2}"
        );
    }

    #[tokio::test]
    async fn interleaved_read_returns_non_consecutive_dedup() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        tokio::fs::write(&a, "a\n").await.unwrap();
        tokio::fs::write(&b, "b\n").await.unwrap();
        let tool = FileReadTool;
        let ctx = ctx_in(dir.path());

        // Read A
        tool.call(
            json!({"file_path": a.to_string_lossy()}),
            ctx.clone(),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();

        // Read B (interleaving)
        tool.call(
            json!({"file_path": b.to_string_lossy()}),
            ctx.clone(),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();

        // Read A again — dedup but NOT consecutive because B was read in between
        let r = tool
            .call(
                json!({"file_path": a.to_string_lossy()}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let t = match r.content {
            ToolResultContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(
            t.contains("file unchanged"),
            "dedup should trigger, got: {t}"
        );
        // Should use the longer "general" message, not the short "consecutive" one
        assert!(
            t.contains("since last read"),
            "should use general message: {t}"
        );
    }

    #[tokio::test]
    async fn modified_file_bypasses_dedup() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("changing.txt");
        tokio::fs::write(&p, "v1\n").await.unwrap();
        let tool = FileReadTool;
        let ctx = ctx_in(dir.path());

        // First read
        tool.call(
            json!({"file_path": p.to_string_lossy()}),
            ctx.clone(),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();

        // Make sure mtime changes — force a brief wait
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        tokio::fs::write(&p, "v2\n").await.unwrap();

        // Second read — should see v2, not dedup
        let r = tool
            .call(
                json!({"file_path": p.to_string_lossy()}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let t = match r.content {
            ToolResultContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(
            t.contains("v2"),
            "modified file should return new content, got: {t}"
        );
        assert!(
            !t.contains("file unchanged"),
            "modified file should not be dedup'd"
        );
    }

    /// A 1x1 PNG. Real bytes so the extension-based media-type detection and
    /// the base64 round-trip are both exercised.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    /// W-4: `Read` on an image returns a real image block plus a small text
    /// header — not a `String` with the whole base64 payload inlined.
    #[tokio::test]
    async fn reading_an_image_returns_an_image_block_not_inlined_base64() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("shot.png");
        tokio::fs::write(&p, TINY_PNG).await.unwrap();

        let tool = FileReadTool;
        let r = tool
            .call(
                json!({"file_path": p.to_string_lossy()}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();

        let ToolResultContent::Blocks(blocks) = r.content else {
            panic!("expected Blocks so the image survives as an image");
        };
        assert_eq!(
            blocks.len(),
            2,
            "expected a header text block + an image block"
        );

        // The header keeps the cheap, genuinely useful metadata.
        let header = blocks[0].text.as_deref().expect("header text block");
        assert!(
            header.contains("shot.png"),
            "header lost the name: {header}"
        );
        assert!(
            header.contains("image/png"),
            "header lost the mime: {header}"
        );
        assert!(
            header.contains(&TINY_PNG.len().to_string()),
            "header lost the size: {header}"
        );

        // The payload rides in the image block, base64 of the real bytes.
        let (media_type, data) = blocks[1].as_image().expect("expected an image block");
        assert_eq!(media_type, "image/png");
        assert_eq!(data, base64_encode(TINY_PNG));

        // Regression guard for the actual defect: no text block may carry the
        // base64 run. A 1 MB PNG used to put ~1.4 MB of it into the transcript.
        for block in &blocks {
            if let Some(text) = block.text.as_deref() {
                assert!(
                    !text.contains(&base64_encode(TINY_PNG)),
                    "base64 payload inlined as text: {text}"
                );
            }
        }
    }

    /// `read_pdf_text` shells out to `pdftotext`, so it returns real extracted
    /// text and never had W-4's inlined-base64 problem. Its no-`pdftotext`
    /// fallback returns a one-line notice, also text. Either way: plain `Text`,
    /// no binary payload.
    #[tokio::test]
    async fn reading_a_pdf_returns_plain_text_not_a_binary_payload() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("doc.pdf");
        // Minimal PDF header — enough for the extension/branch dispatch.
        tokio::fs::write(&p, b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n")
            .await
            .unwrap();

        let tool = FileReadTool;
        let r = tool
            .call(
                json!({"file_path": p.to_string_lossy()}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();

        let ToolResultContent::Text(text) = r.content else {
            panic!("PDF reads stay plain text");
        };
        assert!(
            text.contains("doc.pdf"),
            "expected the PDF header line, got: {text}"
        );
    }

    /// A filesystem holding exactly one file and nothing else, so that what
    /// `Read` reports can only have come from it.
    struct OneFile {
        path: PathBuf,
        body: &'static str,
    }

    #[async_trait]
    impl base::interface::exec::FileSystem for OneFile {
        async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
            if path == self.path {
                Ok(self.body.as_bytes().to_vec())
            } else {
                Err(ExecError::failed(format!(
                    "{}: no such file",
                    path.display()
                )))
            }
        }
        async fn write(&self, _: &Path, _: &[u8]) -> Result<(), ExecError> {
            Err(ExecError::denied("read-only"))
        }
        async fn create_dir_all(&self, _: &Path) -> Result<(), ExecError> {
            Err(ExecError::denied("read-only"))
        }
        async fn read_dir(&self, _: &Path) -> Result<Vec<DirEntry>, ExecError> {
            Ok(Vec::new())
        }
        async fn remove_dir_all(&self, _: &Path) -> Result<(), ExecError> {
            Err(ExecError::denied("read-only"))
        }
        async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
            if path == self.path {
                Ok(Metadata {
                    is_dir: false,
                    is_file: true,
                    is_symlink: false,
                    len: self.body.len() as u64,
                })
            } else {
                Err(ExecError::failed(format!(
                    "{}: no such file",
                    path.display()
                )))
            }
        }
        async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
            if path == self.path {
                Ok(self.path.clone())
            } else {
                Err(ExecError::failed(format!(
                    "{}: no such file",
                    path.display()
                )))
            }
        }
    }

    /// Both halves matter: the provider's file is readable although the disk
    /// has never heard of it, and the disk's file is missing although it is
    /// right there. A tool still reading the disk fails one or the other.
    #[tokio::test]
    async fn read_sees_the_providers_files_and_only_those() {
        let dir = TempDir::new().unwrap();
        let only_on_disk = dir.path().join("on_disk.txt");
        let only_in_provider = dir.path().join("in_provider.txt");
        tokio::fs::write(&only_on_disk, "from the disk\n")
            .await
            .unwrap();

        let mut ctx = ctx_in(dir.path());
        ctx.exec.filesystem = Arc::new(OneFile {
            path: only_in_provider.clone(),
            body: "from the provider\n",
        });

        let r = FileReadTool
            .call(
                json!({"file_path": only_in_provider.to_string_lossy()}),
                ctx.clone(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let ToolResultContent::Text(text) = r.content else {
            panic!("text read");
        };
        assert!(
            text.contains("from the provider"),
            "expected the provider's bytes, got: {text}"
        );

        let r = FileReadTool
            .call(
                json!({"file_path": only_on_disk.to_string_lossy()}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let ToolResultContent::Text(text) = r.content else {
            panic!("text read");
        };
        assert!(
            text.contains("(no such file)"),
            "a file the provider does not have is not there, got: {text}"
        );
    }
}
