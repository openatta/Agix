//! The plugin subsystem, behind one type the rest of the daemon talks to.
//!
//! Two questions, and they are not the same question. *Can this build read a
//! package?* — manifest, fetch, checksum, unpack, cache, disclose, enable —
//! is the `plugin-packages` feature, and it needs no runtime. *Can this build
//! run a package's WebAssembly components?* is the `plugins` feature, which
//! pulls in wasmtime and is exclusive with the script carrier.
//!
//! They used to be one feature, and the cost was that the default build could
//! not install a package at all: a package whose whole content was an MCP
//! server or a script — nothing to run in-process — was refused for want of a
//! WebAssembly engine it never needed.
//!
//! So there are three implementations of the same API here, selected by
//! feature: packages with a carrier, packages without one, and neither. The
//! rest of the daemon needs no conditional compilation of its own, and a
//! locked-down build stays a build-flag decision rather than a code path
//! anyone has to remember to take.
//!
//! Build the locked-down artifact with
//! `cargo build -p daemon --no-default-features`. Not `--workspace`: cargo
//! unifies features across the graph, so any other member enabling a feature
//! turns it back on.

use crate::config::DaemonPaths;
use std::collections::HashMap;
use std::sync::Arc;

/// What `daemon.doctor` reports, so "does this binary have plugins" is
/// answerable from the running process rather than from release notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStatus {
    /// Not compiled into this binary at all.
    CompiledOut,
    /// Packages install and contribute everything but components: this build
    /// carries no WebAssembly engine. Reported distinctly because "the plugin
    /// installed and its tool is missing" has to be answerable without
    /// reading release notes.
    PackagesOnly,
    /// Compiled in with a carrier, and active.
    Enabled,
    /// Compiled in, switched off by configuration.
    DisabledByPolicy,
}

impl PluginStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompiledOut => "compiled-out",
            Self::PackagesOnly => "packages-only",
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
    pub bindings: Vec<base::interface::settings::ScriptBinding>,
}

/// The half of the subsystem that runs nothing: manifests, the cache, the
/// enable state, and what a package declares.
///
/// Shared by the two implementations that have it, rather than written twice,
/// because the copies are where "what a package contributes" would come to
/// mean two different things depending on which carrier was compiled in.
#[cfg(feature = "plugin-packages")]
mod packages {
    use super::{DaemonPaths, PackageScripts};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    pub struct Packages {
        paths: Arc<dyn DaemonPaths>,
        /// The `settings.plugins` policy this pool started with. Held rather
        /// than re-read, so a refresh mid-session cannot quietly widen what
        /// is allowed to load.
        policy: base::interface::settings::PluginsConfig,
        enabled: bool,
        /// The permitted, enabled set. Rescanned by install/uninstall/enable/
        /// disable, so every reader — the MCP configs, the script bindings,
        /// the component loader — is looking at one answer rather than each
        /// walking the directory at a different moment.
        active: RwLock<Arc<Vec<plugin::manifest::Plugin>>>,
        /// Why a package's script binding could not be honored, by package.
        /// A fault here costs that binding and nothing else, so it has to be
        /// reported somewhere the person who installed the package will look.
        script_faults: RwLock<HashMap<String, Vec<String>>>,
        /// Why a package's component could not be loaded, by package. Same
        /// reasoning as the scripts above, and the sharper case: a package
        /// whose only component will not load is listed, enabled, and
        /// contributing nothing, with every other answer over RPC saying it
        /// is fine — the breaker counts faults from calls, and a component
        /// that never loaded is never called. Empty in a build with no
        /// carrier, which loads nothing and so has nothing to report.
        component_faults: RwLock<HashMap<String, Vec<String>>>,
    }

    impl Packages {
        pub fn new(
            paths: Arc<dyn DaemonPaths>,
            policy: base::interface::settings::PluginsConfig,
        ) -> Self {
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
                component_faults: RwLock::new(HashMap::new()),
            }
        }

        pub fn is_enabled(&self) -> bool {
            self.enabled
        }

