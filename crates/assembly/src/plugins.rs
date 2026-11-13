//! The plugin package subsystem, behind one type the rest of the daemon talks
//! to.
//!
//! Reading a package — manifest, fetch, checksum, unpack, cache, disclose,
//! enable — is the `plugin-packages` feature. A package runs nothing of its
//! own: its MCP servers, script bindings, scene directory, agent types and
//! skills are honored by the parts of the engine that already exist. Without
//! the feature the same API answers that there are no plugins, so the rest of
//! the daemon needs no conditional compilation of its own.

use crate::config::DaemonPaths;
use crate::session_pool::api::{
    PluginInfo, PluginInstalled, PluginRemoved, PluginScope, PluginToggled,
};
use std::collections::HashMap;
use std::sync::Arc;

/// What `daemon.doctor` reports, so "does this binary have plugins" is
/// answerable from the running process rather than from release notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStatus {
    /// Not compiled into this binary at all.
    CompiledOut,
    Enabled,
    /// Compiled in, switched off by configuration.
    DisabledByPolicy,
}

impl PluginStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompiledOut => "compiled-out",
            Self::Enabled => "enabled",
            Self::DisabledByPolicy => "disabled-by-policy",
        }
    }
}

/// Error returned by every management call in a build without the package
/// layer.
pub const DISABLED_MESSAGE: &str = "plugins are not available in this build";

/// One installed package's script bindings, ready for the carrier.
///
/// Carries the package's name and its unpacked directory rather than only the
/// bindings, because both decide what the scripts may do: the directory is
/// what a path is resolved against and checked against, and the name is the
/// authority they run under. Inferring either from the other end would put
/// the answer back where a path layout could change it.
pub struct PackageScripts {
    pub name: String,
    pub root: std::path::PathBuf,
    pub bindings: Vec<base::settings::ScriptBinding>,
}

/// One active package's skills, each with the body it expands to.
#[derive(Clone)]
pub struct PackageSkills {
    pub plugin: String,
    pub skills: Vec<(base::frozen::skill::SkillEntry, String)>,
}

/// What the active packages add to a project's catalogs.
#[derive(Clone, Default)]
pub struct Contributions {
    pub agent_types: Vec<base::agent_type::AgentTypeDefinition>,
    /// In package-name order, which is the order the catalog takes them in.
    pub skills: Vec<PackageSkills>,
}

/// Manifests, the cache, the enable state, and what a package declares.
#[cfg(feature = "plugin-packages")]
mod packages {
    use super::{DaemonPaths, PackageScripts, PackageSkills};
    use crate::session_pool::api::PluginInfo;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    pub struct Packages {
        paths: Arc<dyn DaemonPaths>,
        /// The `settings.plugins` policy this pool started with. Held rather
        /// than re-read, so a refresh mid-session cannot quietly widen what
        /// is allowed to load.
        policy: base::settings::PluginsConfig,
        enabled: bool,
        /// The permitted, enabled set. Rescanned by install/uninstall/enable/
        /// disable, so every reader — the MCP configs, the script bindings,
        /// the scenes, the agent types — is looking at one answer rather than each
        /// walking the directory at a different moment.
        active: RwLock<Arc<Vec<plugin::manifest::Plugin>>>,
        /// Why a package's script binding could not be honored, by package.
        /// A fault here costs that binding and nothing else, so it has to be
        /// reported somewhere the person who installed the package will look.
        script_faults: RwLock<HashMap<String, Vec<String>>>,
    }

    impl Packages {
        pub fn new(paths: Arc<dyn DaemonPaths>, policy: base::settings::PluginsConfig) -> Self {
            let enabled = policy.enabled;
            let active = Arc::new(if enabled {
                active_plugins(paths.as_ref(), &policy)
            } else {
                Vec::new()
            });
            Self {
                paths,
                policy,
                enabled,
                active: RwLock::new(active),
                script_faults: RwLock::new(HashMap::new()),
            }
        }

        pub fn is_enabled(&self) -> bool {
            self.enabled
        }

        pub fn policy(&self) -> &base::settings::PluginsConfig {
            &self.policy
        }

