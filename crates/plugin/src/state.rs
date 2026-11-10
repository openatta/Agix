//! Plugin enable/disable state — persisted per plugins tier (global/scene),
//! same override precedence as everything else: scene explicit setting wins
//! over global explicit setting, and an absent entry defaults to enabled.
//!
//! Stored as `<tier_root>/enabled.json` — a flat `{"plugin-name": bool}`
//! map, sibling to `<tier_root>/cache/`. This file is the only off switch a
//! package has, so a tier whose file exists but cannot be read or parsed
//! counts as disabling everything rather than as having no entries, and is
//! never overwritten by [`EnableState::set_enabled`].

use crate::manifest::PluginError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<HashMap<String, bool>, PluginError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => {
                return Err(PluginError::Io(std::io::Error::new(
                    e.kind(),
                    format!("cannot read {}: {e}", self.path.display()),
                )))
            }
        };
        serde_json::from_str(&raw).map_err(|e| {
            PluginError::Schema(format!(
                "{} is not a {{\"name\": bool}} map: {e}",
                self.path.display()
            ))
        })
    }

    fn save(&self, map: &HashMap<String, bool>) -> Result<(), PluginError> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let parent = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let json = serde_json::to_string_pretty(map).map_err(|e| {
            PluginError::Schema(format!("failed to serialize plugin enable state: {e}"))
        })?;
        let tmp = parent.join(format!(
            ".enabled.json.{}-{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let written = std::fs::write(&tmp, json)
            .and_then(|()| std::fs::File::open(&tmp)?.sync_all())
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(written?)
    }

    /// Why this tier's file cannot be trusted, or `None` when it is absent
    /// or well-formed.
    pub fn fault(&self) -> Option<String> {
        self.load().err().map(|e| e.to_string())
    }

    /// Explicit setting for `name` in this tier, or `None` if never set
    /// (caller decides the default — see `resolve_enabled`). A file that
    /// cannot be read answers `Some(false)` for every name.
    pub fn get(&self, name: &str) -> Option<bool> {
        match self.load() {
            Ok(map) => map.get(name).copied(),
            Err(_) => Some(false),
        }
    }

    /// Refuses, rather than starting a fresh map, when the existing file
    /// cannot be read: the entries in it are what keeps other packages off.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), PluginError> {
        let mut map = self.load().map_err(|e| {
            PluginError::Schema(format!(
                "not changing the plugin enable state: {e}. Fix or remove the file by hand; \
                 until then every plugin in this tier stays disabled"
            ))
        })?;
        map.insert(name.to_string(), enabled);
        self.save(&map)
    }

    /// Drop `name`'s entry, so it falls back to the default again.
    pub fn clear(&self, name: &str) -> Result<(), PluginError> {
        let mut map = self.load()?;
        if map.remove(name).is_some() {
            self.save(&map)?;
        }
        Ok(())
    }
}

/// Resolve whether `name` is active: scene explicit setting wins, else
/// global explicit setting, else enabled.
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
        assert_eq!(state.fault(), None);
    }

    #[test]
    fn clear_returns_a_name_to_the_default() {
        let dir = TempDir::new().unwrap();
        let state = EnableState::new(dir.path().to_path_buf());
        state.set_enabled("p", false).unwrap();
        state.set_enabled("q", false).unwrap();
        state.clear("p").unwrap();
        assert_eq!(state.get("p"), None);
        assert_eq!(state.get("q"), Some(false));
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

    #[test]
    fn an_unreadable_enable_state_does_not_enable_what_it_disabled() {
        let global_dir = TempDir::new().unwrap();
        let global = EnableState::new(global_dir.path().to_path_buf());
        global.set_enabled("off", false).unwrap();
        let scene = EnableState::new(TempDir::new().unwrap().keep());

        let path = global.path().to_path_buf();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.truncate(raw.len() / 2);
        std::fs::write(&path, &raw).unwrap();

        assert!(!resolve_enabled("off", &global, &scene));
        assert!(
            !resolve_enabled("never-mentioned", &global, &scene),
            "a tier that cannot be read keeps everything in it off"
        );
        assert!(global.fault().is_some_and(|f| f.contains("enabled.json")));

        let err = global.set_enabled("other", true).unwrap_err().to_string();
        assert!(err.contains("Fix or remove"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            raw,
            "the damaged file is left for a person to look at, not overwritten"
        );
    }

    #[test]
    fn a_save_leaves_no_temporary_file_behind() {
        let dir = TempDir::new().unwrap();
        let state = EnableState::new(dir.path().to_path_buf());
        state.set_enabled("p", false).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["enabled.json"]);
    }
}
