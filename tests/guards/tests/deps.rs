//! Which crate may depend on which, read from `cargo metadata`.
//!
//! Every workspace member has one tier in `CRATES`, and a first-party normal or
//! build edge (any target, optional or not) points to a lower tier. The kernel
//! is three tiers so that its crates cannot lean on each other in a circle: K1
//! needs only the contracts and the machine, K2 builds on K1, K3 (`runtime`)
//! on both. Modules sit on the kernel and reach each other only through the
//! edges in `MODULE_EDGES`; only the assembly and the shell know which modules
//! exist. Test support may use anything and nothing else may use it; dev edges
//! are not limited.
//!
//! `platform` — this machine's execution providers — sits on `base` alone, and
//! only the crates in `MAY_USE_PLATFORM` reach the machine through it.

use std::collections::{BTreeMap, BTreeSet};

use guards::allow::{self, Finding};
use guards::{ws, Dep, Hit, Package};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Contract,
    Machine,
    K1,
    K2,
    K3,
    Module,
    Assembly,
    Shell,
    TestSupport,
}

use Tier::*;

const CRATES: &[(&str, Tier)] = &[
    ("base", Contract),
    ("platform", Machine),
    ("model", K1),
    ("history", K1),
    ("telemetry", K1),
    ("scene", K1),
    ("permissions", K1),
    ("ptc", K1),
    ("hooks", K2),
    ("tools", K2),
    ("session", K2),
    ("compaction", K2),
    ("runtime", K3),
    ("team", Module),
    ("plugin", Module),
    ("script-host", Module),
    ("mcp", Module),
    ("skills", Module),
    ("agents", Module),
    ("demo-rust-plugin", Module),
    ("assembly", Assembly),
    ("daemon", Shell),
    ("rpc-client", TestSupport),
    ("live", TestSupport),
    ("test-runner", TestSupport),
    ("mcp-toy-server", TestSupport),
    ("mcp-oauth-mock", TestSupport),
    ("guards", TestSupport),
];

/// Module → module edges, each with why the one module needs the other.
const MODULE_EDGES: &[(&str, &str, &str)] = &[(
    "agents",
    "team",
    "the team tools live in agents, and team is the coordinator they drive",
)];

/// Edges between two crates of one tier below the modules, each with why.
const SAME_TIER: &[(&str, &str, &str)] = &[];

/// Edges that break the tiers today, each with what removes it.
const NOT_YET: &[(&str, &str, &str)] = &[];

/// The crates whose normal or build dependencies may name `platform`, each
/// with what it does on this machine. The rest only implement or consume
/// contracts, which is what lets a host that executes elsewhere use them
/// unchanged.
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

const WASM_RUNTIMES: &[&str] = &["wasmtime", "wasmer", "wasmi"];

fn tier(name: &str) -> Tier {
    CRATES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, t)| *t)
        .unwrap_or_else(|| panic!("`{name}` has no tier in CRATES (tests/guards/tests/deps.rs)"))
}

/// Normal and build edges into the workspace.
fn first_party_edges(p: &Package) -> impl Iterator<Item = &Dep> {
    p.normal_deps().filter(|d| d.first_party)
}

fn manifest_hit(p: &Package, dep: &str) -> Hit {
    let file = if p.dir.is_empty() {
        "Cargo.toml".to_string()
    } else {
        format!("{}/Cargo.toml", p.dir)
    };
    let text = std::fs::read_to_string(ws().root().join(&file)).unwrap_or_default();
    let declares = |line: &str| {
        let line = line.trim_start();
        line.strip_prefix(dep)
            .is_some_and(|rest| rest.starts_with([' ', '=', '.']))
            || (line.starts_with('[') && line.contains(&format!("dependencies.{dep}]")))
    };
    let (line, snippet) = text
        .lines()
        .enumerate()
        .find(|(_, l)| declares(l))
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .unwrap_or((0, format!("{dep} = …")));
    Hit {
        file,
        line,
        snippet,
    }
}

fn edge(p: &Package, dep: &str) -> Finding {
    Finding {
        key: format!("{} → {dep}", p.name),
        hit: manifest_hit(p, dep),
    }
}

