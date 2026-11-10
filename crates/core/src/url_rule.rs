//! URL rules: the one way a pattern like `https://*.corp.example/**` is
//! matched against a URL, for permission rules on URL-valued tools
//! (`WebFetch(...)`, `Ping(...)`) and the MCP policy's `url` and
//! `allowed_authorization_servers` rules alike.
//!
//! Both sides are taken apart before they meet, and each part is brought to
//! the form the request goes out in. A URL is parsed and written one way
//! ([`NormalizedUrl`]): userinfo, query and fragment dropped, host lowercased
//! (a Unicode host as its punycode) without a trailing `.`, the scheme's
//! default port left out, `ws` / `wss` read as `http` / `https`. A pattern is
//! split into scheme, host, port and path ([`UrlPattern`]); its host goes
//! through the same IDNA mapping, label by label. On both sides a path's
//! percent-escapes of unreserved characters are decoded (`/%61dmin` is
//! `/admin`, as a server reads it) and the rest are kept, in upper case —
//! `%2F` is not a `/`. Each part is then matched against the same part of the
//! URL on its own. So a wildcard in the host stays in the
//! host — it spans `.`-separated labels and nothing else, never `:`, `/`,
//! `?` or `@` — and `https://*.example.com/**` cannot be met by
//! `https://evil.net/.example.com/`. In the path `*` stays within one
//! segment and `**` crosses them, as in path rules.
//!
//! `sandbox.allowed_domains` is not a rule of this kind: it lists domains,
//! each covering its subdomains, and the network layer enforces it on hosts
//! (`interface::exec::domains_name_host`). Its callers use [`NormalizedUrl`]
//! to get at the host.

use globset::{GlobBuilder, GlobMatcher};

/// A URL as rules see it: `scheme://host[:port]path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedUrl {
    scheme: String,
    host: String,
    port: Option<u16>,
    path: String,
}

impl NormalizedUrl {
    /// `None` when `raw` is not an absolute URL with a host.
    pub fn parse(raw: &str) -> Option<Self> {
        let url = url::Url::parse(raw.trim()).ok()?;
        let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return None;
        }
        let scheme = websocket_as_http(url.scheme()).to_string();
        let port = url
            .port_or_known_default()
            .filter(|p| Some(*p) != default_port(&scheme));
        Some(Self {
            scheme,
            host,
            port,
            path: normalize_escapes(url.path()),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// The same URL with any trailing `/` taken off its path, the form an
    /// OAuth issuer is compared in.
    pub fn without_trailing_slash(mut self) -> Self {
        let trimmed = self.path.trim_end_matches('/').len();
        self.path.truncate(trimmed);
        self
    }

    fn path_or_root(&self) -> &str {
        if self.path.is_empty() {
            "/"
        } else {
            &self.path
        }
    }
}

impl std::fmt::Display for NormalizedUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.scheme, self.host)?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        f.write_str(&self.path)
    }
}

/// A compiled `scheme://host[:port][/path]` pattern. Each part is a glob;
/// a port of `*` takes any port, no port takes only the scheme's default,
/// and no path is `/`.
#[derive(Debug, Clone)]
pub struct UrlPattern {
    scheme: GlobMatcher,
    host: GlobMatcher,
    host_text: String,
    port: PortPattern,
    path: GlobMatcher,
}

#[derive(Debug, Clone, Copy)]
enum PortPattern {
    Default,
    Any,
    Exactly(u16),
}

