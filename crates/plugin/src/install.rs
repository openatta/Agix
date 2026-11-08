//! Installing a package into a cache tier and taking it out again.
//!
//! An archive comes from one of two kinds of `download_url`:
//! - `https://` / `http://`, fetched through the execution layer's `Network`
//!   like every other outbound request. A checksum is required, and its
//!   absence is refused before anything is fetched.
//! - `file://`, a local absolute path read off disk, for development and
//!   sideloading; the checksum is optional.
//!
//! Zip entries with `..` or an absolute path are skipped, not extracted.

use crate::cache::PluginCache;
use crate::manifest::PluginError;
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PluginSource {
    pub download_url: String,
    /// SHA-256 of the archive, hex.
    pub checksum: Option<String>,
    pub version: String,
}

#[derive(Debug)]
pub enum Installed {
    Into(PathBuf),
    AlreadyPresent,
}

pub async fn install(
    cache: &PluginCache,
    name: &str,
    source: &PluginSource,
) -> Result<Installed, PluginError> {
    if let Some(blocked) = crate::blocklist::check_blocklist(name) {
        return Err(PluginError::Blocked(format!(
            "plugin name '{name}' is blocked (matches blocklist entry '{blocked}')"
        )));
    }
    if cache.is_cached(name, &source.version) {
        return Ok(Installed::AlreadyPresent);
    }
    let is_local = source.download_url.starts_with("file://");
    if source.checksum.is_none() && !is_local {
        return Err(PluginError::Checksum(format!(
            "plugin '{name}' has no checksum — refusing to install from a network source ({}) without one",
            source.download_url
        )));
    }

    let bytes = fetch_bytes(&source.download_url).await?;
    if let Some(expected) = &source.checksum {
        verify_checksum(&bytes, expected)?;
    }

    cache.ensure_dirs()?;
    let dest = cache.version_dir(name, &source.version);
    extract_zip(&bytes, &dest)?;
    Ok(Installed::Into(dest))
}

/// Remove one version, or every version when `version` is `None`. Returns
/// how many were removed.
pub fn uninstall(
    cache: &PluginCache,
    name: &str,
    version: Option<&str>,
) -> Result<usize, PluginError> {
    let versions = match version {
        Some(v) => vec![v.to_string()],
        None => cache.list_versions(name),
    };
    for v in &versions {
        cache.remove_version(name, v)?;
    }
    Ok(versions.len())
}

async fn fetch_bytes(download_url: &str) -> Result<Vec<u8>, PluginError> {
    if let Some(path) = download_url.strip_prefix("file://") {
        return std::fs::read(path).map_err(PluginError::Io);
    }
    // `Origin::Operator`: a plugin archive comes from a marketplace an
    // operator configured, not from a url the model named.
    let net = base::interface::exec::local::LocalNetwork::default();
    let resp = base::interface::exec::Network::send(
        &net,
        base::interface::exec::HttpRequest::get(download_url),
        base::interface::exec::Origin::Operator,
    )
    .await
    .map_err(|e| PluginError::Io(std::io::Error::other(format!("download failed: {e}"))))?;
    if !(200..300).contains(&resp.status) {
        return Err(PluginError::Schema(format!(
            "download failed: HTTP {} from {download_url}",
            resp.status
        )));
    }
    Ok(resp.body)
}