fn keyed(list: &[(&'static str, &'static str, &'static str)]) -> Vec<(String, &'static str)> {
    list.iter()
        .map(|(a, b, why)| (format!("{a} → {b}"), *why))
        .collect()
}

fn check_keyed(
    guard: &str,
    found: Vec<Finding>,
    list: &[(&'static str, &'static str, &'static str)],
    fix: &str,
) {
    let keyed = keyed(list);
    let allowed: Vec<(&str, &str)> = keyed.iter().map(|(k, why)| (k.as_str(), *why)).collect();
    allow::check(guard, found, &allowed, fix);
}

#[test]
fn every_member_has_a_tier_and_every_tier_is_a_member() {
    let members: BTreeSet<&str> = ws().packages().iter().map(|p| p.name.as_str()).collect();
    let listed: BTreeMap<&str, usize> = CRATES.iter().fold(BTreeMap::new(), |mut m, (n, _)| {
        *m.entry(*n).or_insert(0) += 1;
        m
    });
    let unlisted: Vec<&&str> = members
        .iter()
        .filter(|m| !listed.contains_key(**m))
        .collect();
    let gone: Vec<&&str> = listed.keys().filter(|n| !members.contains(**n)).collect();
    let twice: Vec<&&str> = listed
        .iter()
        .filter(|(_, c)| **c > 1)
        .map(|(n, _)| n)
        .collect();
    assert!(
        unlisted.is_empty() && gone.is_empty() && twice.is_empty(),
        "CRATES in tests/guards/tests/deps.rs is out of step with the workspace:\n  \
         members with no tier: {unlisted:?} (give each one a tier)\n  \
         entries that are no member: {gone:?} (renamed or removed: take them off)\n  \
         entries listed twice: {twice:?}"
    );
}

#[derive(Default)]
struct Edges {
    upward: Vec<Finding>,
    between_modules: Vec<Finding>,
    same_tier: Vec<Finding>,
}

/// Every first-party edge that does not simply point down, by what may
/// excuse it.
fn edges() -> Edges {
    let mut out = Edges::default();
    let mut count = 0;
    for p in ws().packages() {
        let from = tier(&p.name);
        let mut seen = BTreeSet::new();
        for d in first_party_edges(p) {
            if !seen.insert(d.name.as_str()) {
                continue;
            }
            count += 1;
            let to = tier(&d.name);
            if from == TestSupport {
                continue;
            }
            if to == TestSupport || to > from {
                out.upward.push(edge(p, &d.name));
            } else if to == from && from == Module {
                out.between_modules.push(edge(p, &d.name));
            } else if to == from {
                out.same_tier.push(edge(p, &d.name));
            }
        }
    }
    allow::non_vacuous("deps::first-party edges", count, 80);
    out
}

#[test]
fn no_first_party_edge_points_up() {
    check_keyed(
        "deps::NOT_YET",
        edges().upward,
        NOT_YET,
        "depend on a lower tier — reach the higher crate through a kernel trait or a \
         `runtime::module::Module` — or, if the edge must stay for now, list it with what removes it",
    );
}

#[test]
fn a_module_reaches_another_module_only_through_a_listed_edge() {
    check_keyed(
        "deps::MODULE_EDGES",
        edges().between_modules,
        MODULE_EDGES,
        "list the edge with why the one module needs the other, or go through a kernel trait",
    );
}

#[test]
fn crates_of_one_tier_below_the_modules_do_not_depend_on_each_other() {
    check_keyed(
        "deps::SAME_TIER",
        edges().same_tier,
        SAME_TIER,
        "move one of the crates to another tier in CRATES, or list the edge with its reason",
    );
}

#[test]
fn only_the_listed_crates_reach_the_machine_through_platform() {
    let mut found = Vec::new();
    for p in ws().packages() {
        if first_party_edges(p).any(|d| d.name == "platform") {
            found.push(Finding {
                key: p.name.clone(),
                hit: manifest_hit(p, "platform"),
            });
        }
    }
    let allowed: Vec<(&str, &str)> = MAY_USE_PLATFORM
        .iter()
        .map(|(n, _, why)| (*n, *why))
        .collect();
    allow::check(
        "deps::MAY_USE_PLATFORM",
        found,
        &allowed,
        "take the providers from the `ToolContext` or the session, or list the crate with what \
         it does on this machine",
    );
    for (name, dir, _) in MAY_USE_PLATFORM {
        assert_eq!(
            &ws().package(name).dir,
            dir,
            "MAY_USE_PLATFORM lists {name} at {dir}, which is not where it lives"
        );
    }
}

#[test]
fn the_contracts_and_the_machine_sit_at_the_bottom() {
    let base: Vec<&str> = first_party_edges(ws().package("base"))
        .map(|d| d.name.as_str())
        .collect();
    assert!(
        base.is_empty(),
        "base depends on {base:?}; the contracts name no other crate of this workspace"
    );
    let platform: Vec<&str> = first_party_edges(ws().package("platform"))
        .map(|d| d.name.as_str())
        .filter(|d| *d != "base")
        .collect();
    assert!(
        platform.is_empty(),
        "platform depends on {platform:?}; it implements base's contracts and nothing above them"
    );
}

#[test]
fn no_crate_links_a_webassembly_runtime() {
    let mut found = Vec::new();
    for p in ws().packages() {
        for d in &p.deps {
            let is_wasm = WASM_RUNTIMES
                .iter()
                .any(|r| d.name == *r || d.name.starts_with(&format!("{r}-")));
            if is_wasm {
                found.push(manifest_hit(p, &d.name).to_string());
            }
        }
    }
    assert!(
        found.is_empty(),
        "a WebAssembly runtime is linked; extensions are Rust plugins or QuickJS scripts:\n  {}",
        found.join("\n  ")
    );
}

/// A build without `scripts` must link no JavaScript engine for extensions.
#[test]
fn the_script_carrier_is_optional_at_compile_time() {
    for name in ["assembly", "daemon"] {
        let p = ws().package(name);
        let declared: Vec<&Dep> = p
            .normal_deps()
            .filter(|d| d.name == "script-host")
            .collect();
        assert!(!declared.is_empty(), "{name} must declare `script-host`");
        for d in declared {
            assert!(
                d.optional,
                "{name}'s `script-host` must be optional: {}",
                manifest_hit(p, "script-host")
            );
        }
        let scripts = p
            .features
            .get("scripts")
            .unwrap_or_else(|| panic!("{name} has no `scripts` feature"));
        assert!(
            scripts.iter().any(|f| f == "dep:script-host"),
            "{name}'s `scripts` feature must turn on `dep:script-host`: {scripts:?}"
        );
    }
}

/// Module switches are the assembly's: a kernel crate that declared or
/// forwarded one would put a module's `#[cfg]` below the assembly.
#[test]
fn no_kernel_crate_declares_or_forwards_a_module_feature() {
    let modules: BTreeSet<&str> = CRATES
        .iter()
        .filter(|(_, t)| *t == Module)
        .map(|(n, _)| *n)
        .collect();
    let switches: BTreeSet<&str> = ws()
        .package("assembly")
        .features
        .iter()
        .filter(|(_, enables)| {
            enables
                .iter()
                .any(|e| e.strip_prefix("dep:").is_some_and(|d| modules.contains(d)))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    allow::non_vacuous("deps::module switches", switches.len(), 4);

    let mut found = Vec::new();
    for p in ws().packages().iter().filter(|p| tier(&p.name) <= K3) {
        for (feature, enables) in &p.features {
            if modules.contains(feature.as_str()) || switches.contains(feature.as_str()) {
                found.push(format!("{}: declares `{feature}`", p.name));
            }
            for e in enables {
                let (krate, sub) = match e.strip_prefix("dep:") {
                    Some(d) => (d, None),
                    None => match e.split_once('/') {
                        Some((k, f)) => (k.trim_end_matches('?'), Some(f)),
                        None => (e.as_str(), None),
                    },
                };
                let names_a_module =
                    modules.contains(krate) || krate == "assembly" || krate == "daemon";
                let forwards_a_switch = sub.is_some_and(|f| switches.contains(f));
                if names_a_module || forwards_a_switch {
                    found.push(format!("{}: `{feature}` enables `{e}`", p.name));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "kernel crates carry module features; a module's `#[cfg]` belongs in assembly and daemon \
         only:\n  {}",
        found.join("\n  ")
    );
}