        pub fn active(&self) -> Arc<Vec<plugin::manifest::Plugin>> {
            self.active
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        pub fn rescan(&self) {
            if !self.enabled {
                return;
            }
            let refreshed = Arc::new(active_plugins(self.paths.as_ref(), &self.policy));
            *self.active.write().unwrap_or_else(|e| e.into_inner()) = refreshed;
        }

        pub fn count(&self) -> usize {
            self.active().len()
        }

        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            self.active()
                .iter()
                .flat_map(plugin::mcp::servers_for)
                .collect()
        }

        /// The package root each of [`Self::mcp_servers`] comes from.
        pub fn mcp_server_dirs(&self) -> HashMap<String, PathBuf> {
            self.active()
                .iter()
                .flat_map(|p| {
                    plugin::mcp::servers_for(p)
                        .into_iter()
                        .map(|(name, _)| (name, p.root.clone()))
                })
                .collect()
        }

        /// Every active package's `[[script]]` declarations, as bindings the
        /// carrier can take.
        pub fn script_bindings(&self) -> Vec<PackageScripts> {
            self.active()
                .iter()
                .filter(|p| !p.manifest.script.is_empty())
                .map(|p| PackageScripts {
                    name: p.name().to_string(),
                    root: p.root.clone(),
                    bindings: p
                        .manifest
                        .script
                        .iter()
                        .map(|s| base::settings::ScriptBinding {
                            path: PathBuf::from(s.file()),
                            point: s.point.clone(),
                            entry: s.function().to_string(),
                            timeout_ms: s.timeout_ms,
                            calls_per_turn: s.calls_per_turn,
                        })
                        .collect(),
                })
                .collect()
        }

        pub fn agent_types(&self) -> Vec<base::agent_type::AgentTypeDefinition> {
            self.active()
                .iter()
                .flat_map(plugin::agents::agent_types)
                .collect()
        }

        /// Discovery already read these skills; a package whose files changed
        /// since and no longer pass loses its skills here, not the others'.
        pub fn skills(&self) -> Vec<PackageSkills> {
            self.active()
                .iter()
                .filter(|p| p.manifest.skills.is_some())
                .filter_map(|p| match plugin::skills::declared_skills(p) {
                    Ok(skills) => Some(PackageSkills {
                        plugin: p.name().to_string(),
                        skills: skills.into_iter().map(|s| (s.entry, s.body)).collect(),
                    }),
                    Err(e) => {
                        tracing::warn!(
                            plugin = %p.name(),
                            error = %e,
                            "package skills no longer load; skipping them"
                        );
                        None
                    }
                })
                .collect()
        }

        pub fn note_script_faults(&self, faults: Vec<(String, String)>) {
            let mut by_plugin: HashMap<String, Vec<String>> = HashMap::new();
            for (plugin, reason) in faults {
                by_plugin.entry(plugin).or_default().push(reason);
            }
            *self
                .script_faults
                .write()
                .unwrap_or_else(|e| e.into_inner()) = by_plugin;
        }

        /// Every plugin on disk with its enable state — including the
        /// disabled ones, which a management UI has to show in order to turn
        /// them back on.
        pub fn list(&self) -> Vec<PluginInfo> {
            let (global, scene) = self.tier_dirs();
            let global_state = plugin::state::EnableState::new(global.clone());
            let scene_state = plugin::state::EnableState::new(scene.clone());
            let script_faults = self.script_faults.read().unwrap_or_else(|e| e.into_inner());
            plugin::discover_plugins(&global, &scene)
                .iter()
                .map(|p| {
                    let name = &p.manifest.plugin.name;
                    let skills: Vec<String> = plugin::skills::declared_skills(p)
                        .map(|skills| skills.into_iter().map(|s| s.entry.name).collect())
                        .unwrap_or_default();
                    PluginInfo {
                        name: name.clone(),
                        version: p.manifest.plugin.version.clone(),
                        description: p.manifest.plugin.description.clone(),
                        enabled: plugin::state::resolve_enabled(name, &global_state, &scene_state),
                        // The unpacked directory. Without it a host that wants
                        // to read something out of the package — a UI bundle,
                        // an icon — has to rebuild this path itself, which
                        // makes the daemon's disk layout part of its API.
                        root: p.root.display().to_string(),
                        script_faults: script_faults
                            .get(name.as_str())
                            .cloned()
                            .unwrap_or_default(),
                        skills,
                        skill_conflicts: Vec::new(),
                    }
                })
                .collect()
        }

