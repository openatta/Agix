//! `Network` — outbound requests, and whether they may leave.
//!
//! Every client in the engine builds its requests through this. What a host
//! can *replace* today is narrower: the provider a tool uses arrives through
//! `ToolContext.exec`, while the model, OAuth, telemetry and registry clients
//! each construct their own. Their traffic is `Origin::Operator`, which no
//! policy restricts, so the behavior is right — but auditing or going offline
//! as a whole needs an injection point that does not exist yet.

use std::collections::BTreeMap;

use super::ExecError;

/// Who chose this destination.
///
/// The distinction the engine's network policy has always needed and never
/// had. `allowed_domains` answers "where may the model reach", not "what may
/// this process connect to" — applying it to everything would cut the agent
/// off from its own model endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The model picked the destination: a `WebFetch` url, a `Ping` host,
    /// `curl` inside a Bash command. Subject to `allowed_domains`, and kept
    /// off [internal addresses](is_internal_address) unless `allowed_domains`
    /// names the host.
    Agent,
    /// An operator configured the endpoint: the model API, an MCP server,
    /// telemetry, the plugin marketplace, an OAuth provider. Not subject to
    /// `allowed_domains`, but still routed here so a deployment can audit,
    /// rate-limit or go offline as a whole.
    Operator,
}

/// How many redirects a provider follows when the caller does not say.
///
/// Ten, the figure `reqwest` picks by default, so a caller with no opinion on
/// redirects gets the behavior it would have had without this contract.
pub const DEFAULT_MAX_REDIRECTS: u8 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Vec<u8>>,
    /// Largest response body this caller will accept, in bytes. `None` puts
    /// no ceiling on it.
    pub max_bytes: Option<u64>,
    /// How many redirects the provider may follow on this caller's behalf.
    ///
    /// Zero hands the 3xx back instead, which is what a caller wants when the
    /// redirect target is itself a decision — `WebFetch` returns it to the
    /// model rather than chasing it.
    pub max_redirects: u8,
    /// Refuse every hop whose destination is an
    /// [internal address](is_internal_address), whoever chose it.
    ///
    /// For an endpoint an operator wrote down whose content still comes from
    /// outside — an HTTP hook is a URL in a settings file a repository can
    /// ship. `Origin::Agent` requests get the same refusal without asking.
    pub public_only: bool,
    /// With `public_only`, the host and port `url` names may be internal:
    /// a hop to exactly that target is let through, every other hop is still
    /// held to `public_only`. An `Origin::Agent` request gains nothing from it.
    pub internal_target: bool,
}

impl HttpRequest {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
            max_bytes: None,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            public_only: false,
            internal_target: false,
        }
    }

    pub fn public_only(mut self) -> Self {
        self.public_only = true;
        self
    }

    /// `public_only`, except that the target `url` names may be internal.
    pub fn public_only_but_target(mut self) -> Self {
        self.public_only = true;
        self.internal_target = true;
        self
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self::new("POST", url)
    }

    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.insert(k.into(), v.into());
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = Some(n);
        self
    }

    pub fn max_redirects(mut self, n: u8) -> Self {
        self.max_redirects = n;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A response whose body has not been read yet.
///
/// The model API is a server-sent-event stream that stays open for the length
/// of an answer, so an egress that could only hand back a finished body could
/// not carry the one request the engine makes most. Status and headers arrive
/// first because that is what the retry ladder classifies on, before a single
/// body byte exists.
pub struct HttpStream {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: futures::stream::BoxStream<'static, Result<Vec<u8>, ExecError>>,
}

/// Every outbound request, in one place.
///
/// A provider follows redirects up to [`HttpRequest::max_redirects`] and must
/// re-apply its policy to each hop: a destination the model was allowed to
/// name could otherwise redirect to one it was not, and the allowlist would
/// hold only for the first request of a chain. The same goes for refusing
/// internal addresses, which also has to hold for the address it connects to,
/// not only the one a name resolved to when it was checked.
#[async_trait::async_trait]
pub trait Network: Send + Sync {
    /// Send, and read the whole body.
    async fn send(&self, req: HttpRequest, origin: Origin) -> Result<HttpResponse, ExecError>;

    /// Send, and hand back the body as it arrives.
    ///
    /// The default reads it all first, which is right for a provider that
    /// answers from a fixture and wrong for one talking to a live model
    /// endpoint.
    async fn open(&self, req: HttpRequest, origin: Origin) -> Result<HttpStream, ExecError> {
        let r = self.send(req, origin).await?;
        Ok(HttpStream {
            status: r.status,
            headers: r.headers,
            body: Box::pin(futures::stream::once(async move { Ok(r.body) })),
        })
    }

    /// Whether a host would be allowed, without sending anything.
    ///
    /// Exists for the callers that have to decide before there is a request:
    /// `Ping`, and building a sandbox profile that names the hosts a
    /// subprocess may reach.
    fn permits(&self, host: &str, origin: Origin) -> bool;
}

/// Whether `ip` is this machine or a network behind it rather than the public
/// internet: unspecified, loopback, private, link-local, carrier-grade NAT,
/// unique-local, site-local, multicast and the reserved blocks, and an IPv6
/// address that carries one of those IPv4 ones (mapped, compatible, NAT64,
/// 6to4).
///
/// Whether `domains` — `sandbox.allowed_domains`, as written in settings —
/// names `host`, itself or as a subdomain. Case, a trailing dot and the
/// brackets around an IPv6 address do not matter.
pub fn domains_name_host(domains: &[String], host: &str) -> bool {
    let host = domain_key(host);
    domains.iter().any(|d| {
        let d = domain_key(d);
        !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
    })
}

pub(crate) fn domain_key(entry: &str) -> String {
    let bare = entry
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => bare.to_ascii_lowercase(),
    }
}

/// The one answer to "may a destination chosen from outside reach this", for
/// a model-directed fetch and an HTTP hook alike.
pub fn is_internal_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => internal_v4(v4),
        std::net::IpAddr::V6(v6) => internal_v6(v6),
    }
}

fn internal_v4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || ip.is_multicast()
        || a >= 240
}

fn internal_v6(ip: std::net::Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_unspecified()
        || ip.is_loopback()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first & 0xffc0) == 0xfec0
        || ip.is_multicast()
        || carried_v4(ip).is_some_and(internal_v4)
}

fn carried_v4(ip: std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let from = |hi: u16, lo: u16| {
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        std::net::Ipv4Addr::new(a, b, c, d)
    };
    match ip.segments() {
        [0, 0, 0, 0, 0, 0, hi, lo] => Some(from(hi, lo)),
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => Some(from(hi, lo)),
        [0x2002, hi, lo, ..] => Some(from(hi, lo)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::is_internal_address;

    #[test]
    fn every_spelling_of_this_machine_and_the_networks_behind_it_is_internal() {
        for ip in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "127.255.0.9",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.8",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
            "2002:c0a8:101::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "ff02::1",
        ] {
            assert!(is_internal_address(ip.parse().expect("an address")), "{ip}");
        }
    }

    #[test]
    fn the_public_internet_is_not() {
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "93.184.216.34",
            "2001:4860:4860::8888",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert!(
                !is_internal_address(ip.parse().expect("an address")),
                "{ip}"
            );
        }
    }
}
