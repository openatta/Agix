//! References from one file to modules of its own crate, as absolute paths
//! from the crate root.
//!
//! `crate::`, `self::`, `super::` (any depth), a child module named bare,
//! `use` groups and one-level `use` aliases are resolved; a first segment the
//! crate root re-exports with `pub use` is replaced by where it comes from.
//! Paths into other crates are not reported.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::scan;
use crate::source::{SourceFile, View};
use crate::workspace::ws;
use crate::Hit;

pub fn module_refs(f: &SourceFile, v: View) -> Vec<(Vec<String>, Hit)> {
    let root = if f.in_workspace() && !f.module.is_empty() {
        ws().files().find(|r| {
            r.package == f.package
                && r.target == f.target
                && r.module.is_empty()
                && r.in_workspace()
        })
    } else {
        None
    };
    module_refs_with_root(f, v, root)
}

pub fn module_refs_with_root(
    f: &SourceFile,
    v: View,
    root: Option<&SourceFile>,
) -> Vec<(Vec<String>, Hit)> {
    let children = child_mods(&f.tree(v).items);
    let reexports = match root {
        Some(r) => reexports(r),
        None if f.module.is_empty() => reexports(f),
        None => HashMap::new(),
    };
    let root_mods = match root {
        Some(r) => child_mods(&r.tree(View::Full).items)
            .remove(&Vec::new())
            .unwrap_or_default(),
        None if f.module.is_empty() => children.get(&Vec::new()).cloned().unwrap_or_default(),
        None => BTreeSet::new(),
    };
    let mut out = Vec::new();
    for p in scan::scoped_paths(f, v) {
        let mut base = f.module.clone();
        base.extend(p.mods.iter().cloned());
        let local = children.get(&p.mods);
        let Some(mut resolved) = resolve(&p.segments, &base, local) else {
            continue;
        };
        if let Some(first) = resolved.first() {
            if !root_mods.contains(first) {
                if let Some(origin) = reexports.get(first) {
                    let mut replaced = origin.clone();
                    replaced.extend(resolved.drain(1..));
                    resolved = replaced;
                }
            }
        }
        out.push((resolved, p.hit));
    }
    out
}

fn resolve(
    segs: &[String],
    base: &[String],
    local_mods: Option<&BTreeSet<String>>,
) -> Option<Vec<String>> {
    let first = segs.first()?;
    match first.as_str() {
        "crate" => Some(segs[1..].to_vec()),
        "self" => {
            let mut out = base.to_vec();
            out.extend(segs[1..].iter().cloned());
            Some(out)
        }
        "super" => {
            let mut out = base.to_vec();
            let mut rest = segs;
            while rest.first().is_some_and(|s| s == "super") {
                out.pop()?;
                rest = &rest[1..];
            }
            out.extend(rest.iter().cloned());
            Some(out)
        }
        name if local_mods.is_some_and(|m| m.contains(name)) => {
            let mut out = base.to_vec();
            out.extend(segs.iter().cloned());
            Some(out)
        }
        _ => None,
    }
}

/// The `mod` names declared directly inside each inline-module scope.
fn child_mods(items: &[syn::Item]) -> BTreeMap<Vec<String>, BTreeSet<String>> {
    let mut out = BTreeMap::new();
    walk_mods(items, &mut Vec::new(), &mut out);
    out
}

fn walk_mods(
    items: &[syn::Item],
    scope: &mut Vec<String>,
    out: &mut BTreeMap<Vec<String>, BTreeSet<String>>,
) {
    for item in items {
        if let syn::Item::Mod(m) = item {
            out.entry(scope.clone())
                .or_default()
                .insert(m.ident.to_string());
            if let Some((_, inner)) = &m.content {
                scope.push(m.ident.to_string());
                walk_mods(inner, scope, out);
                scope.pop();
            }
        }
    }
}

fn reexports(root: &SourceFile) -> HashMap<String, Vec<String>> {
    let tree = root.tree(View::Full);
    let mods = child_mods(&tree.items)
        .remove(&Vec::new())
        .unwrap_or_default();
    let mut out = HashMap::new();
    for item in &tree.items {
        let syn::Item::Use(u) = item else { continue };
        if matches!(u.vis, syn::Visibility::Inherited) {
            continue;
        }
        for (full, rename, _) in scan::use_leaves(u) {
            let Some(resolved) = resolve(&full, &[], Some(&mods)) else {
                continue;
            };
            let local = rename.unwrap_or_else(|| full.last().cloned().unwrap_or_default());
            if local != "*" {
                out.insert(local, resolved);
            }
        }
    }
    out
}
