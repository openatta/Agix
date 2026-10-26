//! 端到端验证插件 harness：真的拉起 `agixd`，用 `rpc_client::DaemonRpcClient`
//! 把 `tests/fixtures/plugins/demo-plugin/` 打包安装、确认出现在 `plugin.list`、
//! 再卸载——跟 `daemon/tests/daemon_e2e.rs` 里同类测试的区别是这里走的是真实子进程
//! + 提交的插件源码目录，不是 daemon crate 内部直接调用 + 内联现造的 zip。
//!
//! `#[ignore]`：拉子进程，比纯单测慢；`cargo test -- --ignored` 显式跑。
//!
//! **要一个带插件载体的 daemon。** `scripts` 和 `plugins` 是互斥 feature，而
//! `scripts` 是默认的——所以默认构建出来的 `agixd` 会把 `plugin.*` 全部回
//! 成「compiled-out」，这个用例在那种构建上不可能通过。碰到这种情况它会说清楚
//! 怎么跑然后停下，而不是报一个和插件毫无关系的红：
//!
//! ```sh
//! cargo build -p daemon --no-default-features --features plugins \
//!   --target-dir target/plugins
//! AGIX_PLUGIN_DAEMON=target/plugins/debug/agixd \
//!   cargo test -p test-runner --test plugin_lifecycle_smoke -- --ignored
//! ```

use rpc_client::DaemonRpcClient;
use std::path::PathBuf;
use std::process::Stdio;

#[tokio::test]
#[ignore]
async fn plugin_install_list_uninstall_round_trips() {
    // A daemon built somewhere else, when the caller has one with the plugin
    // carrier compiled in. Building it here instead would overwrite the
    // default-featured binary every other test uses.
    let daemon_binary = match std::env::var("AGIX_PLUGIN_DAEMON") {
        Ok(path) => PathBuf::from(path)
            .canonicalize()
            .expect("AGIX_PLUGIN_DAEMON does not point at a binary"),
        Err(_) => {
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "daemon", "--quiet"])
                .status()
                .expect("failed to invoke cargo build -p daemon");
            assert!(status.success(), "cargo build -p daemon failed");
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/debug/agixd")
                .canonicalize()
                .expect("agixd binary should exist after build")
        }
    };

    let tmp = std::env::temp_dir().join(format!("agix_plugin_smoke_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let config_home = tmp.join("config_home"); // AGIX_CONFIG_HOME — isolates from the real ~/.agix
    let socket_path = tmp.join("daemon.sock");
    std::fs::create_dir_all(&config_home).unwrap();

    let (zip_path, checksum) =
        test_runner::plugin_fixture::package_demo_plugin(&tmp.join("packaged"))
            .expect("package demo-plugin");
    let download_url = test_runner::plugin_fixture::file_url(&zip_path);

    let workdir = tmp.join("workdir");
    std::fs::create_dir_all(&workdir).unwrap();

    let mut child = tokio::process::Command::new(&daemon_binary)
        .arg("--socket")
        .arg(&socket_path)
        // A daemon's project is its working directory, and a child that
        // inherits this one's works on the crate it was launched from — it
        // writes `.agix/` into the repository. `AGIX_CONFIG_HOME` redirects
        // the user-level root and says nothing about the project-level one.
        .current_dir(&workdir)
        .env("AGIX_CONFIG_HOME", &config_home)
        // Startup requires *some* auth token even though this test never
        // triggers a real model call (plugin install/list/uninstall is pure
        // local file I/O) — a dummy value is enough to pass the check.
        .env("ANTHROPIC_AUTH_TOKEN", "test-dummy-token-plugin-smoke")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn agixd");

    let mut attempts = 0;
    while attempts < 100 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        attempts += 1;
    }
    assert!(socket_path.exists(), "daemon socket not ready after 10s");

    let mut client = DaemonRpcClient::connect(&socket_path)
        .await
        .expect("connect");

    let install_resp = client
        .call(
            "plugin.install",
            serde_json::json!({
                "name": "demo-plugin",
                "version": "1.0.0",
                "download_url": download_url,
                "checksum": checksum,
            }),
        )
        .await
        .expect("plugin.install RPC");
    if install_resp
        .error
        .as_ref()
        .is_some_and(|e| e.code == daemon::rpc::codes::PLUGINS_DISABLED)
    {
        eprintln!(
            "skipping: this `agixd` carries no plugin subsystem. See this \
             file's header for how to build one and point AGIX_PLUGIN_DAEMON at it."
        );
        return;
    }
    assert!(
        install_resp.error.is_none(),
        "plugin.install failed: {:?}",
        install_resp.error
    );

    let list_resp = client.plugin_list().await.expect("plugin.list RPC");
    let plugins = list_resp.result.expect("plugin.list result");
    let names: Vec<&str> = plugins["plugins"]
        .as_array()
        .expect("plugins is an array")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        names.contains(&"demo-plugin"),
        "expected demo-plugin in plugin.list, got: {names:?}"
    );

    let uninstall_resp = client
        .call(
            "plugin.uninstall",
            serde_json::json!({ "name": "demo-plugin" }),
        )
        .await
        .expect("plugin.uninstall RPC");
    assert!(
        uninstall_resp.error.is_none(),
        "plugin.uninstall failed: {:?}",
        uninstall_resp.error
    );

    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&tmp);
}