impl UrlPattern {
    /// `Err` for text that is not `scheme://host[:port][/path]`. A `?` or `#`
    /// is refused rather than read as a wildcard: in a URL it opens the query
    /// or fragment, and the text of a URL with a query (what "permit always"
    /// records) must not match other URLs.
    pub fn parse(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim();
        if pattern.contains(['?', '#']) {
            return Err(format!(
                "{pattern:?}: a URL pattern has no query or fragment, and `?` is no wildcard in one"
            ));
        }
        let (scheme, rest) = pattern
            .split_once("://")
            .filter(|(scheme, _)| !scheme.is_empty())
            .ok_or_else(|| format!("{pattern:?} is not a URL pattern (scheme://host[/path])"))?;
        let scheme = websocket_as_http(&scheme.to_ascii_lowercase()).to_string();
        let (authority, path) = match rest.find('/') {
            Some(slash) => rest.split_at(slash),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return Err(format!("{pattern:?}: a URL pattern names no userinfo"));
        }
        let (host, port) = split_port(authority).ok_or_else(|| {
            format!("{pattern:?}: an IPv6 host is followed by nothing but `:port`")
        })?;
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return Err(format!("{pattern:?} names no host"));
        }
        let ipv6 = is_ipv6_literal(&host);
        let host = if ipv6 {
            host
        } else {
            host_to_ascii(&host)
                .ok_or_else(|| format!("{pattern:?}: the host is not a domain name IDNA can map"))?
        };
        let port = match port {
            None => PortPattern::Default,
            Some("*") => PortPattern::Any,
            Some(digits) => match digits.parse::<u16>() {
                Ok(p) if Some(p) == default_port(&scheme) => PortPattern::Default,
                Ok(p) => PortPattern::Exactly(p),
                Err(_) => {
                    return Err(format!("{pattern:?}: port {digits:?} is not a number"));
                }
            },
        };
        // An IPv6 literal's brackets are part of the host, not a glob class.
        let host_glob = if ipv6 {
            globset::escape(&host)
        } else {
            host.clone()
        };
        let glob = |part: &str, what: &str| {
            GlobBuilder::new(part)
                .literal_separator(true)
                .build()
                .map(|g| g.compile_matcher())
                .map_err(|e| format!("{pattern:?}: invalid glob in the {what}: {e}"))
        };
        Ok(Self {
            scheme: glob(&scheme, "scheme")?,
            host: glob(&host_glob, "host")?,
            host_text: host,
            port,
            path: glob(&normalize_escapes(&encode_like_a_url_path(path)), "path")?,
        })
    }

    pub fn matches(&self, url: &NormalizedUrl) -> bool {
        let port = match self.port {
            PortPattern::Default => url.port.is_none(),
            PortPattern::Any => true,
            PortPattern::Exactly(p) => url.port == Some(p),
        };
        port && self.scheme.is_match(&url.scheme)
            && self.host.is_match(&url.host)
            && self.path.is_match(url.path_or_root())
    }

    /// Whether the host part pins no domain: it has a wildcard, and the labels
    /// written out after the last one holding a wildcard are not a domain
    /// someone registered but a public suffix (`*`, `*.com`, `*.co.uk`,
    /// `*.github.io`, `api.*`, `*example.com`) or numbers (`*.0.0.1`) — so it
    /// reaches hosts anyone can register or address.
    pub fn host_is_open(&self) -> bool {
        let wildcard = |c: char| matches!(c, '*' | '?' | '[' | ']' | '{' | '}');
        if is_ipv6_literal(&self.host_text) || !self.host_text.contains(wildcard) {
            return false;
        }
        let labels: Vec<&str> = self.host_text.split('.').collect();
        let tail_start = labels
            .iter()
            .rposition(|label| label.contains(wildcard))
            .map_or(0, |i| i + 1);
        let tail = &labels[tail_start..];
        let numeric = tail
            .last()
            .is_none_or(|tld| tld.bytes().all(|b| b.is_ascii_digit()));
        numeric || !crate::public_suffix::is_registrable_or_below(&tail.join("."))
    }
}

/// Rule text that matches `url` and no other URL, for recording a "permit
/// always" answer: the glob characters a URL may carry (`*`, `[`, `{`) are
/// escaped. Text with a query or fragment, which [`UrlPattern::parse`]
/// refuses, already only matches itself and is kept as it is.
pub fn literal_pattern(url: &str) -> String {
    if url.contains(['?', '#']) || !url.contains(['*', '[', ']', '{', '}']) {
        return url.to_string();
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return globset::escape(url);
    };
    let (authority, path) = rest
        .find('/')
        .map_or((rest, ""), |slash| rest.split_at(slash));
    let authority = match split_port(authority) {
        Some((host, _)) if is_ipv6_literal(host) => authority.to_string(),
        _ => globset::escape(authority),
    };
    format!("{scheme}://{authority}{}", globset::escape(path))
}

/// Whether `url` matches `pattern`. `false` when either does not parse.
pub fn matches(pattern: &str, url: &str) -> bool {
    match (UrlPattern::parse(pattern), NormalizedUrl::parse(url)) {
        (Ok(pattern), Some(url)) => pattern.matches(&url),
        _ => false,
    }
}

/// `host` and the text after its port's `:`, if any; `None` for an IPv6
/// literal followed by something other than a port.
fn split_port(authority: &str) -> Option<(&str, Option<&str>)> {
    let bracketed = authority
        .find(']')
        .map(|close| authority.split_at(close + 1))
        .filter(|(host, _)| is_ipv6_literal(host));
    if let Some((host, rest)) = bracketed {
        return match rest {
            "" => Some((host, None)),
            _ => rest.strip_prefix(':').map(|port| (host, Some(port))),
        };
    }
    Some(match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    })
}

fn is_ipv6_literal(host: &str) -> bool {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .is_some_and(|inner| {
            inner.contains(':')
                && inner
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'))
        })
}

