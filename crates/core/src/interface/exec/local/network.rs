//! `LocalNetwork` — outbound requests from this process.
//!
//! Two policies apply to a request the model chose, each on every hop of a
//! redirect chain: the deployment's allowlist, and staying off internal
//! addresses (`is_internal_address`) unless the allowlist names the host. A
//! request that asks for it (`HttpRequest::public_only`) gets the second
//! whoever chose it, except on hops to the target it was sent to when it also
//! says `internal_target`. A name is checked when the hop is sent and again by the
//! resolver the connection itself uses, so a name that answers differently
//! the second time still cannot reach inside.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use futures::StreamExt;

use crate::context::config::NetworkModeConfig;
use crate::interface::exec::{
    is_internal_address, ExecError, HttpRequest, HttpResponse, HttpStream, Network, Origin,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// This process's own network, under the deployment's egress policy.
pub struct LocalNetwork {
    proxied: reqwest::Client,
    direct: reqwest::Client,
    public_only: reqwest::Client,
    /// Hosts the model is allowed to reach. Empty means no restriction, which
    /// is what an unconfigured deployment has always had.
    allowed_domains: Vec<String>,
    /// Hosts the deployment named, which a model-directed request may reach
    /// even at an internal address.
    vouched: Vec<String>,
    /// Whether agent-directed egress is refused outright.
    agent_offline: bool,
}

impl Default for LocalNetwork {
    fn default() -> Self {
        Self::new(Vec::new(), false)
    }
}

impl LocalNetwork {
    pub fn new(allowed_domains: Vec<String>, agent_offline: bool) -> Self {
        Self::with(allowed_domains.clone(), allowed_domains, agent_offline)
    }

    fn with(allowed_domains: Vec<String>, vouched: Vec<String>, agent_offline: bool) -> Self {
        Self {
            proxied: shared_client(ClientKind::Proxied),
            direct: shared_client(ClientKind::Direct),
            public_only: shared_client(ClientKind::PublicOnly),
            allowed_domains,
            vouched,
            agent_offline,
        }
    }

    /// The egress a deployment's `sandbox.network_mode` describes, read the
    /// same way the sandbox backends read it so the two cannot disagree about
    /// where the model may reach.
    ///
    /// Under `Unrestricted` the domain list restricts nothing, but a host it
    /// names may still be reached at an internal address — the one thing the
    /// list says in that mode is "these are mine".
    pub fn for_agent_policy(mode: NetworkModeConfig, allowed_domains: Vec<String>) -> Self {
        match mode {
            NetworkModeConfig::Unrestricted => Self::with(Vec::new(), allowed_domains, false),
            NetworkModeConfig::DenyAll => Self::new(Vec::new(), true),
            // An empty allowlist under this mode names nothing reachable, the
            // same reading the sandbox backends give it.
            NetworkModeConfig::Allowlist if allowed_domains.is_empty() => {
                Self::new(Vec::new(), true)
            }
            NetworkModeConfig::Allowlist => Self::new(allowed_domains, false),
        }
    }

    fn keeps_off_internal(&self, url: &url::Url, origin: Origin, public_only: bool) -> bool {
        public_only || (origin == Origin::Agent && !self.vouches(url))
    }

    /// Whether this hop may reach an internal address: an operator's request
    /// that named its own target as allowed, going back to that target.
    fn may_reach_internal(&self, policy: &HopPolicy, url: &url::Url, origin: Origin) -> bool {
        origin == Origin::Operator
            && policy.internal_target.as_ref().is_some_and(|target| {
                target.host_str() == url.host_str()
                    && target.port_or_known_default() == url.port_or_known_default()
            })
    }

    fn vouches(&self, url: &url::Url) -> bool {
        let Some(host) = host_key(url) else {
            return false;
        };
        let is_name = matches!(url.host(), Some(url::Host::Domain(_)));
        self.vouched
            .iter()
            .map(|d| entry_key(d))
            .any(|d| host == d || (is_name && host.ends_with(&format!(".{d}"))))
    }

    /// The hop's own address, or every address its name resolves to here.
    ///
    /// A name that does not resolve here is let through: sent directly it
    /// fails on its own, and through a proxy it is the proxy's to resolve.
    async fn refuse_internal(&self, url: &url::Url, origin: Origin) -> Result<(), ExecError> {
        let addresses: Vec<IpAddr> = match url.host() {
            Some(url::Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
            Some(url::Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
            Some(url::Host::Domain(name)) => {
                let port = url.port_or_known_default().unwrap_or(0);
                tokio::net::lookup_host((name, port))
                    .await
                    .map(|found| found.map(|a| a.ip()).collect())
                    .unwrap_or_default()
            }
            None => return Err(ExecError::failed(format!("not a url: {url}"))),
        };
        match addresses.into_iter().find(|ip| is_internal_address(*ip)) {
            Some(ip) => Err(internal_refusal(
                url.host_str().unwrap_or_default(),
                ip,
                origin,
            )),
            None => Ok(()),
        }
    }

    fn refuse(&self, url: &url::Url, origin: Origin) -> Result<(), ExecError> {
        let host = url
            .host_str()
            .ok_or_else(|| ExecError::failed(format!("not a url: {url}")))?;
        if self.listed(host, origin) {
            return Ok(());
        }
        Err(if self.agent_offline {
            ExecError::denied(format!(
                "outbound network is off for model-directed requests, so {host} is unreachable"
            ))
        } else {
            ExecError::denied(format!(
                "{host} is not in the allowed domains for model-directed requests"
            ))
        })
    }

    fn client_for(&self, url: &url::Url) -> &reqwest::Client {
        if is_loopback(url) {
            &self.direct
        } else {
            &self.proxied
        }
    }
}

/// The ambient `HTTP(S)_PROXY` means "reach the internet through this egress",
/// not "also proxy calls to my own machine" — but reqwest applies it to
/// loopback targets too unless `NO_PROXY` carves them out, and a local relay,
/// vLLM or Ollama endpoint is a configuration people really run.
fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn host_key(url: &url::Url) -> Option<String> {
    Some(match url.host()? {
        url::Host::Domain(d) => d.trim_end_matches('.').to_ascii_lowercase(),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Ipv6(ip) => ip.to_string(),
    })
}

fn entry_key(entry: &str) -> String {
    let bare = entry
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => bare.to_ascii_lowercase(),
    }
}

fn internal_refusal(host: &str, ip: IpAddr, origin: Origin) -> ExecError {
    let place = if host.trim_matches(['[', ']']) == ip.to_string() {
        format!("{host} is on this machine or a private network")
    } else {
        format!("{host} is {ip}, on this machine or a private network")
    };
    ExecError::denied(match origin {
        Origin::Agent => format!(
            "{place}; a model-directed request reaches such a host only when \
             sandbox.allowed_domains names it"
        ),
        Origin::Operator => format!("{place}, which this request may not reach"),
    })
}

/// What a request carries from hop to hop of its redirect chain.
struct HopPolicy {
    /// The url it was sent to, when that target may be internal.
    internal_target: Option<url::Url>,
}

/// What the `PublicOnly` client's resolver refuses with, found again in the
/// error a refused connection comes back as.
#[derive(Debug)]
struct InternalDestination {
    host: String,
    ip: IpAddr,
}

impl std::fmt::Display for InternalDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} resolves to the internal address {}",
            self.host, self.ip
        )
    }
}

