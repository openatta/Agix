//! A dependency a package declares and its code does not use (UD-1).
//!
//! `cargo machete` reads package directories as text, so it cannot tell a
//! library from its tests and is fooled by a local module named like a
//! dependency; this reads the same syntax trees as the other guards. For each
//! package, the first segment of every path in the production view of its
//! library and binaries is what production uses (a lone identifier only as a
//! `use`, an `extern crate` or a macro: otherwise it is as often a local
//! binding named like the dependency); the full view of every file
//! (test modules, integration tests, examples) is what anything uses. A normal
//! dependency used only by tests belongs in `dev-dependencies`; one used by
//! nothing is deleted. A crate module declared at the root with a
//! dependency's name shadows it, so only `::name::` reaches the dependency.
//!
//! A dependency a feature forwards to (`dep:x`, `x/feat`) is a switch, not a
//! use, and is skipped.

use std::collections::BTreeSet;

use guards::allow::{self, Finding};
use guards::scan;
use guards::{par_flat_map, ws, DepKind, Hit, Package, SourceFile, TargetKind, View};

/// `(package, dependency)`: declared on purpose although no path in its code
/// names it, with why.
const DEP_EXCEPTIONS: &[(&str, &str)] = &[];

/// Packages whose library is not named after the package, other than
/// first-party ones (those come from `cargo metadata`).
const LIB_NAME_OVERRIDES: &[(&str, &str)] = &[];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Used {
    Production,
    Tests,
    Build,
}

type Uses = BTreeSet<(String, Used, String)>;

fn roots(f: &SourceFile, view: View) -> Vec<String> {
    scan::crate_paths(f, view)
        .into_iter()
        .filter_map(|(segments, _)| match segments.as_slice() {
            [colons, name, ..] if colons == "::" => Some(format!("::{name}")),
            [name, ..] => Some(name.clone()),
            [] => None,
        })
        .collect()
}

fn crate_uses() -> Uses {
    par_flat_map(ws().files(), |f| {
        let mut out = Vec::new();
        let mut note = |view: View, used: Used| {
            for name in roots(f, view) {
                out.push((f.package.clone(), used, name));
            }
        };
        if f.target == TargetKind::Build {
            note(View::Full, Used::Build);
            return out;
        }
        note(View::Full, Used::Tests);
        if f.target.is_production() && !f.test_only {
            note(View::Prod, Used::Production);
        }
        out
    })
    .into_iter()
    .collect()
}

fn root_modules(package: &Package) -> BTreeSet<String> {
    ws().files()
        .filter(|f| f.package == package.name && f.target.is_production())
        .filter_map(|f| match f.module.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        })
        .collect()
}

fn lib_ident(dep: &guards::Dep) -> String {
    if let Some((_, lib)) = LIB_NAME_OVERRIDES.iter().find(|(n, _)| *n == dep.name) {
        return lib.to_string();
    }
    if dep.first_party && dep.ident == dep.name.replace('-', "_") {
        if let Some(lib) = &ws().package(&dep.name).lib_name {
            return lib.clone();
        }
    }
    dep.ident.clone()
}

fn forwarded_by_a_feature(package: &Package, dep: &guards::Dep) -> bool {
    package.features.values().flatten().any(|value| {
        let value = value.strip_prefix("dep:").unwrap_or(value);
        value == dep.name
            || value
                .split_once('/')
                .is_some_and(|(d, _)| d.trim_end_matches('?') == dep.name)
    })
}

fn problems() -> Vec<Finding> {
    let uses = crate_uses();
    let mut found = Vec::new();
    for package in ws().packages() {
        let shadowed = root_modules(package);
        let used = |ident: &str, kind: Used| {
            let direct = !shadowed.contains(ident)
                && uses.contains(&(package.name.clone(), kind, ident.to_string()));
            direct || uses.contains(&(package.name.clone(), kind, format!("::{ident}")))
        };
        let manifest = format!("{}/Cargo.toml", package.dir);
        let mut seen = std::collections::HashSet::new();
        for dep in &package.deps {
            if !seen.insert((dep.name.as_str(), dep.kind)) {
                continue;
            }
            if forwarded_by_a_feature(package, dep) {
                continue;
            }
            let ident = lib_ident(dep);
            let in_both_sections = package
                .deps
                .iter()
                .any(|d| d.name == dep.name && d.kind == DepKind::Normal)
                && dep.kind == DepKind::Dev;
            let verdict = match dep.kind {
                DepKind::Normal if used(&ident, Used::Production) => None,
                DepKind::Normal if used(&ident, Used::Tests) => {
                    Some("only tests use it: move it to [dev-dependencies]".to_string())
                }
                DepKind::Normal => Some("nothing uses it: delete it".to_string()),
                DepKind::Dev if in_both_sections || used(&ident, Used::Tests) => None,
                DepKind::Dev => Some("no test uses it: delete it".to_string()),
                DepKind::Build if used(&ident, Used::Build) => None,
                DepKind::Build => Some("build.rs does not use it: delete it".to_string()),
            };
            if let Some(verdict) = verdict {
                found.push(Finding {
                    key: format!("{} {}", package.name, dep.name),
                    hit: Hit {
                        file: manifest.clone(),
                        line: 1,
                        snippet: format!("{} ({:?}): {verdict}", dep.name, dep.kind),
                    },
                });
            }
        }
    }
    found
}

#[test]
fn every_declared_dependency_is_used_where_it_is_declared() {
    allow::check(
        "unused_deps::DEP_EXCEPTIONS",
        problems(),
        DEP_EXCEPTIONS,
        "delete the dependency, or move it to [dev-dependencies] when only tests use it; if it \
         is there on purpose (it only unifies a transitive dependency's features, say), list \
         `(package, dependency)` with the reason",
    );
}

#[test]
fn the_use_scan_sees_what_it_must() {
    let uses = crate_uses();
    for (package, ident) in [
        ("daemon", "tokio"),
        ("assembly", "serde_json"),
        ("hooks", "tokio"),
    ] {
        assert!(
            uses.contains(&(package.to_string(), Used::Production, ident.to_string())),
            "the scan does not see `{ident}` used by `{package}`"
        );
    }
    allow::non_vacuous("unused_deps::uses", uses.len(), 300);
}

#[test]
fn a_local_binding_named_like_a_dependency_is_not_a_use_of_it() {
    let f = guards::source::standalone(
        "a.rs",
        r#"
extern crate base64;
use serde;
fn f() {
    let hex = 1;
    let _ = hex + 1;
    uuid::Uuid::new_v4();
    md5!(hex);
}
"#
        .into(),
    )
    .unwrap();
    let found = roots(&f, View::Prod);
    for used in ["base64", "serde", "uuid", "md5"] {
        assert!(found.iter().any(|r| r == used), "{used}: {found:?}");
    }
    assert!(!found.iter().any(|r| r == "hex"), "{found:?}");
}

#[test]
fn a_local_module_shadows_the_dependency_of_the_same_name_here_too() {
    let mcp = ws().package("mcp");
    assert!(
        root_modules(mcp).contains("tools"),
        "mcp declares a `tools` module at its root: the shadowing rule is how a `tools` \
         dependency would be told from it"
    );
}
