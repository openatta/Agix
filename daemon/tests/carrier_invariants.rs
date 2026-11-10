//! Extensions come in two kinds: Rust plugins compiled into the host, and
//! QuickJS scripts. The manifest side of that — no WebAssembly runtime, the
//! script carrier optional — is checked in `tests/guards/tests/deps.rs`; what
//! is left here is a rule over the source.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon/ has a parent")
        .to_path_buf()
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
