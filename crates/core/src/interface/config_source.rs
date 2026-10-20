//! `ConfigSource` — where the settings layers come from.
//!
//! `Settings::load` merges three tiers, each with a gitignored overlay beside
//! it, and it read all six with `std::fs::read_to_string`. That made the file
//! system part of the definition of "configuration": a deployment that keeps
//! its settings in a config service, a Kubernetes ConfigMap, or a database had
//! no way in that did not involve writing six files to disk first so the
//! engine could read them back.
//!
//! What moves is only *where the layers come from*. The merge — six layers
//! low-to-high, generic recursive JSON merge, `paths` stripped from every
//! layer, `permission_rules` from an overlay held apart — stays exactly where
//! it was, in [`crate::settings::Settings::load_from`]. A source hands over
//! JSON values in priority order and has no say in what happens to them,
//! which is what keeps "settings come from somewhere else" from turning into
//! "settings mean something else".
//!
//! # A source reports its own failures and yields nothing
//!
//! `Settings::load` never fails: a layer that is absent is skipped, and one
//! that will not parse is skipped with a warning rather than taking startup
//! down with it. That property belongs to the loader and cannot be delegated,
//! so [`ConfigSource::layers`] returns layers rather than a `Result` — a
//! source that cannot reach its backing store logs why and returns what it
//! has. Losing a layer is bad; refusing to start because a remote service is
//! slow is worse.
//!
//! # Synchronous, and a remote source is still a fit
//!
//! Configuration is read once, before anything is built, and every caller of
//! `Settings::load` today is synchronous. A source that has to go over a
//! network fetches in [`ConfigSource::layers`] — it is called once per process
//! and is allowed to be slow — or fetches ahead of time and hands the result
//! over as [`InMemoryLayers`]. [`Chain`] is for the common shape of the second
//! case: the files on disk, with what the control plane says layered on top.

use crate::settings::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Which of the three tiers a layer belongs to, lowest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Shared by every scene on this machine.
    Global,
    /// This product instance.
    Scene,
    /// This project.
    Project,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Scene => "scene",
            Self::Project => "project",
        }
    }
}

/// One layer of configuration, already parsed.
#[derive(Debug, Clone)]
pub struct ConfigLayer {
    pub tier: Tier,
    /// Whether this is the per-machine overlay for its tier rather than the
    /// shared settings.
    ///
    /// The merge treats the two identically but for one rule: a
    /// `permission_rules` array in an overlay is held apart as
    /// `local_permission_rules` instead of being merged into the shared
    /// rules, because a rule nobody else on the team can see is not the same
    /// kind of statement as one committed alongside the code.
    pub machine_local: bool,
    /// Where this layer came from, as something a person can act on — a path,
    /// a URL, a config-map key. Only ever used in diagnostics.
    pub origin: String,
    pub value: serde_json::Value,
}

/// Where the settings layers come from.
pub trait ConfigSource: Send + Sync {
    /// Every layer this source has, lowest priority first.
    fn layers(&self) -> Vec<ConfigLayer>;
}

/// The three directories on disk, each with its overlay — what the engine has
/// always done.
pub struct FileTiers {
    global_dir: PathBuf,
    scene_dir: PathBuf,
    local_dir: PathBuf,
}

impl FileTiers {
    pub fn new(global_dir: PathBuf, scene_dir: PathBuf, local_dir: PathBuf) -> Self {
        Self {
            global_dir,
            scene_dir,
            local_dir,
        }
    }
}

