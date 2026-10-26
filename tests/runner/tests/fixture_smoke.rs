//! 模板项目 fixture 的结构性冒烟测试 —— 不跑真实 Agent/LLM，只验证
//! `tests/fixtures/template_project/` 里的配置文件能被真实的解析器认出来
//! （hooks_config / mcp_servers / .agix/agents/*.md），防止 fixture 被改坏
//! 而没人发现。

use std::path::Path;

fn fixture_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../fixtures/template_project"
    ))
}

#[test]
fn settings_json_hooks_config_parses() {
    let path = fixture_dir().join(".agix/settings.json");
    let content = std::fs::read_to_string(&path).expect("read settings.json");
    let v: serde_json::Value = serde_json::from_str(&content).expect("valid json");

    let hooks_value = v.get("hooks_config").expect("hooks_config key present");
    let hooks: hooks::HooksSettings =
        serde_json::from_value(hooks_value.clone()).expect("hooks_config parses as HooksSettings");
    assert!(
        hooks.contains_key(&hooks::HookEvent::PreToolUse),
        "expected a PreToolUse hook"
    );
}

#[test]
fn settings_json_mcp_servers_parses() {
    let path = fixture_dir().join(".agix/settings.json");
    let content = std::fs::read_to_string(&path).expect("read settings.json");
    let v: serde_json::Value = serde_json::from_str(&content).expect("valid json");

    let mcp_value = v.get("mcp_servers").expect("mcp_servers key present");
    let map: std::collections::HashMap<String, serde_json::Value> =
        serde_json::from_value(mcp_value.clone()).expect("mcp_servers is a map");
    assert!(!map.is_empty(), "expected at least one mcp server entry");
    for (name, cfg) in &map {
        let _parsed: mcp::config::McpServerConfig = serde_json::from_value(cfg.clone())
            .unwrap_or_else(|e| panic!("mcp_servers.{name} should parse as McpServerConfig: {e}"));
    }
}

#[test]
fn custom_agent_type_frontmatter_parses() {
    let dir = fixture_dir().join(".agix/agents");
    let types = runtime::agent_tool::load_agent_types_from_dir(&dir);
    assert!(
        !types.is_empty(),
        "expected at least one *.md agent type under .agix/agents/"
    );
    assert!(
        types.iter().any(|t| t.name == "reviewer"),
        "expected the 'reviewer' agent type to parse"
    );
}

/// 端到端验证 api_runner.rs 实际会走的路径：`Settings::load()` 指向拷贝出来的
/// `.agix/`，`project_root()` 落在 fixture 根，hooks_config/mcp_servers 通过
/// 三层合并读出来——不是靠读代码猜的。
#[test]
fn settings_load_resolves_project_root_and_config_from_fixture() {
    let tmp = std::env::temp_dir().join(format!("agix_fixture_smoke_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let settings = base::interface::settings::Settings::load(
        tmp.join("global_empty"),
        tmp.join("scene_empty"),
        fixture_dir().join(".agix"),
        "code",
        "claude-sonnet-4-6",
    );

    assert_eq!(
        settings.paths.project_root(),
        fixture_dir().to_path_buf(),
        "project_root() should resolve to the fixture root"
    );
    let hooks = settings
        .hooks_config
        .expect("hooks_config should load from fixture's .agix/settings.json");
    assert!(hooks.get("PreToolUse").is_some());
    assert!(
        !settings.mcp_servers.is_empty(),
        "mcp_servers should load from fixture's .agix/settings.json"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// 验证 `{{MCP_TOY_SERVER_BIN}}` 占位符替换机制本身：拷贝 fixture 到临时目录、
/// 跑 `resolve_mcp_toy_server_placeholder`，结果应该是一个真实存在、可执行的
/// `mcp-toy-server` 绝对路径——不再含任何占位符文本。真正的连接/调用验证见
/// `tests/mcp_toy_server_smoke.rs`（这里只测替换本身，避免重复拉起子进程）。
#[test]
fn mcp_toy_server_placeholder_resolves_to_real_binary() {
    let tmp = std::env::temp_dir().join(format!("agix_fixture_smoke_mcp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    test_runner::fixture::copy_dir_recursive(fixture_dir(), &tmp).expect("copy fixture");
    test_runner::fixture::resolve_mcp_toy_server_placeholder(&tmp).expect("resolve placeholder");

    let content = std::fs::read_to_string(tmp.join(".agix/settings.json"))
        .expect("read copied settings.json");
    assert!(
        !content.contains("{{MCP_TOY_SERVER_BIN}}"),
        "placeholder should be fully substituted"
    );

    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    let command = v["mcp_servers"]["demo"]["command"]
        .as_str()
        .expect("command is a string");
    let path = Path::new(command);
    assert!(
        path.is_absolute(),
        "resolved command should be an absolute path, got: {command}"
    );
    let meta = std::fs::metadata(path).unwrap_or_else(|e| {
        panic!("resolved mcp-toy-server binary should exist at {command}: {e}")
    });
    use std::os::unix::fs::PermissionsExt;
    assert!(
        meta.permissions().mode() & 0o111 != 0,
        "resolved binary should be executable"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn hook_script_is_executable() {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture_dir().join(".agix/hooks/pre_bash_log.sh");
    let meta = std::fs::metadata(&path).expect("hook script exists");
    assert!(
        meta.permissions().mode() & 0o111 != 0,
        "pre_bash_log.sh must be executable"
    );
}
