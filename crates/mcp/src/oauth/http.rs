//! The HTTP an MCP server's authorization goes over.
//!
//! [`OAuthHttp`] carries every request an MCP server's authorization makes —
//! metadata discovery, registration, token and revocation calls — under an
//! [`AddressPolicy`] taken from the MCP server's own URL: a public server
//! cannot point discovery at a loopback, private or link-local address, so a
//! hostile server cannot make the daemon probe its own network. Names are
//! resolved through the same filter. A loopback MCP server's authorization
//! bypasses `HTTP(S)_PROXY`, as the rest of the daemon's loopback egress
//! does; through a proxy only the host name can be checked, since the proxy
//! resolves it. Everything goes over https, except to a loopback address; for
//! a server off loopback the client itself refuses plain http, so metadata
//! cannot be swapped in transit. Which addresses count as internal is
//! `base`'s [`is_internal_address`], the same answer a model-directed fetch
//! gets.
//!
//! [`transport_client`] is the MCP connection's own client, kept to the
//! server's origin across redirects and filtered the same way.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base::interface::exec::is_internal_address;
use base::interface::exec::network::carried_v4;
use futures::StreamExt;
use rmcp::transport::auth::{
    OAuthHttpClient, OAuthHttpClientError, OAuthHttpClientFuture, OAuthHttpRedirectPolicy,
    OAuthHttpRequest,
};
use url::{Host, Url};

const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY: usize = 1024 * 1024;
const MAX_REDIRECTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddressClass {
    Public,
    Loopback,
    Private,
    Reserved,
}

/// Public is whatever [`is_internal_address`] does not call internal; of
/// the rest, only loopback and private networks are told apart, everything
/// else is reserved.
fn class_of(ip: IpAddr) -> AddressClass {
    if !is_internal_address(ip) {
        return AddressClass::Public;
    }
    let v4 = match ip {
        IpAddr::V6(v6) if v6.is_loopback() => return AddressClass::Loopback,
        IpAddr::V6(v6) if v6.segments()[0] & 0xfe00 == 0xfc00 => return AddressClass::Private,
        IpAddr::V6(v6) => carried_v4(v6),
        IpAddr::V4(v4) => Some(v4),
    };
    match v4 {
        Some(v4) if v4.is_loopback() => AddressClass::Loopback,
        Some(v4) if v4.is_private() || is_shared_address_space(v4) => AddressClass::Private,
        _ => AddressClass::Reserved,
    }
}

/// 100.64.0.0/10, carrier-grade NAT.
fn is_shared_address_space(v4: std::net::Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    a == 100 && (64..128).contains(&b)
}

fn host_class(url: &Url) -> Option<AddressClass> {
    match url.host()? {
        Host::Ipv4(ip) => Some(class_of(ip.into())),
        Host::Ipv6(ip) => Some(class_of(ip.into())),
        Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            Some(if d == "localhost" || d.ends_with(".localhost") {
                AddressClass::Loopback
            } else {
                AddressClass::Public
            })
        }
    }
}

pub(crate) fn is_loopback(url: &Url) -> bool {
    host_class(url) == Some(AddressClass::Loopback)
}

/// OAuth 2.1 §1.5: codes, verifiers, secrets and tokens only travel over
/// TLS. Plain http is left to an authorization server on this machine.
pub(crate) fn require_https_off_loopback(url: &Url) -> Result<(), String> {
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(url) => Ok(()),
        "http" => Err(format!(
            "`{url}`: OAuth uses https; plain http only to an authorization server on this machine"
        )),
        _ => Err(format!(
            "`{url}`: only https (or http on loopback) is used for OAuth"
        )),
    }
}

/// Which addresses the authorization of one MCP server may reach: public
/// ones, plus the class of the server's own host when that is loopback or
/// private. Reserved addresses (link-local — cloud metadata lives there —
/// multicast, unspecified) never.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressPolicy {
    allowed: Option<AddressClass>,
    configured: bool,
}

impl AddressPolicy {
    pub fn for_server(server: &Url) -> Self {
        let allowed = match host_class(server) {
            Some(c @ (AddressClass::Loopback | AddressClass::Private)) => Some(c),
            _ => None,
        };
        Self {
            allowed,
            configured: false,
        }
    }