        pub fn policy(&self) -> &base::interface::settings::PluginsConfig {
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
                        .map(|s| base::interface::settings::ScriptBinding {
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

        /// Group `(plugin, reason)` pairs the way the two fault maps hold
        /// them.
        fn by_plugin(faults: Vec<(String, String)>) -> HashMap<String, Vec<String>> {
            let mut out: HashMap<String, Vec<String>> = HashMap::new();
            for (plugin, reason) in faults {
                out.entry(plugin).or_default().push(reason);
            }
            out
        }

        pub fn note_script_faults(&self, faults: Vec<(String, String)>) {
            *self
                .script_faults
                .write()
                .unwrap_or_else(|e| e.into_inner()) = Self::by_plugin(faults);
        }

        /// Only the carrier loads components, so only the carrier has
        /// anything to report. The field it writes is read in every build —
        /// `plugin.list` carries `component_faults` whether or not this
        /// binary could ever fill it, because a client should not have to
        /// know which build it is talking to in order to read the answer.
        #[cfg(feature = "plugins")]
        pub fn note_component_faults(&self, faults: Vec<(String, String)>) {
            *self
                .component_faults
                .write()
                .unwrap_or_else(|e| e.into_inner()) = Self::by_plugin(faults);
        }

        /// Every plugin on disk with its enable state — including the
        /// disabled ones, which a management UI has to show in order to turn
        /// them back on.
        pub fn list(&self) -> Vec<serde_json::Value> {
            let (global, scene) = self.tier_dirs();
            let global_state = plugin::state::EnableState::new(global.clone());
            let scene_state = plugin::state::EnableState::new(scene.clone());
            let script_faults = self.script_faults.read().unwrap_or_else(|e| e.into_inner());
            let component_faults = self
                .component_faults
                .read()
                .unwrap_or_else(|e| e.into_inner());
            plugin::discover_plugins(&global, &scene)
                .iter()
                .map(|p| {
                    let name = &p.manifest.plugin.name;
                    serde_json::json!({
                        "name": name,
                        "version": p.manifest.plugin.version,
                        "description": p.manifest.plugin.description,
                        "enabled": plugin::state::resolve_enabled(name, &global_state, &scene_state),
                        // The unpacked directory. Without it a host that wants
                        // to read something out of the package — a UI bundle,
                        // an icon — has to rebuild this path itself, which
                        // makes the daemon's disk layout part of its API.
                        "root": p.root.display().to_string(),
                        "script_faults": script_faults.get(name.as_str()).cloned().unwrap_or_default(),
                        "component_faults": component_faults
                            .get(name.as_str())
                            .cloned()
                            .unwrap_or_default(),
                    })
                })
                .collect()
        }

        /// What a package declares, from the manifest alone. The build with
        /// a carrier answers this from the loaded components instead — a
        /// manifest cannot say what text a component's tools carry.
        #[cfg(not(feature = "plugins"))]
        pub fn disclosure(&self, name: &str) -> serde_json::Value {
            let active = self.active();
            let Some(p) = active.iter().find(|p| p.name() == name) else {
                return serde_json::Value::Null;
            };
            match plugin::Disclosure::from_plugin(p) {
                Err(e) => serde_json::json!({"error": e.to_string()}),
                Ok(d) => super::disclosure_json(&d, p.manifest.wasm.len(), false),
            }
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

        /// Load back what was just installed, the way discovery will.
        ///
        /// Unpacking and being found are two different pieces of code and
        /// only the second one reads the manifest, so a package can unpack
        /// cleanly and still be one nothing will ever load. Left alone it is
        /// installed in no sense a caller cares about: absent from `list`,
        /// contributing nothing, with the reason in a log line nobody is
        /// reading. Asking here is what turns that into an answer at the
        /// moment somebody is standing in front of it.
        pub fn load_back(&self, name: &str, version: &str, scope: &str) -> Result<(), String> {
            let cache = plugin::cache::PluginCache::new(self.tier_root(scope)?.join("cache"));
            let manifest = cache
                .cached_manifest(name, version)
                .ok_or_else(|| format!("no plugin.toml in the installed package `{name}`"))?;
            plugin::manifest::Plugin::load(&cache.version_dir(name, version), &manifest)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }

        pub fn commands_for(&self, scope: &str) -> Result<plugin::cli::PluginCommands, String> {
            let cache = plugin::cache::PluginCache::new(self.tier_root(scope)?.join("cache"));
            Ok(plugin::cli::PluginCommands::new(cache, None))
        }

        pub fn set_enabled(&self, name: &str, enabled: bool, scope: &str) -> Result<(), String> {
            plugin::state::EnableState::new(self.tier_root(scope)?)
                .set_enabled(name, enabled)
                .map_err(|e| e.to_string())
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
        policy: &base::interface::settings::PluginsConfig,
    ) -> Vec<plugin::manifest::Plugin> {
        let (global, scene) = tier_dirs(paths);
        let global_state = plugin::state::EnableState::new(global.clone());
        let scene_state = plugin::state::EnableState::new(scene.clone());
        plugin::discover_plugins(&global, &scene)
            .into_iter()
            .filter(|p| {
                let name = &p.manifest.plugin.name;
                if !policy.permits(name) {
                    tracing::info!(plugin = %name, "not permitted by settings.plugins; skipping");
                    return false;
                }
                plugin::state::resolve_enabled(name, &global_state, &scene_state)
            })
            .collect()
    }
}

/// One disclosure as the wire shape, so the two builds that can produce one
/// do not describe a package differently.
///
/// `runs_components` is the honest half of the split: a build without the
/// WASM carrier installs a package with components and does not run them,
/// and saying so beats either hiding the components or refusing the install.
#[cfg(feature = "plugin-packages")]
fn disclosure_json(
    d: &plugin::Disclosure,
    components: usize,
    runs_components: bool,
) -> serde_json::Value {
    serde_json::json!({
        "plugin": d.plugin,
        "version": d.version,
        "capabilities": d.capabilities,
        "events": d.events,
        "scene": d.scene,
        "mcp_servers": d.mcp_servers,
        "model_visible": d.model_visible.iter().map(|v| serde_json::json!({
            "origin": v.origin,
            "text": v.text,
        })).collect::<Vec<_>>(),
        "wasm": {
            "components": components,
            "runnable": runs_components,
        },
        "inert": d.is_inert() && components == 0,
    })
}

#[cfg(feature = "plugins")]
mod imp {
    use super::*;
    use plugin_host::InstalledPlugins;
    use runtime::agent_tool::AgentTypeDefinition;
    use runtime::plugin_host::PluginHost;
    use std::sync::RwLock;

    pub struct PluginSubsystem {
        packages: super::packages::Packages,
        /// The directory `${workspace}` in a capability declaration resolves
        /// against.
        ///
        /// Held here, and nowhere else, because it decides how much of the
        /// filesystem a plugin was granted. Taking it from the caller at each
        /// load let startup and post-install reloads anchor to different
        /// directories, so the same declaration meant different things
        /// depending on which path had run.
        workspace: std::path::PathBuf,
        /// One engine for the whole process — it owns the compiler and its
        /// code cache, so building one per plugin would pay that repeatedly
        /// for no isolation benefit. Isolation comes from the per-call
        /// store, not from the engine.
        ///
        /// `None` when wasmtime could not be initialised at all, which
        /// leaves MCP payloads working and WASM ones absent rather than
        /// taking the daemon down.
        /// Built on first use, and only when there is a component to compile
        /// — see [`PluginSubsystem::engine`].
        engine: std::sync::OnceLock<Option<wasm_host::WasmEngine>>,
        /// Fault records, kept here so they outlive the component instances
        /// a refresh rebuilds — otherwise a plugin that disabled itself came
        /// back the moment the user touched a different one.
        health: Arc<wasm_host::health::HealthRegistry>,
        /// The loaded components. Sessions pick this up when they are
        /// created; already-running sessions keep what they were built with,
        /// same hot-swap semantics as `config.setProvider` / `mcp.addServer`.
        /// A plain lock, not an async one: every access is a short read or
        /// swap of an `Arc` with no `.await` held across it, and keeping the
        /// accessors synchronous is what lets `SessionPool::new` — which is
        /// not async — build its agent-type catalog from them.
        active: RwLock<Arc<InstalledPlugins>>,
    }

    impl PluginSubsystem {
        pub fn new(
            paths: Arc<dyn DaemonPaths>,
            workspace: std::path::PathBuf,
            policy: base::interface::settings::PluginsConfig,
        ) -> Self {
            let packages = super::packages::Packages::new(paths, policy);
            let active = Arc::new(InstalledPlugins::new(packages.active().as_ref().clone()));
            Self {
                packages,
                workspace,
                engine: std::sync::OnceLock::new(),
                health: wasm_host::health::HealthRegistry::new(),
                active: RwLock::new(active),
            }
        }

        /// The WASM engine, built the first time something actually needs to
        /// compile a component.
        ///
        /// Not in `new`: an `Engine` is not free to exist. On macOS the first
        /// one installs a process-wide mach exception handler thread, and
        /// that thread aborts the process if its `mach_msg` ever returns
        /// anything but a message — which `MACH_RCV_INTERRUPTED` is, and
        /// which a `posix_spawn` on another thread produces. A daemon with no
        /// components installed was paying that risk, and a thread, for
        /// nothing; a test binary building one daemon per case was paying it
        /// dozens of times.
        fn engine(&self) -> Option<&wasm_host::WasmEngine> {
            self.engine
                .get_or_init(|| match wasm_host::WasmEngine::new() {
                    Ok(e) => Some(e),
                    Err(e) => {
                        tracing::warn!(error = %e, "wasmtime unavailable; WASM plugins will not load");
                        None
                    }
                })
                .as_ref()
        }

        /// Compile and interrogate every installed component.
        ///
        /// Split out of `new` because it is async and slow, and because the
        /// daemon has an async startup phase to do it in. Until this has
        /// run, plugins contribute their manifests but no tools — so it is
        /// awaited before the daemon serves, not after.
        pub async fn load_components(&self) {
            if !self.packages.is_enabled() {
                return;
            }
            self.packages.rescan();
            let installed = self.packages.active();
            let mut refreshed = InstalledPlugins::new(installed.as_ref().clone());
            // Nothing installed means nothing to compile, and so no reason to
            // build an engine — see `engine()`.
            if !installed.is_empty() {
                if let Some(engine) = self.engine() {
                    refreshed
                        .load_components_with_health(engine, &self.workspace, &self.health)
                        .await;
                }
            }
            self.packages
                .note_component_faults(refreshed.component_faults().to_vec());
            *self.active.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(refreshed);
        }

        pub fn status(&self) -> PluginStatus {
            if self.packages.is_enabled() {
                PluginStatus::Enabled
            } else {
                PluginStatus::DisabledByPolicy
            }
        }

        /// The fault records as a health check, so a plugin this process has
        /// stopped calling is visible to whoever installed it.
        pub fn health_check(&self) -> Option<Arc<dyn base::interface::health::HealthCheck>> {
            Some(Arc::new(wasm_host::health::PluginBreakers::new(
                self.health.clone(),
            )))
        }

        /// The policy in force, for diagnostics.
        pub fn policy(&self) -> &base::interface::settings::PluginsConfig {
            self.packages.policy()
        }

        pub fn host(&self) -> Option<Arc<dyn PluginHost>> {
            let active = self.read();
            if active.is_empty() {
                return None;
            }
            Some(active as Arc<dyn PluginHost>)
        }

        pub fn agent_types(&self) -> Vec<AgentTypeDefinition> {
            self.read().agent_types()
        }

        pub fn scenes(&self) -> Vec<Arc<dyn base::interface::scene::AgentScene>> {
            use runtime::plugin_host::PluginHost;
            self.read().scenes()
        }

        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            self.packages.mcp_servers()
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

        /// What `name` will contribute, as JSON. `null` when the plugin is
        /// not installed; an `error` field when its declarations are
        /// themselves inadmissible.
        ///
        /// Enriched with the text a loaded component supplies, which is the
        /// half of a disclosure the manifest cannot answer.
        pub fn disclosure(&self, name: &str) -> serde_json::Value {
            let components = self
                .packages
                .active()
                .iter()
                .find(|p| p.name() == name)
                .map(|p| p.manifest.wasm.len());
            match self.read().disclose(name) {
                None => serde_json::Value::Null,
                Some(Err(e)) => serde_json::json!({"error": e.to_string()}),
                Some(Ok(d)) => super::disclosure_json(&d, components.unwrap_or(0), true),
            }
        }

        fn read(&self) -> Arc<InstalledPlugins> {
            self.active
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        pub fn list(&self) -> Vec<serde_json::Value> {
            self.packages.list()
        }

        pub async fn install(
            &self,
            name: &str,
            version: &str,
            download_url: &str,
            checksum: Option<&str>,
            scope: &str,
        ) -> Result<serde_json::Value, String> {
            let commands = self.packages.commands_for(scope)?;
            let source = plugin::marketplace::PluginSource {
                download_url: download_url.to_string(),
                checksum: checksum.map(str::to_string),
                version: version.to_string(),
            };
            let result = commands
                .install_source(name, &source)
                .await
                .map_err(|e| e.to_string())?;

            // Before the compiler, which has nothing to say about a
            // manifest that will not parse.
            if let Err(e) = self.packages.load_back(name, version, scope) {
                let _ = commands.uninstall(name, Some(version)).await;
                return Err(format!(
                    "installed but could not be loaded back, so it was removed: {e}"
                ));
            }

            // Ahead of the refresh, and fatal: a plugin whose components
            // cannot be compiled is one that will fail to load every time,
            // and discovering that at install — where the user is standing
            // right here — beats discovering it in the middle of a session.
            // A package with no components compiles nothing and reaches no
            // compiler; that is `plugin_host::precompile_plugin`'s first act.
            if let Err(e) = self.precompile(name).await {
                let _ = commands.uninstall(name, Some(version)).await;
                return Err(format!(
                    "installed but could not be compiled, so it was removed: {e:#}"
                ));
            }

            self.refresh().await;
            Ok(serde_json::json!({
                "success": result.success,
                "message": result.message,
                // What the plugin will contribute, for the caller to show
                // before anyone relies on it. Text reaching the model is the
                // one thing the sandbox cannot check, so it travels back
                // with the install rather than waiting to be asked for.
                "disclosure": self.disclosure(name),
            }))
        }

        pub async fn uninstall(
            &self,
            name: &str,
            version: Option<&str>,
            scope: &str,
        ) -> Result<serde_json::Value, String> {
            let commands = self.packages.commands_for(scope)?;
            let result = commands
                .uninstall(name, version)
                .await
                .map_err(|e| e.to_string())?;
            self.refresh().await;
            Ok(serde_json::json!({
                "success": result.success,
                "message": result.message,
            }))
        }

        pub async fn set_enabled(
            &self,
            name: &str,
            enabled: bool,
            scope: &str,
        ) -> Result<serde_json::Value, String> {
            self.packages.set_enabled(name, enabled, scope)?;
            self.refresh().await;
            Ok(serde_json::json!({"name": name, "enabled": enabled, "scope": scope}))
        }

        /// Re-read the installed set after an install/uninstall/enable/
        /// disable. Components are reloaded too, since the point of the
        /// refresh is usually that they changed.
        pub async fn refresh(&self) {
            self.load_components().await;
        }

        /// Compile the components of every installed version of `name`.
        ///
        /// Versions rather than "the current one" because install leaves the
        /// others in place, and which one resolves can change under a
        /// downgrade — an uncompiled sibling would then fail to load in a
        /// build that cannot compile.
        async fn precompile(&self, name: &str) -> anyhow::Result<()> {
            let (global, scene) = self.packages.tier_dirs();
            for tier in [global, scene] {
                let cache = plugin::cache::PluginCache::new(tier.join("cache"));
                for version in cache.list_versions(name) {
                    let dir = cache.version_dir(name, &version);
                    if dir.join("plugin.toml").is_file() {
                        plugin_host::precompile_plugin(&dir).await?;
                    }
                }
            }
            Ok(())
        }
    }
}

/// Packages without a carrier: the default build.
///
/// Everything a package declares is honored except its WebAssembly
/// components, which this binary has no engine for. A package that has some
/// still installs, and its disclosure says they will not run — the
/// alternative was refusing the whole package over the one part this build
/// cannot serve.
#[cfg(all(feature = "plugin-packages", not(feature = "plugins")))]
mod imp {
    use super::*;
    use runtime::agent_tool::AgentTypeDefinition;
    use runtime::plugin_host::PluginHost;

    pub struct PluginSubsystem {
        packages: super::packages::Packages,
    }

    impl PluginSubsystem {
        pub fn new(
            paths: Arc<dyn DaemonPaths>,
            _workspace: std::path::PathBuf,
            policy: base::interface::settings::PluginsConfig,
        ) -> Self {
            Self {
                packages: super::packages::Packages::new(paths, policy),
            }
        }

        pub fn status(&self) -> PluginStatus {
            if self.packages.is_enabled() {
                PluginStatus::PackagesOnly
            } else {
                PluginStatus::DisabledByPolicy
            }
        }

        pub fn policy(&self) -> &base::interface::settings::PluginsConfig {
            self.packages.policy()
        }

        /// Nothing: a host is what serves components, and this build runs
        /// none.
        pub fn host(&self) -> Option<Arc<dyn PluginHost>> {
            None
        }

        /// Empty for the same reason as [`host`](Self::host) — an agent type
        /// is a prompt plus a tool surface, and the tools come from
        /// components.
        pub fn agent_types(&self) -> Vec<AgentTypeDefinition> {
            Vec::new()
        }

        pub fn scenes(&self) -> Vec<Arc<dyn base::interface::scene::AgentScene>> {
            Vec::new()
        }

        pub fn health_check(&self) -> Option<Arc<dyn base::interface::health::HealthCheck>> {
            None
        }

        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            self.packages.mcp_servers()
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

        pub fn list(&self) -> Vec<serde_json::Value> {
            self.packages.list()
        }

        pub fn disclosure(&self, name: &str) -> serde_json::Value {
            self.packages.disclosure(name)
        }

        pub async fn install(
            &self,
            name: &str,
            version: &str,
            download_url: &str,
            checksum: Option<&str>,
            scope: &str,
        ) -> Result<serde_json::Value, String> {
            let commands = self.packages.commands_for(scope)?;
            let source = plugin::marketplace::PluginSource {
                download_url: download_url.to_string(),
                checksum: checksum.map(str::to_string),
                version: version.to_string(),
            };
            let result = commands
                .install_source(name, &source)
                .await
                .map_err(|e| e.to_string())?;
            if let Err(e) = self.packages.load_back(name, version, scope) {
                let _ = commands.uninstall(name, Some(version)).await;
                return Err(format!(
                    "installed but could not be loaded back, so it was removed: {e}"
                ));
            }

            // No compile step, and nothing to roll back for want of one:
            // this build would not run the components either way, and the
            // disclosure says so.
            self.refresh().await;
            Ok(serde_json::json!({
                "success": result.success,
                "message": result.message,
                "disclosure": self.disclosure(name),
            }))
        }

        pub async fn uninstall(
            &self,
            name: &str,
            version: Option<&str>,
            scope: &str,
        ) -> Result<serde_json::Value, String> {
            let commands = self.packages.commands_for(scope)?;
            let result = commands
                .uninstall(name, version)
                .await
                .map_err(|e| e.to_string())?;
            self.refresh().await;
            Ok(serde_json::json!({
                "success": result.success,
                "message": result.message,
            }))
        }

        pub async fn set_enabled(
            &self,
            name: &str,
            enabled: bool,
            scope: &str,
        ) -> Result<serde_json::Value, String> {
            self.packages.set_enabled(name, enabled, scope)?;
            self.refresh().await;
            Ok(serde_json::json!({"name": name, "enabled": enabled, "scope": scope}))
        }

        pub async fn refresh(&self) {
            self.packages.rescan();
        }

        pub async fn load_components(&self) {}
    }
}

#[cfg(not(feature = "plugin-packages"))]
mod imp {
    use super::*;
    use runtime::agent_tool::AgentTypeDefinition;
    use runtime::plugin_host::PluginHost;

    pub struct PluginSubsystem;

    impl PluginSubsystem {
        pub fn new(
            _paths: Arc<dyn DaemonPaths>,
            _workspace: std::path::PathBuf,
            _policy: base::interface::settings::PluginsConfig,
        ) -> Self {
            Self
        }
        pub fn status(&self) -> PluginStatus {
            PluginStatus::CompiledOut
        }
        pub fn host(&self) -> Option<Arc<dyn PluginHost>> {
            None
        }
        pub fn agent_types(&self) -> Vec<AgentTypeDefinition> {
            Vec::new()
        }
        pub fn scenes(&self) -> Vec<Arc<dyn base::interface::scene::AgentScene>> {
            Vec::new()
        }
        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        pub fn script_bindings(&self) -> Vec<PackageScripts> {
            Vec::new()
        }
        pub fn note_script_faults(&self, _faults: Vec<(String, String)>) {}
        pub fn health_check(&self) -> Option<Arc<dyn base::interface::health::HealthCheck>> {
            None
        }
        pub fn count(&self) -> usize {
            0
        }
        pub fn list(&self) -> Vec<serde_json::Value> {
            Vec::new()
        }
        pub fn disclosure(&self, _name: &str) -> serde_json::Value {
            serde_json::Value::Null
        }
        pub async fn install(
            &self,
            _name: &str,
            _version: &str,
            _download_url: &str,
            _checksum: Option<&str>,
            _scope: &str,
        ) -> Result<serde_json::Value, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn uninstall(
            &self,
            _name: &str,
            _version: Option<&str>,
            _scope: &str,
        ) -> Result<serde_json::Value, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn set_enabled(
            &self,
            _name: &str,
            _enabled: bool,
            _scope: &str,
        ) -> Result<serde_json::Value, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn refresh(&self) {}
        pub async fn load_components(&self) {}
    }
}

pub use imp::PluginSubsystem;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_strings_are_the_ones_doctor_reports() {
        assert_eq!(PluginStatus::CompiledOut.as_str(), "compiled-out");
        assert_eq!(PluginStatus::PackagesOnly.as_str(), "packages-only");
        assert_eq!(PluginStatus::Enabled.as_str(), "enabled");
        assert_eq!(
            PluginStatus::DisabledByPolicy.as_str(),
            "disabled-by-policy"
        );
    }
}
