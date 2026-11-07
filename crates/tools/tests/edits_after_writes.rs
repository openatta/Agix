//! A file the session wrote itself can be edited again straight away: `Write`
//! and `Edit` leave it as read, while a change from outside still asks for a
//! fresh `Read` and a `Read` after an edit still returns the text.

use base::interface::tool::{
    ProgressSender, Tool, ToolContext, ToolResultContent, ValidationResult,
};
use serde_json::{json, Value};
use std::path::Path;

async fn run(tool: &dyn Tool, input: Value, ctx: &ToolContext) -> Result<String, String> {
    if let ValidationResult::Err(msg, _) = tool.validate_input(&input, ctx).await {
        return Err(msg);
    }
    let result = tool
        .call(input, ctx.clone(), ProgressSender::noop("t"))
        .await
        .map_err(|e| e.to_string())?;
    Ok(match result.content {
        ToolResultContent::Text(text) => text,
        ToolResultContent::Blocks(blocks) => blocks.into_iter().filter_map(|b| b.text).collect(),
    })
}

fn edit(path: &Path, old: &str, new: &str) -> Value {
    json!({ "file_path": path.display().to_string(), "old_string": old, "new_string": new })
}

#[tokio::test]
async fn the_session_edits_what_it_wrote_without_reading_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::for_test(dir.path().to_path_buf());
    let path = dir.path().join("report.md");
    let write = tools::file_write::FileWriteTool;
    let editor = tools::file_edit::FileEditTool;

    let input = json!({ "file_path": path.display().to_string(), "content": "alpha\nbeta\n" });
    run(&write, input, &ctx).await.unwrap();
    run(&editor, edit(&path, "alpha", "one"), &ctx)
        .await
        .unwrap();
    run(&editor, edit(&path, "beta", "two"), &ctx)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");

    let read = json!({ "file_path": path.display().to_string() });
    let text = run(&tools::file_read::FileReadTool, read, &ctx)
        .await
        .unwrap();
    assert!(
        text.contains("one") && !text.contains("file unchanged"),
        "{text}"
    );
}

#[tokio::test]
async fn a_change_from_outside_still_needs_a_fresh_read() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::for_test(dir.path().to_path_buf());
    let path = dir.path().join("notes.txt");
    let input = json!({ "file_path": path.display().to_string(), "content": "alpha\n" });
    run(&tools::file_write::FileWriteTool, input, &ctx)
        .await
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, "alpha\nfrom elsewhere\n").unwrap();
    let err = run(
        &tools::file_edit::FileEditTool,
        edit(&path, "alpha", "one"),
        &ctx,
    )
    .await
    .unwrap_err();
    assert!(err.contains("modified since it was read"), "{err}");
}