impl std::error::Error for InternalDestination {}

/// Resolves a name for a connection the `PublicOnly` client makes, refusing
/// it when any of its addresses is internal.
///
/// A configured proxy's own name is exempt: with a proxy, this resolver only
/// ever sees the proxy, and a proxy on the local network is what a proxy
/// usually is.
struct PublicOnlyResolver {
    proxies: Vec<String>,
}

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().trim_end_matches('.').to_ascii_lowercase();
        let checked = !self.proxies.contains(&host);
        Box::pin(async move {
            let found: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if checked {
                if let Some(ip) = found
                    .iter()
                    .map(|a| a.ip())
                    .find(|ip| is_internal_address(*ip))
                {
                    return Err(InternalDestination { host, ip }.into());
                }
            }
            Ok(Box::new(found.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The hosts of the proxies in the environment, read the way reqwest reads
/// them when it builds a client.
fn proxy_hosts() -> Vec<String> {
    [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok())
    .filter_map(|v| {
        let v = if v.contains("://") {
            v
        } else {
            format!("http://{v}")
        };
        url::Url::parse(&v).ok().as_ref().and_then(host_key)
    })
    .collect()
}

#[derive(Clone, Copy, Debug)]
enum ClientKind {
    Proxied,
    Direct,
    PublicOnly,
}

/// One connection pool per process, not one per `LocalNetwork`.
///
/// The client carries no policy — the allowlist and the offline switch live on
/// the wrapper — so two deployments' worth of egress rules can share it, and a
/// fresh one per tool call would throw away keep-alive and re-derive the TLS
/// roots every time.
fn shared_client(kind: ClientKind) -> reqwest::Client {
    static PROXIED: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    static DIRECT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    static PUBLIC_ONLY: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let cell = match kind {
        ClientKind::Proxied => &PROXIED,
        ClientKind::Direct => &DIRECT,
        ClientKind::PublicOnly => &PUBLIC_ONLY,
    };
    cell.get_or_init(|| build_client(kind)).clone()
}

/// No total request timeout: it would bound the whole response body, which for
/// a server-sent-event stream means tearing the connection down mid-answer.
/// How long a call may take belongs to the caller.
fn build_client(kind: ClientKind) -> reqwest::Client {
    install_tls_provider();
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("agix/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    match kind {
        ClientKind::Proxied => {}
        ClientKind::Direct => b = b.no_proxy(),
        ClientKind::PublicOnly => {
            b = b.dns_resolver(PublicOnlyResolver {
                proxies: proxy_hosts(),
            })
        }
    }
    b.build().unwrap_or_else(|e| {
        // The fallback is `reqwest`'s default, which is the *proxied* client —
        // so falling back here would send loopback traffic through the ambient
        // proxy, the one thing `no_proxy` exists to prevent. Say so rather
        // than silently inverting the intent.
        tracing::error!(error = %e, ?kind, "could not build the HTTP client; falling back to the default one");
        reqwest::Client::default()
    })
}

/// reqwest is built with `rustls-no-provider` (ring, not its aws-lc default)
/// and panics on the first client it builds if no process-wide provider is
/// installed — including clients built inside dependencies such as rmcp.
pub fn install_tls_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn headers_of(map: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    map.iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
        .collect()
}

fn transport_error(url: &url::Url, e: reqwest::Error, origin: Origin) -> ExecError {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
    while let Some(c) = cause {
        if let Some(d) = c.downcast_ref::<InternalDestination>() {
            return internal_refusal(&d.host, d.ip, origin);
        }
        cause = c.source();
    }
    // A request that never reached a server is this side failing, and the two
    // are different sentences to whoever reads them.
    if e.is_connect() || e.is_timeout() {
        ExecError::unavailable(format!("{url}: {e}"))
    } else {
        ExecError::failed(format!("{url}: {e}"))
    }
}

fn redirect_target(from: &url::Url, resp: &reqwest::Response) -> Option<url::Url> {
    if !matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)?
        .to_str()
        .ok()?;
    from.join(location).ok()
}

fn strip_credentials(headers: &mut BTreeMap<String, String>) {
    headers.retain(|k, _| {
        !matches!(
            k.to_ascii_lowercase().as_str(),
            "authorization" | "cookie" | "proxy-authorization"
        )
    });
}

#[async_trait::async_trait]
impl Network for LocalNetwork {
    async fn send(&self, req: HttpRequest, origin: Origin) -> Result<HttpResponse, ExecError> {
        let cap = req.max_bytes;
        let stream = self.open(req, origin).await?;

        if let Some(cap) = cap {
            if let Some(len) = stream
                .headers
                .get("content-length")
                .and_then(|v| v.parse::<u64>().ok())
            {
                if len > cap {
                    return Err(ExecError::failed(format!(
                        "response is {len} bytes; cap is {cap} bytes"
                    )));
                }
            }
        }

        let HttpStream {
            status,
            headers,
            mut body,
        } = stream;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk?);
            if let Some(cap) = cap {
                if bytes.len() as u64 > cap {
                    return Err(ExecError::failed(format!(
                        "response is over the {cap} byte cap"
                    )));
                }
            }
        }
        Ok(HttpResponse {
            status,
            headers,
            body: bytes,
        })
    }

    async fn open(&self, req: HttpRequest, origin: Origin) -> Result<HttpStream, ExecError> {
        let mut url = url::Url::parse(&req.url)
            .map_err(|e| ExecError::failed(format!("not a url: {} ({e})", req.url)))?;
        let policy = HopPolicy {
            internal_target: req.internal_target.then(|| url.clone()),
        };
        let mut method = req.method;
        let mut body = req.body;
        let mut headers = req.headers;

        for hop in 0..=req.max_redirects {
            self.refuse(&url, origin)?;
            let guarded = self.keeps_off_internal(&url, origin, req.public_only)
                && !self.may_reach_internal(&policy, &url, origin);
            if guarded {
                self.refuse_internal(&url, origin).await?;
            }

            let m = reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| ExecError::failed(format!("{method}: {e}")))?;
            let client = if guarded {
                &self.public_only
            } else {
                self.client_for(&url)
            };
            let mut b = client.request(m.clone(), url.clone());
            for (k, v) in &headers {
                b = b.header(k, v);
            }
            if let Some(body) = &body {
                b = b.body(body.clone());
            }
            let resp = b
                .send()
                .await
                .map_err(|e| transport_error(&url, e, origin))?;

            if hop < req.max_redirects {
                if let Some(next) = redirect_target(&url, &resp) {
                    let code = resp.status().as_u16();
                    let rewrites_to_get = code == 303
                        || (matches!(code, 301 | 302)
                            && m != reqwest::Method::GET
                            && m != reqwest::Method::HEAD);
                    if rewrites_to_get {
                        method = "GET".into();
                        body = None;
                    }
                    // Scheme as well as host: a redirect from `https` to
                    // `http` on the same host would otherwise put the
                    // Authorization header on the wire in plaintext.
                    if next.host_str() != url.host_str() || next.scheme() != url.scheme() {
                        strip_credentials(&mut headers);
                    }
                    url = next;
                    continue;
                }
            }

            return Ok(HttpStream {
                status: resp.status().as_u16(),
                headers: headers_of(resp.headers()),
                body: Box::pin(
                    resp.bytes_stream()
                        .map(|c| c.map(|b| b.to_vec()).map_err(ExecError::failed)),
                ),
            });
        }
        unreachable!("the loop returns or continues, and continues are bounded by max_redirects")
    }

    /// The allowlist, and for an address written out, whether it is internal.
    /// A name's addresses are only known once it is resolved, at send.
    fn permits(&self, host: &str, origin: Origin) -> bool {
        if !self.listed(host, origin) {
            return false;
        }
        let literal = entry_key(host).parse::<IpAddr>().ok();
        match (origin, literal) {
            (Origin::Agent, Some(ip)) if is_internal_address(ip) => {
                let as_url = match ip {
                    IpAddr::V4(_) => format!("http://{ip}/"),
                    IpAddr::V6(_) => format!("http://[{ip}]/"),
                };
                url::Url::parse(&as_url).is_ok_and(|u| self.vouches(&u))
            }
            _ => true,
        }
    }
}

