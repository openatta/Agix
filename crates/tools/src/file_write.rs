//! FileWriteTool —— "Write".
//!
//! 写文件全覆盖：是 destructive，不是只读。
//! 目录不在则自动创建。：路径白名单 / 黑名单（`.env*` 等）由上层权限闸做，
//! 这里只做 schema + 大小校验。

use crate::cancel::run_with_cancel;
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::FileSystem;
use base::interface::permission::PermissionFacts;
use base::tool::{
    PathStyle, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

/// 内容大小硬上限 —— 防止模型把整个仓库塞进一次 Write。
pub const MAX_WRITE_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileWriteInput {
    /// The absolute path to the file to write. Relative paths are resolved
    /// against the current working directory.
    pub file_path: String,

    /// The content to write. Replaces the file's existing content entirely.
    pub content: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FileWriteTool;

#[async_trait]
impl Tool for FileWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new().callable_from_code(true).destructive(true)
    }
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file with given content."
    }

    fn input_schema(&self, style: PathStyle) -> Value {
        crate::file_read::workspace_relative_file_path(
            crate::input_schema::<FileWriteInput>(),
            style,
            "write",
        )
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("prompts/coding/file_write.prompt.md").to_string())
    }

    async fn validate_input(&self, input: &Value, ctx: &ToolContext) -> ValidationResult {
        let parsed: Result<FileWriteInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.file_path.is_empty() => {
                ValidationResult::err("file_path must not be empty", 1)
            }
            Ok(p) if p.content.len() > MAX_WRITE_BYTES => ValidationResult::err(
                format!(
                    "content too large ({} bytes); cap is {} bytes",
                    p.content.len(),
                    MAX_WRITE_BYTES
                ),
                3,
            ),
            Ok(p) => {
                if let Err(e) =
                    permissions::path_safety::check_requested_path(Path::new(&p.file_path))
                {
                    return ValidationResult::err(e.to_string(), 5);
                }
                let target = crate::security::resolve_write_target(
                    &p.file_path,
                    &ctx.cwd,
                    &*ctx.exec.filesystem,
                )
                .await;
                if let Some(msg) = ctx.session.check_read_staleness(&target) {
                    return ValidationResult::err(msg, 4);
                }
                ValidationResult::Ok
            }
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn permission_facts(&self, input: &Value, ctx: &ToolContext) -> PermissionFacts {
        let file_path = self.subject_content(input);
        crate::security::write_facts(self.name(), file_path.as_deref(), ctx).await
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: FileWriteInput = serde_json::from_value(input)?;
        let path = crate::security::resolve_write_target(
            &input.file_path,
            &ctx.cwd,
            &*ctx.exec.filesystem,
        )
        .await;

        if let Some(snapshot) = &ctx.snapshot_file {
            snapshot.record(&path, "Write");
        }

        let result = run_with_cancel(
            &ctx.cancel,
            write_file(&*ctx.exec.filesystem, &path, &input.content),
        )
        .await?;
        if result.is_ok() {
            ctx.session.record_write(&path);
        }

        result.map(|bytes| {
            ToolResult::text(format!(
                "Wrote {} byte{} to {}",
                bytes,
                if bytes == 1 { "" } else { "s" },
                ctx.config.path_style.render(&path, &ctx.cwd)
            ))
        })
    }
}

impl FileWriteTool {
    fn subject_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<FileWriteInput>(input.clone())
            .ok()
            .map(|i| i.file_path)
    }
}

