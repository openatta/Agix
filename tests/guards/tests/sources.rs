//! Rules over what the source says, found in the syntax tree rather than in
//! the text: no crate finds its own `$HOME` (SRC-1), a plugin's disclosure
//! names no script engine (SRC-2), and every `.rs` file in a package belongs
//! to a target (SRC-3).
//!
//! Where an instance keeps its state is decided once, by the process entry
//! point, and passed down. A module that reads `$HOME` instead has quietly
//! opted out: it writes to the invoking user's home no matter what the
//! instance was configured with, which breaks redirected deployments and
//! leaves test runs writing to a real `~/.agix`. Two things earn a place in
//! `HOME_READERS`: the entry point, and facts about the machine — the sandbox
//! has to know the real home to protect `~/.ssh`, and a system prompt
//! describing the environment describes the user's. A storage root never
//! qualifies; the answer there is to take the root as a parameter.

use std::collections::BTreeSet;

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{fswalk, par_flat_map, ws, View};
use syn::{Expr, Lit};

const HOME_READERS: &[(&str, &str)] = &[
    (
        "crates/assembly/src/config.rs",
        "the entry point: resolves AGIX_CONFIG_HOME (or $HOME/.agix) once, \
         then passes it down. Everything else receives what this decides.",
    ),
    (
        "crates/permissions/src/dangerous.rs",
        "judges whether a grant reaches the user's home or above — a fact \
         about the machine the rule will run on, not a place to store things.",
    ),
    (
        "crates/core/src/frozen/mod.rs",
        "collects the environment snapshot — home alongside os, shell and \
         git state. A fact about the machine, put into the system prompt, \
         and replaceable by tests because it lives in the snapshot.",
    ),
    (
        "crates/core/src/interface/exec/sandbox.rs",
        "the default deny-read list is ~/.ssh and friends, so building it \
         needs this machine's home. It is policy, which is why it sits with \
         the contract rather than with a backend.",
    ),
    (
        "crates/platform/src/sandbox.rs",
        "a backend confines a process on this machine, and the state root it \
         protects falls back to $HOME/.agix when the policy does not name one. \
         Where Agix's own settings.json lives comes from \
         `SandboxPolicy::state_root` first.",
    ),
    (
        "crates/core/src/project_trust.rs",
        "expands a leading `~/` in `project_trust.trusted` patterns, the same \
         `~` permission rules expand — the home the user means by \
         `~/work/**`. A fact about the machine, never a place Agix writes.",
    ),
    (
        "crates/permissions/src/path_rule.rs",
        "expands `~` in permission rules and in the paths tools are asked to \
         touch — the home a shell expands `~` to when the command runs. A fact \
         about the machine, never a place Agix writes.",
    ),
    (
        "crates/tools/src/bash/classify/mod.rs",
        "same deny-list, reused by the command classifier so `cat ~/.ssh/id_rsa` \
         is not waved through as a read-only command.",
    ),
    (
        "crates/tools/src/bash/sandbox.rs",
        "the `~/` of `sandbox.cache_directories`, where pip and npm keep their \
         caches.",
    ),
    (
        "crates/tools/src/bash/tests.rs",
        "the tests check both of the above against this machine's home.",
    ),
];

const ENGINE_MODULES: &[&str] = &["rquickjs", "script_host"];
const ENGINE_TYPES: &[&str] = &["ScriptEngine"];

const DISCLOSURE: &str = "crates/plugin/src/disclosure.rs";

const NOT_IN_A_TARGET: &[(&str, &str)] = &[];

const SELFTEST_FIXTURES: &str = "tests/guards/fixtures/";

const HOME_VARS: &[&str] = &["HOME", "USERPROFILE"];

