//! Allow-lists checked both ways: a finding the list does not name fails, and
//! so does an entry nothing is found for, so a list cannot outlive the code.

use std::collections::{BTreeMap, BTreeSet};

use crate::Hit;

#[derive(Clone, Debug)]
pub struct Finding {
    pub key: String,
    pub hit: Hit,
}

/// `guard` is `<family>::<LIST>`, e.g. `process_state::PROCESS_STATE`; the
/// failure says the list lives in `tests/guards/tests/<family>.rs`.
pub fn check(guard: &str, found: Vec<Finding>, allowed: &[(&str, &str)], fix: &str) {
    let problems = problems(found, allowed);
    if problems.is_empty() {
        return;
    }
    panic!("{}", report(guard, &problems, fix));
}

pub fn non_vacuous(guard: &str, scanned: usize, at_least: usize) {
    assert!(
        scanned >= at_least,
        "[{guard}] scanned {scanned}, expected at least {at_least}: the guard is reading \
         almost nothing, so its passing means nothing. Fix the scan, not the threshold."
    );
}

fn problems(found: Vec<Finding>, allowed: &[(&str, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut listed: BTreeMap<&str, usize> = BTreeMap::new();
    for (key, why) in allowed {
        *listed.entry(key).or_default() += 1;
        if why.trim().is_empty() {
            out.push(format!(
                "entry `{key}` gives no reason: say why it may stay"
            ));
        }
    }
    for (key, n) in &listed {
        if *n > 1 {
            out.push(format!("entry `{key}` is listed {n} times"));
        }
    }
    let mut seen = BTreeSet::new();
    let mut extra: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
    for finding in found {
        seen.insert(finding.key.clone());
        if !listed.contains_key(finding.key.as_str()) {
            extra.entry(finding.key).or_default().push(finding.hit);
        }
    }
    for (key, hits) in extra {
        for hit in hits {
            out.push(format!("{hit}\n      not on the list (key `{key}`)"));
        }
    }
    for key in listed.keys() {
        if !seen.contains(*key) {
            out.push(format!(
                "entry `{key}` matches nothing any more: take it off the list"
            ));
        }
    }
    out
}

fn report(guard: &str, problems: &[String], fix: &str) -> String {
    let (family, list) = guard.split_once("::").unwrap_or((guard, "the list"));
    format!(
        "[{guard}] {} problem(s):\n{}\n\nHow to fix: {fix}\nThe list is `{list}` in tests/guards/tests/{family}.rs.",
        problems.len(),
        problems
            .iter()
            .map(|p| format!("  - {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}
