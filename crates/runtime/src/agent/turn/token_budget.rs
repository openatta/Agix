//! 用户输入里的 token 预算指令（"+500k"、"spend 2m tokens"）的识别与剥离。

pub(super) const MAX_TOKEN_TARGET: u64 = 2_000_000_000;

/// Words that may introduce a natural-language budget, taken out with it.
pub(super) const BUDGET_VERBS: &[&str] = &["spend", "use", "set", "budget"];

/// An output-token target the user set in their message, and the bytes of the
/// message that say it: `+500k` opening the message, or `500k tokens` /
/// `2M output tokens` anywhere in it, with a `spend` / `use` / `set` /
/// `budget` just before. The multiplier is required, so a bare number next to
/// a word (`use 4 spaces`) is never a budget.
pub(super) fn find_token_budget_directive(input: &str) -> Option<(u64, std::ops::Range<usize>)> {
    let bytes = input.as_bytes();
    let first = input.len() - input.trim_start().len();
    if bytes.get(first) == Some(&b'+') {
        if let Some((target, end)) = suffixed_number(bytes, first + 1) {
            if at_word_end(bytes, end) {
                return Some((target, first..end));
            }
        }
    }
    (0..bytes.len())
        .filter(|&i| bytes[i].is_ascii_digit() && (i == 0 || bytes[i - 1].is_ascii_whitespace()))
        .find_map(|i| {
            let (target, after) = suffixed_number(bytes, i)?;
            let end = tokens_word(bytes, after)?;
            Some((target, budget_verb_before(bytes, i)..end))
        })
}

/// `500k` / `2M` / `1B` starting at `at`: the count and where it ends.
pub(super) fn suffixed_number(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let digits = bytes[at.min(bytes.len())..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let multiplier = match bytes.get(at + digits)?.to_ascii_lowercase() {
        b'k' => 1_000,
        b'm' => 1_000_000,
        b'b' => 1_000_000_000,
        _ => return None,
    };
    let n: u64 = std::str::from_utf8(&bytes[at..at + digits])
        .ok()?
        .parse()
        .ok()?;
    let target = n.checked_mul(multiplier)?;
    (1..=MAX_TOKEN_TARGET)
        .contains(&target)
        .then_some((target, at + digits + 1))
}

/// ` tokens` / ` output tokens` starting at `at`: where it ends.
pub(super) fn tokens_word(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = skip_spaces(bytes, at)?;
    if let Some(after) = word_at(bytes, i, "output") {
        i = skip_spaces(bytes, after)?;
    }
    let mut end = word_at(bytes, i, "token")?;
    if bytes
        .get(end)
        .is_some_and(|b| b.eq_ignore_ascii_case(&b's'))
    {
        end += 1;
    }
    at_word_end(bytes, end).then_some(end)
}

pub(super) fn skip_spaces(bytes: &[u8], at: usize) -> Option<usize> {
    let n = bytes[at..].iter().take_while(|b| **b == b' ').count();
    (n > 0).then_some(at + n)
}

pub(super) fn word_at(bytes: &[u8], at: usize, word: &str) -> Option<usize> {
    let end = at + word.len();
    bytes
        .get(at..end)
        .is_some_and(|w| w.eq_ignore_ascii_case(word.as_bytes()))
        .then_some(end)
}

pub(super) fn at_word_end(bytes: &[u8], at: usize) -> bool {
    !bytes.get(at).is_some_and(u8::is_ascii_alphanumeric)
}

/// Where a budget verb right before `at` starts, or `at` when there is none.
pub(super) fn budget_verb_before(bytes: &[u8], at: usize) -> usize {
    let mut end = at;
    while end > 0 && bytes[end - 1] == b' ' {
        end -= 1;
    }
    BUDGET_VERBS
        .iter()
        .find_map(|verb| {
            let start = end.checked_sub(verb.len())?;
            let word_start = start == 0 || bytes[start - 1].is_ascii_whitespace();
            (word_start && bytes[start..end].eq_ignore_ascii_case(verb.as_bytes())).then_some(start)
        })
        .unwrap_or(at)
}

/// The message without the directive at `range`.
pub(super) fn strip_token_budget_directive(input: &str, range: std::ops::Range<usize>) -> String {
    let (head, tail) = (&input[..range.start], &input[range.end..]);
    let (before, after) = (head.trim(), tail.trim());
    if before.is_empty() || after.is_empty() {
        return format!("{before}{after}");
    }
    let gap = |s: &str| s.contains('\n');
    let sep = if gap(&head[head.trim_end().len()..])
        || gap(&tail[..tail.len() - tail.trim_start().len()])
    {
        "\n"
    } else {
        " "
    };
    format!("{before}{sep}{after}")
}
