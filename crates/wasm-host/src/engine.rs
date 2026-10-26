//! The process-wide wasmtime engine and component loading.
//!
//! One `Engine` per process, shared by every plugin: it owns the compiler and
//! the code cache, and building one per plugin would pay that cost repeatedly
//! for no isolation benefit — isolation comes from the per-call `Store`, not
//! from the engine.

use crate::cache::AotCache;
use anyhow::{anyhow, Context, Result};
use std::path::Path;
#[cfg(feature = "compile")]
use std::path::PathBuf;
use std::sync::Arc;
use wasmtime::component::Component;
use wasmtime::{Config, Engine};

/// wasmtime has its own error type rather than re-exporting `anyhow`'s, so
/// it needs flattening before `anyhow`'s context combinators apply.
fn wasm_err(e: wasmtime::Error) -> anyhow::Error {
    anyhow!("{e}")
}

/// Shared wasmtime engine, configured once for every plugin this process runs.
#[derive(Clone)]
pub struct WasmEngine {
    engine: Engine,
    /// Keeps the epoch ticker alive for as long as any clone of this engine
    /// exists. The ticker is what makes a deadline mean anything: without it
    /// the epoch never advances, guests never reach a yield point, and a
    /// spinning plugin blocks the thread it is running on with no way out.
    _ticker: Arc<EpochTicker>,
}

/// Advances the engine's epoch on a fixed interval.
///
/// A dedicated thread rather than a timer task: it has to keep running even
/// when every async worker is occupied — and "every worker is occupied" is
/// precisely the situation a runaway guest creates.
struct EpochTicker {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl EpochTicker {
    fn start(engine: &Engine) -> Result<Self> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let engine = engine.weak();
        let flag = stop.clone();
        std::thread::Builder::new()
            .name("agix-plugin-epoch".into())
            .spawn(move || {
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(crate::instance::EPOCH_TICK);
                    match engine.upgrade() {
                        Some(engine) => engine.increment_epoch(),
                        None => break,
                    }
                }
            })
            // Without this thread the epoch never advances, so no deadline
            // is enforceable and a runaway guest would hang the runtime it
            // is on. An engine that cannot start it is an engine that must
            // not be used — the host already degrades to "no WASM plugins"
            // when one cannot be built.
            .context("could not start the plugin epoch ticker")?;
        Ok(Self { stop })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl WasmEngine {
    /// The engine configuration, and the only place it is written.
    ///
    /// Single-sourced because the AOT cache key derives from it. A build that
    /// compiles artifacts and a build that loads them are separate binaries;
    /// if their `Config`s differ by so much as one flag their compatibility
    /// hashes differ, every artifact misses, and in a runtime-only build a
    /// miss is a refusal to load. Two copies of this function would be two
    /// chances to get that wrong silently.
    pub fn config() -> Config {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // Timeouts and cancellation are wall-clock questions — "has the user
        // pressed Ctrl-C", "has this call outlived its budget" — so the
        // interruption mechanism is epochs, not fuel. Fuel counts
        // instructions, which answers a different question.
        config.epoch_interruption(true);
        config
    }

    pub fn new() -> Result<Self> {
        let config = Self::config();
        let engine = Engine::new(&config)
            .map_err(wasm_err)
            .context("failed to construct the wasmtime engine")?;
        let ticker = Arc::new(EpochTicker::start(&engine)?);
        Ok(Self {
            engine,
            _ticker: ticker,
        })
    }

    pub fn inner(&self) -> &Engine {
        &self.engine
    }

    /// Load a component, compiling it only when the AOT cache misses.
    ///
    /// `plugin_version_dir` is the plugin's extracted directory, which is
    /// where the artifact is cached (see [`AotCache`]).
    pub fn load(
        &self,
        component_path: &Path,
        plugin_version_dir: &Path,
    ) -> Result<ComponentHandle> {
        let bytes = std::fs::read(component_path)
            .with_context(|| format!("reading component {}", component_path.display()))?;

        let cache = AotCache::new(plugin_version_dir);
        let artifact_path = cache.artifact_path(&bytes);

        if let Some(artifact) = cache.read(&artifact_path) {
            // SAFETY: the artifact is one this toolchain wrote, in a
            // directory the daemon owns, keyed by the exact engine
            // configuration now running. `deserialize` still validates its
            // own header, so a corrupted or foreign file is rejected rather
            // than executed — which is why a failure here is demoted to a
            // recompile instead of an error.
            match unsafe { Component::deserialize(&self.engine, &artifact) } {
                Ok(component) => {
                    return Ok(ComponentHandle {
                        component: Arc::new(component),
                        from_cache: true,
                    });
                }
                Err(e) => tracing::warn!(
                    path = %artifact_path.display(),
                    error = %e,
                    "cached AOT artifact was rejected"
                ),
            }
        }

        self.compile_and_cache(component_path, &bytes, &cache, &artifact_path)
    }

    /// Compile `bytes` and record the artifact.
    ///
    /// Only exists in a build that links Cranelift. Without it a cache miss
    /// is where loading stops: the artifact was supposed to be produced by
    /// `agix-plugin-compile` at install, and quietly compiling here instead
    /// would defeat the point of removing the compiler.
    #[cfg(feature = "compile")]
    fn compile_and_cache(
        &self,
        component_path: &Path,
        bytes: &[u8],
        cache: &AotCache,
        artifact_path: &Path,
    ) -> Result<ComponentHandle> {
        let component = Component::new(&self.engine, bytes)
            .map_err(wasm_err)
            .with_context(|| format!("compiling component {}", component_path.display()))?;
        match component.serialize() {
            Ok(artifact) => cache.write(artifact_path, &artifact),
            Err(e) => tracing::warn!(error = %e, "component could not be serialized for caching"),
        }
        Ok(ComponentHandle {
            component: Arc::new(component),
            from_cache: false,
        })
    }

    #[cfg(not(feature = "compile"))]
    fn compile_and_cache(
        &self,
        component_path: &Path,
        _bytes: &[u8],
        _cache: &AotCache,
        artifact_path: &Path,
    ) -> Result<ComponentHandle> {
        anyhow::bail!(
            "no precompiled artifact for {} (expected {}), and this build cannot compile one. \
             Reinstall the plugin so `agix-plugin-compile` runs, or check that the compiler and \
             this binary are the same version.",
            component_path.display(),
            artifact_path.display()
        )
    }

    /// Compile a component and leave the artifact in its plugin's cache.
    ///
    /// What `agix-plugin-compile` calls. Returns where the artifact landed.
    #[cfg(feature = "compile")]
    pub fn precompile(&self, component_path: &Path, plugin_version_dir: &Path) -> Result<PathBuf> {
        let bytes = std::fs::read(component_path)
            .with_context(|| format!("reading component {}", component_path.display()))?;
        let cache = AotCache::new(plugin_version_dir);
        let artifact_path = cache.artifact_path(&bytes);

        let component = Component::new(&self.engine, &bytes)
            .map_err(wasm_err)
            .with_context(|| format!("compiling component {}", component_path.display()))?;
        let artifact = component
            .serialize()
            .map_err(wasm_err)
            .context("serializing the compiled component")?;

        // Not best-effort here, unlike the load path's cache write: this *is*
        // the job, and a caller that was told the plugin compiled must be
        // able to rely on the artifact existing.
        std::fs::create_dir_all(cache.dir())
            .and_then(|_| std::fs::write(&artifact_path, &artifact))
            .with_context(|| format!("writing {}", artifact_path.display()))?;
        Ok(artifact_path)
    }
}

/// A loaded component, cheap to clone and share across calls.
#[derive(Clone)]
pub struct ComponentHandle {
    component: Arc<Component>,
    /// Whether this load skipped compilation. Reported by diagnostics; a
    /// plugin that never hits its cache is a symptom worth seeing.
    from_cache: bool,
}

impl ComponentHandle {
    pub fn component(&self) -> &Component {
        &self.component
    }

