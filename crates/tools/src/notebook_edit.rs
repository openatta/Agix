//! `NotebookEditTool` —— 编辑 Jupyter `.ipynb` 文件。
//!
//! Jupyter notebook 是 JSON：`{"cells": [{"cell_type":"code","source":[...],"outputs":[],...}], ...}`。
//! 三种操作：
//! - **insert**: 在指定 index 插入新 cell
//! - **edit**: 替换指定 index cell 的 source
//! - **delete**: 删指定 index cell
//!
//! 与 NotebookEditTool schema 对齐。

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotebookEditMode {
    Insert,
    Edit,
    Delete,
}

#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotebookCellType {
    Code,
    Markdown,
    Raw,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotebookEditInput {
    /// Absolute path to the .ipynb file
    pub file_path: String,
    /// Operation: insert / edit / delete
    pub mode: NotebookEditMode,
    /// 0-based cell index. For `insert`, the position to insert at; for
    /// edit / delete, the cell to operate on.
    pub cell_index: usize,
    /// New cell source (required for insert + edit; ignored for delete)
    #[serde(default)]
    pub new_source: Option<String>,
    /// Cell type for new cells (required for insert; defaults to code)
    #[serde(default)]
    pub cell_type: Option<NotebookCellType>,
}

pub struct NotebookEditTool;