impl LocalNetwork {
    fn listed(&self, host: &str, origin: Origin) -> bool {
        match origin {
            // An operator chose this endpoint. Applying the model's allowlist
            // to it would cut the agent off from its own model.
            Origin::Operator => true,
            Origin::Agent if self.agent_offline => false,
            Origin::Agent => {
                // Hosts arrive lowercased from the url parser; the list
                // arrives verbatim from settings.json. Compared as written,
                // `Example.com` is an allowlist that matches nothing, and the
                // deployment gets every request refused with no sign that its
                // configuration is what refused them.
                let host = entry_key(host);
                self.allowed_domains.is_empty()
                    || self.allowed_domains.iter().any(|d| {
                        let d = entry_key(d);
                        host == d || host.ends_with(&format!(".{d}"))
                    })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allowlist_binds_the_model_and_not_the_operator() {
        let n = LocalNetwork::new(vec!["example.com".into()], false);
        assert!(n.permits("example.com", Origin::Agent));
        assert!(
            n.permits("api.example.com", Origin::Agent),
            "subdomains too"
        );
        assert!(!n.permits("evil.test", Origin::Agent));
        assert!(
            n.permits("evil.test", Origin::Operator),
            "an operator-configured endpoint is not the model's choice to be \
             restricted from — applying the allowlist here is how an agent \
             loses its own model"
        );
    }

    #[test]
    fn an_empty_allowlist_is_no_restriction() {
        let n = LocalNetwork::default();
        assert!(n.permits("anything.test", Origin::Agent));
    }

    #[test]
    fn offline_stops_the_model_and_leaves_the_engine_running() {
        let n = LocalNetwork::new(Vec::new(), true);
        assert!(!n.permits("example.com", Origin::Agent));
        assert!(n.permits("example.com", Origin::Operator));
    }

    /// Hosts arrive lowercased from the parser; a list written by hand does
    /// not. Compared as written, `Example.com` matches nothing and every
    /// request is refused with no sign that the configuration is at fault.
    #[test]
    fn the_allowlist_is_not_case_sensitive() {
        let n = LocalNetwork::for_agent_policy(
            crate::context::config::NetworkModeConfig::Allowlist,
            vec!["Example.COM".into()],
        );
        assert!(n.permits("example.com", Origin::Agent));
        assert!(n.permits("api.example.com", Origin::Agent));
    }

    #[test]
    fn a_suffix_that_is_not_a_subdomain_does_not_match() {
        let n = LocalNetwork::new(vec!["example.com".into()], false);
        assert!(
            !n.permits("notexample.com", Origin::Agent),
            "matching on a bare suffix would let any attacker-registered \
             domain ending in the allowed one through"
        );
    }

    #[test]
    fn loopback_is_recognized_whatever_shape_it_is_written_in() {
        for u in [
            "http://127.0.0.1:8080/health",
            "http://localhost/",
            "https://LocalHost:9000",
            "http://[::1]:3000",
        ] {
            assert!(is_loopback(&url::Url::parse(u).unwrap()), "{u}");
        }
        for u in [
            "https://example.com",
            "https://10.211.55.2:8001",
            "https://api.anthropic.com/v1/messages",
        ] {
            assert!(!is_loopback(&url::Url::parse(u).unwrap()), "{u}");
        }
    }

    #[test]
    fn the_sandbox_network_mode_is_what_binds_the_model() {
        let unrestricted = LocalNetwork::for_agent_policy(
            NetworkModeConfig::Unrestricted,
            vec!["example.com".into()],
        );
        assert!(
            unrestricted.permits("evil.test", Origin::Agent),
            "a domain list without the mode that reads it restricts nothing, \
             exactly as it does not restrict a sandboxed subprocess"
        );

        let listed = LocalNetwork::for_agent_policy(
            NetworkModeConfig::Allowlist,
            vec!["example.com".into()],
        );
        assert!(listed.permits("example.com", Origin::Agent));
        assert!(!listed.permits("evil.test", Origin::Agent));

        let empty_list = LocalNetwork::for_agent_policy(NetworkModeConfig::Allowlist, Vec::new());
        assert!(!empty_list.permits("example.com", Origin::Agent));

        let denied = LocalNetwork::for_agent_policy(NetworkModeConfig::DenyAll, Vec::new());
        assert!(!denied.permits("example.com", Origin::Agent));
        assert!(denied.permits("api.anthropic.com", Origin::Operator));
    }

    #[tokio::test]
    async fn a_refused_host_never_reaches_the_socket() {
        let n = LocalNetwork::new(vec!["example.com".into()], false);
        let err = n
            .send(HttpRequest::get("http://127.0.0.1:1/"), Origin::Agent)
            .await
            .expect_err("port 1 on loopback would be a connection error, not a policy one");
        assert!(
            matches!(err, ExecError::Denied(_)),
            "the policy has to answer before the connection is attempted, or \
             a denied host is still a DNS lookup and a SYN: {err:?}"
        );
    }

    #[tokio::test]
    async fn the_same_host_is_reachable_for_an_operator() {
        let n = LocalNetwork::new(vec!["example.com".into()], false);
        let err = n
            .send(HttpRequest::get("http://127.0.0.1:1/"), Origin::Operator)
            .await
            .expect_err("nothing listens on port 1");
        assert!(
            !matches!(err, ExecError::Denied(_)),
            "an operator endpoint must get as far as the socket: {err:?}"
        );
    }

    /// A server on this machine that counts what reaches it and answers each
    /// request with `answer`.
    async fn a_counting_server(
        answer: impl Fn(u16) -> String + Send + Sync + 'static,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(answer(port).as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (port, hits)
    }

    fn ok_response(_: u16) -> String {
        "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".into()
    }

    #[tokio::test]
    async fn agent_requests_to_private_addresses_are_refused() {
        let (port, hits) = a_counting_server(ok_response).await;
        let n = LocalNetwork::default();
        for host in [
            "127.0.0.1",
            "localhost",
            "LOCALHOST.",
            "0.0.0.0",
            "[::ffff:127.0.0.1]",
            "[::ffff:7f00:1]",
            "127.1",
            "2130706433",
            "x@127.0.0.1",
            "x:y@localhost",
        ] {
            let err = n
                .send(
                    HttpRequest::get(format!("http://{host}:{port}/")),
                    Origin::Agent,
                )
                .await
                .expect_err(host);
            assert!(matches!(err, ExecError::Denied(_)), "{host}: {err:?}");
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!n.permits("127.0.0.1", Origin::Agent));
        assert!(!n.permits("[::1]", Origin::Agent));
        assert!(n.permits("example.com", Origin::Agent));
    }

    #[tokio::test]
    async fn a_host_the_deployment_named_is_reachable_at_an_internal_address() {
        let (port, hits) = a_counting_server(ok_response).await;
        for n in [
            LocalNetwork::for_agent_policy(
                NetworkModeConfig::Unrestricted,
                vec!["127.0.0.1".into()],
            ),
            LocalNetwork::for_agent_policy(NetworkModeConfig::Allowlist, vec!["127.0.0.1".into()]),
        ] {
            let r = n
                .send(
                    HttpRequest::get(format!("http://127.0.0.1:{port}/")),
                    Origin::Agent,
                )
                .await
                .expect("a vouched host");
            assert_eq!(r.body_text(), "ok");
            assert!(n.permits("127.0.0.1", Origin::Agent));
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn every_hop_of_a_redirect_is_kept_off_internal_addresses() {
        let (inner_port, inner_hits) = a_counting_server(ok_response).await;
        let (outer_port, _) = a_counting_server(move |_| {
            format!(
                "HTTP/1.1 302 Found\r\nlocation: http://localhost:{inner_port}/\r\n\
                 content-length: 0\r\nconnection: close\r\n\r\n"
            )
        })
        .await;
        let n = LocalNetwork::for_agent_policy(
            NetworkModeConfig::Unrestricted,
            vec!["127.0.0.1".into()],
        );
        let err = n
            .send(
                HttpRequest::get(format!("http://127.0.0.1:{outer_port}/")),
                Origin::Agent,
            )
            .await
            .expect_err("the second hop is not a vouched host");
        assert!(matches!(err, ExecError::Denied(_)), "{err:?}");
        assert_eq!(inner_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_public_only_request_is_refused_whoever_chose_it() {
        let (port, hits) = a_counting_server(ok_response).await;
        let n = LocalNetwork::default();
        let url = format!("http://127.0.0.1:{port}/");
        let err = n
            .send(HttpRequest::post(&url).public_only(), Origin::Operator)
            .await
            .expect_err("an internal address");
        assert!(matches!(err, ExecError::Denied(_)), "{err:?}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);

        n.send(HttpRequest::post(&url), Origin::Operator)
            .await
            .expect("without the flag an operator endpoint is reached");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_internal_target_is_reached_and_only_that_target() {
        let (port, hits) = a_counting_server(ok_response).await;
        let n = LocalNetwork::default();
        let url = format!("http://127.0.0.1:{port}/");
        n.send(
            HttpRequest::post(&url).public_only_but_target(),
            Origin::Operator,
        )
        .await
        .expect("the named target is reachable");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        let err = n
            .send(
                HttpRequest::post(format!("http://localhost:{port}/")).public_only_but_target(),
                Origin::Agent,
            )
            .await
            .expect_err("a model-directed request gains nothing from it");
        assert!(matches!(err, ExecError::Denied(_)), "{err:?}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_internal_target_cannot_redirect_to_another_internal_host() {
        let (inner_port, inner_hits) = a_counting_server(ok_response).await;
        let (outer_port, outer_hits) = a_counting_server(move |_| {
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://127.0.0.1:{inner_port}/\r\ncontent-length: 0\r\n\r\n"
            )
        })
        .await;
        let n = LocalNetwork::default();
        let err = n
            .send(
                HttpRequest::post(format!("http://127.0.0.1:{outer_port}/"))
                    .public_only_but_target(),
                Origin::Operator,
            )
            .await
            .expect_err("the second hop is another internal target");
        assert!(matches!(err, ExecError::Denied(_)), "{err:?}");
        assert_eq!(outer_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(inner_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// The connection resolves the name again; what it gets is checked there
    /// too, so a name that answers differently the second time is still
    /// refused — and the refusal comes back as one, not a connect error.
    #[tokio::test]
    async fn the_connection_itself_refuses_a_name_that_resolves_inside() {
        let (port, hits) = a_counting_server(ok_response).await;
        install_tls_provider();
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(PublicOnlyResolver {
                proxies: Vec::new(),
            })
            .build()
            .expect("client");
        let url = url::Url::parse(&format!("http://localhost:{port}/")).expect("url");
        let e = client
            .get(url.clone())
            .send()
            .await
            .expect_err("refused at resolution");
        let err = transport_error(&url, e, Origin::Agent);
        assert!(matches!(err, ExecError::Denied(_)), "{err:?}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);

        let through_proxy = PublicOnlyResolver {
            proxies: vec!["localhost".into()],
        };
        let name: reqwest::dns::Name = "localhost".parse().expect("name");
        assert!(
            reqwest::dns::Resolve::resolve(&through_proxy, name)
                .await
                .is_ok(),
            "a configured proxy's own name is the one internal name this resolver serves"
        );
    }
}
