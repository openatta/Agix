//! `agix-plugin-compile` — turns a plugin's components into machine code.
//!
//! Exists so the daemon does not have to. A build serving plugins can run
//! without Cranelift linked at all, which makes "this process cannot compile
//! WebAssembly" a fact the type system enforces rather than a policy someone
//! has to keep enforcing; the compiling has to happen somewhere, and this is
//! where.
//!
//! Run against a plugin's installed directory. Every `[[wasm]]` component the
//! manifest declares is compiled and its artifact written into that
//! directory's `.aot/`, which is exactly where the runtime looks.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Compile every component `dir`'s manifest declares.
pub fn compile_plugin(dir: &Path) -> Result<Vec<PathBuf>> {
    let manifest = dir.join("plugin.toml");
    let plugin = plugin::manifest::Plugin::load(dir, &manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;

    let engine = wasm_host::WasmEngine::new()?;
    let mut written = Vec::new();
    for payload in &plugin.manifest.wasm {
        let component = plugin.path(&payload.component);
        written.push(
            engine
                .precompile(&component, dir)
                .with_context(|| format!("compiling {}", component.display()))?,
        );
    }
    Ok(written)
}
