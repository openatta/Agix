//! token 预算指令的识别与剥离。

use crate::turn::token_budget::{find_token_budget_directive, strip_token_budget_directive};

fn budget_target(input: &str) -> Option<u64> {
    find_token_budget_directive(input).map(|(target, _)| target)
}

fn strip_budget(input: &str) -> String {
    match find_token_budget_directive(input) {
        Some((_, range)) => strip_token_budget_directive(input, range),
        None => input.to_string(),
    }
}

#[test]
fn a_number_without_a_multiplier_or_the_word_tokens_is_not_a_budget() {
    for text in [
        "please use 4 spaces",
        "use 1 banana",
        "+1",
        "+12 more reviewers",
        "set 3 tokens aside",
        "use 500k",
        "spend 2M tokensmith",
    ] {
        assert_eq!(budget_target(text), None, "{text}");
        assert_eq!(strip_budget(text), text, "{text}");
    }
}

#[test]
fn a_budget_after_multibyte_text_is_found_and_stripped_without_panicking() {
    assert_eq!(budget_target("İ use é 500k tokens"), Some(500_000));
    assert_eq!(strip_budget("İ use é 500k tokens"), "İ use é");
    assert_eq!(
        strip_budget("重构这个模块 USE 2M Output Tokens 然后提交"),
        "重构这个模块 然后提交"
    );
}

#[test]
fn stripping_keeps_the_rest_of_the_message_and_its_line_breaks() {
    assert_eq!(
        strip_budget("Fix the parser.\nspend 1m tokens\nThen run the tests."),
        "Fix the parser.\nThen run the tests."
    );
    assert_eq!(strip_budget("  +2M  "), "");
}

#[test]
fn parse_shorthand_500k() {
    assert_eq!(budget_target("+500k do this task"), Some(500_000));
}

#[test]
fn parse_shorthand_2m() {
    assert_eq!(budget_target("+2M"), Some(2_000_000));
}

#[test]
fn parse_shorthand_1b() {
    assert_eq!(budget_target("+1B"), Some(1_000_000_000));
}

#[test]
fn parse_spend_natural_language() {
    assert_eq!(
        budget_target("spend 2M tokens on refactoring"),
        Some(2_000_000)
    );
}

#[test]
fn parse_use_natural_language() {
    assert_eq!(budget_target("use 500k output tokens"), Some(500_000));
}

#[test]
fn parse_set_natural_language() {
    assert_eq!(budget_target("set 1B output tokens"), Some(1_000_000_000));
}

#[test]
fn parse_budget_natural_language() {
    assert_eq!(
        budget_target("budget 100k tokens for testing"),
        Some(100_000)
    );
}

#[test]
fn parse_no_directive() {
    assert_eq!(budget_target("hello world"), None);
}

#[test]
fn parse_empty_string() {
    assert_eq!(budget_target(""), None);
}

#[test]
fn strip_shorthand_500k() {
    assert_eq!(strip_budget("+500k do this task"), "do this task");
}

#[test]
fn strip_spend_natural_language() {
    assert_eq!(
        strip_budget("spend 2M tokens refactor this code"),
        "refactor this code"
    );
}

#[test]
fn strip_no_directive() {
    assert_eq!(strip_budget("hello world"), "hello world");
}

#[test]
fn test_token_budget_exceeds_limit() {
    // Above 2B should be rejected
    assert_eq!(budget_target("+999999999999999999"), None);
}
