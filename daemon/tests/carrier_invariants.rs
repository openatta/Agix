//! The four rules that hold for every extension carrier, as tests.
//!
//! WebAssembly is one carrier and a script engine is the next. The failure
//! mode when a second carrier arrives is not that it is written badly — it is
//! that it is written *separately*: its own allow-list, its own idea of what a
//! declaration means, its own path from an extension to the host. Each of
//! those drifts from the first carrier's, and the drift is discovered by an
//! incident rather than by a test.
//!
//! So the rules are checked here rather than remembered:
//!
//! 1. One capability table and one authorization function, shared.
//! 2. Carriers do not call each other.
//! 3. A carrier is a compile-time feature.
//! 4. Disclosure covers every carrier, not just the sandboxed one.
//!
//! Same technique and the same caveat as `hook_event_wiring.rs`: a source-text
//! scan, not call-graph analysis. It answers "does this crate's source reach
//! for that", which is the question that matters for this bug class.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon/ has a parent")
        .to_path_buf()
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}

fn read_all(dir: &Path) -> Vec<(PathBuf, String)> {
    rust_sources(dir)
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|s| (p, s)))
        .collect()
}

/// Strip `#[cfg(test)]` bodies as crudely as line-based scanning allows: a
/// test may legitimately name things production code may not.
fn without_tests(source: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    let mut depth = 0i32;
    for line in source.lines() {
        if !skipping && line.trim_start().starts_with("#[cfg(test)]") {
            skipping = true;
            depth = 0;
            continue;
        }
        if skipping {
            depth += line.matches('{').count() as i32;
            depth -= line.matches('}').count() as i32;
            if depth <= 0 && line.contains('}') {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// **Rule 1.** The capability table and its predicates live in the kernel, and
/// a carrier converts its manifest into the kernel's declaration rather than
/// answering the question itself.
///
/// The check is for a *second* implementation: a carrier defining its own
/// `allows_…` predicate is a carrier with its own allow-list, whatever it is
/// called.
#[test]
fn no_carrier_defines_its_own_authorization() {
    let root = repo_root();
    let kernel = root.join("crates/core/src/interface/capabilities.rs");
    let kernel_src = std::fs::read_to_string(&kernel).expect("the kernel table exists");
    for predicate in [
        "fn allows_url",
        "fn allows_env",
        "fn allows_read",
        "fn allows_write",
    ] {
        assert!(
            kernel_src.contains(predicate),
            "the kernel table must define `{predicate}`"
        );
    }

    for carrier in [
        "crates/wasm-host/src",
        "crates/plugin-host/src",
        "crates/script-host/src",
    ] {
        for (path, source) in read_all(&root.join(carrier)) {
            let production = without_tests(&source);
            for predicate in [
                "fn allows_url",
                "fn allows_env",
                "fn allows_read",
                "fn allows_write",
            ] {
                assert!(
                    !production.contains(predicate),
                    "{} defines `{predicate}` — authorization belongs to \
                     base::interface::capabilities, and a second copy is a second \
                     allow-list that will drift from it",
                    path.display()
                );
            }
        }
    }
}

/// **Rule 2.** Carriers reach each other only through the host's contracts.
/// A direct call across memory models is the kind of shortcut that makes both
/// carriers' isolation arguments false at once.
#[test]
fn carriers_do_not_reach_for_each_other() {
    let root = repo_root();

    for (path, source) in read_all(&root.join("crates/wasm-host/src")) {
        let production = without_tests(&source);
        assert!(
            !production.contains("interface::script"),
            "{} names the script carrier; carriers talk through host contracts only",
            path.display()
        );
    }

    for script_side in [
        "crates/core/src/interface/script.rs",
        "crates/script-host/src/lib.rs",
    ] {
        let path = root.join(script_side);
        let production = without_tests(&std::fs::read_to_string(&path).expect("script carrier"));
        for forbidden in ["wasm_host", "wasmtime"] {
            assert!(
                !production.contains(forbidden),
                "{} names `{forbidden}`; carriers talk through host contracts only",
                path.display()
            );
        }
    }
}

/// **Rule 3.** Every carrier is a compile-time feature, independently. A build
/// carries one, both, or neither — dropping the plugin tier must not drag a
/// WebAssembly runtime in, and a build with no scripts must link no JavaScript
/// engine.
///
/// `tests/scripts/locked_build.sh` asserts the same thing against a real
/// dependency graph; this asserts the declarations that make it possible, so a
/// carrier quietly becoming non-optional fails here rather than in whatever
/// ships.
#[test]
fn every_carrier_is_optional_at_compile_time() {
    let root = repo_root();
    let manifest = std::fs::read_to_string(root.join("crates/assembly/Cargo.toml"))
        .expect("assembly manifest");

    for (dependency, feature) in [
        ("plugin-host", "plugins = ["),
        ("script-host", "scripts = ["),
    ] {
        let declared = manifest
            .lines()
            .find(|l| l.trim_start().starts_with(dependency))
            .unwrap_or_else(|| panic!("assembly must declare `{dependency}`"));
        assert!(
            declared.contains("optional = true"),
            "`{dependency}` must be an optional dependency of assembly, found: {declared}"
        );
        assert!(
            manifest.contains(feature),
            "there must be a `{feature}…]` feature to turn `{dependency}` off"
        );
    }
}

/// **Rule 3, the other half.** Optional is not enough: two optional features
/// can both be on, and cargo's unification means that is reachable without
/// anyone choosing it. The guard that refuses it lives in `lib.rs`, where a
/// grep for the feature names would not find it — so this looks for it by
/// name, and `locked_build.sh` proves it actually fires.
#[test]
fn the_two_carriers_cannot_both_be_compiled_in() {
    let root = repo_root();
    let lib =
        std::fs::read_to_string(root.join("crates/assembly/src/lib.rs")).expect("assembly lib");
    let guard = lib
        .lines()
        .position(|l| l.contains(r#"cfg(all(feature = "scripts", feature = "plugins"))"#))
        .expect("crates/assembly/src/lib.rs must refuse a build carrying both carriers");
    assert!(
        lib.lines()
            .skip(guard)
            .take(3)
            .any(|l| l.contains("compile_error!")),
        "the both-carriers cfg must be attached to a `compile_error!`, not to \
         something that merely warns"
    );

    let manifest =
        std::fs::read_to_string(root.join("daemon/Cargo.toml")).expect("daemon manifest");
    let default = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("default = ["))
        .expect("daemon must declare default features");
    assert!(
        default.contains("scripts") && !default.contains("plugins"),
        "the script carrier is the default one; found: {default}"
    );
}

/// **Rule 4.** Disclosure is about what an extension *says*, not about how it
/// runs, so it cannot be a property of one carrier. This checks the module
/// that builds it reaches for the manifest and not for a runtime.
#[test]
fn disclosure_is_carrier_neutral() {
    let root = repo_root();
    let path = root.join("crates/plugin/src/disclosure.rs");
    let source = std::fs::read_to_string(&path).expect("disclosure module");
    let production = without_tests(&source);
    for carrier_specific in ["wasmtime", "wasm_host", "Component", "ScriptEngine"] {
        assert!(
            !production.contains(carrier_specific),
            "disclosure names `{carrier_specific}` — it would then apply to one carrier \
             and quietly not to the next"
        );
    }
    // And it is reachable without the plugin runtime at all: `plugin` has no
    // internal dependencies, so nothing here can be carrier-specific even by
    // accident.
    let manifest = std::fs::read_to_string(root.join("crates/plugin/Cargo.toml")).unwrap();
    let deps = manifest
        .split("[dependencies]")
        .nth(1)
        .expect("plugin has a dependency section");
    for internal in ["wasm-host", "plugin-host", "base ="] {
        assert!(
            !deps.contains(internal),
            "the disclosure crate depends on `{internal}`, which ties it to a carrier"
        );
    }
}
