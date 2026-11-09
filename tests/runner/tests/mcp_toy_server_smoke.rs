//! 端到端验证 `mcp-toy-server`（`tests/fixtures/mcp_toy_server`）真的是一个能连上的
//! MCP server——不是像 template_project 早期版本那样一个预期失败的占位。
//! 用真实的 `mcp::manager::McpManager::connect_all` 走 stdio transport 连接，
//! 再通过真实的 `McpToolAdapter::call` 调用 `ping` 工具，验证往返正确。
//!
//! `#[ignore]`：会拉起一个子进程，跑得比纯单测慢；`cargo test -- --ignored` 显式跑，
//! 或用 `tests/run_api.sh --fixture` 间接覆盖。

use std::collections::HashMap;
use std::path::PathBuf;

fn toy_server_binary() -> PathBuf {
    // 保证是最新构建 —— 跟 run_cli.sh 对 daemon 二进制的处理方式一致。
    let status = std::process::Command::new("cargo")
        .args(["build", "-p", "mcp-toy-server", "--quiet"])
        .status()
        .expect("failed to invoke cargo build -p mcp-toy-server");
    assert!(status.success(), "cargo build -p mcp-toy-server failed");

    // tests/runner/../../target/debug/mcp-toy-server
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/mcp-toy-server")
        .canonicalize()
        .expect("mcp-toy-server binary should exist after build")
}

#[tokio::test]
#[ignore]
async fn mcp_toy_server_connects_and_ping_round_trips() {
    let binary = toy_server_binary();

    let mut servers = HashMap::new();
    servers.insert(
        "demo".to_string(),
        mcp::config::McpServerConfig::Stdio {
            command: binary.to_string_lossy().to_string(),
            args: vec![],
            env: HashMap::new(),
            scope: None,
            timeouts: Default::default(),
            working_dir: None,
        },
    );

    let manager = mcp::manager::McpManager::connect_all(servers).await;
    assert_eq!(
        manager.server_count(),
        1,
        "expected the demo server to connect"
    );

    let statuses = manager.server_statuses();
    assert!(
        statuses
            .iter()
            .any(|s| s.name == "demo" && s.tool_count > 0),
        "expected demo server status to report at least one tool, got: {statuses:?}"
    );

    let tool = manager
        .tool_adapters()
        .iter()
        .find(|t| base::tool::Tool::name(t.as_ref()) == "mcp__demo__ping")
        .unwrap_or_else(|| {
            panic!(
                "expected mcp__demo__ping among: {:?}",
                manager
                    .tool_adapters()
                    .iter()
                    .map(|t| base::tool::Tool::name(t.as_ref()))
                    .collect::<Vec<_>>()
            )
        });

    let ctx = base::tool::ToolContext::for_test(std::env::temp_dir());
    let progress = base::tool::ProgressSender::noop("test-tool-use");
    let result = tool
        .call(serde_json::json!({ "message": "hello" }), ctx, progress)
        .await
        .expect("ping call should succeed");

    let text = match &result.content {
        base::tool::ToolResultContent::Text(t) => t.clone(),
        base::tool::ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join(""),
    };
    assert!(!result.is_error, "ping call reported an error: {text}");
    assert!(
        text.contains("toy: hello"),
        "expected echoed message, got: {text}"
    );

    let greeted = manager
        .invoke_prompt_command("mcp__demo__greet", "name=Ada style=formal")
        .await;
    assert!(
        !greeted.is_error && greeted.text.contains("Greet Ada in a formal style."),
        "{}",
        greeted.text
    );
    let missing = manager.invoke_prompt_command("mcp__demo__greet", "").await;
    assert!(missing.is_error, "a missing required argument is refused");
    let unicode = manager.read_resource_text("demo", "toy://unicode").await;
    assert!(
        unicode.as_deref().is_ok_and(|t| t.contains("🦀")),
        "{unicode:?}"
    );
}
