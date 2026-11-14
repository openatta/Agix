//! 冻结 code 场景一次请求里 `tools` 数组的字节。跨会话的提示缓存按前缀命中，工具表
//! 差一个字节就整段失效；确实要改时用
//! `UPDATE_GOLDEN=1 cargo test -p runtime tool_defs_golden` 重新生成。

use super::support::{recall_test_settings, FixedUsageModel};
use crate::module::fakes::{mcp_module, mcp_tool};
use std::path::Path;
use std::sync::Arc;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/code_scene_tool_defs.json"
);

#[tokio::test]
async fn code_scene_tool_defs_match_the_golden_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let template = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools::register_builtin_tools(&template);
    let (mut agent, _rx, _tx) = crate::agent::Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(FixedUsageModel {
            usage: Default::default(),
        }))
        .tools(template)
        .settings(Arc::new(recall_test_settings(dir.path())))
        .module(mcp_module(vec![
            mcp_tool("github", "search_issues"),
            mcp_tool("github", "create_issue"),
        ]))
        .skip_warmup(true)
        .build()
        .unwrap();

    let defs = agent.build_tool_defs().await;
    let actual = serde_json::to_string_pretty(&defs).unwrap() + "\n";
    let root = dir.path().display().to_string();
    assert!(
        !actual.contains(&root),
        "the tool table must not depend on where the session lives"
    );

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
            "tool defs drifted from {GOLDEN} at line {}: {:?} != {:?}",
            first + 1,
            actual.lines().nth(first),
            expected.lines().nth(first),
        );
    }
}
