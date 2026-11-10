//! The search tools against a project big enough to hit their limits.
//!
//! Every existing test for `Grep`, `Glob` and `Read` runs against a handful
//! of files written inline, which is the wrong size to catch anything: the
//! behavior that matters at scale is what these tools do when there is *too
//! much* — the 250-match head limit, a glob spanning hundreds of paths, a
//! file longer than one screenful. None of that is reachable with four
//! files, so none of it was covered.
//!
//! The tree is generated rather than committed. A few hundred files of
//! plausible-looking source would be a few hundred files nobody reads, and
//! the properties here care about shape and volume, not content.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use base::tool::{ProgressSender, Tool, ToolContext, ToolResultContent};
use std::path::Path;

/// A project with enough files, and enough matches per file, to run past
/// every default limit these tools apply.
fn generate_project(root: &Path) {
    for module in 0..40 {
        let dir = root.join(format!("src/module_{module:02}"));
        std::fs::create_dir_all(&dir).unwrap();

        for file in 0..5 {
            let mut body = String::new();
            body.push_str(&format!("// module_{module:02}/file_{file}\n"));
            for line in 0..40 {
                // `needle` appears on most lines, so the total match count is
                // far past `Grep`'s 250-line default.
                body.push_str(&format!(
                    "pub fn needle_{module:02}_{file}_{line}() {{ /* needle */ }}\n"
                ));
            }
            std::fs::write(dir.join(format!("file_{file}.rs")), body).unwrap();
        }
        std::fs::write(
            dir.join("README.md"),
            // Deliberately free of the search term — these are what
            // `files_with_matches` must leave out.
            format!("# module_{module:02}\n\nDocumentation only.\n"),
        )
        .unwrap();
    }
}

fn ctx(root: &Path) -> ToolContext {
    platform::tool_context_for_test(root.to_path_buf())
}

async fn run(tool: &dyn Tool, input: serde_json::Value, root: &Path) -> String {
    let result = tool
        .call(input, ctx(root), ProgressSender::noop("t"))
        .await
        .expect("tool call");
    match result.content {
        ToolResultContent::Text(t) => t,
        other => panic!("expected text output, got {other:?}"),
    }
}

/// Content mode caps at 250 lines by default. Past that the cap is the whole
/// behavior: a model handed 8000 matches learns nothing it could not learn
/// from 250, and pays for all of them.
#[tokio::test]
async fn grep_content_mode_stops_at_its_head_limit() {
    let dir = tempfile::tempdir().unwrap();
    generate_project(dir.path());

    let out = run(
        &tools::grep::GrepTool,
        serde_json::json!({ "pattern": "needle", "output_mode": "content" }),
        dir.path(),
    )
    .await;

    let lines = out.lines().filter(|l| l.contains("needle")).count();
    assert!(
        lines <= 250,
        "content mode returned {lines} matching lines, past its 250-line default"
    );
    assert!(
        lines > 100,
        "only {lines} lines came back — the fixture is not large enough to test the limit"
    );
}

/// An explicit `head_limit` is honoured, and is what a caller uses when the
/// default is the wrong size for the question.
#[tokio::test]
async fn grep_honours_an_explicit_head_limit() {
    let dir = tempfile::tempdir().unwrap();
    generate_project(dir.path());

    let out = run(
        &tools::grep::GrepTool,
        serde_json::json!({ "pattern": "needle", "output_mode": "content", "head_limit": 7 }),
        dir.path(),
    )
    .await;

    let lines = out.lines().filter(|l| l.contains("needle")).count();
    assert!(lines <= 7, "asked for 7 lines, got {lines}");
}

/// `files_with_matches` across 200 files: the answer is a file list, and the
/// files that genuinely have no match must not be in it.
#[tokio::test]
async fn grep_file_mode_lists_only_files_that_match() {
    let dir = tempfile::tempdir().unwrap();
    generate_project(dir.path());

    let out = run(
        &tools::grep::GrepTool,
        serde_json::json!({ "pattern": "needle", "output_mode": "files_with_matches" }),
        dir.path(),
    )
    .await;

    assert!(
        !out.contains("README.md"),
        "a file with no match was listed: {out}"
    );
    assert!(out.contains("file_0.rs"), "expected source files: {out}");
}

/// A glob that spans the whole tree. The interesting part is that the
/// pattern selects — `**/*.rs` must not drag the READMEs along.
#[tokio::test]
async fn glob_spans_the_tree_and_filters_by_extension() {
    let dir = tempfile::tempdir().unwrap();
    generate_project(dir.path());

    let out = run(
        &tools::glob::GlobTool,
        serde_json::json!({ "pattern": "**/*.rs" }),
        dir.path(),
    )
    .await;

    assert!(
        !out.contains("README.md"),
        "the glob picked up files the pattern excludes: {out}"
    );
    let hits = out.lines().filter(|l| l.ends_with(".rs")).count();
    assert!(hits > 100, "expected the whole tree, got {hits} files");
}

/// Reading a long file in windows. Two adjacent pages must not overlap and
/// must not skip a line between them — off-by-one here shows up as a model
/// citing a line that is not where it says it is.
#[tokio::test]
async fn read_pages_a_long_file_without_gaps_or_overlap() {
    let dir = tempfile::tempdir().unwrap();
    generate_project(dir.path());
    let file = dir.path().join("src/module_00/file_0.rs");

    let first = run(
        &tools::file_read::FileReadTool,
        serde_json::json!({ "file_path": file.display().to_string(), "offset": 1, "limit": 10 }),
        dir.path(),
    )
    .await;
    let second = run(
        &tools::file_read::FileReadTool,
        serde_json::json!({ "file_path": file.display().to_string(), "offset": 11, "limit": 10 }),
        dir.path(),
    )
    .await;

    // The generated bodies are one distinct function per line, so a line's
    // own text identifies it.
    assert!(first.contains("needle_00_0_0("), "first page: {first}");
    assert!(
        !first.contains("needle_00_0_11("),
        "the first page ran past its limit: {first}"
    );
    assert!(
        second.contains("needle_00_0_11("),
        "the second page skipped a line: {second}"
    );
    assert!(
        !second.contains("needle_00_0_0("),
        "the pages overlap: {second}"
    );
}
