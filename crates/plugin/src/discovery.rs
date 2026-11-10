//! Plugin discovery — loads installed plugins from the versioned cache (see
//! `crate::cache`), applying the same override precedence as
//! skills/agents/rules: global < scene.
//!
//! Callers (e.g. `daemon::SessionPool`) pass the plain `plugins/` directory
//! for each tier (not `plugins/cache/` — this module joins `cache` itself,
//! mirroring `crate::cache::PluginCache`'s own layout doc).

use crate::cache::PluginCache;
use crate::disclosure::Disclosure;
use crate::manifest::Plugin;
use std::collections::HashMap;
use std::path::Path;

/// An installed package discovery set aside, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub name: String,
    pub version: String,
    pub reason: String,
}

/// What a scan of both tiers found.
#[derive(Debug, Default)]
pub struct Discovered {
    pub plugins: Vec<Plugin>,
    pub refused: Vec<Refused>,
}

/// Discover every plugin available to this daemon instance.
///
/// Precedence (same name wins, later tier overrides earlier):
/// `global_plugins_dir` < `scene_plugins_dir`.
///
/// Missing/empty tier directories are not an error — they simply contribute
/// nothing (matches `PluginCache`'s own tolerant-of-absence behavior).
pub fn discover_plugins(global_plugins_dir: &Path, scene_plugins_dir: &Path) -> Vec<Plugin> {
    scan_plugins(global_plugins_dir, scene_plugins_dir).plugins
}

/// [`discover_plugins`] that also says what it set aside. The cache is read
/// under its shared lock, so a package an install is still placing, or one
/// that install is about to take back out, is not seen half-done.
pub fn scan_plugins(global_plugins_dir: &Path, scene_plugins_dir: &Path) -> Discovered {
    let mut by_name: HashMap<String, Plugin> = HashMap::new();
    let mut refused = Vec::new();

    for tier_dir in [global_plugins_dir, scene_plugins_dir] {
        let cache = PluginCache::new(tier_dir.join("cache"));
        let _lock = cache.lock_shared();
        let (plugins, set_aside) = load_installed_tier(&cache);
        refused.extend(set_aside);
        for plugin in plugins {
            by_name.insert(plugin.manifest.plugin.name.clone(), plugin);
        }
    }

    let mut plugins: Vec<Plugin> = by_name.into_values().collect();
    plugins.sort_by(|a, b| a.manifest.plugin.name.cmp(&b.manifest.plugin.name));
    Discovered { plugins, refused }
}

/// Load every plugin installed in one tier's versioned cache
/// (`<tier_dir>/cache/{name}/{version}/plugin.toml`), picking the highest
/// version per name (semver order — see `PluginCache::list_versions`).
///
/// A manifest that fails to load — unsupported `api_version`, malformed
/// TOML, an event outside the subscribable set — or whose disclosure is over
/// its limits, is skipped with a warning rather than failing discovery for
/// everyone else. The limits are checked here as well as at install because
/// a package can reach the cache without going through `plugin.install`.
fn load_installed_tier(cache: &PluginCache) -> (Vec<Plugin>, Vec<Refused>) {
    let root = cache.root_path();
    let mut plugins = Vec::new();
    let mut refused = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (plugins, refused);
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
        let loaded = Plugin::load(&path.join(version), &manifest_path).and_then(|plugin| {
            Disclosure::from_plugin(&plugin)?;
            Ok(plugin)
        });
        match loaded {
            Ok(plugin) => plugins.push(plugin),
            Err(e) => {
                tracing::warn!(
                    plugin = name,
                    version = version.as_str(),
                    error = %e,
                    "failed to load installed plugin, skipping"
                );
                refused.push(Refused {
                    name: name.to_string(),
                    version: version.clone(),
                    reason: e.to_string(),
                });
            }
        }
    }
    (plugins, refused)
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
    fn a_package_over_the_disclosure_limits_is_refused_not_loaded() {
        let global = TempDir::new().unwrap();
        let scene = TempDir::new().unwrap();
        write_plugin(global.path(), "fine", "1.0.0", "");
        let long = "x".repeat(crate::disclosure::MAX_DESCRIPTION_CHARS + 1);
        write_plugin(
            global.path(),
            "wordy",
            "1.0.0",
            &format!("description = \"{long}\"\n"),
        );
        let found = scan_plugins(global.path(), scene.path());
        let names: Vec<_> = found.plugins.iter().map(|p| p.name()).collect();
        assert_eq!(names, ["fine"]);
        assert_eq!(found.refused.len(), 1);
        assert_eq!(found.refused[0].name, "wordy");
        assert!(
            found.refused[0].reason.contains("description"),
            "{:?}",
            found.refused
        );
    }

    /// A package placed in the cache by hand never went through install's
    /// checks, so its skills are read here as strictly as there.
    #[test]
    fn a_cached_package_with_a_skill_over_the_limits_is_refused() {
        let global = TempDir::new().unwrap();
        let scene = TempDir::new().unwrap();
        write_plugin(
            global.path(),
            "skilled",
            "1.0.0",
            "\n[skills]\ndir = \"skills\"\n",
        );
        let skill = global
            .path()
            .join("cache/skilled/1.0.0/skills/tour/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(
            &skill,
            format!(
                "---\ndescription: d\n---\n{}\n",
                "x".repeat(crate::disclosure::MAX_PROMPT_CHARS + 1)
            ),
        )
        .unwrap();
        let found = scan_plugins(global.path(), scene.path());
        assert!(found.plugins.is_empty());
        assert_eq!(found.refused.len(), 1);
        assert!(
            found.refused[0].reason.contains("skill `tour` body"),
            "{:?}",
            found.refused
        );
    }

    #[test]
    fn a_scan_waits_for_an_install_holding_the_cache_lock() {
        let global = TempDir::new().unwrap();
        let scene = TempDir::new().unwrap();
        write_plugin(global.path(), "p", "1.0.0", "");
        let cache = PluginCache::new(global.path().join("cache"));
        let held = cache.lock().unwrap();

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (g, s) = (global.path().to_path_buf(), scene.path().to_path_buf());
        let scanner = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            scan_plugins(&g, &s).plugins.len()
        });
        started_rx.recv().unwrap();
        cache.remove_version("p", "1.0.0").unwrap();
        drop(held);
        assert_eq!(
            scanner.join().unwrap(),
            0,
            "the scan read the cache while an uninstall held its lock"
        );
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
