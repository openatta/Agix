//! 把 `examples/plugins/quickjs/`（示例插件包 `demo-quickjs` 的源码）或用例现造的
//! 插件目录打成 zip，给 `plugin.install` RPC 用。
//!
//! 为什么不直接提交一个 .zip：未打包的目录 diff 可读、可 review；打包是纯机械
//! 操作，测试运行时现打现用，没必要把二进制 blob 放进 git history。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const DEMO_PACKAGE: &str = "demo-quickjs";
pub const DEMO_PACKAGE_VERSION: &str = "1.0.0";

pub fn demo_package_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/quickjs")
}

/// 打包 `demo_package_dir()`，返回 `(zip 绝对路径, sha256 hex 校验和)`。
pub fn package_demo(out_dir: &Path) -> anyhow::Result<(PathBuf, String)> {
    package_dir(&demo_package_dir(), out_dir, DEMO_PACKAGE)
}

/// 打包任意一个插件源码目录到 `out_dir/<name>-<n>.zip`（`<n>` 自增，避免并发测试
/// 互相覆盖）。用例现造的包（JS 本身就是被测的东西，放在用例里比放在目录里好读）
/// 直接走这里。
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

/// `file://` URL for `plugin.install`'s `download_url` param. This scheme
/// doesn't require a checksum (`crates/plugin/src/install.rs`), but the
/// packers compute one anyway and a real install sends one.
pub fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}