impl ConfigSource for FileTiers {
    fn layers(&self) -> Vec<ConfigLayer> {
        let tiers = [
            (Tier::Global, &self.global_dir),
            (Tier::Scene, &self.scene_dir),
            (Tier::Project, &self.local_dir),
        ];
        let mut layers = Vec::new();
        for (i, (tier, dir)) in tiers.iter().enumerate() {
            // Two tiers can be one directory — a project rooted at `$HOME`
            // shares `~/.atta` with the global tier. Read it once, at the
            // higher tier: a merge applying it twice changes nothing, but
            // `permission_rules` are concatenated and would be doubled.
            if tiers[i + 1..].iter().any(|(_, later)| later == dir) {
                continue;
            }
            let tier = *tier;
            for (file, machine_local) in [(SETTINGS_FILE, false), (SETTINGS_LOCAL_FILE, true)] {
                let path = dir.join(file);
                if let Some(value) = read_settings_file(tier, &path) {
                    layers.push(ConfigLayer {
                        tier,
                        machine_local,
                        origin: path.display().to_string(),
                        value,
                    });
                }
            }
        }
        layers
    }
}

/// Read and parse one settings file. `None` when it doesn't exist, can't be
/// read, or doesn't parse — each of the latter two warns rather than
/// aborting, so one broken file can't stop the process from starting.
fn read_settings_file(tier: Tier, path: &Path) -> Option<serde_json::Value> {
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(layer = tier.name(), path = %path.display(), error = %e, "failed to read settings file, skipping this layer");
            return None;
        }
    };
    match serde_json::from_str(&content) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(layer = tier.name(), path = %path.display(), error = %e, "failed to parse settings file, skipping this layer");
            None
        }
    }
}

/// Layers the host already has in hand.
///
/// This is what a deployment that keeps configuration somewhere other than a
/// disk actually uses: fetch from the config service, the ConfigMap or the
/// database, and hand the JSON over. It is also the whole of a test that wants
/// a settings tree without a temporary directory.
pub struct InMemoryLayers(pub Vec<ConfigLayer>);

impl InMemoryLayers {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Append a layer above everything added so far.
    pub fn with(
        mut self,
        tier: Tier,
        machine_local: bool,
        origin: impl Into<String>,
        value: serde_json::Value,
    ) -> Self {
        self.0.push(ConfigLayer {
            tier,
            machine_local,
            origin: origin.into(),
            value,
        });
        self
    }
}

impl Default for InMemoryLayers {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigSource for InMemoryLayers {
    fn layers(&self) -> Vec<ConfigLayer> {
        self.0.clone()
    }
}

/// Several sources end to end, each outranking the one before it.
///
/// The shape a remote deployment wants: `Chain(vec![files, from_control_plane])`
/// keeps whatever is on the machine and lets the fleet-wide answer win.
pub struct Chain(pub Vec<Arc<dyn ConfigSource>>);

impl ConfigSource for Chain {
    fn layers(&self) -> Vec<ConfigLayer> {
        self.0.iter().flat_map(|s| s.layers()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tier_yields_its_shared_file_below_its_overlay() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), r#"{"a":1}"#).unwrap();
        std::fs::write(dir.path().join(SETTINGS_LOCAL_FILE), r#"{"a":2}"#).unwrap();
        let source = FileTiers::new(
            dir.path().to_path_buf(),
            dir.path().join("absent"),
            dir.path().join("absent"),
        );

        let layers = source.layers();
        assert_eq!(layers.len(), 2);
        assert!(!layers[0].machine_local);
        assert!(layers[1].machine_local);
        assert_eq!(layers[1].value["a"], 2);
    }

    #[test]
    fn a_file_that_will_not_parse_is_skipped_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "{not json").unwrap();
        let source = FileTiers::new(
            dir.path().to_path_buf(),
            dir.path().join("absent"),
            dir.path().join("absent"),
        );
        assert!(source.layers().is_empty());
    }

    #[test]
    fn a_chain_puts_later_sources_above_earlier_ones() {
        let lower = Arc::new(InMemoryLayers::new().with(
            Tier::Global,
            false,
            "lower",
            serde_json::json!({"a": 1}),
        ));
        let upper = Arc::new(InMemoryLayers::new().with(
            Tier::Project,
            false,
            "upper",
            serde_json::json!({"a": 2}),
        ));
        let chain = Chain(vec![lower, upper]);

        let origins: Vec<_> = chain.layers().into_iter().map(|l| l.origin).collect();
        assert_eq!(origins, vec!["lower", "upper"]);
    }
}
