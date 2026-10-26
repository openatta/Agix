//! Unicode homograph attack protection for plugin names.
//!
//! Prevents plugin names that impersonate official names using confusable Unicode
//! characters (e.g., Cyrillic, Greek, Armenian lookalikes that are visually
//! identical to ASCII letters).
//!
//! ## How it works
//!
//! 1. **Blocklist**: Plugin names that are known-malicious (or their confusable
//!    variants) are rejected outright.
//! 2. **Confusable normalization**: Each plugin name is normalized by replacing
//!    known confusable Unicode codepoints with their ASCII equivalents, then
//!    checked against the set of official plugin names. If the normalized form
//!    matches an official name but the original differs, it is a homograph attack.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Map of ASCII characters to their common confusable Unicode lookalikes.
///
/// Each entry maps a single ASCII character to one or more Unicode codepoints
/// that are visually indistinguishable from it in most fonts.
static CONFUSABLE_MAP: LazyLock<HashMap<char, Vec<char>>> = LazyLock::new(|| {
    let mut m = HashMap::new();

    // ── Latin lowercase confusables ──
    m.insert(
        'a',
        vec![
            '\u{0430}', // Cyrillic small letter a
            '\u{0251}', // Latin small letter alpha
            '\u{03B1}', // Greek small letter alpha
        ],
    );
    m.insert(
        'c',
        vec![
            '\u{0441}', // Cyrillic small letter es
            '\u{03F2}', // Greek lunate sigma symbol
            '\u{03C2}', // Greek small letter final sigma
        ],
    );
    m.insert('e', vec!['\u{0435}', '\u{0451}']); // Cyrillic small ie, Cyrillic small io
    m.insert(
        'i',
        vec![
            '\u{0456}', // Cyrillic small letter byelorussian-ukrainian i
            '\u{026A}', // Latin letter small capital i
            '\u{0438}', // Cyrillic small letter i
        ],
    );
    m.insert('j', vec!['\u{0458}']); // Cyrillic small letter je
    m.insert('k', vec!['\u{043A}']); // Cyrillic small letter ka
    m.insert('m', vec!['\u{043C}']); // Cyrillic small letter em
    m.insert('n', vec!['\u{03B7}']); // Greek small letter eta
    m.insert(
        'o',
        vec![
            '\u{043E}', // Cyrillic small letter o
            '\u{03BF}', // Greek small letter omicron
            '\u{0585}', // Armenian small letter oh
        ],
    );
    m.insert('p', vec!['\u{0440}']); // Cyrillic small letter er
    m.insert('s', vec!['\u{0455}']); // Cyrillic small letter dze
    m.insert('t', vec!['\u{0442}']); // Cyrillic small letter te
    m.insert('u', vec!['\u{03BD}']); // Greek small letter nu
    m.insert('x', vec!['\u{0445}']); // Cyrillic small letter ha
    m.insert('y', vec!['\u{0443}']); // Cyrillic small letter u

    // ── Latin uppercase confusables ──
    m.insert(
        'A',
        vec![
            '\u{0410}', // Cyrillic capital letter a
            '\u{0391}', // Greek capital letter alpha
        ],
    );
    m.insert('B', vec!['\u{0412}']); // Cyrillic capital letter ve
    m.insert('C', vec!['\u{0421}']); // Cyrillic capital letter es
    m.insert(
        'E',
        vec![
            '\u{0415}', // Cyrillic capital letter ie
            '\u{0395}', // Greek capital letter epsilon
        ],
    );
    m.insert(
        'H',
        vec![
            '\u{041D}', // Cyrillic capital letter en
            '\u{0397}', // Greek capital letter eta
        ],
    );
    m.insert(
        'I',
        vec![
            '\u{0406}', // Cyrillic capital letter byelorussian-ukrainian i
            '\u{0399}', // Greek capital letter iota
        ],
    );
    m.insert('K', vec!['\u{041A}']); // Cyrillic capital letter ka
    m.insert('M', vec!['\u{041C}']); // Cyrillic capital letter em
    m.insert(
        'O',
        vec![
            '\u{041E}', // Cyrillic capital letter o
            '\u{039F}', // Greek capital letter omicron
        ],
    );
    m.insert('P', vec!['\u{0420}']); // Cyrillic capital letter er
    m.insert('T', vec!['\u{0422}']); // Cyrillic capital letter te
    m.insert('X', vec!['\u{0425}']); // Cyrillic capital letter ha
    m.insert('Y', vec!['\u{0423}']); // Cyrillic capital letter u

    m
});

/// Known-malicious plugin names that are always blocked.
///
/// The check normalizes confusable characters first, so confusable variants of
/// these names are also caught.
pub static BLOCKLIST: &[&str] = &[
    "evil-plugin",
    "malware",
    "ransomware",
    "keylogger",
    "password-stealer",
    "crypto-miner",
    "trojan",
    "spyware",
    "backdoor",
    "rootkit",
    "credential-harvester",
    "data-exfiltrator",
    "injector",
    "shell-reverse",
];