    /// For the connection to an MCP server off loopback, whose URL is
    /// configured: public and private networks, where an organisation's own
    /// servers live, but never this machine or a reserved address.
    pub fn for_remote_transport() -> Self {
        Self {
            allowed: Some(AddressClass::Private),
            configured: false,
        }
    }

    /// For an endpoint the deployment named itself (an enterprise IdP), which
    /// nothing a server says can redirect.
    pub fn configured() -> Self {
        Self {
            allowed: None,
            configured: true,
        }
    }

    pub fn admits_private(&self) -> bool {
        self.allowed == Some(AddressClass::Private)
    }

    fn allows_class(&self, class: AddressClass) -> bool {
        self.configured || class == AddressClass::Public || Some(class) == self.allowed
    }

    pub fn allows_ip(&self, ip: IpAddr) -> bool {
        self.allows_class(class_of(ip))
    }

    pub fn check_url(&self, url: &Url) -> Result<(), String> {
        require_https_off_loopback(url)?;
        match host_class(url) {
            Some(class) if self.allows_class(class) => Ok(()),
            Some(_) => Err(format!(
                "`{url}`: OAuth requests for this MCP server may not reach that address"
            )),
            None => Err(format!("`{url}` names no host")),
        }
    }
}

struct FilteringResolver {
    policy: AddressPolicy,
}

impl reqwest::dns::Resolve for FilteringResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let policy = self.policy;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| policy.allows_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!(
                    "`{host}` resolves to no address OAuth requests for this MCP server may reach"
                )
                .into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Refused(String);

/// See the module docs.
pub struct OAuthHttp {
    client: reqwest::Client,
    policy: AddressPolicy,
}

impl OAuthHttp {
    pub fn for_server(server: &Url) -> Result<Self, String> {
        Self::build(server, AddressPolicy::for_server(server))
    }

    /// For calls to `endpoint`, which the deployment configured.
    pub fn for_configured(endpoint: &Url) -> Result<Self, String> {
        Self::build(endpoint, AddressPolicy::configured())
    }

