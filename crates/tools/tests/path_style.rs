//! What the file tools tell the model about paths under each `PathStyle`:
//! argument docs and the Read guide, and the paths Grep, Glob, Write and Edit
//! print. The default style must keep every byte of today's output.

use base::interface::tool::{PathStyle, ProgressSender, PromptContext, Tool, ToolContext};
use base::tool::ToolResultContent;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tools::file_edit::FileEditTool;
use tools::file_read::FileReadTool;
use tools::file_write::FileWriteTool;
use tools::glob::GlobTool;
use tools::grep::GrepTool;

fn ctx_in(cwd: &Path, style: PathStyle) -> ToolContext {
    let mut ctx = ToolContext::for_test(cwd.to_path_buf());
    let mut config = (*ctx.config).clone();
    config.path_style = style;
    ctx.config = Arc::new(config);
    ctx
}

async fn run(tool: &dyn Tool, input: Value, cwd: &Path, style: PathStyle) -> String {
    let r = tool
        .call(input, ctx_in(cwd, style), ProgressSender::noop("t"))
        .await
        .unwrap();
    match r.content {
        ToolResultContent::Text(s) => s,
        _ => panic!("expected text"),
    }
}

async fn workspace() -> TempDir {
    let dir = TempDir::new().unwrap();
    tokio::fs::create_dir_all(dir.path().join("src"))
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("src/a.rs"), "needle\n")
        .await
        .unwrap();
    dir
}

#[tokio::test]
async fn default_style_prints_absolute_paths() {
    let dir = workspace().await;
    let abs = dir.path().join("src/a.rs").display().to_string();

    let grep = run(
        &GrepTool,
        json!({"pattern": "needle"}),
        dir.path(),
        PathStyle::Absolute,
    )
    .await;
    assert_eq!(grep, format!("{abs}\n"));

    let glob = run(
        &GlobTool,
        json!({"pattern": "**/*.rs"}),
        dir.path(),
        PathStyle::Absolute,
    )
    .await;
    assert_eq!(glob, format!("{abs}\n"));

    let written = run(
        &FileWriteTool,
        json!({"file_path": "w.txt", "content": "x"}),
        dir.path(),
        PathStyle::Absolute,
    )
    .await;
    assert_eq!(
        written,
        format!("Wrote 1 byte to {}", dir.path().join("w.txt").display())
    );

    let edited = run(
        &FileEditTool,
        json!({"file_path": "src/a.rs", "old_string": "needle", "new_string": "pin"}),
        dir.path(),
        PathStyle::Absolute,
    )
    .await;
    assert!(edited.starts_with(&format!("Applied 1 edit (1 replacement) to {abs} (")));
}

#[tokio::test]
async fn default_style_leaves_schemas_and_guide_as_generated() {
    for (tool, doc) in [
        (
            &FileReadTool as &dyn Tool,
            "The absolute path to the file to read.",
        ),
        (&FileWriteTool, "The absolute path to the file to write."),
        (&FileEditTool, "The absolute path to the file to edit."),
    ] {
        assert_eq!(
            tool.input_schema_for(PathStyle::Absolute),
            tool.input_schema()
        );
        let schema = tool.input_schema_for(PathStyle::Absolute);
        assert!(schema["properties"]["file_path"]["description"]
            .as_str()
            .unwrap()
            .starts_with(doc));
    }
    let guide = FileReadTool.prompt(&PromptContext::default()).await;
    assert!(guide.contains("must be an absolute path, not a relative path"));
}

#[tokio::test]
async fn workspace_relative_prints_paths_under_the_working_directory_relative() {
    let dir = workspace().await;
    let style = PathStyle::WorkspaceRelative;
    let rel = Path::new("src").join("a.rs").display().to_string();

    let grep = run(&GrepTool, json!({"pattern": "needle"}), dir.path(), style).await;
    assert_eq!(grep, format!("{rel}\n"));

    let counted = run(
        &GrepTool,
        json!({"pattern": "needle", "output_mode": "count"}),
        dir.path(),
        style,
    )
    .await;
    assert_eq!(counted, format!("{rel}:1\n"));

    let glob = run(&GlobTool, json!({"pattern": "**/*.rs"}), dir.path(), style).await;
    assert_eq!(glob, format!("{rel}\n"));

    let written = run(
        &FileWriteTool,
        json!({"file_path": "w.txt", "content": "x"}),
        dir.path(),
        style,
    )
    .await;
    assert_eq!(written, "Wrote 1 byte to w.txt");

    let edited = run(
        &FileEditTool,
        json!({"file_path": dir.path().join("src/a.rs").display().to_string(),
               "old_string": "needle", "new_string": "pin"}),
        dir.path(),
        style,
    )
    .await;
    assert!(edited.starts_with(&format!("Applied 1 edit (1 replacement) to {rel} (")));
    assert!(!edited.contains(&dir.path().display().to_string()));
}

#[tokio::test]
async fn workspace_relative_keeps_paths_outside_the_working_directory_absolute() {
    let ws = workspace().await;
    let other = TempDir::new().unwrap();
    tokio::fs::write(other.path().join("o.rs"), "needle\n")
        .await
        .unwrap();
    let out = run(
        &GlobTool,
        json!({"pattern": "*.rs", "path": other.path().display().to_string()}),
        ws.path(),
        PathStyle::WorkspaceRelative,
    )
    .await;
    assert_eq!(out, format!("{}\n", other.path().join("o.rs").display()));
}

#[tokio::test]
async fn workspace_relative_rewords_argument_docs_and_the_read_guide() {
    for tool in [&FileReadTool as &dyn Tool, &FileWriteTool, &FileEditTool] {
        let schema = tool.input_schema_for(PathStyle::WorkspaceRelative);
        let doc = schema["properties"]["file_path"]["description"]
            .as_str()
            .unwrap();
        assert!(
            doc.contains("relative to the current working directory"),
            "{doc}"
        );
        assert!(doc.contains("absolute path is also accepted"), "{doc}");
        assert!(!doc.contains("The absolute path"), "{doc}");
    }
    let ctx = PromptContext {
        path_style: PathStyle::WorkspaceRelative,
        ..PromptContext::default()
    };
    let guide = FileReadTool.prompt(&ctx).await;
    assert!(guide
        .contains("relative to the current working directory; an absolute path is also accepted"));
    assert!(!guide.contains("must be an absolute path"));
}
