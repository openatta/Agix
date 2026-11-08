//! The kernel does not depend on modules.
//!
//! Kernel crates may use the contract crate and each other; a module may use
//! both; only the assembly and the shell know which modules exist. A kernel
//! crate that lists a module crate in its `[dependencies]` is how a capability
//! the minimal build cannot shed comes back in, so that fails here — except
//! for the edges listed in `NOT_YET`, each with what removes it.

const KERNEL: &[(&str, &str)] = &[
    ("base", "crates/core"),
    ("model", "crates/model"),
    ("scene", "crates/scene"),
    ("history", "crates/history"),
    ("session", "crates/session"),
    ("telemetry", "crates/telemetry"),
    ("hooks", "crates/hooks"),
    ("permissions", "crates/permissions"),
    ("compaction", "crates/compaction"),
    ("tools", "crates/tools"),
    ("runtime", "crates/runtime"),
];

const MODULES: &[&str] = &[
    "agents",
    "team",
    "mcp",
    "skills",
    "auth",
    "task",
    "plugin",
    "script-host",
];

const NOT_YET: &[(&str, &str, &str)] = &[];

fn dependencies(manifest: &str) -> Vec<String> {
    let mut in_deps = false;
    let mut out = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = line == "[dependencies]";
            continue;
        }
        if in_deps {
            if let Some((name, _)) = line.split_once('=') {
                let name = name.trim();
                if !name.is_empty() && !name.starts_with('#') {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

#[test]
fn no_kernel_crate_depends_on_a_module() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
    for (kernel, dir) in KERNEL {
        let manifest = std::fs::read_to_string(root.join(dir).join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("{dir}/Cargo.toml: {e}"));
        for dep in dependencies(&manifest) {
            if MODULES.contains(&dep.as_str())
                && !NOT_YET.iter().any(|(k, m, _)| k == kernel && *m == dep)
            {
                found.push(format!("{kernel} → {dep}"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "kernel crates depend on modules: {found:?}. Reach the module through a \
         kernel trait or a `runtime::module::Module` instead."
    );
}

#[test]
fn every_edge_still_waiting_still_exists() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for (kernel, module, _) in NOT_YET {
        let dir = KERNEL
            .iter()
            .find(|(k, _)| k == kernel)
            .map(|(_, d)| *d)
            .unwrap_or_else(|| panic!("{kernel} is not a kernel crate"));
        let manifest = std::fs::read_to_string(root.join(dir).join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("{dir}/Cargo.toml: {e}"));
        assert!(
            dependencies(&manifest).iter().any(|d| d == module),
            "{kernel} no longer depends on {module}: take it off NOT_YET"
        );
    }
}
