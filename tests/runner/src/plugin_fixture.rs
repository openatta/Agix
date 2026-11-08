//! 把 `tests/fixtures/plugins/demo-plugin/`（提交进仓库的、未打包的插件源码）
//! 打成 zip，给 `plugin.install` RPC 用。
//!
//! 为什么不直接提交一个 .zip：未打包的目录 diff 可读、可 review；打包是纯机械
//! 操作，测试运行时现打现用，没必要把二进制 blob 放进 git history。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// `tests/fixtures/plugins/demo-plugin/` 的绝对路径。
pub fn demo_plugin_source_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/plugins/demo-plugin")
}

/// 打包 `demo_plugin_source_dir()` 到 `out_dir/demo-plugin-<n>.zip`（`<n>` 自增，
/// 避免并发测试互相覆盖），返回 `(zip 绝对路径, sha256 hex 校验和)`。
pub fn package_demo_plugin(out_dir: &Path) -> anyhow::Result<(PathBuf, String)> {
    package_dir(&demo_plugin_source_dir(), out_dir, "demo-plugin")
}

/// 打包任意一个插件源码目录，给 `plugin.install` 用。
///
/// 提交进仓库的 fixture 走 [`package_demo_plugin`]；用例现造的包（比如一个只带
/// `[[script]]` 的包，JS 本身就是被测的东西，放在用例里比放在 fixture 目录里更
/// 好读）走这里。两条路共用同一个打包器。
pub fn package_dir(src: &Path, out_dir: &Path, name: &str) -> anyhow::Result<(PathBuf, String)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);

    anyhow::ensure!(
        src.join("plugin.toml").exists(),
        "expected plugin.toml under {}",
        src.display()
    );

    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        add_dir_to_zip(&mut writer, src, src, opts)?;
        writer.finish()?;
    }

    std::fs::create_dir_all(out_dir)?;
    let zip_path = out_dir.join(format!("{name}-{n}.zip"));
    std::fs::write(&zip_path, &buf)?;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    let checksum = hex::encode(hasher.finalize());

    Ok((zip_path, checksum))
}

fn add_dir_to_zip<W: std::io::Write + std::io::Seek>(
    writer: &mut zip::ZipWriter<W>,
    root: &Path,
    dir: &Path,
    opts: zip::write::FileOptions<'_, ()>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            add_dir_to_zip(writer, root, &path, opts)?;
        } else {
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            writer.start_file(rel, opts)?;
            let bytes = std::fs::read(&path)?;
            std::io::Write::write_all(writer, &bytes)?;
        }
    }
    Ok(())
}

/// `file://` URL for `plugin.install`'s `download_url` param — this scheme
/// doesn't require a checksum (see `crates/plugin/src/fetch.rs`), but we pass
/// one anyway since `package_demo_plugin` computes it for free and it's the
/// more realistic shape of what a real install would send.
pub fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}
