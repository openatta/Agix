//! Plugin enable/disable state — persisted per plugins tier (global/scene),
//! same override precedence as everything else: scene explicit setting wins
//! over global explicit setting, and an
//! absent entry defaults to enabled (matches the pre-existing implicit
//! behavior where "installed" == "active").
//!
//! Stored as `<tier_root>/enabled.json` — a flat `{"plugin-name": bool}`
//! map, sibling to `<tier_root>/cache/`.

use crate::manifest::PluginError;
use std::collections::HashMap;
use std::path::PathBuf;

/// Enable/disable state for one plugins tier (global or scene).
pub struct EnableState {
    path: PathBuf,
}

impl EnableState {
    /// `tier_root` is the plain `plugins/` directory for a tier (same root
    /// `PluginCache::new` expects, before joining `cache`).
    pub fn new(tier_root: PathBuf) -> Self {
        Self {
            path: tier_root.join("enabled.json"),
        }
    }

    fn load(&self) -> HashMap<String, bool> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn save(&self, map: &HashMap<String, bool>) -> Result<(), PluginError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(map).map_err(|e| {
            PluginError::Schema(format!("failed to serialize plugin enable state: {e}"))
        })?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }

    /// Explicit setting for `name` in this tier, or `None` if never set
    /// (caller decides the default — see `resolve_enabled`).
    pub fn get(&self, name: &str) -> Option<bool> {
        self.load().get(name).copied()
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), PluginError> {
        let mut map = self.load();
        map.insert(name.to_string(), enabled);
        self.save(&map)
    }
}

/// Resolve whether `name` is active: scene explicit setting wins, else
/// global explicit setting, else default enabled (installed == active,
/// matching prior implicit behavior for anyone who never touches
/// enable/disable at all).
pub fn resolve_enabled(name: &str, global: &EnableState, scene: &EnableState) -> bool {
    scene.get(name).or_else(|| global.get(name)).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn set_and_get_round_trips() {
        let dir = TempDir::new().unwrap();
        let state = EnableState::new(dir.path().to_path_buf());
        assert_eq!(state.get("my-plugin"), None);
        state.set_enabled("my-plugin", false).unwrap();
        assert_eq!(state.get("my-plugin"), Some(false));
        state.set_enabled("my-plugin", true).unwrap();
        assert_eq!(state.get("my-plugin"), Some(true));
    }

    #[test]
    fn resolve_enabled_defaults_true_when_unset() {
        let global = EnableState::new(TempDir::new().unwrap().keep());
        let scene = EnableState::new(TempDir::new().unwrap().keep());
        assert!(resolve_enabled("anything", &global, &scene));
    }

    #[test]
    fn resolve_enabled_respects_global_disable() {
        let global_dir = TempDir::new().unwrap();
        let global = EnableState::new(global_dir.path().to_path_buf());
        global.set_enabled("p", false).unwrap();
        let scene = EnableState::new(TempDir::new().unwrap().keep());
        assert!(!resolve_enabled("p", &global, &scene));
    }

    #[test]
    fn resolve_enabled_scene_overrides_global() {
        let global_dir = TempDir::new().unwrap();
        let global = EnableState::new(global_dir.path().to_path_buf());
        global.set_enabled("p", false).unwrap();
        let scene_dir = TempDir::new().unwrap();
        let scene = EnableState::new(scene_dir.path().to_path_buf());
        scene.set_enabled("p", true).unwrap();
        assert!(resolve_enabled("p", &global, &scene));
    }
}