#[async_trait]
impl Tool for NotebookEditTool {
    fn description(&self) -> &str {
        "Edit Jupyter notebook cells (replace, insert, delete)"
    }
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    /// **P3b **: 标 deferred —— 系统 prompt 仅暴露 name + 短描述，模型用
    /// ToolSearch 激活后下一轮拉 full schema。减少静态 prompt 占用。
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(NotebookEditInput)).expect("schema")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/notebook_edit.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false // 写文件
    }
    fn interrupt_behavior(&self, _input: &Value) -> base::tool::InterruptBehavior {
        base::tool::InterruptBehavior::Block
    }
    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<NotebookEditInput>(input.clone())
            .ok()
            .map(|i| i.file_path)
    }
    fn permission_content_kind(&self) -> base::tool::PermissionContentKind {
        base::tool::PermissionContentKind::WritePath
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<NotebookEditInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if !p.file_path.ends_with(".ipynb") => {
                ValidationResult::err("file_path must end with .ipynb", 1)
            }
            Ok(p)
                if matches!(p.mode, NotebookEditMode::Insert | NotebookEditMode::Edit)
                    && p.new_source.is_none() =>
            {
                ValidationResult::err("new_source is required for insert/edit modes", 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
    }
    async fn check_permissions(&self, input: &Value, ctx: &ToolContext) -> PermissionDecision {
        // 复用 path safety：notebook 不能写出 cwd（除非 additional_writable_dirs）
        let parsed: Result<NotebookEditInput, _> = serde_json::from_value(input.clone());
        let Ok(p) = parsed else {
            return PermissionDecision::allow();
        };
        let requested = std::path::PathBuf::from(&p.file_path);
        let path = {
            let raw = requested.clone();
            let resolved = if raw.is_absolute() {
                raw
            } else {
                ctx.cwd.join(raw)
            };
            crate::security::normalize_path_lexically(&resolved)
        };
        let path = ctx.exec.filesystem.canonicalize_best_effort(&path).await;
        let policy = crate::security::write_policy(ctx).await;
        match permissions::path_safety::check_write_for(&requested, &path, &policy) {
            Ok(_) => PermissionDecision::allow(),
            Err(permissions::path_safety::PathSafetyError::OutsideAllowedRoots { .. }) => {
                PermissionDecision::Ask {
                    message: "NotebookEdit outside the project requires confirmation".into(),
                    decision_reason: None,
                }
            }
            Err(e) => PermissionDecision::Deny {
                reason: Some(format!("path safety: {e:?}")),
                decision_reason: Some("notebook path outside cwd".into()),
            },
        }
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: NotebookEditInput = serde_json::from_value(input)?;
        let path = std::path::Path::new(&input.file_path);

        // **P1 **: snapshot before mutating so /rewind can restore.
        if let Some(snapshot) = &ctx.snapshot_file {
            snapshot.record(path, "NotebookEdit");
        }

        // 读 + 解析
        let content = ctx
            .exec
            .filesystem
            .read_to_string(path)
            .await
            .map_err(|e| ToolError::exec(format!("read {}: {e}", input.file_path)))?;
        let mut nb: Value = serde_json::from_str(&content)
            .map_err(|e| ToolError::exec(format!("parse ipynb JSON: {e}")))?;

        let cells = nb
            .get_mut("cells")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| ToolError::exec("ipynb missing 'cells' array".to_string()))?;

        match input.mode {
            NotebookEditMode::Insert => {
                if input.cell_index > cells.len() {
                    return Err(ToolError::exec(format!(
                        "cell_index {} out of range (cells.len={})",
                        input.cell_index,
                        cells.len()
                    )));
                }
                let new_cell = build_cell(
                    input.cell_type.unwrap_or(NotebookCellType::Code),
                    input.new_source.as_deref().unwrap_or(""),
                );
                cells.insert(input.cell_index, new_cell);
            }
            NotebookEditMode::Edit => {
                if input.cell_index >= cells.len() {
                    return Err(ToolError::exec(format!(
                        "cell_index {} out of range (cells.len={})",
                        input.cell_index,
                        cells.len()
                    )));
                }
                if let Some(cell) = cells
                    .get_mut(input.cell_index)
                    .and_then(|v| v.as_object_mut())
                {
                    let new_source = input.new_source.as_deref().unwrap_or("");
                    cell.insert("source".into(), source_to_lines(new_source));
                    // edit code cell 时清 outputs / execution_count（旧值过时）
                    if cell.get("cell_type").and_then(|v| v.as_str()) == Some("code") {
                        cell.insert("outputs".into(), json!([]));
                        cell.insert("execution_count".into(), Value::Null);
                    }
                }
            }
            NotebookEditMode::Delete => {
                if input.cell_index >= cells.len() {
                    return Err(ToolError::exec(format!(
                        "cell_index {} out of range (cells.len={})",
                        input.cell_index,
                        cells.len()
                    )));
                }
                cells.remove(input.cell_index);
            }
        }

        // 写回（pretty-print 让 git diff 可读）
        let new_content = serde_json::to_string_pretty(&nb)
            .map_err(|e| ToolError::exec(format!("serialize: {e}")))?;
        ctx.exec
            .filesystem
            .write_str(path, &new_content)
            .await
            .map_err(|e| ToolError::exec(format!("write: {e}")))?;

        let cells_after = nb
            .get("cells")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(format!(
                "{:?} cell at index {} in {}; notebook now has {} cells.",
                input.mode, input.cell_index, input.file_path, cells_after
            )),
            is_error: false,
            structured_content: Some(json!({
                "file_path": input.file_path,
                "mode": format!("{:?}", input.mode).to_lowercase(),
                "cells_after": cells_after})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

/// 构造一个新 cell 的 JSON 对象。
fn build_cell(ctype: NotebookCellType, source: &str) -> Value {
    let cell_type = match ctype {
        NotebookCellType::Code => "code",
        NotebookCellType::Markdown => "markdown",
        NotebookCellType::Raw => "raw",
    };
    let mut obj = Map::new();
    obj.insert("cell_type".into(), json!(cell_type));
    obj.insert("source".into(), source_to_lines(source));
    obj.insert("metadata".into(), json!({}));
    if cell_type == "code" {
        obj.insert("outputs".into(), json!([]));
        obj.insert("execution_count".into(), Value::Null);
    }
    Value::Object(obj)
}

/// Jupyter 期望 `source` 是 array-of-lines（每行带尾 `\n`，最后一行可没）。
fn source_to_lines(source: &str) -> Value {
    if source.is_empty() {
        return json!([]);
    }
    let mut lines: Vec<String> = source
        .split_inclusive('\n')
        .map(|s| s.to_string())
        .collect();
    // 最后一行没 `\n` 时保持原样（Jupyter 不强制）
    if lines.is_empty() {
        lines.push(source.to_string());
    }
    json!(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::exec::local::LocalFileSystem;
    use base::interface::exec::{DirEntry, ExecError, FileSystem, Metadata};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn empty_notebook_json() -> String {
        r#"{
  "cells": [],
  "metadata": {},
  "nbformat": 4,
  "nbformat_minor": 5
}"#
        .to_string()
    }

    fn one_code_cell_json() -> String {
        r#"{
  "cells": [
    {"cell_type": "code", "source": ["print('hi')\n"], "metadata": {}, "outputs": [], "execution_count": null}
  ],
  "metadata": {},
  "nbformat": 4,
  "nbformat_minor": 5
}"#
        .to_string()
    }

    #[tokio::test]
    async fn insert_cell_into_empty_notebook() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.ipynb");
        tokio::fs::write(&p, empty_notebook_json()).await.unwrap();
        let tool = NotebookEditTool;
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        // session state now managed by static stores
        let r = tool
            .call(
                json!({
                    "file_path": p.display().to_string(),
                    "mode": "insert",
                    "cell_index": 0,
                    "new_source": "x = 1",
                    "cell_type": "code"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let after = tokio::fs::read_to_string(&p).await.unwrap();
        let nb: Value = serde_json::from_str(&after).unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 1);
        assert_eq!(nb["cells"][0]["cell_type"], "code");
    }

    #[tokio::test]
    async fn edit_replaces_source_and_clears_outputs() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("b.ipynb");
        tokio::fs::write(&p, one_code_cell_json()).await.unwrap();
        let tool = NotebookEditTool;
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        // session state now managed by static stores
        let r = tool
            .call(
                json!({
                    "file_path": p.display().to_string(),
                    "mode": "edit",
                    "cell_index": 0,
                    "new_source": "y = 2"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let nb: Value =
            serde_json::from_str(&tokio::fs::read_to_string(&p).await.unwrap()).unwrap();
        let src: Vec<String> = serde_json::from_value(nb["cells"][0]["source"].clone()).unwrap();
        assert_eq!(src.join(""), "y = 2");
        assert_eq!(nb["cells"][0]["execution_count"], Value::Null);
    }

    #[tokio::test]
    async fn delete_removes_cell() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("c.ipynb");
        tokio::fs::write(&p, one_code_cell_json()).await.unwrap();
        let tool = NotebookEditTool;
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        // session state now managed by static stores
        let r = tool
            .call(
                json!({
                    "file_path": p.display().to_string(),
                    "mode": "delete",
                    "cell_index": 0}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let nb: Value =
            serde_json::from_str(&tokio::fs::read_to_string(&p).await.unwrap()).unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn out_of_range_index_errors() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("d.ipynb");
        tokio::fs::write(&p, empty_notebook_json()).await.unwrap();
        let tool = NotebookEditTool;
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        // session state now managed by static stores
        let r = tool
            .call(
                json!({
                    "file_path": p.display().to_string(),
                    "mode": "edit",
                    "cell_index": 99,
                    "new_source": "x"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn validates_extension() {
        let tool = NotebookEditTool;
        let r = tool
            .validate_input(
                &json!({
                    "file_path": "/tmp/not-a-notebook.txt",
                    "mode": "edit",
                    "cell_index": 0,
                    "new_source": "x"
                }),
                &ToolContext::for_test("/tmp".into()),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[test]
    fn source_to_lines_empty() {
        assert_eq!(source_to_lines(""), json!([]));
    }

    #[test]
    fn source_to_lines_multi() {
        let v = source_to_lines("a\nb\nc");
        let arr: Vec<String> = serde_json::from_value(v).unwrap();
        assert_eq!(arr, vec!["a\n".to_string(), "b\n".into(), "c".into()]);
    }

    /// The notebook the tool edits is the one the provider hands it, and the
    /// result goes back through the provider.
    #[tokio::test]
    async fn the_notebook_is_read_and_written_through_the_provider() {
        struct Interposed {
            body: String,
            written: std::sync::Mutex<Vec<String>>,
        }

        #[async_trait]
        impl FileSystem for Interposed {
            async fn read(&self, _: &Path) -> Result<Vec<u8>, ExecError> {
                Ok(self.body.as_bytes().to_vec())
            }
            async fn write(&self, _: &Path, bytes: &[u8]) -> Result<(), ExecError> {
                self.written
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(bytes).into_owned());
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
            async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
                LocalFileSystem.metadata(path).await
            }
            async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
                Ok(path.to_path_buf())
            }
        }

        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nb.ipynb");
        tokio::fs::write(&p, "not a notebook at all").await.unwrap();

        let provider = Arc::new(Interposed {
            body: one_code_cell_json(),
            written: std::sync::Mutex::new(Vec::new()),
        });
        let mut ctx = ToolContext::for_test(dir.path().to_path_buf());
        ctx.exec.filesystem = provider.clone();

        NotebookEditTool
            .call(
                json!({"file_path": p.to_string_lossy(), "cell_index": 0,
                       "mode": "edit", "new_source": "print(2)"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .expect("the disk holds no parsable notebook; the provider does");

        let written = provider.written.lock().unwrap().clone();
        assert_eq!(written.len(), 1);
        assert!(written[0].contains("print(2)"));
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "not a notebook at all",
            "the disk must not have been touched"
        );
    }
}
