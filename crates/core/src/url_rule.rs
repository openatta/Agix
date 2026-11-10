//! URL rules: the one way a pattern like `https://*.corp.example/**` is
//! matched against a URL, for permission rules on URL-valued tools
//! (`WebFetch(...)`, `Ping(...)`) and the MCP policy's `url` and
//! `allowed_authorization_servers` rules alike.
//!
//! Both sides are taken apart before they meet. A URL is parsed and written
//! one way ([`NormalizedUrl`]): userinfo, query and fragment dropped, host
//! lowercased without a trailing `.`, the scheme's default port left out,
//! `ws` / `wss` read as `http` / `https`. A pattern is split into scheme,
//! host, port and path ([`UrlPattern`]), and each part is matched against the
//! same part of the URL on its own. So a wildcard in the host stays in the
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
            path: url.path().to_string(),
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
    pub fn parse(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim();
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
        let host_glob = if host.starts_with('[') {
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
            path: glob(path, "path")?,
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

    /// Whether the host part pins no domain: it has a wildcard and fewer than
    /// two labels written out (`*`, `*.com`, `api.*`), so it reaches hosts
    /// anyone can register.
    pub fn host_is_open(&self) -> bool {
        let wildcard = |c: char| matches!(c, '*' | '?' | '[' | ']' | '{' | '}');
        if !self.host_text.contains(wildcard) {
            return false;
        }
        let literal_labels = self
            .host_text
            .split('.')
            .filter(|label| !label.is_empty() && !label.contains(wildcard))
            .count();
        literal_labels < 2
    }
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
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        let (host, rest) = authority.split_at(close + 1);
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