/// Normalize a string by replacing confusable Unicode characters with their
/// ASCII equivalents. Characters not in the confusable map pass through unchanged.
fn normalize(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        let mut replaced = false;
        for (&ascii, confusables) in CONFUSABLE_MAP.iter() {
            if confusables.contains(&c) {
                result.push(ascii);
                replaced = true;
                break;
            }
        }
        if !replaced {
            result.push(c);
        }
    }
    result
}

/// Check if a plugin name is a homograph attack impersonating an official name.
///
/// Returns `Some(error_message)` if the name is a homograph of an official name,
/// or `None` if the name passes inspection.
///
/// A name is considered a homograph if:
/// 1. Its confusable-normalized form matches an official name, AND
/// 2. The original name text differs from the official name (exact character match)
///
/// This means legitimate installations of official plugins are not flagged.
pub fn check_homograph_name(name: &str, official_names: &[&str]) -> Option<String> {
    let normalized = normalize(name);

    for &official in official_names {
        if normalized == *official && name != official {
            return Some(format!(
                "name '{name}' is a homograph of official '{official}'"
            ));
        }
    }

    None
}

/// Check if a name matches the blocklist (after confusable normalization).
///
/// Returns the blocklist entry that matched, or `None` if the name is not blocked.
pub fn check_blocklist(name: &str) -> Option<&'static str> {
    let normalized = normalize(name);
    BLOCKLIST
        .iter()
        .find(|&&blocked| normalized == *blocked)
        .copied()
        .map(|v| v as _)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── check_homograph_name tests ──

    #[test]
    fn detect_cyrillic_a_homograph() {
        // "аgixplugin" with Cyrillic 'а' (U+0430) replacing Latin 'a'
        let name = "\u{0430}gixplugin";
        let official = &["agixplugin"];
        let msg = check_homograph_name(name, official);
        assert!(msg.is_some(), "should detect Cyrillic а homograph");
        assert!(msg.as_ref().unwrap().contains(name));
        assert!(msg.unwrap().contains("agixplugin"));
    }

    #[test]
    fn pass_legitimate_name() {
        let name = "agixplugin";
        let official = &["agixplugin"];
        let msg = check_homograph_name(name, official);
        assert!(msg.is_none(), "exact match should not be flagged");
    }

    #[test]
    fn detect_cyrillic_o_homograph() {
        // "cоde-review" with Cyrillic 'о' (U+043E) instead of Latin 'o'
        let name = "c\u{043E}de-review";
        let official = &["code-review"];
        let msg = check_homograph_name(name, official);
        assert!(
            msg.is_some(),
            "should detect Cyrillic о homograph in 'cоde-review'"
        );
    }

    #[test]
    fn detect_mixed_confusables_in_word() {
        // "mуplugin" with Cyrillic 'у' (U+0443) masking as Latin 'y'
        let name = "m\u{0443}plugin";
        let official = &["myplugin"];
        let msg = check_homograph_name(name, official);
        assert!(msg.is_some(), "should detect mixed confusables");
    }

    #[test]
    fn multiple_official_names_no_false_positive() {
        let name = "safe-tool";
        let official = &["agixsafe", "atool"];
        let msg = check_homograph_name(name, official);
        assert!(msg.is_none());
    }

    #[test]
    fn empty_name_does_not_panic() {
        let msg = check_homograph_name("", &["plugin"]);
        assert!(msg.is_none());
    }

    #[test]
    fn empty_official_list_does_not_panic() {
        let msg = check_homograph_name("test", &[]);
        assert!(msg.is_none());
    }

    #[test]
    fn same_name_different_case_not_confusable() {
        // Uppercase vs lowercase is not a confusable issue — exact matching
        // means "AgixPlugin" vs "agixplugin" will not match (expected).
        let msg = check_homograph_name("AgixPlugin", &["agixplugin"]);
        assert!(msg.is_none(), "case difference is not a confusable attack");
    }

    #[test]
    fn greek_omicron_homograph() {
        // "prоject" with Greek omicron (U+03BF) instead of 'o'
        let name = "pr\u{03BF}ject";
        let official = &["project"];
        let msg = check_homograph_name(name, official);
        assert!(msg.is_some(), "should detect Greek omicron homograph");
    }

    // ── check_blocklist tests ──

    #[test]
    fn blocklist_matches_exact() {
        let result = check_blocklist("evil-plugin");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "evil-plugin");
    }

    #[test]
    fn blocklist_no_false_positive() {
        let result = check_blocklist("safe-plugin");
        assert!(result.is_none());
    }

    #[test]
    fn blocklist_detects_confusable_variant() {
        // Cyrillic 'е' (U+0435) looks like Latin 'e'
        let name = "\u{0435}vil-plugin";
        let result = check_blocklist(name);
        assert!(
            result.is_some(),
            "should detect confusable variant of blocked name"
        );
        assert_eq!(result.unwrap(), "evil-plugin");
    }

    #[test]
    fn blocklist_detects_cyrillic_yo_variant() {
        // Cyrillic 'ё' (U+0451) also maps to 'e'
        let name = "\u{0451}vil-plugin";
        let result = check_blocklist(name);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "evil-plugin");
    }

    #[test]
    fn blocklist_returns_first_match() {
        let result = check_blocklist("trojan");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "trojan");
    }
}
