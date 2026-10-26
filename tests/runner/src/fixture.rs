//! 模板项目 fixture 拷贝——api/cli 两种模式共用，避免各自维护一份递归拷贝逻辑。

use std::path::Path;

/// `tests/fixtures/template_project/.agix/settings.json` 里 `mcp_servers.demo.command`
/// 的占位符——fixture 会被拷贝到随机临时目录运行，没法在提交的文件里写死绝对路径。
const MCP_TOY_SERVER_PLACEHOLDER: &str = "{{MCP_TOY_SERVER_BIN}}";

/// 递归拷贝 `src` 到 `dst`（含权限位，hook 脚本要保持可执行）。
/// `dst` 不存在会被创建；已存在的同名文件会被覆盖。
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
            let perm = std::fs::metadata(entry.path())?.permissions();
            std::fs::set_permissions(&dst_path, perm)?;
        }
    }
    Ok(())
}

/// 拷贝完 fixture 之后调用：把 `.agix/settings.json` 里的 `{{MCP_TOY_SERVER_BIN}}`
/// 占位符换成 `mcp-toy-server` 真实编译出来的绝对路径（没编译过会先编译一次）。
/// 占位符不存在（fixture 没用到 mcp-toy-server，或本来就不含 mcp_servers）时是 no-op。
pub fn resolve_mcp_toy_server_placeholder(workdir: &Path) -> anyhow::Result<()> {
    let settings_path = workdir.join(".agix").join("settings.json");
    let content = match std::fs::read_to_string(&settings_path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    if !content.contains(MCP_TOY_SERVER_PLACEHOLDER) {
        return Ok(());
    }

    let status = std::process::Command::new("cargo")
        .args(["build", "-p", "mcp-toy-server", "--quiet"])
        .status()?;
    anyhow::ensure!(status.success(), "cargo build -p mcp-toy-server failed");

    let binary = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/mcp-toy-server")
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("mcp-toy-server binary should exist after build: {e}"))?;

    let replaced = content.replace(MCP_TOY_SERVER_PLACEHOLDER, &binary.to_string_lossy());
    std::fs::write(&settings_path, replaced)?;
    Ok(())
}