        pub fn tier_dirs(&self) -> (PathBuf, PathBuf) {
            tier_dirs(self.paths.as_ref())
        }

        pub fn tier_root(&self, scope: &str) -> Result<PathBuf, String> {
            let (global, scene) = self.tier_dirs();
            match scope {
                "global" => Ok(global),
                "scene" => Ok(scene),
                other => Err(format!(
                    "invalid scope '{other}' — expected 'global' or 'scene'"
                )),
            }
        }

        /// Load back what was just installed, the way discovery will, and
        /// produce the disclosure an installer shows.
        ///
        /// Unpacking and being found are two different pieces of code and
        /// only the second one reads the manifest, so a package can unpack
        /// cleanly and still be one nothing will ever load. Left alone it is
        /// installed in no sense a caller cares about: absent from `list`,
        /// contributing nothing, with the reason in a log line nobody is
        /// reading. Asking here is what turns that into an answer at the
        /// moment somebody is standing in front of it. A disclosure over its
        /// limits is the same kind of answer: the package is refused, not
        /// installed with a note attached.
        pub fn load_back(
            &self,
            name: &str,
            version: &str,
            scope: &str,
        ) -> Result<plugin::Disclosure, String> {
            let cache = plugin::cache::PluginCache::new(self.tier_root(scope)?.join("cache"));
            let manifest = cache
                .cached_manifest(name, version)
                .ok_or_else(|| format!("no plugin.toml in the installed package `{name}`"))?;
            let loaded =
                plugin::manifest::Plugin::load(&cache.version_dir(name, version), &manifest)
                    .map_err(|e| e.to_string())?;
            if loaded.manifest.plugin.name != name {
                return Err(format!(
                    "the package calls itself `{}`, not `{name}`",
                    loaded.manifest.plugin.name
                ));
            }
            plugin::Disclosure::from_plugin(&loaded).map_err(|e| e.to_string())
        }

        pub fn cache_for(&self, scope: &str) -> Result<plugin::cache::PluginCache, String> {
            Ok(plugin::cache::PluginCache::new(
                self.tier_root(scope)?.join("cache"),
            ))
        }

        pub fn set_enabled(&self, name: &str, enabled: bool, scope: &str) -> Result<(), String> {
            plugin::state::EnableState::new(self.tier_root(scope)?)
                .set_enabled(name, enabled)
                .map_err(|e| e.to_string())
        }

        /// Keep a package that is about to be installed off until the host
        /// enables it, by writing its entry before it exists: writing it
        /// afterwards would leave a window in which a rescan counts it as
        /// enabled. Returns whether an entry was written, which
        /// [`release_hold`](Self::release_hold) takes back if the install
        /// fails. A name the tier already has an entry for keeps it.
        pub fn hold_disabled(&self, name: &str, scope: &str) -> Result<bool, String> {
            let state = plugin::state::EnableState::new(self.tier_root(scope)?);
            if state.get(name).is_some() {
                return Ok(false);
            }
            state.set_enabled(name, false).map_err(|e| e.to_string())?;
            Ok(true)
        }

        pub fn release_hold(&self, name: &str, scope: &str) {
            if let Ok(root) = self.tier_root(scope) {
                let _ = plugin::state::EnableState::new(root).clear(name);
            }
        }

        pub fn resolved_enabled(&self, name: &str) -> bool {
            let (global, scene) = self.tier_dirs();
            plugin::state::resolve_enabled(
                name,
                &plugin::state::EnableState::new(global),
                &plugin::state::EnableState::new(scene),
            )
        }
    }

    /// The two plugin-tier roots (plain `plugins/`, not `plugins/cache/` —
    /// see `plugin::cache`): global, shared across scenes, and this daemon's
    /// own scene.
    fn tier_dirs(paths: &dyn DaemonPaths) -> (PathBuf, PathBuf) {
        (
            paths.global_root().join("plugins"),
            paths.config_root().join("plugins"),
        )
    }