/// A host pattern as the URL parser writes hosts: each label without a glob
/// character through IDNA (`bücher` is `xn--bcher-kva`). A label that holds a
/// glob and is not ASCII has no such form: `None`.
fn host_to_ascii(host: &str) -> Option<String> {
    if host.is_ascii() {
        return Some(host.to_string());
    }
    let glob = |c: char| matches!(c, '*' | '?' | '[' | ']' | '{' | '}');
    let labels = host
        .split('.')
        .map(|label| {
            if label.is_ascii() {
                Some(label.to_string())
            } else if label.contains(glob) {
                None
            } else {
                match url::Host::parse(label).ok()? {
                    url::Host::Domain(d) => Some(d),
                    _ => None,
                }
            }
        })
        .collect::<Option<Vec<_>>>()?;
    Some(labels.join("."))
}

/// What the URL parser percent-encodes in a path — controls, space, `"`,
/// `<`, `>`, `` ` `` and anything not ASCII — encoded, so a pattern written
/// with them meets the URL that carries them. `{` and `}` stay: in a pattern
/// they are glob syntax.
fn encode_like_a_url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii() && !c.is_ascii_control() && !matches!(c, ' ' | '"' | '<' | '>' | '`') {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// Percent-escapes of unreserved characters (letters, digits, `-`, `.`, `_`,
/// `~`) decoded, every other escape kept with upper-case hex.
fn normalize_escapes(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(c) = rest.chars().next() {
        let escaped = rest
            .strip_prefix('%')
            .and_then(|r| r.get(..2))
            .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(b) if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') => {
                out.push(b as char);
                rest = &rest[3..];
            }
            Some(b) => {
                out.push_str(&format!("%{b:02X}"));
                rest = &rest[3..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

fn websocket_as_http(scheme: &str) -> &str {
    match scheme {
        "ws" => "http",
        "wss" => "https",
        other => other,
    }
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_wildcard_stays_in_the_host() {
        let p = "https://*.example.com/**";
        for url in [
            "https://api.example.com/v1",
            "https://a.b.example.com/",
            "https://API.example.com./v1?x=1#f",
            "https://api.example.com:443/v1",
            "https://user:pw@api.example.com/v1",
            "wss://api.example.com/socket",
        ] {
            assert!(matches(p, url), "{url}");
        }
        for url in [
            "https://evil.net/.example.com/",
            "https://evil.net/x.example.com/steal",
            "https://evil.net/?q=.example.com/",
            "https://evil.net#.example.com/",
            "https://api.example.com@evil.net/",
            "https://api.example.com.evil.net/",
            "https://api.example.com:8443/",
            "http://api.example.com/",
            "not a url",
        ] {
            assert!(!matches(p, url), "{url}");
        }
    }

    #[test]
    fn the_path_glob_keeps_star_within_a_segment() {
        assert!(matches("https://h.example/*", "https://h.example/mcp"));
        assert!(!matches("https://h.example/*", "https://h.example/v1/mcp"));
        assert!(matches("https://h.example/**", "https://h.example/v1/mcp"));
        assert!(matches("https://h.example", "https://h.example/"));
        assert!(!matches("https://h.example", "https://h.example/x"));
    }

    #[test]
    fn ports_are_matched_on_their_own() {
        assert!(matches(
            "https://h.example:8443/**",
            "https://h.example:8443/x"
        ));
        assert!(!matches("https://h.example:8443/**", "https://h.example/x"));
        assert!(matches("https://h.example:*/**", "https://h.example:9/x"));
        assert!(matches("https://h.example:443/**", "https://h.example/x"));
        assert!(matches("http://[::1]:8080/**", "http://[::1]:8080/x"));
        assert!(!matches("http://[::1]/**", "http://[::2]/x"));
    }

    #[test]
    fn what_is_not_a_url_pattern_is_refused() {
        for pattern in [
            "**",
            "*.example.com",
            "example.com/**",
            "https://",
            "https://u@h/",
        ] {
            assert!(UrlPattern::parse(pattern).is_err(), "{pattern}");
        }
        assert!(UrlPattern::parse("https://h:port/").is_err());
    }

    #[test]
    fn an_open_host_pins_no_domain() {
        for p in [
            "https://*/**",
            "https://*.com/**",
            "https://api.*/**",
            "https://**",
        ] {
            assert!(UrlPattern::parse(p).unwrap().host_is_open(), "{p}");
        }
        for p in [
            "https://*.example.com/**",
            "https://example.com/**",
            "https://localhost/**",
        ] {
            assert!(!UrlPattern::parse(p).unwrap().host_is_open(), "{p}");
        }
    }

    /// A rule meets a URL in the form the request goes out in: a host written
    /// in Unicode is the punycode the URL parser turns it into, and an
    /// unreserved character spelled as a percent-escape is that character —
    /// the server reads `/%61dmin/` as `/admin/`. Otherwise a deny written
    /// for the host or the path does not refuse the very URL it names.
    #[test]
    fn audit2_a_rule_meets_the_url_the_request_goes_to() {
        assert!(
            matches("https://bücher.example/**", "https://bücher.example/x"),
            "a Unicode host does not match itself"
        );
        assert!(
            matches("https://h.example/admin/**", "https://h.example/%61dmin/x"),
            "a percent-escaped letter walks past a path rule"
        );
    }

    #[test]
    fn a_unicode_host_in_a_pattern_is_its_punycode() {
        for url in [
            "https://bücher.example/x",
            "https://BÜCHER.example/x",
            "https://xn--bcher-kva.example/x",
        ] {
            assert!(matches("https://bücher.example/**", url), "{url}");
            assert!(matches(
                "https://*.bücher.example/**",
                &url.replace("://", "://a.")
            ));
        }
        assert!(matches(
            "https://xn--bcher-kva.example/**",
            "https://bücher.example/x"
        ));
        assert!(!matches(
            "https://bücher.example/**",
            "https://bucher.example/x"
        ));
        assert!(UrlPattern::parse("https://bü*.example/**").is_err());
    }

    #[test]
    fn an_escaped_unreserved_character_is_that_character_and_nothing_else_is_decoded() {
        let deny = "https://h.example/admin/**";
        for url in [
            "https://h.example/admin/x",
            "https://h.example/%61dmin/x",
            "https://h.example/%61%64%6D%69%6E/x",
            "https://h.example/%61%64%6d%69%6e/x",
        ] {
            assert!(matches(deny, url), "{url}");
        }
        assert!(!matches(deny, "https://h.example/Admin/x"));
        assert!(!matches("https://h.example/a/b", "https://h.example/a%2Fb"));
        assert!(matches(
            "https://h.example/a%2fb",
            "https://h.example/a%2Fb"
        ));
        assert!(matches(
            "https://h.example/a%2Fb",
            "https://h.example/a%2fb"
        ));
        assert!(!matches("https://h.example/*", "https://h.example/a/b"));
        assert!(matches("https://h.example/*", "https://h.example/a%2Fb"));
        assert!(matches(
            "https://h.example/%7euser/**",
            "https://h.example/~user/x"
        ));
        assert!(matches(
            "https://h.example/ä/**",
            "https://h.example/%C3%A4/x"
        ));
        assert!(matches("https://h.example/a b", "https://h.example/a%20b"));
        assert!(matches("https://h.example/100%", "https://h.example/100%"));
    }

    #[test]
    fn a_query_or_fragment_makes_no_pattern() {
        for pattern in [
            "https://h.example/a?b",
            "https://h.example/a#b",
            "https://h?.example/",
        ] {
            assert!(UrlPattern::parse(pattern).is_err(), "{pattern}");
        }
    }

    #[test]
    fn a_literal_pattern_matches_only_its_own_url() {
        assert_eq!(
            literal_pattern("https://h.example/a"),
            "https://h.example/a"
        );
        assert_eq!(
            literal_pattern("https://h.example/a?b*"),
            "https://h.example/a?b*"
        );
        let star = literal_pattern("https://h.example/**");
        assert!(matches(&star, "https://h.example/**"));
        assert!(!matches(&star, "https://h.example/x"));
        let host = literal_pattern("https://*.com/x");
        assert!(matches(&host, "https://*.com/x"), "{host}");
        assert!(!matches(&host, "https://evil.com/x"));
        let ipv6 = literal_pattern("http://[::1]:8080/a*");
        assert!(matches(&ipv6, "http://[::1]:8080/a*"), "{ipv6}");
        assert!(!matches(&ipv6, "http://[::1]:8080/ab"));
    }

    #[test]
    fn a_host_wildcard_under_a_public_suffix_is_open() {
        for p in [
            "https://*.co.uk/**",
            "https://*.com.cn/**",
            "https://*.github.io/**",
            "https://*example.com/**",
            "https://a.*.com/**",
            "https://*.0.0.1/**",
            "https://example.com{,.evil.net}/**",
        ] {
            assert!(UrlPattern::parse(p).unwrap().host_is_open(), "{p}");
        }
        for p in [
            "https://*.bbc.co.uk/**",
            "https://*.alice.github.io/**",
            "https://api-*.example.com/**",
            "https://*.bücher.example/**",
            "http://[::1]:8080/**",
        ] {
            assert!(!UrlPattern::parse(p).unwrap().host_is_open(), "{p}");
        }
    }

    #[test]
    fn a_normalized_url_is_written_one_way() {
        let u = NormalizedUrl::parse("WSS://User@Corp.Example.:443/a/?q#f").unwrap();
        assert_eq!(u.to_string(), "https://corp.example/a/");
        assert_eq!(
            u.without_trailing_slash().to_string(),
            "https://corp.example/a"
        );
        assert_eq!(
            NormalizedUrl::parse("http://h.example:8080")
                .unwrap()
                .to_string(),
            "http://h.example:8080/"
        );
    }
}
