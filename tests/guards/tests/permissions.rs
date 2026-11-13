//! The permission pipeline's guards (D1 PERM-*).
//!
//! PERM-8: the decision matrix names every `Stage` the policy can refuse or
//! ask at, and its stage test reaches each one, so a stage added to the policy
//! cannot go unexercised.

use std::collections::BTreeSet;

use guards::{scan, ws, View};

const MATRIX: &str = "crates/tools/tests/decision_matrix.rs";

#[test]
fn every_stage_appears_in_the_decision_matrix() {
    let variants = scan::enum_variants(ws(), "base", "Stage");
    let named: BTreeSet<String> = scan::paths(ws().file(MATRIX), View::Full)
        .into_iter()
        .filter_map(|(segments, _)| match segments.as_slice() {
            [.., ty, variant] if ty == "Stage" => Some(variant.clone()),
            _ => None,
        })
        .collect();
    let missing: Vec<&String> = variants.iter().filter(|v| !named.contains(*v)).collect();
    assert!(
        missing.is_empty(),
        "[permissions::PERM-8] `base` `Stage` variants no call in {MATRIX} reaches: \
         {missing:?} — add a case for each to `every_stage_of_the_policy_is_reached`"
    );
}