    fn active_plugins(
        paths: &dyn DaemonPaths,
        policy: &base::settings::PluginsConfig,
    ) -> Vec<plugin::manifest::Plugin> {
        let (global, scene) = tier_dirs(paths);
        let global_state = plugin::state::EnableState::new(global.clone());
        let scene_state = plugin::state::EnableState::new(scene.clone());
        let active: Vec<plugin::manifest::Plugin> = plugin::discover_plugins(&global, &scene)
            .into_iter()
            .filter(|p| {
                let name = &p.manifest.plugin.name;
                if !policy.permits(name) {
                    tracing::info!(plugin = %name, "not permitted by settings.plugins; skipping");
                    return false;
                }
                plugin::state::resolve_enabled(name, &global_state, &scene_state)
            })
            .collect();
        for (_, server) in active.iter().flat_map(plugin::mcp::servers_for) {
            base::interface::exec::env::hold_back_what_an_mcp_server_reads(&server);
        }
        active
    }
}

#[cfg(feature = "plugin-packages")]
fn disclosure_of(d: &plugin::Disclosure) -> crate::session_pool::api::PluginDisclosure {
    use crate::session_pool::api::{PluginDisclosure, VisibleText};
    PluginDisclosure {
        plugin: d.plugin.clone(),
        version: d.version.clone(),
        capabilities: d.capabilities.clone(),
        scene: d.scene.clone(),
        skills: d.skills.clone(),
        mcp_servers: d.mcp_servers.clone(),
        model_visible: d
            .model_visible
            .iter()
            .map(|v| VisibleText {
                origin: v.origin.clone(),
                text: v.text.clone(),
            })
            .collect(),
        inert: d.is_inert(),
    }
}

#[cfg(feature = "plugin-packages")]
mod imp {
    use super::*;
    use base::agent_type::AgentTypeDefinition;
    use base::interface::health::{CheckResult, HealthCheck};

    /// `daemon.doctor`'s view of the two `enabled.json` files: a tier whose
    /// file cannot be read holds every package in it disabled, and nothing
    /// else would say why.
    struct EnableStateCheck((std::path::PathBuf, std::path::PathBuf));

    impl HealthCheck for EnableStateCheck {
        fn name(&self) -> &str {
            "plugins.enable_state"
        }

        fn check(&self) -> CheckResult {
            let (global, scene) = &self.0;
            let faults: Vec<serde_json::Value> = [("global", global), ("scene", scene)]
                .into_iter()
                .filter_map(|(tier, root)| {
                    plugin::state::EnableState::new(root.clone())
                        .fault()
                        .map(|fault| serde_json::json!({"tier": tier, "fault": fault}))
                })
                .collect();
            if faults.is_empty() {
                CheckResult::ok("plugin enable state reads")
            } else {
                CheckResult::degraded(
                    "an unreadable enabled.json holds every plugin in its tier disabled",
                )
                .with_details(serde_json::json!({"tiers": faults}))
            }
        }
    }

    /// Packages on disk that discovery sets aside — a disclosure over its
    /// limits, a manifest that no longer loads — and so never load, however
    /// they got into the cache.
    struct RefusedPackagesCheck((std::path::PathBuf, std::path::PathBuf));

    impl HealthCheck for RefusedPackagesCheck {
        fn name(&self) -> &str {
            "plugins.refused"
        }

