//! GrepTool —— "Grep"。
//!
//! 原生 Rust 实现 —— `regex` + `ignore::WalkBuilder` + `globset`，不依赖系统
//! `rg`。文件内容经会话的 `FileSystem` 读；超过 [`MAX_FILE_BYTES`] 的文件不读，
//! 在结果里点名。收集的行按排序保留最前面的、受行数与字节上限约束，所以
//! 大目录树上内存不随匹配数增长；walk 每个文件前看一次 cancel。

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::FileSystem;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::{
    PathStyle, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use ignore::{WalkBuilder, WalkState};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 默认行数上限（content 模式）。
const DEFAULT_HEAD_LIMIT: usize = 250;
/// 输出字节硬上限。
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Files larger than this are not searched, and the result names them.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// How many of the files left out for size the result lists by name.
const LISTED_SKIPPED_FILES: usize = 10;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepInput {
    /// Regex pattern to search for (Rust regex syntax).
    pub pattern: String,

    /// Root directory; defaults to cwd.
    #[serde(default)]
    pub path: Option<String>,

    /// Glob filter restricting which files to search (e.g. "*.rs").
    #[serde(default)]
    pub glob: Option<String>,

    /// "files_with_matches" (default), "content", or "count".
    #[serde(default)]
    pub output_mode: Option<String>,

    /// Cap output lines; default 250 in content mode.
    #[serde(default)]
    pub head_limit: Option<usize>,

    /// Case-insensitive match.
    #[serde(default)]
    pub case_insensitive: Option<bool>,

    /// Enable multiline mode where . matches newlines and ^/$ match line boundaries.
    #[serde(default)]
    pub multiline: Option<bool>,

    /// Prefix each line with its line number (content mode only).
    #[serde(default)]
    pub line_numbers: Option<bool>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new().callable_from_code(true).code_result("files_with_matches (default): {files: string[]}; count: {counts: {path: n}}; content: text, one 'path:line' match per line").concurrency_safe(true).read_only(true)
    }
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents with regex (ripgrep-style)."
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<GrepInput>()
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("prompts/coding/grep.prompt.md").to_string())
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<GrepInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.pattern.is_empty() => ValidationResult::err("pattern must not be empty", 1),
            Ok(p) => {
                match GrepMode::parse(p.output_mode.as_deref().unwrap_or("files_with_matches")) {
                    Ok(_) => ValidationResult::Ok,
                    Err(_) => ValidationResult::err(
                        "output_mode must be 'content', 'files_with_matches', or 'count'",
                        2,
                    ),
                }
            }
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
    }

    async fn permission_facts(&self, input: &Value, ctx: &ToolContext) -> PermissionFacts {
        let subject = self
            .subject_content(input)
            .map_or(Subject::None, Subject::ReadPath);
        match serde_json::from_value::<GrepInput>(input.clone()).map(|i| i.path) {
            Ok(Some(path)) => crate::security::read_facts(
                self.name(),
                subject,
                Some(&resolve_path(&path, &ctx.cwd)),
                ctx,
            ),
            Ok(None) => PermissionFacts::new(subject, Reach::Inside),
            Err(_) => crate::security::read_facts(self.name(), subject, None, ctx),
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: GrepInput = serde_json::from_value(input)?;
        let mode = GrepMode::parse(input.output_mode.as_deref().unwrap_or("files_with_matches"))?;
        let head_limit = input.head_limit.unwrap_or(match mode {
            GrepMode::Content => DEFAULT_HEAD_LIMIT,
            _ => 0,
        });
        let root = match &input.path {
            Some(p) => resolve_path(p, &ctx.cwd),
            None => ctx.cwd.clone(),
        };
        let (re, glob_matcher) = compile_matchers(&input)?;

        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let walk = Walk {
            excluded: Box::new(crate::security::credentials(&ctx).excluded_from_walk(&root)),
            root,
            pattern: Arc::new(re),
            glob: Arc::new(glob_matcher),
            style: ctx.config.path_style,
            cwd: ctx.cwd.clone(),
            filesystem: ctx.exec.filesystem.clone(),
            // The walk runs on `ignore`'s own threads, which are outside any
            // runtime, so blocking on them cannot stall a reactor.
            runtime: tokio::runtime::Handle::current(),
            cancel: ctx.cancel.clone(),
            mode,
            multiline: input.multiline.unwrap_or(false),
            line_numbers: input.line_numbers.unwrap_or(false),
            cap: if head_limit > 0 {
                head_limit
            } else {
                usize::MAX
            },
        };
        let result = tokio::task::spawn_blocking(move || walk.run()).await;

        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let found = result.map_err(|e| ToolError::exec(format!("grep walk panicked: {e}")))?;
        Ok(render_result(found, mode, head_limit))
    }
}

