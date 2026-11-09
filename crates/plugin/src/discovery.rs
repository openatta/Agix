//! Plugin discovery — loads installed plugins from the versioned cache (see
//! `crate::cache`), applying the same override precedence as
//! skills/agents/rules: global < scene.
//!
//! Callers (e.g. `daemon::SessionPool`) pass the plain `plugins/` directory
//! for each tier (not `plugins/cache/` — this module joins `cache` itself,
//! mirroring `crate::cache::PluginCache`'s own layout doc).

use crate::cache::PluginCache;
use crate::manifest::Plugin;
use std::collections::HashMap;
use std::path::Path;

/// Discover every plugin available to this daemon instance.
///
/// Precedence (same name wins, later tier overrides earlier):
/// `global_plugins_dir` < `scene_plugins_dir`.
///
/// Missing/empty tier directories are not an error — they simply contribute
/// nothing (matches `PluginCache`'s own tolerant-of-absence behavior).
pub fn discover_plugins(global_plugins_dir: &Path, scene_plugins_dir: &Path) -> Vec<Plugin> {
    let mut by_name: HashMap<String, Plugin> = HashMap::new();

    for tier_dir in [global_plugins_dir, scene_plugins_dir] {
        for plugin in load_installed_tier(tier_dir) {
            by_name.insert(plugin.manifest.plugin.name.clone(), plugin);
        }
    }

    let mut plugins: Vec<Plugin> = by_name.into_values().collect();
    plugins.sort_by(|a, b| a.manifest.plugin.name.cmp(&b.manifest.plugin.name));
    plugins
}

/// Load every plugin installed in one tier's versioned cache
/// (`<tier_dir>/cache/{name}/{version}/plugin.toml`), picking the highest
/// version per name (semver order — see `PluginCache::list_versions`).
///
/// A manifest that fails to load — unsupported `api_version`, malformed
/// TOML, an event outside the subscribable set — is skipped with a warning
/// rather than failing discovery for everyone else.
fn load_installed_tier(tier_dir: &Path) -> Vec<Plugin> {
    let cache = PluginCache::new(tier_dir.join("cache"));
    let root = cache.root_path();
    let mut plugins = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return plugins;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let versions = cache.list_versions(name);
        let Some(version) = versions.first() else {
            continue;
        };
        let Some(manifest_path) = cache.cached_manifest(name, version) else {
            continue;
        };
        match Plugin::load(&path.join(version), &manifest_path) {
            Ok(plugin) => plugins.push(plugin),
            Err(e) => {
                tracing::warn!(
                    plugin = name,
                    version = version.as_str(),
                    error = %e,
                    "failed to load installed plugin, skipping"
                );
            }
        }
    }
    plugins
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_plugin(tier_root: &Path, name: &str, version: &str, extra_toml: &str) {
        let dir = tier_root.join("cache").join(name).join(version);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            format!(
                "[plugin]\nname = \"{name}\"\nversion = \"{version}\"\napi_version = \"0.1\"\n{extra_toml}"
            ),
        )
        .unwrap();
    }

    #[test]
    fn empty_tiers_discover_nothing() {
        let global = TempDir::new().unwrap();
        let scene = TempDir::new().unwrap();
        assert!(discover_plugins(global.path(), scene.path()).is_empty());
    }

    #[test]
    fn global_tier_plugin_is_discovered() {
        let global = TempDir::new().unwrap();
        let scene = TempDir::new().unwrap();
        write_plugin(global.path(), "my-plugin", "1.0.0", "");
        let plugins = discover_plugins(global.path(), scene.path());
        assert!(plugins
            .iter()
            .any(|p| p.manifest.plugin.name == "my-plugin"));
    }

    #[test]
    fn scene_tier_overrides_global_tier_same_name() {
        let global = TempDir::new().unwrap();
        let scene = TempDir::new().unwrap();
        write_plugin(
            global.path(),
            "my-plugin",
            "1.0.0",
            "description = \"global\"\n",
        );
        write_plugin(
            scene.path(),
            "my-plugin",
            "1.0.0",
            "description = \"scene\"\n",
        );
        let plugins = discover_plugins(global.path(), scene.path());
        let found: Vec<_> = plugins
            .iter()
            .filter(|p| p.manifest.plugin.name == "my-plugin")
            .collect();
        assert_eq!(found.len(), 1, "same-name plugin must not appear twice");
        assert_eq!(found[0].manifest.plugin.description, "scene");
    }

    #[test]
    fn distinct_names_across_tiers_all_appear() {
        let global = TempDir::new().unwrap();
        let scene = TempDir::new().unwrap();
        write_plugin(global.path(), "from-global", "1.0.0", "");
        write_plugin(scene.path(), "from-scene", "1.0.0", "");
        let names: Vec<String> = discover_plugins(global.path(), scene.path())
            .into_iter()
            .map(|p| p.manifest.plugin.name)
            .collect();
        assert_eq!(names, ["from-global", "from-scene"]);
    }

    #[test]
    fn highest_version_is_picked_when_multiple_installed() {
        let global = TempDir::new().unwrap();
        let scene = TempDir::new().unwrap();
        write_plugin(global.path(), "my-plugin", "1.0.0", "");
        write_plugin(global.path(), "my-plugin", "2.0.0", "");
        let plugins = discover_plugins(global.path(), scene.path());
        let found = plugins
            .iter()
            .find(|p| p.manifest.plugin.name == "my-plugin")
            .unwrap();
        assert_eq!(found.manifest.plugin.version, "2.0.0");
    }
}