        fn check(&self) -> CheckResult {
            let (global, scene) = &self.0;
            let refused = plugin::scan_plugins(global, scene).refused;
            if refused.is_empty() {
                return CheckResult::ok("every installed plugin loads");
            }
            let details: Vec<serde_json::Value> = refused
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r.name,
                        "version": r.version,
                        "reason": r.reason,
                    })
                })
                .collect();
            CheckResult::degraded(format!(
                "{} installed plugin(s) are refused on loading and not active: {}",
                refused.len(),
                refused
                    .iter()
                    .map(|r| r.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .with_details(serde_json::json!({"refused": details}))
        }
    }

    /// Says which way `plugins.enable_on_install` is set, and, while it is
    /// still `true`, that its default is planned to change.
    struct EnableOnInstallCheck(bool);

    impl HealthCheck for EnableOnInstallCheck {
        fn name(&self) -> &str {
            "plugins.enable_on_install"
        }

        fn check(&self) -> CheckResult {
            let details = serde_json::json!({"enable_on_install": self.0});
            if self.0 {
                CheckResult::ok(
                    "plugin.install enables a package as soon as it is installed; \
                     plugins.enable_on_install is planned to default to false in a later \
                     0.3.x release, after which the host must call plugin.enable",
                )
            } else {
                CheckResult::ok(
                    "plugin.install leaves a package disabled until the host calls plugin.enable",
                )
            }
            .with_details(details)
        }
    }

    pub struct PluginSubsystem {
        packages: super::packages::Packages,
    }

    impl PluginSubsystem {
        pub fn new(paths: Arc<dyn DaemonPaths>, policy: base::settings::PluginsConfig) -> Self {
            Self {
                packages: super::packages::Packages::new(paths, policy),
            }
        }

        pub fn status(&self) -> PluginStatus {
            if self.packages.is_enabled() {
                PluginStatus::Enabled
            } else {
                PluginStatus::DisabledByPolicy
            }
        }

        pub fn policy(&self) -> &base::settings::PluginsConfig {
            self.packages.policy()
        }

        pub fn agent_types(&self) -> Vec<AgentTypeDefinition> {
            self.packages.agent_types()
        }

        pub fn contributions(&self) -> Contributions {
            Contributions {
                agent_types: self.packages.agent_types(),
                skills: self.packages.skills(),
            }
        }

        pub fn scenes(&self) -> Vec<Arc<base::interface::scene::SceneSpec>> {
            plugin::scenes::declared_scenes(&self.packages.active())
        }

        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            self.packages.mcp_servers()
        }

        pub fn mcp_server_dirs(&self) -> HashMap<String, std::path::PathBuf> {
            self.packages.mcp_server_dirs()
        }

        pub fn script_bindings(&self) -> Vec<PackageScripts> {
            self.packages.script_bindings()
        }

        pub fn note_script_faults(&self, faults: Vec<(String, String)>) {
            self.packages.note_script_faults(faults);
        }

        pub fn count(&self) -> usize {
            self.packages.count()
        }

        pub fn list(&self) -> Vec<PluginInfo> {
            self.packages.list()
        }

        pub fn health_check(&self) -> Vec<Arc<dyn base::interface::health::HealthCheck>> {
            if !self.packages.is_enabled() {
                return Vec::new();
            }
            vec![
                Arc::new(EnableStateCheck(self.packages.tier_dirs())),
                Arc::new(RefusedPackagesCheck(self.packages.tier_dirs())),
                Arc::new(EnableOnInstallCheck(
                    self.packages.policy().enable_on_install,
                )),
            ]
        }

        pub async fn install(
            &self,
            name: &str,
            version: &str,
            download_url: &str,
            checksum: Option<&str>,
            scope: PluginScope,
        ) -> Result<PluginInstalled, String> {
            let scope = scope.as_str();
            let cache = self.packages.cache_for(scope)?;
            let held = !self.packages.policy().enable_on_install
                && !cache.is_cached(name, version)
                && self.packages.hold_disabled(name, scope)?;
            let installed = self
                .install_and_load_back(&cache, name, version, download_url, checksum, scope)
                .await;
            if held && installed.is_err() {
                self.packages.release_hold(name, scope);
            }
            let (message, disclosure) = installed?;

            self.refresh().await;
            Ok(PluginInstalled {
                success: true,
                message,
                enabled: self.packages.resolved_enabled(name),
                disclosure: super::disclosure_of(&disclosure),
            })
        }

        async fn install_and_load_back(
            &self,
            cache: &plugin::cache::PluginCache,
            name: &str,
            version: &str,
            download_url: &str,
            checksum: Option<&str>,
            scope: &str,
        ) -> Result<(String, plugin::Disclosure), String> {
            let source = plugin::install::PluginSource {
                download_url: download_url.to_string(),
                checksum: checksum.map(str::to_string),
                version: version.to_string(),
            };
            let installed = plugin::install::install(cache, name, &source)
                .await
                .map_err(|e| e.to_string())?;
            let loaded = self.packages.load_back(name, version, scope);
            let (message, disclosure) = match (installed, loaded) {
                (plugin::install::Installed::AlreadyPresent, Ok(d)) => (
                    format!("plugin '{name}' v{version} is already installed"),
                    d,
                ),
                (plugin::install::Installed::AlreadyPresent, Err(e)) => {
                    return Err(format!(
                        "plugin '{name}' v{version} is already installed but is refused on \
                         loading it back: {e}; uninstall it before installing it again"
                    ));
                }
                (plugin::install::Installed::Into(_), Ok(d)) => (
                    format!("installed plugin '{name}' v{version} from {download_url}"),
                    d,
                ),
                (plugin::install::Installed::Into(_), Err(e)) => {
                    let _ = plugin::install::uninstall(cache, name, Some(version));
                    return Err(format!(
                        "installed but refused on loading it back, so it was removed: {e}"
                    ));
                }
            };
            Ok((message, disclosure))
        }

        pub async fn uninstall(
            &self,
            name: &str,
            version: Option<&str>,
            scope: PluginScope,
        ) -> Result<PluginRemoved, String> {
            let cache = self.packages.cache_for(scope.as_str())?;
            let removed =
                plugin::install::uninstall(&cache, name, version).map_err(|e| e.to_string())?;
            let message = match version {
                Some(version) => format!("removed plugin '{name}' v{version}"),
                None => format!("removed plugin '{name}' ({removed} versions)"),
            };
            self.refresh().await;
            Ok(PluginRemoved {
                success: true,
                message,
            })
        }

        pub async fn set_enabled(
            &self,
            name: &str,
            enabled: bool,
            scope: PluginScope,
        ) -> Result<PluginToggled, String> {
            self.packages.set_enabled(name, enabled, scope.as_str())?;
            self.refresh().await;
            Ok(PluginToggled {
                name: name.to_string(),
                enabled,
                scope,
            })
        }

        pub async fn refresh(&self) {
            self.packages.rescan();
        }
    }
}