impl GrepTool {
    fn subject_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<GrepInput>(input.clone())
            .ok()
            .map(|i| i.path.unwrap_or_else(|| ".".into()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrepMode {
    Content,
    FilesWithMatches,
    Count,
}

impl GrepMode {
    fn parse(s: &str) -> Result<Self, ToolError> {
        match s {
            "content" => Ok(Self::Content),
            "files_with_matches" => Ok(Self::FilesWithMatches),
            "count" => Ok(Self::Count),
            other => Err(ToolError::Validation(format!(
                "invalid output_mode '{other}' (expected content / files_with_matches / count)"
            ))),
        }
    }
}

/// The output lines one file contributes.
fn match_lines(
    pat: &regex::Regex,
    content: &str,
    mode: GrepMode,
    multiline: bool,
    line_numbers: bool,
    shown: &str,
) -> Vec<String> {
    let line = |no: usize, text: &str| {
        if line_numbers {
            format!("{shown}:{no}:{text}\n")
        } else {
            format!("{shown}:{text}\n")
        }
    };
    match mode {
        GrepMode::FilesWithMatches if pat.is_match(content) => vec![format!("{shown}\n")],
        GrepMode::FilesWithMatches => vec![],
        GrepMode::Count => match pat.find_iter(content).count() {
            0 => vec![],
            n => vec![format!("{shown}:{n}\n")],
        },
        GrepMode::Content if multiline => {
            let mut out = Vec::new();
            let mut last_line = 0;
            let (mut counted_to, mut newlines_before) = (0, 0);
            for m in pat.find_iter(content) {
                let start = content[..m.start()].rfind('\n').map_or(0, |i| i + 1);
                let tail = if content[m.start()..m.end()].ends_with('\n') {
                    m.end() - 1
                } else {
                    m.end()
                };
                let end = content[tail..]
                    .find('\n')
                    .map_or(content.len(), |i| tail + i);
                newlines_before += content[counted_to..start].matches('\n').count();
                counted_to = start;
                let first = newlines_before + 1;
                for (i, text) in content[start..end].lines().enumerate() {
                    if first + i > last_line {
                        out.push(line(first + i, text));
                        last_line = first + i;
                    }
                }
            }
            out
        }
        GrepMode::Content => content
            .lines()
            .enumerate()
            .filter(|(_, text)| pat.is_match(text))
            .map(|(idx, text)| line(idx + 1, text))
            .collect(),
    }
}

/// The smallest output lines in sort order, bounded by a line cap and by the
/// output byte budget, so a walk over a huge tree holds no more than it can
/// return; `total` still counts every line found.
struct Collected {
    kept: std::collections::BinaryHeap<String>,
    bytes: usize,
    cap: usize,
    total: usize,
    dropped_for_size: bool,
    too_large: Vec<String>,
}

impl Collected {
    fn new(cap: usize) -> Self {
        Self {
            kept: std::collections::BinaryHeap::new(),
            bytes: 0,
            cap,
            total: 0,
            dropped_for_size: false,
            too_large: Vec::new(),
        }
    }

    fn push(&mut self, line: String) {
        self.total += 1;
        self.bytes += line.len();
        self.kept.push(line);
        while self.kept.len() > self.cap || (self.bytes > MAX_OUTPUT_BYTES && self.kept.len() > 1) {
            let over_bytes = self.kept.len() <= self.cap;
            let Some(largest) = self.kept.pop() else {
                break;
            };
            self.bytes -= largest.len();
            self.dropped_for_size |= over_bytes;
        }
    }

    fn into_sorted(self) -> Vec<String> {
        self.kept.into_sorted_vec()
    }
}

/// 简化二进制检测：前 1KB 含 NUL byte 视为二进制
fn is_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    head.contains(&0)
}

fn resolve_path(s: &str, cwd: &Path) -> PathBuf {
    let p = PathBuf::from(s);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

fn compile_matchers(
    input: &GrepInput,
) -> Result<(regex::Regex, Option<globset::GlobMatcher>), ToolError> {
    let mut regex_builder = regex::RegexBuilder::new(&input.pattern);
    regex_builder.case_insensitive(input.case_insensitive.unwrap_or(false));
    if input.multiline.unwrap_or(false) {
        regex_builder.multi_line(true).dot_matches_new_line(true);
    }
    let re = regex_builder
        .build()
        .map_err(|e| ToolError::Validation(format!("invalid regex pattern: {e}")))?;
    let glob_matcher = match &input.glob {
        Some(g) => Some(
            globset::Glob::new(g)
                .map_err(|e| ToolError::Validation(format!("invalid glob '{g}': {e}")))?
                .compile_matcher(),
        ),
        None => None,
    };
    Ok((re, glob_matcher))
}

fn render_result(found: Collected, mode: GrepMode, head_limit: usize) -> ToolResult {
    let total_lines = found.total;
    let dropped_for_size = found.dropped_for_size;
    let too_large = found.too_large.clone();
    let mut lines = found.into_sorted();

    let structured = match mode {
        GrepMode::FilesWithMatches => Some(serde_json::json!({
            "files": lines.iter().map(|l| l.trim_end_matches('\n')).collect::<Vec<_>>(),
        })),
        GrepMode::Count => Some(serde_json::json!({
            "counts": lines
                .iter()
                .filter_map(|l| {
                    let (path, n) = l.trim_end_matches('\n').rsplit_once(':')?;
                    Some((path.to_string(), serde_json::json!(n.parse::<u64>().ok()?)))
                })
                .collect::<serde_json::Map<_, _>>(),
        })),
        GrepMode::Content => None,
    };

    if head_limit > 0 && total_lines > head_limit && !dropped_for_size {
        lines.push(format!(
            "\n[output truncated to first {} of {} lines]\n",
            head_limit, total_lines
        ));
    }

    let mut text: String = lines.concat();
    if text.len() > MAX_OUTPUT_BYTES {
        let kept = base::text::truncate_at_char_boundary(&text, MAX_OUTPUT_BYTES).len();
        text.truncate(kept);
        text.push_str("\n[output truncated]\n");
    } else if dropped_for_size {
        text.push_str("\n[output truncated]\n");
    }
    if text.is_empty() {
        text.push_str("(no matches)\n");
    }
    if !too_large.is_empty() {
        let listed = too_large
            .iter()
            .take(LISTED_SKIPPED_FILES)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = too_large.len().saturating_sub(LISTED_SKIPPED_FILES);
        text.push_str(&format!(
            "\n[{} file(s) larger than {} MiB were not searched: {listed}{}]\n",
            too_large.len(),
            MAX_FILE_BYTES / (1024 * 1024),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ));
    }

    let mut result = ToolResult::text(text);
    result.structured_content = structured;
    result
}

struct Walk {
    root: PathBuf,
    excluded: Box<dyn Fn(&Path) -> bool + Send + Sync>,
    pattern: Arc<regex::Regex>,
    glob: Arc<Option<globset::GlobMatcher>>,
    style: base::tool::PathStyle,
    cwd: PathBuf,
    filesystem: Arc<dyn FileSystem>,
    runtime: tokio::runtime::Handle,
    cancel: tokio_util::sync::CancellationToken,
    mode: GrepMode,
    multiline: bool,
    line_numbers: bool,
    cap: usize,
}

impl Walk {
    fn run(self) -> Collected {
        let found = Arc::new(Mutex::new(Collected::new(self.cap)));
        let mut wb = WalkBuilder::new(&self.root);
        let excluded = self.excluded;
        wb.standard_filters(true)
            .hidden(false)
            .filter_entry(move |entry| !excluded(entry.path()));
        let shared = Arc::new(WalkShared {
            pattern: self.pattern,
            glob: self.glob,
            style: self.style,
            cwd: self.cwd,
            filesystem: self.filesystem,
            runtime: self.runtime,
            mode: self.mode,
            multiline: self.multiline,
            line_numbers: self.line_numbers,
        });
        let cancel = self.cancel;
        wb.build_parallel().run(|| {
            let found = found.clone();
            let shared = shared.clone();
            let cancel = cancel.clone();
            Box::new(move |entry| {
                if cancel.is_cancelled() {
                    return WalkState::Quit;
                }
                if let Ok(entry) = entry {
                    shared.search_entry(&entry, &found);
                }
                WalkState::Continue
            })
        });

        let mut found = std::mem::replace(
            &mut *found
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Collected::new(self.cap),
        );
        found.too_large.sort();
        found
    }
}

struct WalkShared {
    pattern: Arc<regex::Regex>,
    glob: Arc<Option<globset::GlobMatcher>>,
    style: base::tool::PathStyle,
    cwd: PathBuf,
    filesystem: Arc<dyn FileSystem>,
    runtime: tokio::runtime::Handle,
    mode: GrepMode,
    multiline: bool,
    line_numbers: bool,
}

impl WalkShared {
    fn search_entry(&self, entry: &ignore::DirEntry, found: &Mutex<Collected>) {
        let path = entry.path();
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            return;
        }
        if let Some(g) = self.glob.as_ref().as_ref() {
            let name_match = path.file_name().map(|n| g.is_match(n)).unwrap_or(false);
            let path_match = g.is_match(path);
            if !name_match && !path_match {
                return;
            }
        }
        let shown = self.style.render(path, &self.cwd);
        if self
            .runtime
            .block_on(self.filesystem.metadata(path))
            .is_ok_and(|m| m.len > MAX_FILE_BYTES)
        {
            found
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .too_large
                .push(shown);
            return;
        }
        let Ok(bytes) = self.runtime.block_on(self.filesystem.read(path)) else {
            return;
        };
        if is_binary(&bytes) {
            return;
        }
        let Ok(content) = std::str::from_utf8(&bytes) else {
            return;
        };
        let lines = match_lines(
            &self.pattern,
            content,
            self.mode,
            self.multiline,
            self.line_numbers,
            &shown,
        );
        if !lines.is_empty() {
            let mut g = found
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for line in lines {
                g.push(line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::exec::{DirEntry, ExecError, Metadata};
    use base::tool::ToolResultContent;
    use platform::LocalFileSystem;
    use serde_json::json;
    use tempfile::TempDir;

    fn ctx_in(cwd: &Path) -> ToolContext {
        platform::tool_context_for_test(cwd.to_path_buf())
    }

    /// 起 grep 工具不再依赖 rg；这个 gate 函数保留为永远返回 true 兼容
    /// 测试结构（call points 仍引用），但不再 spawn rg 子进程
    fn rg_available() -> bool {
        true
    }

    #[tokio::test]
    async fn finds_simple_pattern_in_content_mode() {
        if !rg_available() {
            eprintln!("skipping: rg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "foo\nbar\nfoo bar\n")
            .await
            .unwrap();
        let tool = GrepTool;
        let r = tool
            .call(
                json!({"pattern": "foo", "output_mode": "content"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("foo"));
                let foo_lines = t.matches("foo").count();
                assert!(foo_lines >= 2);
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn files_with_matches_mode() {
        if !rg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "needle\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), "no match\n")
            .await
            .unwrap();
        let tool = GrepTool;
        let r = tool
            .call(
                json!({"pattern": "needle", "output_mode": "files_with_matches"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let files = r.structured_content.clone().unwrap()["files"].clone();
        assert_eq!(files.as_array().unwrap().len(), 1);
        assert!(files[0].as_str().unwrap().ends_with("a.txt"), "{files}");
        match r.content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("a.txt"));
                assert!(!t.contains("b.txt"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn count_mode() {
        if !rg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "x\nx\nx\n")
            .await
            .unwrap();
        let tool = GrepTool;
        let r = tool
            .call(
                json!({"pattern": "x", "output_mode": "count"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("a.txt:3") || t.contains("a.txt:3\n"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn glob_filter_works() {
        if !rg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "needle\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), "needle\n")
            .await
            .unwrap();
        let tool = GrepTool;
        let r = tool
            .call(
                json!({"pattern": "needle", "glob": "*.rs", "output_mode": "files_with_matches"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("a.rs"));
                assert!(!t.contains("b.txt"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn no_matches_returns_marker() {
        if !rg_available() {
            return;
        }
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "hello\n")
            .await
            .unwrap();
        let tool = GrepTool;
        let r = tool
            .call(
                json!({"pattern": "definitely-not-here-zzzzzz"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Text(t) => assert!(t.contains("(no matches)")),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn invalid_output_mode_validates_err() {
        let dir = TempDir::new().unwrap();
        let tool = GrepTool;
        let v = tool
            .validate_input(
                &json!({"pattern": "x", "output_mode": "bogus"}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(!matches!(v, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn empty_pattern_validates_err() {
        let dir = TempDir::new().unwrap();
        let tool = GrepTool;
        let v = tool
            .validate_input(&json!({"pattern": ""}), &ctx_in(dir.path()))
            .await;
        assert!(!matches!(v, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn flags_say_readonly_and_concurrent_safe() {
        let tool = GrepTool;
        assert!(tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).concurrency_safe);
        assert_eq!(tool.name(), "Grep");
    }

    #[test]
    fn input_schema_is_object_with_pattern_field() {
        let schema = GrepTool.input_schema(base::tool::PathStyle::Absolute);
        let s = serde_json::to_value(&schema).unwrap();
        assert_eq!(s["type"], "object");
        assert!(s["properties"].get("pattern").is_some());
    }

    #[tokio::test]
    async fn cancel_aborts() {
        let dir = TempDir::new().unwrap();
        let ctx = platform::tool_context_for_test(dir.path().to_path_buf());
        let cancel = ctx.cancel.clone();
        // We cancel before the call starts since grep is usually fast.
        // This verifies the cancel path exists and returns correctly.
        cancel.cancel();
        let tool = GrepTool;
        let r = tool
            .call(json!({"pattern": "x"}), ctx, ProgressSender::noop("t"))
            .await;
        // Either cancelled or completed (due to race) — both acceptable
        match r {
            Ok(_) => {}                     // completed before cancel took effect
            Err(ToolError::Cancelled) => {} // cancel worked
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[tokio::test]
    async fn output_cut_at_the_size_cap_never_splits_a_multibyte_character() {
        let line = "中".repeat(40);
        for shift in 0..3 {
            let dir = TempDir::new().unwrap();
            let mut body = format!("{}{line}\n", "x".repeat(shift));
            body.push_str(&format!("{line}\n").repeat(MAX_OUTPUT_BYTES / line.len() + 100));
            tokio::fs::write(dir.path().join("a.txt"), body)
                .await
                .unwrap();
            let r = GrepTool
                .call(
                    json!({"pattern": "中", "output_mode": "content", "head_limit": 0}),
                    ctx_in(dir.path()),
                    ProgressSender::noop("t"),
                )
                .await
                .unwrap();
            let ToolResultContent::Text(text) = r.content else {
                panic!("text result");
            };
            assert!(text.ends_with("[output truncated]\n"), "shift {shift}");
            assert!(text.len() <= MAX_OUTPUT_BYTES + "\n[output truncated]\n".len());
        }
    }

    fn text_of(r: ToolResult) -> String {
        match r.content {
            ToolResultContent::Text(t) => t,
            other => panic!("text result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_file_of_several_mib_is_searched_and_one_past_the_cap_is_named() {
        let dir = TempDir::new().unwrap();
        let mut log = "routine reading, all values nominal\n".repeat(150_000);
        log.push_str("DEEP-NEEDLE\n");
        assert!(log.len() > 5 * 1024 * 1024);
        tokio::fs::write(dir.path().join("big.log"), &log)
            .await
            .unwrap();
        let huge = std::fs::File::create(dir.path().join("huge.log")).unwrap();
        huge.set_len(MAX_FILE_BYTES + 1).unwrap();

        let text = text_of(
            GrepTool
                .call(
                    json!({"pattern": "DEEP-NEEDLE"}),
                    ctx_in(dir.path()),
                    ProgressSender::noop("t"),
                )
                .await
                .unwrap(),
        );
        assert!(text.contains("big.log"), "{text}");
        assert!(
            text.contains("1 file(s) larger than 32 MiB were not searched")
                && text.contains("huge.log"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn the_lines_kept_under_a_head_limit_are_the_first_in_sort_order() {
        let dir = TempDir::new().unwrap();
        for name in ["c.txt", "a.txt", "d.txt", "b.txt"] {
            tokio::fs::write(dir.path().join(name), "hit\nhit\n")
                .await
                .unwrap();
        }
        let text = text_of(
            GrepTool
                .call(
                    json!({"pattern": "hit", "output_mode": "content", "head_limit": 3, "line_numbers": true}),
                    ctx_in(dir.path()),
                    ProgressSender::noop("t"),
                )
                .await
                .unwrap(),
        );
        let lines: Vec<&str> = text.lines().filter(|l| l.contains(":hit")).collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with("a.txt:1:hit"), "{text}");
        assert!(lines[2].ends_with("b.txt:1:hit"), "{text}");
        assert!(text.contains("first 3 of 8 lines"), "{text}");
    }

    #[tokio::test]
    async fn a_multiline_match_in_content_mode_shows_the_lines_it_spans() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("a.rs"),
            "fn x() {}\nstruct S {\n    a: u8,\n    field: u8,\n}\n",
        )
        .await
        .unwrap();
        let text = text_of(
            GrepTool
                .call(
                    json!({"pattern": "struct S \\{.*?field", "output_mode": "content",
                           "multiline": true, "line_numbers": true}),
                    ctx_in(dir.path()),
                    ProgressSender::noop("t"),
                )
                .await
                .unwrap(),
        );
        assert!(text.contains("a.rs:2:struct S {"), "{text}");
        assert!(text.contains("a.rs:4:    field: u8,"), "{text}");
        assert!(!text.contains("fn x"), "{text}");
    }

    #[tokio::test]
    async fn cancel_mid_walk_stops() {
        struct CancelOnFirstRead {
            cancel: tokio_util::sync::CancellationToken,
            reads: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl FileSystem for CancelOnFirstRead {
            async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
                self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.cancel.cancel();
                LocalFileSystem.read(path).await
            }
            async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
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
            async fn rename(&self, from: &Path, to: &Path) -> Result<(), ExecError> {
                LocalFileSystem.rename(from, to).await
            }
            async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
                LocalFileSystem.metadata(path).await
            }
            async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
                LocalFileSystem.canonicalize(path).await
            }
        }

        let dir = TempDir::new().unwrap();
        let files = 2000;
        for i in 0..files {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "x\n").unwrap();
        }
        let mut ctx = ctx_in(dir.path());
        let fs = Arc::new(CancelOnFirstRead {
            cancel: ctx.cancel.clone(),
            reads: Default::default(),
        });
        ctx.exec.filesystem = fs.clone();
        let r = GrepTool
            .call(json!({"pattern": "x"}), ctx, ProgressSender::noop("t"))
            .await;
        assert!(matches!(r, Err(ToolError::Cancelled)), "{r:?}");
        let reads = fs.reads.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            reads < files / 10,
            "{reads} of {files} files read after cancel"
        );
    }

    /// The walk stays on this machine — it is not part of the contract — but
    /// the bytes that decide a match come from the provider.
    #[tokio::test]
    async fn what_grep_matches_on_is_the_providers_bytes() {
        struct SameBodyEverywhere;

        #[async_trait]
        impl FileSystem for SameBodyEverywhere {
            async fn read(&self, _: &Path) -> Result<Vec<u8>, ExecError> {
                Ok(b"needle\n".to_vec())
            }
            async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
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
            async fn rename(&self, from: &Path, to: &Path) -> Result<(), ExecError> {
                LocalFileSystem.rename(from, to).await
            }
            async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
                LocalFileSystem.metadata(path).await
            }
            async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
                LocalFileSystem.canonicalize(path).await
            }
        }

        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "haystack only\n")
            .await
            .unwrap();

        let mut ctx = ctx_in(dir.path());
        ctx.exec.filesystem = Arc::new(SameBodyEverywhere);

        let r = GrepTool
            .call(
                json!({"pattern": "needle", "path": dir.path().to_string_lossy()}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let ToolResultContent::Text(text) = r.content else {
            panic!("text result");
        };
        assert!(
            text.contains("a.txt"),
            "the file matches only through the provider's bytes, got: {text}"
        );
    }
}