fn verify_checksum(bytes: &[u8], expected_hex: &str) -> Result<(), PluginError> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_hex) {
        return Err(PluginError::Checksum(format!(
            "checksum mismatch — expected {expected_hex}, got {actual}"
        )));
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), PluginError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| PluginError::Schema(format!("invalid plugin archive: {e}")))?;
    std::fs::create_dir_all(dest)?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| PluginError::Schema(format!("invalid plugin archive entry: {e}")))?;
        let Some(relative) = entry.enclosed_name() else {
            tracing::warn!(name = %entry.name(), "plugin archive entry has an unsafe path, skipping");
            continue;
        };
        let out_path = dest.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out_file = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out_file)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            for (path, content) in entries {
                writer.start_file(*path, opts).unwrap();
                writer.write_all(content.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buf
    }

    #[tokio::test]
    async fn installs_from_file_url_without_checksum() {
        let src_dir = TempDir::new().unwrap();
        let archive = zip_bytes(&[(
            "plugin.toml",
            "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\n",
        )]);
        let archive_path = src_dir.path().join("demo.zip");
        std::fs::write(&archive_path, &archive).unwrap();

        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().to_path_buf());
        let source = PluginSource {
            download_url: format!("file://{}", archive_path.display()),
            checksum: None,
            version: "1.0.0".into(),
        };
        let Installed::Into(dest) = install(&cache, "demo", &source).await.unwrap() else {
            panic!("an empty cache already had the package");
        };
        assert!(dest.join("plugin.toml").is_file());
    }

    #[tokio::test]
    async fn installs_from_file_url_with_correct_checksum() {
        let src_dir = TempDir::new().unwrap();
        let archive = zip_bytes(&[(
            "plugin.toml",
            "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\n",
        )]);
        let archive_path = src_dir.path().join("demo.zip");
        std::fs::write(&archive_path, &archive).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&archive);
        let checksum = hex::encode(hasher.finalize());

        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().to_path_buf());
        let source = PluginSource {
            download_url: format!("file://{}", archive_path.display()),
            checksum: Some(checksum),
            version: "1.0.0".into(),
        };
        let Installed::Into(dest) = install(&cache, "demo", &source).await.unwrap() else {
            panic!("an empty cache already had the package");
        };
        assert!(dest.join("plugin.toml").is_file());
    }

    #[tokio::test]
    async fn rejects_mismatched_checksum() {
        let src_dir = TempDir::new().unwrap();
        let archive = zip_bytes(&[(
            "plugin.toml",
            "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\n",
        )]);
        let archive_path = src_dir.path().join("demo.zip");
        std::fs::write(&archive_path, &archive).unwrap();

        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().to_path_buf());
        let source = PluginSource {
            download_url: format!("file://{}", archive_path.display()),
            checksum: Some("0".repeat(64)),
            version: "1.0.0".into(),
        };
        let err = install(&cache, "demo", &source).await.unwrap_err();
        assert!(matches!(err, PluginError::Checksum(_)));
    }

    #[tokio::test]
    async fn rejects_network_source_without_checksum() {
        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().to_path_buf());
        let source = PluginSource {
            download_url: "https://example.invalid/demo.zip".into(),
            checksum: None,
            version: "1.0.0".into(),
        };
        let err = install(&cache, "demo", &source).await.unwrap_err();
        assert!(matches!(err, PluginError::Checksum(_)));
    }

    #[test]
    fn extract_zip_skips_path_traversal_entries() {
        let archive = zip_bytes(&[("../../etc/passwd", "pwned")]);
        let dest = TempDir::new().unwrap();
        extract_zip(&archive, dest.path()).unwrap();
        // Nothing should have escaped `dest` — the traversal entry is
        // skipped, not extracted anywhere (including inside `dest` itself,
        // since zip's `enclosed_name()` rejects any `..` component).
        let mut found_any_file = false;
        for entry in walkdir_files(dest.path()) {
            found_any_file = true;
            let _ = entry;
        }
        assert!(!found_any_file, "no files should have been extracted");
    }

    #[test]
    fn uninstall_without_a_version_removes_every_version() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("cache"));
        for v in ["1.0.0", "2.0.0"] {
            let dir = cache.version_dir("p", v);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("plugin.toml"), "").unwrap();
        }
        assert_eq!(uninstall(&cache, "p", None).unwrap(), 2);
        assert!(cache.list_versions("p").is_empty());
    }

    #[tokio::test]
    async fn a_blocked_name_is_refused_before_anything_is_fetched() {
        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().to_path_buf());
        let source = PluginSource {
            download_url: "https://example.invalid/x.zip".into(),
            checksum: Some("0".repeat(64)),
            version: "1.0.0".into(),
        };
        let err = install(&cache, "\u{0435}vil-plugin", &source)
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Blocked(_)));
    }

    fn walkdir_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    out.extend(walkdir_files(&path));
                } else {
                    out.push(path);
                }
            }
        }
        out
    }
}