#[cfg(not(feature = "plugin-packages"))]
mod imp {
    use super::*;
    use base::agent_type::AgentTypeDefinition;

    pub struct PluginSubsystem;

    impl PluginSubsystem {
        pub fn new(_paths: Arc<dyn DaemonPaths>, _policy: base::settings::PluginsConfig) -> Self {
            Self
        }
        pub fn status(&self) -> PluginStatus {
            PluginStatus::CompiledOut
        }
        pub fn agent_types(&self) -> Vec<AgentTypeDefinition> {
            Vec::new()
        }
        pub fn contributions(&self) -> Contributions {
            Contributions::default()
        }
        pub fn scenes(&self) -> Vec<Arc<base::interface::scene::SceneSpec>> {
            Vec::new()
        }
        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        pub fn mcp_server_dirs(&self) -> HashMap<String, std::path::PathBuf> {
            HashMap::new()
        }
        pub fn script_bindings(&self) -> Vec<PackageScripts> {
            Vec::new()
        }
        pub fn note_script_faults(&self, _faults: Vec<(String, String)>) {}
        pub fn count(&self) -> usize {
            0
        }
        pub fn list(&self) -> Vec<PluginInfo> {
            Vec::new()
        }
        pub fn health_check(&self) -> Vec<Arc<dyn base::interface::health::HealthCheck>> {
            Vec::new()
        }
        pub async fn install(
            &self,
            _name: &str,
            _version: &str,
            _download_url: &str,
            _checksum: Option<&str>,
            _scope: PluginScope,
        ) -> Result<PluginInstalled, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn uninstall(
            &self,
            _name: &str,
            _version: Option<&str>,
            _scope: PluginScope,
        ) -> Result<PluginRemoved, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn set_enabled(
            &self,
            _name: &str,
            _enabled: bool,
            _scope: PluginScope,
        ) -> Result<PluginToggled, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn refresh(&self) {}
    }
}

pub use imp::PluginSubsystem;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_strings_are_the_ones_doctor_reports() {
        assert_eq!(PluginStatus::CompiledOut.as_str(), "compiled-out");
        assert_eq!(PluginStatus::Enabled.as_str(), "enabled");
        assert_eq!(
            PluginStatus::DisabledByPolicy.as_str(),
            "disabled-by-policy"
        );
    }
}
