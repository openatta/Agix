//! 冻结每个内建工具对自己的声明：spec、两种路径风格下的输入 schema、详细指南，
//! 以及一组样例输入下看输入的回答（只读、并发安全、破坏性、acceptEdits、权限匹配内容）。
//! 权限判定和请求里的工具表都读这些答案，改动必须是有意的：
//! `UPDATE_GOLDEN=1 cargo test -p tools --test integration spec_snapshot` 重新生成。

use base::tool::{PathStyle, PromptContext, Tool};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/tool_specs.json");

#[derive(Debug)]
struct NoSearch;

#[async_trait::async_trait]
impl tools::web_search::SearchProvider for NoSearch {
    async fn search(
        &self,
        _: &str,
        _: usize,
        _: &str,
    ) -> Result<tools::web_search::SearchOutput, tools::web_search::SearchError> {
        unreachable!("the snapshot never calls a tool")
    }
}

fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools::register_builtin_tools(&reg);
    tools::register_cron_tools(&reg, Arc::new(tools::cron::CronStore::new()));
    tools::register_worktree_tools(
        &reg,
        Arc::new(tools::worktree_tools::WorktreeRegistry::new()),
    );
    reg.register(Arc::new(tools::web_search::WebSearchTool::new(Box::new(
        NoSearch,
    ))));
    reg.register(Arc::new(tools::task_stop::TaskStopTool));
    reg.register(Arc::new(tools::task_output::TaskOutputTool));
    reg.register(Arc::new(tools::task_wait::TaskWaitTool));
    reg.register(Arc::new(tools::task_message::TaskMessageTool));
    let mut all = reg.list();
    all.sort_by(|a, b| a.name().cmp(b.name()));
    all
}

fn samples(name: &str) -> Vec<Value> {
    let mut inputs = vec![Value::Null, json!({})];
    let shell =
        |cmds: &[&str]| -> Vec<Value> { cmds.iter().map(|c| json!({ "command": c })).collect() };
    match name {
        "Bash" => inputs.extend(shell(&[
            "ls -la",
            "cat README.md",
            "git status",
            "git log --oneline -5 | head -3",
            "rm -rf build",
            "cargo build",
            "echo hi > out.txt",
            "curl https://example.com",
            "cat /etc/passwd",
            "",
        ])),
        "Monitor" => inputs.extend(shell(&["tail -f log.txt", "cargo watch", "ls"])),
        "Read" => inputs.push(json!({ "file_path": "/work/project/src/lib.rs" })),
        "Write" => {
            inputs.push(json!({ "file_path": "/work/project/a.txt", "content": "x" }));
            inputs.push(json!({ "file_path": "/elsewhere/a.txt", "content": "x" }));
        }
        "Edit" => inputs.push(json!({
            "file_path": "/work/project/a.txt", "old_string": "a", "new_string": "b"
        })),
        "NotebookEdit" => {
            inputs.push(json!({ "notebook_path": "/work/project/n.ipynb", "new_source": "x" }))
        }
        "Grep" => inputs.push(json!({ "pattern": "fn main", "path": "/work/project" })),
        "Glob" => inputs.push(json!({ "pattern": "**/*.rs", "path": "/work/project" })),
        "WebFetch" => inputs.push(json!({ "url": "https://example.com", "prompt": "x" })),
        "WebSearch" => inputs.push(json!({ "query": "rust" })),
        _ => {}
    }
    inputs
}

fn fnv1a(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

async fn snapshot(tool: &dyn Tool, cwd: &Path) -> Value {
    let spec = tool.spec();
    let prompt_ctx = PromptContext {
        cwd: cwd.to_path_buf(),
        all_tool_names: vec!["Agent".into(), "Bash".into(), "Read".into()],
        ..PromptContext::default()
    };
    let guide = tool
        .guide(&prompt_ctx)
        .await
        .map(|g| json!({ "bytes": g.len(), "fnv1a": fnv1a(&g) }));
    let ctx = platform::tool_context_for_test(cwd.to_path_buf());
    let calls: Vec<Value> = samples(tool.name())
        .into_iter()
        .map(|input| {
            let traits = tool.call_traits(&input);
            json!({
                "input": input,
                "read_only": traits.read_only,
                "concurrency_safe": traits.concurrency_safe,
                "destructive": traits.destructive,
                "accept_edits_allows": tool.accept_edits_allows(&input, &ctx),
                "permission_match_content": tool.permission_match_content(&input),
            })
        })
        .collect();
    json!({
        "name": tool.name(),
        "description": tool.description(),
        "origin": spec.origin.wire(),
        "short_description": spec.short_description,
        "traits": {
            "read_only": spec.traits.read_only,
            "concurrency_safe": spec.traits.concurrency_safe,
            "destructive": spec.traits.destructive,
        },
        "callable_from_code": spec.callable_from_code,
        "code_result": spec.code_result,
        "needs_dispatch": spec.needs_dispatch,
        "permission": {
            "content_kind": format!("{:?}", spec.permission.content_kind),
            "self_allow_overrides_mode": spec.permission.self_allow_overrides_mode,
            "leaves_plan_mode": spec.permission.leaves_plan_mode,
        },
        "input_schema": {
            "absolute": tool.input_schema_for(PathStyle::Absolute),
            "workspace_relative": tool.input_schema_for(PathStyle::WorkspaceRelative),
        },
        "guide": guide,
        "calls": calls,
    })
}

#[tokio::test]
async fn every_builtin_tool_says_the_same_about_itself() {
    let cwd = PathBuf::from("/work/project");
    let mut out = Vec::new();
    for tool in builtin_tools() {
        out.push(snapshot(tool.as_ref(), &cwd).await);
    }
    let actual = serde_json::to_string_pretty(&out).unwrap() + "\n";

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|e| panic!("{GOLDEN}: {e}; generate it with UPDATE_GOLDEN=1"));
    if actual != expected {
        let first = actual
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or(0);
        panic!(
            "tool specs drifted from {GOLDEN} at line {}: {:?} != {:?}; if intended, \
             regenerate with UPDATE_GOLDEN=1",
            first + 1,
            actual.lines().nth(first),
            expected.lines().nth(first),
        );
    }
}
