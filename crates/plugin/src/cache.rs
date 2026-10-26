//! Plugin cache — versioned storage for downloaded plugins.
//!
//! Plugins have both a global tier (`~/.agix/plugins/cache/`, shared by every
//! scene) and a scene-specific tier (`~/.agix/scenes/<scope>/plugins/cache/`)
//! — see `base::paths::ConfigPaths::{global_plugins_dir, user_plugins_dir}`.
//! This type itself is root-agnostic (caller resolves which tier's root to
//! pass in).
//!
//! Layout (either tier):
//! ```text
//! .../plugins/cache/
//! ├── {name}/
//! │   ├── {version}/
//! │   │   ├── plugin.toml
//! │   │   ├── SKILL.md (or skills/)
//! │   │   └── ... (extracted archive contents)
//! │   └── latest -> {version}/  (symlink)
//! └── registry.json  (cached index)
//! ```

use crate::manifest::PluginError;
use std::path::PathBuf;

/// Manages the plugin cache directory.
pub struct PluginCache {
    pub(crate) root: PathBuf,
}

impl PluginCache {
    /// Create a cache rooted at either the global or scene-specific plugins
    /// tier (caller resolves `root`; this type doesn't construct the path
    /// itself — see module docs).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Ensure the cache directory exists.
    pub fn ensure_dirs(&self) -> Result<(), PluginError> {
        std::fs::create_dir_all(&self.root).map_err(|e| {
            PluginError::Io(std::io::Error::other(format!(
                "failed to create cache dir: {e}"
            )))
        })?;
        Ok(())
    }

    /// Get the versioned directory for a plugin.
    pub fn version_dir(&self, name: &str, version: &str) -> PathBuf {
        self.root.join(name).join(version)
    }

    /// Check if a plugin version is already cached.
    pub fn is_cached(&self, name: &str, version: &str) -> bool {
        let dir = self.version_dir(name, version);
        dir.join("plugin.toml").is_file()
    }

    /// Cached versions of a plugin, newest first.
    ///
    /// Ordered by semver, not by string: lexicographically `0.10.0` sorts
    /// *before* `0.9.0`, so a plugin with both installed would resolve to the
    /// older one. Directory names that don't parse as semver sort last (among
    /// themselves lexicographically) rather than being dropped — an
    /// unparseable name still names a real installed directory, and silently
    /// hiding it would make `plugin.list` disagree with the filesystem.
    pub fn list_versions(&self, name: &str) -> Vec<String> {
        let dir = self.root.join(name);
        if !dir.is_dir() {
            return Vec::new();
        }
        let mut versions = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(v) = path.file_name().and_then(|s| s.to_str()) {
                        if v != "latest" {
                            versions.push(v.to_string());
                        }
                    }
                }
            }
        }
        versions.sort_by(
            |a, b| match (semver::Version::parse(a), semver::Version::parse(b)) {
                (Ok(a), Ok(b)) => b.cmp(&a),
                (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                (Err(_), Err(_)) => a.cmp(b),
            },
        );
        versions
    }

    /// Get the path for a cached plugin's extracted directory.
    /// Returns the expanded plugin manifest path.
    pub fn cached_manifest(&self, name: &str, version: &str) -> Option<PathBuf> {
        let dir = self.version_dir(name, version);
        let manifest_path = dir.join("plugin.toml");
        if manifest_path.is_file() {
            Some(manifest_path)
        } else {
            None
        }
    }

    /// Remove a cached plugin version.
    pub fn remove_version(&self, name: &str, version: &str) -> Result<(), PluginError> {
        let dir = self.version_dir(name, version);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir).map_err(|e| {
                PluginError::Io(std::io::Error::other(format!(
                    "failed to remove cached plugin: {e}"
                )))
            })?;
        }
        // If no more versions remain, remove the plugin directory
        let plugin_dir = self.root.join(name);
        if plugin_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&plugin_dir) {
                if entries.flatten().count() == 0 {
                    let _ = std::fs::remove_dir(&plugin_dir);
                }
            }
        }
        Ok(())
    }

    /// Store cached registry index.
    pub fn cache_registry_index(&self, index_json: &str) -> Result<(), PluginError> {
        let path = self.root.join("registry.json");
        std::fs::write(&path, index_json).map_err(|e| {
            PluginError::Io(std::io::Error::other(format!(
                "failed to cache registry index: {e}"
            )))
        })?;
        Ok(())
    }

    /// Load cached registry index.
    pub fn load_registry_index(&self) -> Option<String> {
        let path = self.root.join("registry.json");
        std::fs::read_to_string(&path).ok()
    }

    /// Total number of cached plugin versions.
    pub fn total_cached(&self) -> usize {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && path.file_name() != Some(std::ffi::OsStr::new("registry.json"))
                {
                    if let Ok(versions) = std::fs::read_dir(&path) {
                        count += versions
                            .flatten()
                            .filter(|e| e.path().is_dir() && e.file_name() != "latest")
                            .count();
                    }
                }
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cache_with_versions(tmp: &TempDir, name: &str, versions: &[&str]) -> PluginCache {
        let cache = PluginCache::new(tmp.path().join("plugins").join("cache"));
        for v in versions {
            std::fs::create_dir_all(cache.version_dir(name, v)).unwrap();
        }
        cache
    }

    /// Lexicographic ordering puts `0.10.0` before `0.9.0`, which would make
    /// `discover_plugins` resolve to the older of two installed versions.
    #[test]
    fn versions_are_ordered_by_semver_newest_first() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_with_versions(&tmp, "p", &["0.9.0", "0.10.0", "1.0.0"]);
        assert_eq!(cache.list_versions("p"), ["1.0.0", "0.10.0", "0.9.0"]);
    }

    #[test]
    fn prerelease_sorts_below_its_release() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_with_versions(&tmp, "p", &["1.0.0", "1.0.0-rc1"]);
        assert_eq!(cache.list_versions("p"), ["1.0.0", "1.0.0-rc1"]);
    }

    #[test]
    fn unparseable_versions_sort_last_but_are_not_dropped() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_with_versions(&tmp, "p", &["nightly", "1.0.0"]);
        assert_eq!(cache.list_versions("p"), ["1.0.0", "nightly"]);
    }

    #[test]
    fn latest_symlink_name_is_excluded() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_with_versions(&tmp, "p", &["1.0.0", "latest"]);
        assert_eq!(cache.list_versions("p"), ["1.0.0"]);
    }

    #[test]
    fn empty_cache_has_no_versions() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins").join("cache"));
        assert_eq!(cache.list_versions("test-plugin"), Vec::<String>::new());
        assert_eq!(cache.total_cached(), 0);
    }

    #[test]
    fn is_cached_detects_installed_plugin() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins").join("cache"));
        let dir = cache.version_dir("my-plugin", "1.0.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            "[plugin]\nname = \"my-plugin\"\nversion = \"1.0.0\"",
        )
        .unwrap();
        assert!(cache.is_cached("my-plugin", "1.0.0"));
        assert!(!cache.is_cached("my-plugin", "2.0.0"));
    }
}
