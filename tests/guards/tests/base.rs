//! What `base` — the contracts and kernel services every crate builds on —
//! may link, read from `cargo metadata`: its normal dependencies, target
//! sections included, are exactly `BASE_MAY_DEPEND_ON`. A crate added there
//! is linked into every host, so it needs a reason here first; one the code
//! stopped using has to come off. Dev dependencies are not limited. That base
//! names no other crate of this workspace is `deps.rs`'s rule.
//!
//! base's tokio features stay within `BASE_TOKIO_FEATURES`: no `process`,
//! `signal` or `net`, so `cargo check -p base` proves base starts no process
//! and opens no socket through tokio — the workspace's tokio leaves those
//! three to the crates that ask for them.

use std::collections::BTreeSet;

use guards::allow::{self, Finding};
use guards::{ws, Dep, Hit, Package};

const BASE_TOKIO_FEATURES: &[(&str, &str)] = &[
    ("fs", "the kernel services' own files"),
    ("io-util", "the workspace default"),
    ("macros", "select! and the tests"),
    ("parking_lot", "the workspace default"),
    ("rt", "spawn, spawn_blocking and the runtime handle"),
    ("rt-multi-thread", "the workspace default"),
    ("sync", "channels and locks"),
    ("time", "timeouts"),
];

const BASE_MAY_DEPEND_ON: &[(&str, &str)] = &[
    ("tokio", "files, locks, timers and the runtime handle"),
    ("tokio-util", "CancellationToken in the contracts"),
    ("futures", "the streaming contracts"),
    ("async-trait", "the contracts"),
    ("serde", "the shared data types"),
    ("serde_json", "the shared data types"),
    ("schemars", "the settings schema"),
    ("thiserror", "the error types"),
    ("anyhow", "error codes"),
    ("tracing", "logging"),
    ("uuid", "ids"),
    ("time", "the environment's clock and dates"),
    ("regex", "memory and skill parsing"),
    ("globset", "project_trust patterns and URL rules"),
    ("bs58", "ids"),
    ("url", "URL rules and the public-suffix check"),
    ("libc", "dir_lock's flock, unix only"),
];

fn manifest_hit(p: &Package, dep: &Dep) -> Hit {
    let file = format!("{}/Cargo.toml", p.dir);
    let text = std::fs::read_to_string(ws().root().join(&file)).unwrap_or_default();
    let (line, snippet) = text
        .lines()
        .enumerate()
        .find(|(_, l)| {
            l.trim_start()
                .strip_prefix(dep.name.as_str())
                .is_some_and(|rest| rest.starts_with([' ', '=', '.']))
        })
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .unwrap_or((0, format!("{} = …", dep.name)));
    Hit {
        file,
        line,
        snippet,
    }
}

#[test]
fn base_links_only_the_listed_crates() {
    let base = ws().package("base");
    let found: Vec<Finding> = base
        .normal_deps()
        .map(|d| Finding {
            key: d.name.clone(),
            hit: manifest_hit(base, d),
        })
        .collect();
    allow::non_vacuous("base::BASE_MAY_DEPEND_ON", found.len(), 5);
    allow::check(
        "base::BASE_MAY_DEPEND_ON",
        found,
        BASE_MAY_DEPEND_ON,
        "base is linked into every host: keep a dependency out of it (put the code that needs \
         it in the crate that uses it), or list it with what base needs it for; a dependency \
         base no longer uses comes off both the manifest and the list",
    );
}

#[test]
fn base_builds_tokio_without_processes_signals_or_sockets() {
    let base = ws().package("base");
    let allowed: BTreeSet<&str> = BASE_TOKIO_FEATURES.iter().map(|(f, _)| *f).collect();
    let tokio: Vec<&Dep> = base.normal_deps().filter(|d| d.name == "tokio").collect();
    allow::non_vacuous("base::BASE_TOKIO_FEATURES", tokio.len(), 1);
    let extra: Vec<String> = tokio
        .iter()
        .flat_map(|d| {
            d.features
                .iter()
                .filter(|f| !allowed.contains(f.as_str()))
                .map(|f| format!("{}: `{f}`", manifest_hit(base, d)))
        })
        .collect();
    assert!(
        extra.is_empty(),
        "[base::BASE_TOKIO_FEATURES] base's tokio has features outside the list:\n  {}\n\n\
         How to fix: a process goes through `&dyn Process`, a request through `&dyn Network`; \
         the crate that implements them asks tokio for the feature. The workspace's tokio \
         (root Cargo.toml) counts too, since base inherits it.\n\
         The list is `BASE_TOKIO_FEATURES` in tests/guards/tests/base.rs.",
        extra.join("\n  ")
    );
}