async fn write_file(fs: &dyn FileSystem, path: &Path, content: &str) -> Result<usize, ToolError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && fs.metadata_following_symlinks(parent).await.is_err() {
            fs.create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Validation(format!("cannot create parent dir: {e}")))?;
        }
    }
    fs.write_str(path, content).await?;
    Ok(content.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::exec::{DirEntry, ExecError, Metadata};
    use base::interface::permission::Reach;
    use base::tool::ToolResultContent;
    use platform::LocalFileSystem;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx_in(cwd: &Path) -> ToolContext {
        platform::tool_context_for_test(cwd.to_path_buf())
    }

    #[tokio::test]
    async fn writes_new_file() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("out.txt");
        let tool = FileWriteTool;
        let r = tool
            .call(
                json!({"file_path": p.to_string_lossy(), "content": "hello\n"}),
                ctx_in(dir.path()),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let on_disk = tokio::fs::read_to_string(&p).await.unwrap();
        assert_eq!(on_disk, "hello\n");
        match r.content {
            ToolResultContent::Text(s) => {
                assert!(s.contains("Wrote 6 bytes"));
                assert!(s.contains(&p.to_string_lossy().to_string()));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("out.txt");
        tokio::fs::write(&p, "old content").await.unwrap();
        let tool = FileWriteTool;
        tool.call(
            json!({"file_path": p.to_string_lossy(), "content": "new"}),
            ctx_in(dir.path()),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
        let on_disk = tokio::fs::read_to_string(&p).await.unwrap();
        assert_eq!(on_disk, "new");
    }

    #[tokio::test]
    async fn relative_path_resolves_against_cwd() {
        let dir = TempDir::new().unwrap();
        let tool = FileWriteTool;
        tool.call(
            json!({"file_path": "rel.txt", "content": "x"}),
            ctx_in(dir.path()),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
        let on_disk = tokio::fs::read_to_string(dir.path().join("rel.txt"))
            .await
            .unwrap();
        assert_eq!(on_disk, "x");
    }

    #[tokio::test]
    async fn creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nested/sub/dir/file.txt");
        let tool = FileWriteTool;
        tool.call(
            json!({"file_path": p.to_string_lossy(), "content": "deep"}),
            ctx_in(dir.path()),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
        assert!(p.exists());
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "deep");
    }

    #[tokio::test]
    async fn empty_path_validates_err() {
        let dir = TempDir::new().unwrap();
        let tool = FileWriteTool;
        let res = tool
            .validate_input(
                &json!({"file_path": "", "content": "x"}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(!matches!(res, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn too_large_content_validates_err() {
        let dir = TempDir::new().unwrap();
        let tool = FileWriteTool;
        let big = "x".repeat(MAX_WRITE_BYTES + 1);
        let res = tool
            .validate_input(
                &json!({"file_path": "/tmp/x.txt", "content": big}),
                &ctx_in(dir.path()),
            )
            .await;
        assert!(!matches!(res, ValidationResult::Ok));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dotdot_after_a_symlink_is_judged_where_the_write_really_lands() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (proj, outside) = (root.join("proj"), root.join("outside"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::os::unix::fs::symlink(outside.join("sub"), proj.join("link")).unwrap();
        let input = json!({"file_path": "link/../x", "content": "x"});

        let decision = FileWriteTool
            .permission_facts(&input, &ctx_in(&proj))
            .await
            .reach;
        assert!(
            matches!(decision, Reach::Outside | Reach::Unknown),
            "{decision:?}"
        );
        if matches!(decision, Reach::Inside) {
            FileWriteTool
                .call(input, ctx_in(&proj), ProgressSender::noop("t"))
                .await
                .unwrap();
        }
        assert!(!outside.join("x").exists());
        assert!(!proj.join("x").exists());
    }

    #[tokio::test]
    async fn a_path_the_shell_would_have_expanded_is_invalid_input() {
        let dir = TempDir::new().unwrap();
        for raw in ["~/notes.txt", "$(whoami).txt", "\\\\server\\share\\a.txt"] {
            let res = FileWriteTool
                .validate_input(
                    &json!({"file_path": raw, "content": "x"}),
                    &ctx_in(dir.path()),
                )
                .await;
            assert!(!matches!(res, ValidationResult::Ok), "{raw}");
        }
    }

    #[tokio::test]
    async fn project_write_allows_inside_asks_outside_and_denies_sensitive() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let tool = FileWriteTool;

        let inside = tool
            .permission_facts(
                &json!({"file_path": "ok.txt", "content": "x"}),
                &ctx_in(dir.path()),
            )
            .await
            .reach;
        assert!(matches!(inside, Reach::Inside));

        let outside = tool
            .permission_facts(
                &json!({
                    "file_path": outside.path().join("ok.txt").to_string_lossy(),
                    "content": "x"
                }),
                &ctx_in(dir.path()),
            )
            .await
            .reach;
        assert!(matches!(outside, Reach::Outside | Reach::Unknown));

        let sensitive = tool
            .permission_facts(
                &json!({"file_path": ".env", "content": "SECRET=1"}),
                &ctx_in(dir.path()),
            )
            .await
            .reach;
        assert!(matches!(sensitive, Reach::Refused(_)));
    }

    #[tokio::test]
    async fn cancel_aborts_call() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("c.txt");
        let tool = FileWriteTool;
        let ctx = platform::tool_context_for_test(dir.path().to_path_buf());
        ctx.cancel.cancel();
        let err = tool
            .call(
                json!({"file_path": p.to_string_lossy(), "content": "x"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
        assert!(!p.exists());
    }

    #[tokio::test]
    async fn flags_are_destructive_not_readonly() {
        let tool = FileWriteTool;
        assert!(!tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).destructive);
        assert!(!tool.call_traits(&Value::Null).concurrency_safe);
        assert_eq!(tool.name(), "Write");
    }

    /// A filesystem that reports one path as living somewhere else, the way a
    /// remote provider would report its own symlinks.
    struct ResolvesElsewhere {
        from: PathBuf,
        to: PathBuf,
    }

    #[async_trait]
    impl FileSystem for ResolvesElsewhere {
        async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
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
        async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
            LocalFileSystem.metadata(path).await
        }
        async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
            if path == self.from {
                Ok(self.to.clone())
            } else {
                LocalFileSystem.canonicalize(path).await
            }
        }
    }

    /// The write policy has to judge the path the *provider* says the write
    /// will land on. A tool that resolved the path against this machine's disk
    /// instead would allow the write here: nothing on this disk links
    /// `inside/note.txt` anywhere, and lexically it sits in the project.
    #[tokio::test]
    async fn the_policy_judges_the_path_the_provider_resolves_to() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        let elsewhere = dir.path().join("elsewhere");
        tokio::fs::create_dir_all(&project).await.unwrap();
        tokio::fs::create_dir_all(&elsewhere).await.unwrap();

        let target = project.join("note.txt");
        let mut ctx = ctx_in(&project);
        ctx.exec.filesystem = Arc::new(ResolvesElsewhere {
            from: target.clone(),
            to: elsewhere.join("note.txt"),
        });

        let decision = FileWriteTool
            .permission_facts(
                &json!({"file_path": target.to_string_lossy(), "content": "x"}),
                &ctx,
            )
            .await
            .reach;
        assert!(
            matches!(decision, Reach::Outside | Reach::Unknown),
            "a write the provider places outside the project must be asked \
             about, got {decision:?}"
        );

        // Same tool, same path, a provider that resolves it where it looks
        // like it is: allowed. Without this half the assertion above would
        // also pass on a tool that denied everything.
        let inside = ctx_in(&project);
        let decision = FileWriteTool
            .permission_facts(
                &json!({"file_path": target.to_string_lossy(), "content": "x"}),
                &inside,
            )
            .await
            .reach;
        assert!(matches!(decision, Reach::Inside), "got {decision:?}");
    }

    /// The bytes go where the provider puts them, not where the path points.
    #[tokio::test]
    async fn the_write_itself_goes_through_the_provider() {
        struct Redirects(PathBuf);

        #[async_trait]
        impl FileSystem for Redirects {
            async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
                LocalFileSystem.read(path).await
            }
            async fn write(&self, _path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
                LocalFileSystem.write(&self.0, bytes).await
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

        let dir = TempDir::new().unwrap();
        let named = dir.path().join("named.txt");
        let actual = dir.path().join("actual.txt");
        let mut ctx = ctx_in(dir.path());
        ctx.exec.filesystem = Arc::new(Redirects(actual.clone()));

        FileWriteTool
            .call(
                json!({"file_path": named.to_string_lossy(), "content": "hello"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();

        assert_eq!(tokio::fs::read_to_string(&actual).await.unwrap(), "hello");
        assert!(
            !named.exists(),
            "the tool wrote to the path directly instead of through the provider"
        );
    }

    #[tokio::test]
    async fn schema_has_required_fields() {
        let tool = FileWriteTool;
        let s = tool.input_schema(PathStyle::Absolute);
        let body = s
            .get("properties")
            .or_else(|| s.get("schema").and_then(|s| s.get("properties")))
            .expect("schema must have properties");
        assert!(body.get("file_path").is_some());
        assert!(body.get("content").is_some());
    }
}