    fn build(server: &Url, policy: AddressPolicy) -> Result<Self, String> {
        platform::install_tls_provider();
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("agix/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(FilteringResolver { policy }));
        builder = if is_loopback(server) {
            builder.no_proxy()
        } else {
            builder.https_only(true)
        };
        let client = builder
            .build()
            .map_err(|e| format!("could not build the OAuth HTTP client: {e}"))?;
        Ok(Self { client, policy })
    }

    pub fn policy(&self) -> AddressPolicy {
        self.policy
    }

    /// The client itself, for rmcp's `AuthorizationManager::with_client`.
    /// Its resolver applies the policy; host names and IP literals are also
    /// checked by rmcp before any discovered URL is fetched.
    pub fn client(&self) -> reqwest::Client {
        self.client.clone()
    }

    /// Send `request` after checking its target, returning status and body.
    pub async fn send(
        &self,
        request: reqwest::Request,
    ) -> Result<(reqwest::StatusCode, Vec<u8>), String> {
        self.policy.check_url(request.url())?;
        let url = request.url().clone();
        let response = self
            .client
            .execute(request)
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let status = response.status();
        let body = read_body(response)
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        Ok((status, body))
    }

    pub fn request(&self, method: reqwest::Method, url: &Url) -> reqwest::RequestBuilder {
        self.client.request(method, url.clone())
    }

    async fn execute(
        &self,
        request: OAuthHttpRequest,
    ) -> Result<http::Response<Vec<u8>>, OAuthHttpClientError> {
        let follow = request.redirect_policy == OAuthHttpRedirectPolicy::Follow;
        let mut request = reqwest::Request::try_from(request.request)?;
        for _ in 0..=MAX_REDIRECTS {
            self.policy
                .check_url(request.url())
                .map_err(|e| Box::new(Refused(e)) as OAuthHttpClientError)?;
            let url = request.url().clone();
            let retry = request.try_clone();
            let response = self.client.execute(request).await?;
            let status = response.status();
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url.join(l).ok());
            match (follow && status.is_redirection(), location, retry) {
                (true, Some(next), Some(mut again)) => {
                    if !matches!(status.as_u16(), 307 | 308) {
                        *again.method_mut() = reqwest::Method::GET;
                        *again.body_mut() = None;
                    }
                    *again.url_mut() = next;
                    request = again;
                }
                _ => return to_oauth_response(response).await,
            }
        }
        Err(Box::new(Refused(format!(
            "more than {MAX_REDIRECTS} redirects"
        ))))
    }
}
async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, OAuthHttpClientError> {
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > MAX_BODY {
            return Err(Box::new(Refused(format!(
                "response body exceeds {MAX_BODY} bytes"
            ))));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn to_oauth_response(
    response: reqwest::Response,
) -> Result<http::Response<Vec<u8>>, OAuthHttpClientError> {
    let mut builder = http::Response::builder()
        .status(response.status())
        .version(response.version());
    for (name, value) in response.headers() {
        builder = builder.header(name, value);
    }
    Ok(builder.body(read_body(response).await?)?)
}

impl OAuthHttpClient for OAuthHttp {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(OAuthHttp::execute(self, request))
    }
}

/// The client an MCP server's streamable HTTP transport talks through. It
/// follows a redirect only within the server's origin, so neither the
/// session nor the configured headers go anywhere else. Loopback servers
/// bypass `HTTP(S)_PROXY`; any other server's host names resolve under
/// [`AddressPolicy::for_remote_transport`]. No total timeout, since a
/// response may be a stream.
pub(crate) fn transport_client(server: &str) -> reqwest::Client {
    platform::install_tls_provider();
    let same_origin = reqwest::redirect::Policy::custom(|attempt| {
        let from = attempt.previous().last().map(Url::origin);
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error(format!("more than {MAX_REDIRECTS} redirects"))
        } else if from.as_ref() == Some(&attempt.url().origin()) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    let mut b = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(same_origin);
    if Url::parse(server).is_ok_and(|u| is_loopback(&u)) {
        b = b.no_proxy();
    } else {
        b = b.dns_resolver(Arc::new(FilteringResolver {
            policy: AddressPolicy::for_remote_transport(),
        }));
    }
    b.build().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn a_public_server_reaches_only_public_addresses() {
        let policy = AddressPolicy::for_server(&url("https://mcp.example.com/mcp"));
        assert!(policy.check_url(&url("https://as.example.com/")).is_ok());
        for blocked in [
            "http://127.0.0.1:8080/",
            "http://localhost/",
            "http://10.0.0.9/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://0.0.0.0/",
        ] {
            assert!(policy.check_url(&url(blocked)).is_err(), "{blocked}");
        }
        assert!(policy.check_url(&url("file:///etc/passwd")).is_err());
    }

    #[test]
    fn every_ipv6_spelling_of_an_internal_ipv4_address_is_refused_too() {
        let policy = AddressPolicy::for_server(&url("https://mcp.example.com/mcp"));
        for blocked in [
            "::ffff:0:7f00:1",
            "::ffff:0:a9fe:a9fe",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b:1::808:808",
            "2002:a9fe:a9fe::1",
            "2001:0:4136:e378:8000:63bf:80ff:fffe",
            "192.0.0.8",
            "240.0.0.1",
        ] {
            assert!(!policy.allows_ip(blocked.parse().unwrap()), "{blocked}");
        }
        assert!(policy.allows_ip("2001:4860:4860::8888".parse().unwrap()));
        assert!(policy.allows_ip("64:ff9b::808:808".parse().unwrap()));

        let local = AddressPolicy::for_server(&url("http://127.0.0.1:9000/mcp"));
        assert!(local.allows_ip("::1".parse().unwrap()));
        assert!(local.allows_ip("::ffff:0:7f00:1".parse().unwrap()));
        assert!(!local.allows_ip("::ffff:0:a9fe:a9fe".parse().unwrap()));
        let corp = AddressPolicy::for_server(&url("https://10.1.2.3/mcp"));
        assert!(corp.allows_ip("fd00::1".parse().unwrap()));
        assert!(corp.allows_ip("::ffff:0:a00:9".parse().unwrap()));
        assert!(!corp.allows_ip("fe80::1".parse().unwrap()));
    }

    #[test]
    fn a_loopback_or_private_server_also_reaches_its_own_class() {
        let local = AddressPolicy::for_server(&url("http://127.0.0.1:9000/mcp"));
        assert!(local.check_url(&url("http://localhost:9001/")).is_ok());
        assert!(local.check_url(&url("http://10.0.0.9/")).is_err());
        assert!(local.check_url(&url("http://169.254.169.254/")).is_err());

        let corp = AddressPolicy::for_server(&url("https://10.1.2.3/mcp"));
        assert!(corp.check_url(&url("https://10.0.0.9/")).is_ok());
        assert!(corp.check_url(&url("https://127.0.0.1/")).is_err());
    }

    #[test]
    fn plain_http_reaches_only_loopback() {
        let corp = AddressPolicy::for_server(&url("http://10.1.2.3/mcp"));
        assert!(corp.check_url(&url("http://10.0.0.9/")).is_err());
        let public = AddressPolicy::for_server(&url("https://mcp.example.com/mcp"));
        assert!(public
            .check_url(&url("http://as.example.com/token"))
            .is_err());
        let configured = AddressPolicy::configured();
        assert!(configured
            .check_url(&url("http://idp.example.com/token"))
            .is_err());
        assert!(configured
            .check_url(&url("https://idp.example.com/token"))
            .is_ok());
        assert!(configured
            .check_url(&url("http://127.0.0.1:9/token"))
            .is_ok());
        let local = AddressPolicy::for_server(&url("http://127.0.0.1:9000/mcp"));
        assert!(local.check_url(&url("http://[::1]:9001/token")).is_ok());
    }

    #[tokio::test]
    async fn discovery_for_a_server_off_loopback_never_goes_out_over_plain_http() {
        let http = OAuthHttp::for_server(&url("https://mcp.example.com/mcp")).unwrap();
        let sent = tokio::time::timeout(
            Duration::from_secs(2),
            http.client()
                .get("http://192.0.2.1/.well-known/oauth-authorization-server")
                .send(),
        )
        .await
        .expect("refused before any connection was tried");
        assert!(sent.is_err());
    }

    #[tokio::test]
    async fn a_refused_address_is_never_contacted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let http = OAuthHttp::for_server(&url("https://mcp.example.com/mcp")).unwrap();
        let target = url(&format!(
            "http://127.0.0.1:{port}/.well-known/oauth-authorization-server"
        ));
        let request = http.request(reqwest::Method::GET, &target).build().unwrap();
        let err = http.send(request).await.unwrap_err();
        assert!(err.contains("may not reach"), "{err}");
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(accepted.is_err(), "the refused address was connected to");
    }

    /// A server on loopback answering every request with `reply(path)`,
    /// counting the requests it was sent.
    async fn answering(
        reply: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (counter, reply) = (seen.clone(), Arc::new(reply));
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let reply = reply.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split(' ').nth(1).unwrap_or("/").to_string();
                    let _ = stream.write_all(reply(&path).as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (base, seen)
    }

    fn redirect_to(location: &str) -> String {
        format!(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\n\
             connection: close\r\n\r\n"
        )
    }

    const OK: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";

    #[tokio::test]
    async fn the_transport_follows_a_redirect_only_within_the_servers_origin() {
        let (elsewhere, reached) = answering(|_| OK.to_string()).await;
        let away = format!("{elsewhere}/stolen");
        let (server, _) = answering(move |path| match path {
            "/mcp" => redirect_to("/moved"),
            "/moved" => OK.to_string(),
            _ => redirect_to(&away),
        })
        .await;
        let client = transport_client(&format!("{server}/mcp"));

        let response = client.get(format!("{server}/mcp")).send().await.unwrap();
        assert_eq!(response.status(), 200, "a same-origin redirect is followed");

        let response = client
            .get(format!("{server}/away"))
            .header("x-api-key", "secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 307, "a cross-origin one is handed back");
        assert_eq!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the other origin was never contacted"
        );
    }

    #[tokio::test]
    async fn a_remote_servers_transport_never_resolves_to_this_machine() {
        let policy = AddressPolicy::for_remote_transport();
        let resolver = FilteringResolver { policy };
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        assert!(reqwest::dns::Resolve::resolve(&resolver, name)
            .await
            .is_err());
        for refused in [
            "127.0.0.1",
            "::1",
            "169.254.169.254",
            "::ffff:0:7f00:1",
            "0.0.0.0",
        ] {
            assert!(!policy.allows_ip(refused.parse().unwrap()), "{refused}");
        }
        for allowed in ["10.0.0.5", "192.168.1.9", "fd00::1", "93.184.216.34"] {
            assert!(policy.allows_ip(allowed.parse().unwrap()), "{allowed}");
        }
    }
}
