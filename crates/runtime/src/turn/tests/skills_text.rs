//! 技能清单文本的预算与截断。

use crate::turn::attachments::truncate_at_char_boundary;
use crate::turn::prompt::{
    combined_skill_text_capped, select_skills_keeping_description, SKILL_COMBINED_TEXT_CAP,
};

#[test]
fn select_skills_keeping_description_prefers_most_invoked_when_budget_is_tight() {
    // Three skills, each description costs 50+overhead; budget only
    // fits one. "b" has the highest count, so it alone should keep its
    // description; "a" and "c" degrade to name-only despite being
    // alphabetically first/earlier in the list.
    let names = ["a", "b", "c"];
    let counts = [1u32, 5, 2];
    let lens = [50usize, 50, 50];
    let keeps = select_skills_keeping_description(&names, &counts, &lens, 60, 5);
    assert_eq!(keeps, vec![false, true, false]);
}

#[test]
fn select_skills_keeping_description_keeps_everyone_when_budget_is_ample() {
    let names = ["a", "b"];
    let counts = [0u32, 0];
    let lens = [50usize, 50];
    let keeps = select_skills_keeping_description(&names, &counts, &lens, 10_000, 5);
    assert_eq!(keeps, vec![true, true]);
}

#[test]
fn select_skills_keeping_description_ties_break_on_name_not_input_order() {
    // Equal counts (the common fresh-session case: everything at 0) —
    // must not depend on whatever order the caller's Vec happened to be
    // in; alphabetically-first name wins the tiebreak deterministically.
    let names = ["zebra", "apple"];
    let counts = [0u32, 0];
    let lens = [50usize, 50];
    // Budget fits exactly one entry.
    let keeps = select_skills_keeping_description(&names, &counts, &lens, 55, 5);
    assert_eq!(
        keeps,
        vec![false, true],
        "apple (alphabetically first) should win the tie"
    );
}

#[test]
fn select_skills_keeping_description_drops_everyone_under_a_near_zero_budget() {
    let names = ["a"];
    let counts = [99u32];
    let lens = [50usize];
    let keeps = select_skills_keeping_description(&names, &counts, &lens, 1, 5);
    assert_eq!(keeps, vec![false]);
}

#[test]
fn combined_skill_text_capped_appends_when_to_use() {
    let combined = combined_skill_text_capped("Reviews code", Some("Use after edits"));
    assert_eq!(combined, "Reviews code Use after edits");
}

#[test]
fn combined_skill_text_capped_handles_absent_when_to_use() {
    let combined = combined_skill_text_capped("Reviews code", None);
    assert_eq!(combined, "Reviews code");
}

#[test]
fn combined_skill_text_capped_enforces_1536_char_ceiling_independent_of_listing_budget() {
    let long_desc = "a".repeat(2000);
    let combined = combined_skill_text_capped(&long_desc, Some(&"b".repeat(200)));
    assert_eq!(combined.len(), SKILL_COMBINED_TEXT_CAP);
}

#[test]
fn truncate_at_char_boundary_never_splits_a_multibyte_char() {
    // Every char here is 3 bytes (UTF-8) — a naive byte-index cut at an
    // odd offset would panic or produce invalid UTF-8.
    let s = "审查代码变更的正确性";
    for n in 0..s.len() + 2 {
        let truncated = truncate_at_char_boundary(s, n);
        assert!(truncated.len() <= n.min(s.len()));
        // Must still be valid UTF-8 (would already have panicked in
        // `truncate_at_char_boundary` itself if not, but assert the
        // round-trip explicitly as the actual behavioral guarantee).
        let _ = truncated.chars().count();
    }
}

#[test]
fn truncate_at_char_boundary_returns_whole_string_when_under_limit() {
    assert_eq!(truncate_at_char_boundary("short", 100), "short");
}
