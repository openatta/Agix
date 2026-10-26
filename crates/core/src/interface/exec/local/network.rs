//! `LocalNetwork` — outbound requests from this process.

use std::collections::BTreeMap;
use std::time::Duration;

use futures::StreamExt;

use crate::context::config::NetworkModeConfig;
use crate::interface::exec::{ExecError, HttpRequest, HttpResponse, HttpStream, Network, Origin};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// This process's own network, under the deployment's egress policy.
pub struct LocalNetwork {
    proxied: reqwest::Client,
    direct: reqwest::Client,
    /// Hosts the model is allowed to reach. Empty means no restriction, which
    /// is what an unconfigured deployment has always had.
    allowed_domains: Vec<String>,
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
        Self {
            proxied: shared_client(false),
            direct: shared_client(true),
            allowed_domains,
            agent_offline,
        }
    }

    /// The egress a deployment's `sandbox.network_mode` describes, read the
    /// same way the sandbox backends read it so the two cannot disagree about
    /// where the model may reach.
    pub fn for_agent_policy(mode: NetworkModeConfig, allowed_domains: Vec<String>) -> Self {
        match mode {
            NetworkModeConfig::Unrestricted => Self::new(Vec::new(), false),
            NetworkModeConfig::DenyAll => Self::new(Vec::new(), true),
            // An empty allowlist under this mode names nothing reachable, the
            // same reading the sandbox backends give it.
            NetworkModeConfig::Allowlist if allowed_domains.is_empty() => {
                Self::new(Vec::new(), true)
            }
            NetworkModeConfig::Allowlist => Self::new(allowed_domains, false),
        }
    }

    fn refuse(&self, url: &url::Url, origin: Origin) -> Result<(), ExecError> {
        let host = url
            .host_str()
            .ok_or_else(|| ExecError::failed(format!("not a url: {url}")))?;
        if self.permits(host, origin) {
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

/// One connection pool per process, not one per `LocalNetwork`.
///
/// The client carries no policy — the allowlist and the offline switch live on
/// the wrapper — so two deployments' worth of egress rules can share it, and a
/// fresh one per tool call would throw away keep-alive and re-derive the TLS
/// roots every time.
fn shared_client(no_proxy: bool) -> reqwest::Client {
    static PROXIED: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    static DIRECT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let cell = if no_proxy { &DIRECT } else { &PROXIED };
    cell.get_or_init(|| build_client(no_proxy)).clone()
}

/// No total request timeout: it would bound the whole response body, which for
/// a server-sent-event stream means tearing the connection down mid-answer.
/// How long a call may take belongs to the caller.
fn build_client(no_proxy: bool) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("agix/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if no_proxy {
        b = b.no_proxy();
    }
    b.build().unwrap_or_else(|e| {
        // The fallback is `reqwest`'s default, which is the *proxied* client —
        // so falling back here would send loopback traffic through the ambient
        // proxy, the one thing `no_proxy` exists to prevent. Say so rather
        // than silently inverting the intent.
        tracing::error!(error = %e, no_proxy, "could not build the HTTP client; falling back to the default one");
        reqwest::Client::default()
    })
}

fn headers_of(map: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    map.iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
        .collect()
}

fn transport_error(url: &url::Url, e: reqwest::Error) -> ExecError {
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
        let mut method = req.method;
        let mut body = req.body;
        let mut headers = req.headers;

        for hop in 0..=req.max_redirects {
            self.refuse(&url, origin)?;

            let m = reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| ExecError::failed(format!("{method}: {e}")))?;
            let mut b = self.client_for(&url).request(m.clone(), url.clone());
            for (k, v) in &headers {
                b = b.header(k, v);
            }
            if let Some(body) = &body {
                b = b.body(body.clone());
            }
            let resp = b.send().await.map_err(|e| transport_error(&url, e))?;

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

    fn permits(&self, host: &str, origin: Origin) -> bool {
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
                let host = host.to_ascii_lowercase();
                self.allowed_domains.is_empty()
                    || self.allowed_domains.iter().any(|d| {
                        let d = d.to_ascii_lowercase();
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
}
