//! Plugin archive fetch + checksum verification + extraction — what
//! `cli.rs::install` runs.
//!
//! Supports two `download_url` schemes:
//! - `https://`/`http://` — fetched through the execution layer's
//!   `Network`, like every other outbound request. A `checksum` is
//!   **required** for network sources — installing unverified bytes from
//!   the network is exactly the supply-chain risk `homograph.rs` already
//!   guards the *name* side of; this guards the *content* side.
//! - `file://` — a local absolute path, read straight off disk. Meant for
//!   local development/sideloading (e.g. a marketplace-less demo install).
//!   `checksum` is optional here since the source is already a trusted
//!   local path, not something that travelled over the network.

use crate::cache::PluginCache;
use crate::manifest::PluginError;
use crate::marketplace::PluginSource;
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// Fetch `source`'s archive, verify its checksum (if required — see module
/// docs), extract it into `cache`'s versioned directory for `name`, and
/// return that directory.
pub async fn install_from_source(
    cache: &PluginCache,
    name: &str,
    source: &PluginSource,
) -> Result<PathBuf, PluginError> {
    // Reject a checksum-less network source *before* fetching anything —
    // no point spending a real network round-trip (which, for an
    // unreachable/bogus host, can hang for the full DNS/connect timeout)
    // on bytes we're going to refuse to install anyway.
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
    Ok(dest)
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

/// Extract a zip archive's bytes into `dest`. Rejects entries whose path
/// would escape `dest` (zip-slip) — `zip`'s `enclosed_name()` returns
/// `None` for any entry containing `..` or an absolute path, and those
/// entries are skipped with a warning rather than extracted.
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
        let dest = install_from_source(&cache, "demo", &source).await.unwrap();
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
        let dest = install_from_source(&cache, "demo", &source).await.unwrap();
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
        let err = install_from_source(&cache, "demo", &source)
            .await
            .unwrap_err();
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
        let err = install_from_source(&cache, "demo", &source)
            .await
            .unwrap_err();
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
