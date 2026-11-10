//! GlobTool —— "Glob"。
//!
//! 用 `ignore::WalkBuilder`（gitignore 感知）扫指定 root，按 `globset::Glob`
//! 模式过滤；按 mtime 倒序输出，硬上限 1000 条。

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PathStyle, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ToolSpec, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const DEFAULT_MAX_RESULTS: usize = 1000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GlobInput {
    /// Shell-style glob pattern (e.g. "*.rs", "**/*.toml", "src/**/test_*.rs").
    pub pattern: String,

    /// Root directory to search; defaults to the current working directory.
    #[serde(default)]
    pub path: Option<String>,

    /// When false, include files ignored by .gitignore. Default true.
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,

    /// Maximum number of results to return. Default 1000.
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

fn default_max_results() -> usize {
    DEFAULT_MAX_RESULTS
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Clone, Copy)]
pub struct GlobTool;

fn searches_within_roots(input: &Value, ctx: &ToolContext) -> bool {
    match serde_json::from_value::<GlobInput>(input.clone()).map(|i| i.path) {
        Ok(Some(p)) => crate::security::read_allowed(&resolve_path(&p, &ctx.cwd), ctx),
        _ => true,
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .callable_from_code(true)
            .code_result(
                "{paths: string[], more: number} — newest first; more counts what max_results cut",
            )
            .concurrency_safe(true)
            .read_only(true)
            .permission_content(base::tool::PermissionContentKind::ReadPath)
    }
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern (e.g. src/**/*.rs)."
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<GlobInput>()
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("prompts/coding/glob.prompt.md").to_string())
    }

    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<GlobInput>(input.clone())
            .ok()
            .map(|i| i.path.unwrap_or_else(|| ".".into()))
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<GlobInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.pattern.is_empty() => ValidationResult::err("pattern must not be empty", 1),
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    fn accept_edits_allows(&self, input: &Value, ctx: &ToolContext) -> bool {
        searches_within_roots(input, ctx)
    }

    async fn check_permissions(&self, input: &Value, ctx: &ToolContext) -> PermissionDecision {
        if searches_within_roots(input, ctx) {
            return PermissionDecision::allow();
        }
        PermissionDecision::Ask {
            message: "Glob outside the project requires confirmation".into(),
            decision_reason: None,
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: GlobInput = serde_json::from_value(input)?;
        let max_results = input.max_results.max(1); // ensure at least 1
        let root = match &input.path {
            Some(p) => resolve_path(p, &ctx.cwd),
            None => ctx.cwd.clone(),
        };

        // 解析 pattern；不合法直接 validation error。
        // `literal_separator=true` 让 `*` 不跨 `/`（shell 行为）；`**` 仍可跨。
        let glob = globset::GlobBuilder::new(&input.pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| ToolError::Validation(format!("invalid glob pattern: {e}")))?
            .compile_matcher();

        // walk 在阻塞线程里跑（ignore crate 是 sync IO）
        let walker_root = root.clone();
        let cancel = ctx.cancel.clone();
        let scan_early_stop = max_results * 4; // scan up to 4x for early cutoff
        let excluded = crate::security::credentials(&ctx).excluded_from_walk(&root);
        let scan = tokio::task::spawn_blocking(
            move || -> Result<Vec<(PathBuf, SystemTime)>, ToolError> {
                let mut results = Vec::new();
                let walker = ignore::WalkBuilder::new(&walker_root)
                    .git_ignore(input.respect_gitignore)
                    .git_exclude(input.respect_gitignore)
                    .git_global(input.respect_gitignore)
                    .hidden(false)
                    .filter_entry(move |entry| !excluded(entry.path()))
                    .build();
                for entry in walker {
                    if cancel.is_cancelled() {
                        return Err(ToolError::Cancelled);
                    }
                    let Ok(entry) = entry else { continue };
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    // glob 匹配：优先用相对 root 的路径；fallback 绝对路径
                    let rel_path = path.strip_prefix(&walker_root).unwrap_or(path);
                    if !glob.is_match(rel_path) && !glob.is_match(path) {
                        continue;
                    }
                    // entry.metadata() 是 ignore::Result，m.modified() 是 std::io::Result —— 跨不通；分两步
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    results.push((path.to_path_buf(), mtime));
                    // 硬上限：超过 max_results * 4 时早停省时
                    if results.len() >= scan_early_stop {
                        break;
                    }
                }
                Ok(results)
            },
        );

        let result = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        r = scan => match r {
            Ok(inner) => inner?,
            Err(e) => return Err(ToolError::exec(e.to_string()))}};

        let total = result.len();
        let mut paths = result;
        paths.sort_by_key(|b| std::cmp::Reverse(b.1)); // 新→旧
        let truncated = paths.len() > max_results;
        if truncated {
            paths.truncate(max_results);
        }

        let mut output = String::with_capacity(paths.len() * 80);
        for (p, _) in &paths {
            output.push_str(&ctx.config.path_style.render(p, &ctx.cwd));
            output.push('\n');
        }
        if truncated {
            output.push_str(&format!(
                "\n[{} more not shown — narrow the pattern or path to see them]\n",
                total - max_results
            ));
        }
        if output.is_empty() {
            output.push_str("(no matches)\n");
        }

        let mut result = ToolResult::text(output);
        result.structured_content = Some(serde_json::json!({
            "paths": paths
                .iter()
                .map(|(p, _)| ctx.config.path_style.render(p, &ctx.cwd))
                .collect::<Vec<_>>(),
            "more": total - paths.len(),
        }));
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

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::ToolResultContent;
    use serde_json::json;
    use tempfile::TempDir;

    fn ctx_in(cwd: &Path) -> ToolContext {
        platform::tool_context_for_test(cwd.to_path_buf())
    }

    async fn make_tree(dir: &Path) {
        tokio::fs::write(dir.join("a.rs"), "x").await.unwrap();
        tokio::fs::write(dir.join("b.rs"), "x").await.unwrap();
        tokio::fs::write(dir.join("c.toml"), "x").await.unwrap();
        tokio::fs::create_dir_all(dir.join("src")).await.unwrap();
        tokio::fs::write(dir.join("src/lib.rs"), "x").await.unwrap();
        tokio::fs::write(dir.join("src/mod.rs"), "x").await.unwrap();
    }

    #[tokio::test]
    async fn finds_top_level_pattern() {
        let dir = TempDir::new().unwrap();
        make_tree(dir.path()).await;
        let tool = GlobTool;
        let r = tool
            .call(
                json!({"pattern": "*.rs"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let txt = match r.content {
            ToolResultContent::Text(t) => t,
            _ => panic!(),
        };
        assert!(txt.contains("a.rs"));
        assert!(txt.contains("b.rs"));
        assert!(!txt.contains("c.toml"));
        assert!(!txt.contains("lib.rs"), "*.rs shouldn't match nested");
    }

    #[tokio::test]
    async fn a_program_gets_the_paths_as_a_list() {
        let dir = TempDir::new().unwrap();
        make_tree(dir.path()).await;
        let r = GlobTool
            .call(
                json!({"pattern": "*.rs"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let s = r.structured_content.unwrap();
        let mut paths: Vec<String> = s["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().rsplit('/').next().unwrap().to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, ["a.rs", "b.rs"]);
        assert_eq!(s["more"], 0);
    }

    #[tokio::test]
    async fn double_star_matches_nested() {
        let dir = TempDir::new().unwrap();
        make_tree(dir.path()).await;
        let tool = GlobTool;
        let r = tool
            .call(
                json!({"pattern": "**/*.rs"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let txt = match r.content {
            ToolResultContent::Text(t) => t,
            _ => panic!(),
        };
        assert!(txt.contains("a.rs"));
        assert!(txt.contains("b.rs"));
        assert!(txt.contains("lib.rs"));
        assert!(txt.contains("mod.rs"));
        assert!(!txt.contains("c.toml"));
    }

    #[tokio::test]
    async fn no_match_returns_message() {
        let dir = TempDir::new().unwrap();
        make_tree(dir.path()).await;
        let tool = GlobTool;
        let r = tool
            .call(
                json!({"pattern": "*.nonexistent"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Text(t) => {
                assert!(t.contains("(no matches)"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn invalid_glob_yields_validation_error() {
        let dir = TempDir::new().unwrap();
        let tool = GlobTool;
        let err = tool
            .call(
                json!({"pattern": "[invalid"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn empty_pattern_validates_err() {
        let dir = TempDir::new().unwrap();
        let tool = GlobTool;
        let v = tool
            .validate_input(&json!({"pattern": ""}), &ctx_in(dir.path()))
            .await;
        assert!(!matches!(v, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn cancel_aborts() {
        let dir = TempDir::new().unwrap();
        make_tree(dir.path()).await;
        let tool = GlobTool;
        let ctx = platform::tool_context_for_test(dir.path().to_path_buf());
        ctx.cancel.cancel();
        let err = tool
            .call(json!({"pattern": "**/*"}), ctx, ProgressSender::noop("t"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn flags_say_readonly_and_concurrent_safe() {
        let tool = GlobTool;
        assert!(tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).concurrency_safe);
        assert_eq!(tool.name(), "Glob");
    }
}
