//! UTF-8-safe string truncation.
//!
//! Rust's `&s[..n]` **panics** when `n` lands inside a multi-byte character.
//! Every truncation in this codebase is expressed as a byte budget (token
//! estimates, tool-result caps, prompt budgets), and the content being
//! truncated is routinely non-ASCII — skill descriptions, prompts, and
//! conversation text in this repo are frequently Chinese, where one character
//! is 3 bytes and a cut at an arbitrary byte offset lands mid-character with
//! probability ~2/3.
//!
//! Three private near-duplicates of this helper already existed (in
//! `compaction::compact`, `runtime::turn`, and `core::frozen::utils`) and were
//! correct; the bug was that four *other* truncation sites sliced raw bytes
//! instead of calling one. This module is the single canonical implementation
//! those sites now use.

/// Truncate `s` to at most `max_bytes` bytes, cutting at a character boundary.
///
/// Returns a prefix of `s` whose length is `<= max_bytes`. When `max_bytes`
/// falls inside a multi-byte character, the cut moves *backwards* to the start
/// of that character, so the result is never longer than requested (important
/// when the caller is enforcing a hard budget).
///
/// ```
/// # use base::text::truncate_at_char_boundary as t;
/// assert_eq!(t("hello", 10), "hello");        // shorter than budget
/// assert_eq!(t("hello", 3), "hel");           // ASCII cut
/// assert_eq!(t("中文测试", 5), "中");          // byte 5 is mid-character → back off
/// assert_eq!(t("中文测试", 6), "中文");        // byte 6 is a boundary
/// ```
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::truncate_at_char_boundary as t;

    #[test]
    fn shorter_than_budget_is_returned_whole() {
        assert_eq!(t("hello", 10), "hello");
        assert_eq!(t("中文", 100), "中文");
        assert_eq!(t("", 0), "");
    }

    #[test]
    fn ascii_cuts_exactly_at_the_budget() {
        assert_eq!(t("hello world", 5), "hello");
    }

    /// Every byte offset inside a multi-byte string must be safe, not just the
    /// boundaries.
    #[test]
    fn every_byte_offset_of_a_multibyte_string_is_safe() {
        let s = "中文测试abc混合デ";
        for n in 0..=s.len() + 4 {
            let out = t(s, n);
            assert!(
                out.len() <= n.min(s.len()),
                "n={n} produced {} bytes",
                out.len()
            );
            assert!(s.starts_with(out), "n={n} produced a non-prefix");
        }
    }

    #[test]
    fn cut_moves_backwards_never_forwards() {
        // byte 5 is inside the second 3-byte char, so we must fall back to 3.
        assert_eq!(t("中文测试", 5), "中");
        assert_eq!(t("中文测试", 6), "中文");
        // A budget of 0, or one smaller than the first character, yields "".
        assert_eq!(t("中文", 0), "");
        assert_eq!(t("中文", 2), "");
    }

    #[test]
    fn combining_marks_and_emoji_survive() {
        let s = "e\u{301}👨‍👩‍👧"; // combining acute + ZWJ family sequence
        for n in 0..=s.len() {
            let out = t(s, n);
            assert!(s.starts_with(out));
        }
    }
}
