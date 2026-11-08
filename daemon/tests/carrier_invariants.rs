//! Extensions come in two kinds: Rust plugins compiled into the host, and
//! QuickJS scripts. The rules that keep it that way, as tests over the source
//! and the manifests.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon/ has a parent")
        .to_path_buf()
}

fn cargo_manifests(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != "node_modules" && name != ".git" {
                out.extend(cargo_manifests(&path));
            }
        } else if name == "Cargo.toml" {
            out.push(path);
        }
    }
    out
}

#[test]
fn no_crate_links_a_webassembly_runtime() {
    let root = repo_root();
    for manifest in cargo_manifests(&root) {
        let text = std::fs::read_to_string(&manifest).unwrap();
        for runtime in ["wasmtime", "wasmer", "wasmi"] {
            assert!(
                !text.lines().any(|l| l.trim_start().starts_with(runtime)),
                "{} depends on `{runtime}`; extensions are Rust plugins or QuickJS scripts",
                manifest.display()
            );
        }
    }
}

/// A build without `scripts` must link no JavaScript engine for extensions.
#[test]
fn the_script_carrier_is_optional_at_compile_time() {
    let root = repo_root();
    let manifest = std::fs::read_to_string(root.join("crates/assembly/Cargo.toml"))
        .expect("assembly manifest");
    let declared = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("script-host"))
        .expect("assembly must declare `script-host`");
    assert!(declared.contains("optional = true"), "{declared}");
    assert!(manifest.contains("scripts = [\"dep:script-host\"]"));
}

/// Disclosure is about what a package *says*; it must not depend on how its
/// scripts run.
#[test]
fn disclosure_names_no_engine() {
    let root = repo_root();
    let source = std::fs::read_to_string(root.join("crates/plugin/src/disclosure.rs"))
        .expect("disclosure module");
    for engine in ["rquickjs", "ScriptEngine", "script_host"] {
        assert!(!source.contains(engine), "disclosure names `{engine}`");
    }
}