    pub fn was_cached(&self) -> bool {
        self.from_cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest thing that is a valid component.
    fn empty_component() -> Vec<u8> {
        wat::parse_str("(component)").unwrap()
    }

    fn write_component(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join("plugin.wasm");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn the_engine_is_configured_for_components_and_interruption() {
        // Construction is the assertion: `Engine::new` rejects a `Config`
        // whose options don't agree.
        WasmEngine::new().expect("engine config must be self-consistent");
    }

    #[test]
    fn a_component_compiles_then_loads_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_component(dir.path(), &empty_component());
        let engine = WasmEngine::new().unwrap();

        let first = engine.load(&path, dir.path()).unwrap();
        assert!(!first.was_cached(), "the first load has nothing to reuse");

        let second = engine.load(&path, dir.path()).unwrap();
        assert!(
            second.was_cached(),
            "the second load must reuse the artifact the first one wrote"
        );
    }

    /// Editing the component must invalidate its artifact — the cache is
    /// keyed on content precisely so a rebuilt plugin doesn't keep running
    /// the old code.
    #[test]
    fn changing_the_component_misses_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_component(dir.path(), &empty_component());
        let engine = WasmEngine::new().unwrap();
        engine.load(&path, dir.path()).unwrap();

        // A component with a distinct (still empty) inner module, so the
        // bytes differ while remaining valid.
        std::fs::write(
            &path,
            wat::parse_str(r#"(component (core module))"#).unwrap(),
        )
        .unwrap();
        assert!(!engine.load(&path, dir.path()).unwrap().was_cached());
    }

    /// A corrupt artifact must degrade to a recompile, not to a failed load —
    /// and certainly not to executing whatever the bytes happen to be.
    #[test]
    fn a_corrupt_artifact_falls_back_to_compiling() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = empty_component();
        let path = write_component(dir.path(), &bytes);
        let engine = WasmEngine::new().unwrap();
        engine.load(&path, dir.path()).unwrap();

        let artifact = AotCache::new(dir.path()).artifact_path(&bytes);
        std::fs::write(&artifact, b"not a wasmtime artifact").unwrap();

        let handle = engine.load(&path, dir.path()).unwrap();
        assert!(!handle.was_cached());
    }

    #[test]
    fn a_missing_component_file_reports_which_path() {
        let dir = tempfile::tempdir().unwrap();
        let engine = WasmEngine::new().unwrap();
        let err = engine
            .load(&dir.path().join("absent.wasm"), dir.path())
            .err()
            .expect("loading a component that isn't there must fail");
        assert!(err.to_string().contains("absent.wasm"), "{err}");
    }

    #[test]
    fn a_file_that_is_not_a_component_fails_to_compile() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_component(dir.path(), b"definitely not wasm");
        let engine = WasmEngine::new().unwrap();
        assert!(engine.load(&path, dir.path()).is_err());
    }
}
