//! Path sanitization utilities.
//!
//! Provides `sanitize_for_fs()` which transforms an arbitrary string into a
//! filesystem-safe identifier using a djb2 hash and radix-36 encoding. Used
//! for session directories, worktree slugs, and any other case where an
//! untrusted string must become a safe directory or file name.

/// djb2 hash constant for initial value.
const DJB2_INIT: u64 = 5381;

/// Compute a djb2 hash of the given string, returning a u64.
fn djb2_hash(s: &str) -> u64 {
    let mut hash = DJB2_INIT;
    for byte in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
    }
    hash
}

/// Encode a u64 value into a radix-36 (base36) string.
///
/// Uses digits 0-9 and lowercase letters a-z for 36 symbols.
fn radix36_encode(mut value: u64) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut result = Vec::new();
    while value > 0 {
        result.push(CHARS[(value % 36) as usize]);
        value /= 36;
    }
    result.reverse();
    String::from_utf8(result).expect("radix36 encoding is always valid ASCII")
}

/// Maximum length before truncation with hash suffix.
const SANITIZE_MAX_LEN: usize = 200;
/// Public constant for external crates (e.g. agix-history tests) that need
/// to verify sanitized output length bounds.
pub const MAX_SANITIZED_LENGTH: usize = SANITIZE_MAX_LEN;

/// Sanitize a string for use as a filesystem component (file name or directory).
///
/// 1. Replaces all non-`[a-zA-Z0-9]` characters with `-`
/// 2. If the result exceeds `SANITIZE_MAX_LEN` (200) bytes, truncates and appends
///    a djb2 hash of the original string encoded in radix-36.
///
/// The output is guaranteed to be safe for all major filesystems.
pub fn sanitize_for_fs(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    if sanitized.len() <= SANITIZE_MAX_LEN {
        return sanitized;
    }

    let hash = djb2_hash(name);
    let suffix = radix36_encode(hash);
    let prefix_len = SANITIZE_MAX_LEN.saturating_sub(suffix.len() + 1);
    let prefix: String = sanitized.chars().take(prefix_len).collect();
    format!("{prefix}-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_string() {
        let result = sanitize_for_fs("hello");
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_special_chars_replaced() {
        let result = sanitize_for_fs("/Users/foo/my-project");
        assert_eq!(result, "-Users-foo-my-project");
    }

    #[test]
    fn test_spaces_replaced() {
        let result = sanitize_for_fs("hello world");
        assert_eq!(result, "hello-world");
    }

    #[test]
    fn test_djb2_hash_consistency() {
        let h1 = djb2_hash("test");
        let h2 = djb2_hash("test");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_djb2_hash_different_inputs() {
        let h1 = djb2_hash("abc");
        let h2 = djb2_hash("xyz");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_radix36_zero() {
        assert_eq!(radix36_encode(0), "0");
    }

    #[test]
    fn test_radix36_encoding() {
        let encoded = radix36_encode(10);
        assert_eq!(encoded, "a");
    }

    #[test]
    fn test_sanitize_produces_different_outputs_for_different_inputs() {
        let r1 = sanitize_for_fs("project alpha");
        let r2 = sanitize_for_fs("project beta");
        assert_ne!(r1, r2);
    }

    #[test]
    fn test_sanitize_truncates_long_inputs() {
        let long = "a".repeat(300);
        let result = sanitize_for_fs(&long);
        // Should be truncated with hash suffix
        assert!(result.len() < 300);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_sanitize_allows_alphanumeric() {
        let result = sanitize_for_fs("MyProject_v2.0");
        assert_eq!(result, "MyProject-v2-0");
    }
}
