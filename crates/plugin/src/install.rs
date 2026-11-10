//! Installing a package into a cache tier and taking it out again.
//!
//! An archive comes from one of two kinds of `download_url`:
//! - `https://` / `http://`, fetched through the execution layer's `Network`
//!   like every other outbound request. A checksum is required, and its
//!   absence is refused before anything is fetched.
//! - `file://`, a local absolute path read off disk, for development and
//!   sideloading; the checksum is optional.
//!
//! An archive is bounded on the way in ([`MAX_ARCHIVE_BYTES`]) and while it
//! unpacks ([`UnpackLimits`]). It unpacks into a staging directory beside its
//! final one and is renamed into place only once complete, under the cache's
//! lock, so a version directory is either a finished install or absent. The
//! staging directory is also where the package is read back the way discovery
//! will read it and its disclosure is checked, so a package nothing would load
//! never reaches the cache. The archive's SHA-256 goes into
//! [`INSTALLED_MARKER`]; a later install that names a different checksum for
//! the same version treats what is cached as not the package asked for and
//! replaces it. Zip
//! entries with `..` or an absolute path are skipped, not extracted, and a
//! `name` or `version` that is not one plain cache directory name is refused
//! before anything is read or removed (see
//! [`check_component`](crate::cache::check_component)).

use crate::cache::{PluginCache, INSTALLED_MARKER};
use crate::manifest::PluginError;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

/// Largest archive `install` will download or read.
pub const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;

/// What one archive may unpack to.
#[derive(Debug, Clone, Copy)]
pub struct UnpackLimits {
    pub max_total_bytes: u64,
    pub max_entries: usize,
    /// Largest uncompressed/compressed ratio for an entry bigger than
    /// [`UnpackLimits::ratio_floor_bytes`].
    pub max_ratio: u64,
    pub ratio_floor_bytes: u64,
}

impl Default for UnpackLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: 512 * 1024 * 1024,
            max_entries: 10_000,
            max_ratio: 200,
            ratio_floor_bytes: 1024 * 1024,
        }
    }
}

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
    crate::cache::check_component("name", name)?;
    crate::cache::check_component("version", &source.version)?;
    if cache.is_cached(name, &source.version)
        && marker_agrees(cache, name, &source.version, source.checksum.as_deref())
    {
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
    let sha256 = sha256_hex(&bytes);
    if let Some(expected) = &source.checksum {
        verify_checksum(&sha256, expected)?;
    }

    let root = cache.root_path().to_path_buf();
    let name = name.to_string();
    let version = source.version.clone();
    let expected = source.checksum.clone();
    tokio::task::spawn_blocking(move || {
        unpack_into(
            &PluginCache::new(root),
            &name,
            &version,
            &bytes,
            &sha256,
            expected.as_deref(),
            &UnpackLimits::default(),
        )
    })
    .await
    .map_err(|e| PluginError::Io(std::io::Error::other(format!("install task failed: {e}"))))?
}

/// Remove one version, or every version when `version` is `None`. Returns
/// how many were removed.
pub fn uninstall(
    cache: &PluginCache,
    name: &str,
    version: Option<&str>,
) -> Result<usize, PluginError> {
    crate::cache::check_component("name", name)?;
    if let Some(v) = version {
        crate::cache::check_component("version", v)?;
    }
    let _lock = match cache.root_path().is_dir() {
        true => Some(cache.lock()?),
        false => None,
    };
    let versions = match version {
        Some(v) => vec![v.to_string()],
        None => cache.list_versions(name),
    };
    for v in &versions {
        cache.remove_version(name, v)?;
    }
    Ok(versions.len())
}

/// A marker with no recorded hash predates this check and cannot disagree.
fn marker_agrees(cache: &PluginCache, name: &str, version: &str, expected: Option<&str>) -> bool {
    match (expected, cache.installed_sha256(name, version)) {
        (Some(expected), Some(recorded)) => recorded.eq_ignore_ascii_case(expected),
        _ => true,
    }
}

fn unpack_into(
    cache: &PluginCache,
    name: &str,
    version: &str,
    bytes: &[u8],
    sha256: &str,
    expected: Option<&str>,
    limits: &UnpackLimits,
) -> Result<Installed, PluginError> {
    let _lock = cache.lock()?;
    if cache.is_cached(name, version) && marker_agrees(cache, name, version, expected) {
        return Ok(Installed::AlreadyPresent);
    }
    let plugin_dir = cache.plugin_dir(name)?;
    std::fs::create_dir_all(&plugin_dir)?;
    let staging = plugin_dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let dest = cache.version_dir(name, version);
    let placed = extract_zip(bytes, &staging, limits)
        .and_then(|()| check_staged(&staging, name))
        .and_then(|()| Ok(std::fs::write(staging.join(INSTALLED_MARKER), sha256)?))
        .and_then(|()| {
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            Ok(std::fs::rename(&staging, &dest)?)
        });
    if let Err(e) = placed {
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir(&plugin_dir);
        return Err(e);
    }
    Ok(Installed::Into(dest))
}

