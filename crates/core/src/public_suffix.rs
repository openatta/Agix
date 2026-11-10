//! The Public Suffix List (<https://publicsuffix.org/>), for telling a domain
//! someone owns (`example.com`, `alice.github.io`) from a suffix under which
//! anyone can register one (`com`, `co.uk`, `github.io`). Permission checks use
//! it to decide whether a host wildcard pins a domain (`base::url_rule`).
//!
//! The list is vendored as `public_suffix_list.dat`, rules of two or more
//! labels only. A copy that has gone stale misses suffixes added since, so it
//! is refreshed with each release: download the list from the URL in its
//! header and keep the lines with a `.` in them (the header stays). A crate
//! that compiles the list into code (`psl`) costs the release `agixd` about
//! 800 KB; the text costs what it weighs, about 140 KB.

use std::collections::HashSet;
use std::sync::OnceLock;

struct Rules {
    exact: HashSet<String>,
    wildcard: HashSet<String>,
    exception: HashSet<String>,
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| {
        let mut rules = Rules {
            exact: HashSet::new(),
            wildcard: HashSet::new(),
            exception: HashSet::new(),
        };
        for line in include_str!("public_suffix_list.dat").lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let (set, rule) = if let Some(rest) = line.strip_prefix('!') {
                (&mut rules.exception, rest)
            } else if let Some(rest) = line.strip_prefix("*.") {
                (&mut rules.wildcard, rest)
            } else {
                (&mut rules.exact, line)
            };
            if let Some(ascii) = to_ascii(rule) {
                set.insert(ascii);
            }
        }
        rules
    })
}

fn to_ascii(domain: &str) -> Option<String> {
    if domain.is_ascii() {
        return Some(domain.to_ascii_lowercase());
    }
    match url::Host::parse(domain).ok()? {
        url::Host::Domain(d) => Some(d),
        _ => None,
    }
}

/// How many trailing labels of `domain` (ASCII, lowercase, no trailing `.`)
/// are its public suffix, by the list's algorithm: an exception rule wins,
/// otherwise the longest matching rule, otherwise the last label alone.
pub fn suffix_labels(domain: &str) -> usize {
    let labels: Vec<&str> = domain.split('.').collect();
    let rules = rules();
    let mut longest = 1;
    for start in 0..labels.len() {
        let candidate = labels[start..].join(".");
        let count = labels.len() - start;
        if rules.exception.contains(&candidate) {
            return count - 1;
        }
        if rules.exact.contains(&candidate) {
            longest = longest.max(count);
        }
        if count >= 2 && rules.wildcard.contains(&labels[start + 1..].join(".")) {
            longest = longest.max(count);
        }
    }
    longest
}

/// Whether `domain` is, or lies within, a domain someone registered — it has
/// at least one label beyond its public suffix.
pub fn is_registrable_or_below(domain: &str) -> bool {
    let labels = domain.split('.').count();
    !domain.split('.').any(str::is_empty) && labels > suffix_labels(domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_suffix_is_nobody_s_domain() {
        for suffix in [
            "com",
            "co.uk",
            "com.cn",
            "github.io",
            "blogspot.com",
            "anything.ck",
            "town.kawasaki.jp",
            "unknown-tld",
        ] {
            assert!(!is_registrable_or_below(suffix), "{suffix}");
        }
        for domain in [
            "example.com",
            "api.example.com",
            "bbc.co.uk",
            "alice.github.io",
            "www.ck",
            "city.kawasaki.jp",
            "example.unknown-tld",
        ] {
            assert!(is_registrable_or_below(domain), "{domain}");
        }
    }

    #[test]
    fn rules_written_in_unicode_are_read_as_punycode() {
        assert_eq!(suffix_labels("xn--55qx5d.cn"), 2, "公司.cn");
        assert!(!is_registrable_or_below("xn--55qx5d.cn"));
    }

    #[test]
    fn an_empty_label_is_no_domain() {
        assert!(!is_registrable_or_below(""));
        assert!(!is_registrable_or_below("example..com"));
    }
}
