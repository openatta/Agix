//! The kernel does not depend on modules, and the contracts do not depend on
//! this machine.
//!
//! Kernel crates may use the contract crate and each other; a module may use
//! both; only the assembly and the shell know which modules exist. A kernel
//! crate that lists a module crate in its `[dependencies]` is how a capability
//! the minimal build cannot shed comes back in, so that fails here — except
//! for the edges listed in `NOT_YET`, each with what removes it.
//!
//! `platform` — this machine's execution providers — sits on `base` alone,
//! and only the crates in `MAY_USE_PLATFORM` reach the machine through it.

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
    ("ptc", "crates/ptc"),
];

const MODULES: &[&str] = &["agents", "team", "mcp", "skills", "plugin", "script-host"];

const NOT_YET: &[(&str, &str, &str)] = &[];

/// The crates whose `[dependencies]` may name `platform`, each with what it
/// does on this machine. The rest only implement or consume contracts, which
/// is what lets a host that executes elsewhere use them unchanged.
const MAY_USE_PLATFORM: &[(&str, &str, &str)] = &[
    (
        "tools",
        "crates/tools",
        "the sandbox status and probes `Bash` reports",
    ),
    (
        "runtime",
        "crates/runtime",
        "builds a session's providers when the host gives none",
    ),
    ("model", "crates/model", "the model endpoint's HTTP client"),
    (
        "hooks",
        "crates/hooks",
        "command hooks are local processes, HTTP hooks local requests",
    ),
    (
        "permissions",
        "crates/permissions",
        "the providers a permission check judges paths against when none is bound",
    ),
    (
        "mcp",
        "crates/mcp",
        "stdio servers are local processes; OAuth is a local HTTP client",
    ),
    (
        "plugin",
        "crates/plugin",
        "fetches packages over this machine's network",
    ),
    (
        "agents",
        "crates/agents",
        "a sub-agent's commands record their process groups in the parent's ledger",
    ),
    (
        "assembly",
        "crates/assembly",
        "fences the sockets it serves from the sandbox; the discovery lock's process identity",
    ),
    (
        "daemon",
        "daemon",
        "the crash hook and reaping a previous run's commands",
    ),
    (
        "live",
        "tests/live",
        "installs the TLS provider for its own clients",
    ),
];

const FIRST_PARTY: &[&str] = &[
    "base",
    "platform",
    "model",
    "scene",
    "history",
    "session",
    "telemetry",
    "hooks",
    "permissions",
    "compaction",
    "tools",
    "runtime",
    "ptc",
    "agents",
    "team",
    "mcp",
    "skills",
    "plugin",
    "script-host",
    "assembly",
    "daemon",
];

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

/// A name in either list that is not a crate checks nothing and fails
/// nothing, so only this notices one.
#[test]
fn every_listed_crate_is_a_workspace_member() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let package_names: Vec<String> = std::fs::read_dir(root.join("crates"))
        .expect("crates/ is readable")
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("Cargo.toml")).ok())
        .filter_map(|manifest| {
            manifest
                .lines()
                .map(str::trim)
                .find_map(|l| l.strip_prefix("name"))
                .and_then(|rest| rest.split('"').nth(1))
                .map(str::to_string)
        })
        .collect();
    let workspace = std::fs::read_to_string(root.join("Cargo.toml")).expect("workspace manifest");

    for (kernel, dir) in KERNEL {
        assert!(
            workspace.contains(&format!("\"{dir}\"")),
            "KERNEL lists {kernel} at {dir}, which is not a workspace member"
        );
    }
    for module in MODULES {
        assert!(
            package_names.iter().any(|n| n == module),
            "MODULES lists {module}, which no crate under crates/ is called"
        );
    }
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

#[test]
fn platform_sits_on_the_contracts_alone() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let read = |dir: &str| {
        std::fs::read_to_string(root.join(dir).join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("{dir}/Cargo.toml: {e}"))
    };
    let base_uses: Vec<String> = dependencies(&read("crates/core"))
        .into_iter()
        .filter(|d| FIRST_PARTY.contains(&d.as_str()))
        .collect();
    assert!(
        base_uses.is_empty(),
        "base depends on {base_uses:?}; the contracts name no other crate of this workspace"
    );
    let platform_uses: Vec<String> = dependencies(&read("crates/platform"))
        .into_iter()
        .filter(|d| FIRST_PARTY.contains(&d.as_str()) && d != "base")
        .collect();
    assert!(
        platform_uses.is_empty(),
        "platform depends on {platform_uses:?}; it implements base's contracts and nothing above them"
    );
}

#[test]
fn only_the_listed_crates_reach_the_machine_through_platform() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut manifests = Vec::new();
    for top in ["crates", "tests", "examples"] {
        let Ok(entries) = std::fs::read_dir(root.join(top)) else {
            continue;
        };
        for entry in entries.flatten() {
            let rel = format!("{top}/{}", entry.file_name().to_string_lossy());
            if let Ok(manifest) = std::fs::read_to_string(entry.path().join("Cargo.toml")) {
                manifests.push((rel, manifest));
            }
        }
    }
    manifests.push((
        "daemon".to_string(),
        std::fs::read_to_string(root.join("daemon/Cargo.toml")).expect("daemon manifest"),
    ));

    let mut unlisted = Vec::new();
    for (dir, manifest) in &manifests {
        if dependencies(manifest).iter().any(|d| d == "platform")
            && !MAY_USE_PLATFORM.iter().any(|(_, d, _)| d == dir)
        {
            unlisted.push(dir.clone());
        }
    }
    assert!(
        unlisted.is_empty(),
        "{unlisted:?} depend on platform without being in MAY_USE_PLATFORM: take the \
         providers from the `ToolContext` or the session, or list the crate with what it \
         does on this machine"
    );

    for (name, dir, _) in MAY_USE_PLATFORM {
        let manifest = manifests
            .iter()
            .find(|(d, _)| d == dir)
            .map(|(_, m)| m)
            .unwrap_or_else(|| {
                panic!("MAY_USE_PLATFORM lists {name} at {dir}, which has no manifest")
            });
        assert!(
            dependencies(manifest).iter().any(|d| d == "platform"),
            "{name} no longer depends on platform: take it off MAY_USE_PLATFORM"
        );
    }
}