async fn fetch_bytes(download_url: &str) -> Result<Vec<u8>, PluginError> {
    if let Some(path) = download_url.strip_prefix("file://") {
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(MAX_ARCHIVE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
            return Err(too_large(MAX_ARCHIVE_BYTES));
        }
        return Ok(bytes);
    }
    // `Origin::Operator`: a plugin archive comes from a marketplace an
    // operator configured, not from a url the model named.
    let net = platform::LocalNetwork::default();
    let mut request = base::interface::exec::HttpRequest::get(download_url);
    request.max_bytes = Some(MAX_ARCHIVE_BYTES);
    let resp = base::interface::exec::Network::send(
        &net,
        request,
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

fn too_large(limit: u64) -> PluginError {
    PluginError::Schema(format!(
        "plugin archive is larger than the {limit} bytes allowed"
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn check_staged(staging: &Path, name: &str) -> Result<(), PluginError> {
    let plugin = crate::manifest::Plugin::load(staging, &staging.join("plugin.toml"))?;
    if plugin.name() != name {
        return Err(PluginError::Schema(format!(
            "the package calls itself `{}`, not `{name}`",
            plugin.name()
        )));
    }
    crate::Disclosure::from_plugin(&plugin).map(|_| ())
}

fn verify_checksum(actual: &str, expected_hex: &str) -> Result<(), PluginError> {
    if !actual.eq_ignore_ascii_case(expected_hex) {
        return Err(PluginError::Checksum(format!(
            "checksum mismatch — expected {expected_hex}, got {actual}"
        )));
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path, limits: &UnpackLimits) -> Result<(), PluginError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| PluginError::Schema(format!("invalid plugin archive: {e}")))?;
    if archive.len() > limits.max_entries {
        return Err(PluginError::Schema(format!(
            "plugin archive has {} entries, over the {} allowed",
            archive.len(),
            limits.max_entries
        )));
    }
    std::fs::create_dir_all(dest)?;
    let mut remaining = limits.max_total_bytes;
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
        if entry.size() > limits.ratio_floor_bytes
            && entry.size() / entry.compressed_size().max(1) > limits.max_ratio
        {
            return Err(PluginError::Schema(format!(
                "plugin archive entry `{}` expands {}x, over the {}x allowed",
                entry.name(),
                entry.size() / entry.compressed_size().max(1),
                limits.max_ratio
            )));
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out_file = std::fs::File::create(&out_path)?;
        let written = std::io::copy(&mut (&mut entry).take(remaining + 1), &mut out_file)?;
        if written > remaining {
            return Err(PluginError::Schema(format!(
                "plugin archive unpacks to more than the {} bytes allowed",
                limits.max_total_bytes
            )));
        }
        remaining -= written;
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
            "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n",
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
            "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n",
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
            "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n",
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
        extract_zip(&archive, dest.path(), &UnpackLimits::default()).unwrap();
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
    async fn a_name_or_version_that_is_not_one_directory_is_refused() {
        let tmp = TempDir::new().unwrap();
        let archive = zip_bytes(&[("plugin.toml", "[plugin]\nname = \"x\"\n")]);
        let archive_path = tmp.path().join("x.zip");
        std::fs::write(&archive_path, &archive).unwrap();
        let cache = PluginCache::new(tmp.path().join("cache"));
        for (name, version) in [("../out", "1.0.0"), ("..", "out"), ("x", "../../out")] {
            let source = PluginSource {
                download_url: format!("file://{}", archive_path.display()),
                checksum: None,
                version: version.into(),
            };
            assert!(install(&cache, name, &source).await.is_err());
            assert!(uninstall(&cache, name, Some(version)).is_err());
        }
        assert!(uninstall(&cache, "..", None).is_err());
        assert!(!tmp.path().join("out").exists());
        assert!(!tmp.path().join("cache").exists());
    }

    #[test]
    fn an_archive_over_the_unpack_limits_is_refused_and_leaves_nothing() {
        let loose = UnpackLimits::default();
        let big = "x".repeat(600);
        for (limits, says, entries) in [
            (
                UnpackLimits {
                    max_total_bytes: 1000,
                    ..loose
                },
                "unpacks to more than",
                vec![("a", big.as_str()), ("b", big.as_str())],
            ),
            (
                UnpackLimits {
                    max_entries: 3,
                    ..loose
                },
                "entries, over",
                vec![("a", ""), ("b", ""), ("c", ""), ("d", "")],
            ),
            (
                UnpackLimits {
                    max_ratio: 10,
                    ratio_floor_bytes: 100,
                    ..loose
                },
                "expands",
                vec![("a", big.as_str())],
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let cache = PluginCache::new(tmp.path().join("cache"));
            let mut entries = entries;
            entries.push(("plugin.toml", "[plugin]\nname = \"p\"\n"));
            let err = unpack_into(
                &cache,
                "p",
                "1.0.0",
                &zip_bytes(&entries),
                "",
                None,
                &limits,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains(says), "{says}: {err}");
            assert!(!cache.is_cached("p", "1.0.0"), "{says}");
            assert_eq!(
                walkdir_files(&tmp.path().join("cache").join("p")),
                Vec::<PathBuf>::new(),
                "{says}: the staging directory is cleaned up"
            );
        }
    }

    /// A directory left by an install that stopped part way is not an
    /// install: the next one replaces it rather than answering that the
    /// package is already there.
    #[tokio::test]
    async fn a_half_written_version_directory_is_replaced_not_trusted() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("cache"));
        let dir = cache.version_dir("demo", "1.0.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), "truncated").unwrap();
        let archive = zip_bytes(&[(
            "plugin.toml",
            "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n",
        )]);
        let archive_path = tmp.path().join("demo.zip");
        std::fs::write(&archive_path, &archive).unwrap();
        let source = PluginSource {
            download_url: format!("file://{}", archive_path.display()),
            checksum: None,
            version: "1.0.0".into(),
        };
        let Installed::Into(dest) = install(&cache, "demo", &source).await.unwrap() else {
            panic!("an unfinished directory was taken for an install");
        };
        assert!(std::fs::read_to_string(dest.join("plugin.toml"))
            .unwrap()
            .contains("demo"));
        assert!(cache.is_cached("demo", "1.0.0"));
        assert!(matches!(
            install(&cache, "demo", &source).await.unwrap(),
            Installed::AlreadyPresent
        ));
    }

    fn file_source(dir: &Path, archive: &[u8], checksum: Option<String>) -> PluginSource {
        let path = dir.join("demo.zip");
        std::fs::write(&path, archive).unwrap();
        PluginSource {
            download_url: format!("file://{}", path.display()),
            checksum,
            version: "1.0.0".into(),
        }
    }

    const DEMO_MANIFEST: &str =
        "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    #[tokio::test]
    async fn a_cached_package_that_is_not_the_archive_asked_for_is_replaced() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("cache"));
        let original = zip_bytes(&[("plugin.toml", DEMO_MANIFEST), ("tool.js", "good")]);
        let original_sum = sha256_hex(&original);
        let source = file_source(tmp.path(), &original, Some(original_sum.clone()));
        assert!(matches!(
            install(&cache, "demo", &source).await.unwrap(),
            Installed::Into(_)
        ));
        assert_eq!(
            cache.installed_sha256("demo", "1.0.0").as_deref(),
            Some(original_sum.as_str())
        );
        assert!(matches!(
            install(&cache, "demo", &source).await.unwrap(),
            Installed::AlreadyPresent
        ));

        let other = zip_bytes(&[("plugin.toml", DEMO_MANIFEST), ("tool.js", "different")]);
        let other_source = file_source(tmp.path(), &other, Some(sha256_hex(&other)));
        let Installed::Into(dest) = install(&cache, "demo", &other_source).await.unwrap() else {
            panic!("a different checksum for the same version was taken for what is cached");
        };
        assert_eq!(
            std::fs::read_to_string(dest.join("tool.js")).unwrap(),
            "different"
        );
    }

    #[tokio::test]
    async fn a_package_the_manifest_check_refuses_never_reaches_the_cache() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("cache"));
        let long = "x".repeat(crate::disclosure::MAX_DESCRIPTION_CHARS + 1);
        let manifest = format!("{DEMO_MANIFEST}description = \"{long}\"\n");
        let archive = zip_bytes(&[("plugin.toml", manifest.as_str())]);
        let source = file_source(tmp.path(), &archive, None);
        assert!(install(&cache, "demo", &source).await.is_err());
        assert!(!cache.version_dir("demo", "1.0.0").exists());
        assert!(walkdir_files(&tmp.path().join("cache").join("demo")).is_empty());
    }

    #[tokio::test]
    async fn one_skill_with_bad_frontmatter_keeps_the_whole_package_out() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("cache"));
        let manifest = format!("{DEMO_MANIFEST}\n[skills]\ndir = \"skills\"\n");
        let archive = zip_bytes(&[
            ("plugin.toml", manifest.as_str()),
            (
                "skills/good/SKILL.md",
                "---\ndescription: fine\n---\nbody\n",
            ),
            (
                "skills/bad/SKILL.md",
                "---\ndescription: d\nuser_invocable: sometimes\n---\nbody\n",
            ),
        ]);
        let source = file_source(tmp.path(), &archive, None);
        let err = install(&cache, "demo", &source)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("skill `bad`") && err.contains("`user_invocable`"),
            "{err}"
        );
        assert!(!cache.version_dir("demo", "1.0.0").exists());
        assert!(walkdir_files(&tmp.path().join("cache").join("demo")).is_empty());
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
