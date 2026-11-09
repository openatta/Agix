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
//! resolves it.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

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

fn class_of(ip: IpAddr) -> AddressClass {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            if v4.is_loopback() {
                AddressClass::Loopback
            } else if v4.is_private() || (a == 100 && (64..=127).contains(&b)) {
                AddressClass::Private
            } else if v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || a == 0
                || (a == 198 && (b == 18 || b == 19))
            {
                AddressClass::Reserved
            } else {
                AddressClass::Public
            }
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            if v6.is_loopback() {
                AddressClass::Loopback
            } else if first & 0xfe00 == 0xfc00 {
                AddressClass::Private
            } else if v6.is_unspecified() || v6.is_multicast() || first & 0xffc0 == 0xfe80 {
                AddressClass::Reserved
            } else {
                AddressClass::Public
            }
        }
    }
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
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("`{url}`: only http and https are used for OAuth"));
        }
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
        base::interface::exec::local::install_tls_provider();
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("agix/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(FilteringResolver { policy }));
        if is_loopback(server) {
            builder = builder.no_proxy();
        }
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

/// The client an MCP server's streamable HTTP transport talks through:
/// loopback servers bypass `HTTP(S)_PROXY`. No total timeout, since a
/// response may be a stream.
pub(crate) fn transport_client(server: &str) -> reqwest::Client {
    base::interface::exec::local::install_tls_provider();
    let mut b = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
    if Url::parse(server).is_ok_and(|u| is_loopback(&u)) {
        b = b.no_proxy();
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
    fn a_loopback_or_private_server_also_reaches_its_own_class() {
        let local = AddressPolicy::for_server(&url("http://127.0.0.1:9000/mcp"));
        assert!(local.check_url(&url("http://localhost:9001/")).is_ok());
        assert!(local.check_url(&url("http://10.0.0.9/")).is_err());
        assert!(local.check_url(&url("http://169.254.169.254/")).is_err());

        let corp = AddressPolicy::for_server(&url("http://10.1.2.3/mcp"));
        assert!(corp.check_url(&url("http://10.0.0.9/")).is_ok());
        assert!(corp.check_url(&url("http://127.0.0.1/")).is_err());
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
}