fn is_home_var_call(segments: &[String], args: &[Expr]) -> bool {
    let reads_env =
        matches!(segments, [.., env, var] if env == "env" && (var == "var" || var == "var_os"));
    reads_env
        && matches!(
            args.first(),
            Some(Expr::Lit(l)) if matches!(&l.lit, Lit::Str(s) if HOME_VARS.contains(&s.value().as_str()))
        )
}

fn home_lookups() -> Vec<Finding> {
    par_flat_map(ws().files(), |f| {
        scan::calls(f, View::Full)
            .into_iter()
            .filter(|c| match &c.callee {
                Callee::Path(p) => {
                    is_home_var_call(p, &c.args) || p.last().is_some_and(|s| s == "home_dir")
                }
                Callee::Method(_) => false,
            })
            .map(|c| Finding {
                key: c.hit.file.clone(),
                hit: c.hit,
            })
            .collect()
    })
}

#[test]
fn only_the_entry_point_and_environment_facts_resolve_a_home_directory() {
    allow::check(
        "sources::HOME_READERS",
        home_lookups(),
        HOME_READERS,
        "take the state root as a parameter (see `base::paths::ConfigPaths`); if this really \
         is a fact about the machine rather than a place to store things, list the file with \
         the reason",
    );
}

#[test]
fn the_home_scan_recognises_every_way_of_asking() {
    let src = r#"
        use std::env;
        fn a() {
            let _ = std::env::var("HOME");
            let _ = env::var_os("USERPROFILE");
            let _ = dirs::home_dir();
            let _ = env::var("PATH");
            // std::env::var("HOME")
        }
    "#;
    let f = guards::source::standalone("fixture.rs", src.to_string()).expect("parses");
    let found: Vec<usize> = scan::calls(&f, View::Full)
        .into_iter()
        .filter(|c| match &c.callee {
            Callee::Path(p) => {
                is_home_var_call(p, &c.args) || p.last().is_some_and(|s| s == "home_dir")
            }
            Callee::Method(_) => false,
        })
        .map(|c| c.hit.line)
        .collect();
    assert_eq!(found, vec![4, 5, 6]);
    allow::non_vacuous("sources::home_lookups", ws().files().count(), 400);
}

#[test]
fn plugin_disclosure_names_no_script_engine() {
    let file = ws().file(DISCLOSURE);
    let paths = scan::paths(file, View::Prod);
    allow::non_vacuous("sources::disclosure", paths.len(), 10);
    let offenders: Vec<String> = paths
        .iter()
        .filter(|(segments, _)| {
            segments
                .first()
                .is_some_and(|s| ENGINE_MODULES.contains(&s.as_str()))
                || segments.iter().any(|s| ENGINE_TYPES.contains(&s.as_str()))
        })
        .map(|(_, hit)| hit.to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "{DISCLOSURE} names a script engine:\n  {}\n\nDisclosure is about what a package \
         says; it must not depend on how its scripts run. Move the engine-specific part to \
         the script host.",
        offenders.join("\n  ")
    );
}

#[test]
fn every_rust_file_in_a_package_belongs_to_a_target() {
    let known: BTreeSet<&str> = ws().files().map(|f| f.rel.as_str()).collect();
    let mut found = Vec::new();
    let mut seen = 0;
    for package in ws().packages() {
        for path in fswalk::rust_files(&ws().root().join(&package.dir)) {
            seen += 1;
            let rel = path
                .strip_prefix(ws().root())
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !rel.starts_with(SELFTEST_FIXTURES) && !known.contains(rel.as_str()) {
                found.push(Finding {
                    hit: guards::Hit {
                        file: rel.clone(),
                        line: 1,
                        snippet: "no target's module tree reaches this file".to_string(),
                    },
                    key: rel,
                });
            }
        }
    }
    allow::non_vacuous("sources::orphans", seen, 400);
    allow::check(
        "sources::NOT_IN_A_TARGET",
        found,
        NOT_IN_A_TARGET,
        "declare the file with `mod`, or delete it: the compiler never reads it, so nothing \
         it says is checked",
    );
}
