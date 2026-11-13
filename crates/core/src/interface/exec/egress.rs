//! `EgressProxy` — the way a sandboxed command reaches only the hosts a
//! domain allowlist names.
//!
//! Neither OS backend can filter outbound traffic by domain: a sandbox profile
//! names a remote host only as `*` or `localhost`, and bubblewrap can only cut
//! the network as a whole. So [`NetworkMode::Allowlist`] is delivered by cutting
//! the network and leaving one way out — a proxy outside the sandbox that
//! admits a connection only to a host on the list. The sandbox lets the
//! command reach the proxy's [`ProxyEndpoint`] (`SandboxPolicy::egress`) and
//! nothing else; the command finds the proxy through the variables of
//! [`EgressHandle::env`].
//!
//! Only the contract is here. No provider implements it yet, so a policy
//! carries no endpoint and an allowlist stays [`Enforcement::Partial`]: the
//! network is cut and the domains it names are reported unmet. A provider
//! plugged in later raises that to `Full` without changing what
//! `Allowlist` promises — only the hosts on the list.
//!
//! [`NetworkMode::Allowlist`]: super::NetworkMode::Allowlist
//! [`Enforcement::Partial`]: super::Enforcement::Partial

use std::any::Any;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use super::network::domains_name_host;
use super::ExecError;

/// The hosts a proxy lets a command connect to: `sandbox.allowed_domains`,
/// each entry itself or any subdomain of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowedHosts(Vec<String>);

impl AllowedHosts {
    pub fn new(domains: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(domains.into_iter().map(Into::into).collect())
    }

    pub fn domains(&self) -> &[String] {
        &self.0
    }

    /// Whether `host` — as a `CONNECT` target or an absolute URI names it —
    /// is on the list. A proxy checks the address the name resolves to as
    /// well, so that a listed name pointing at an internal address does not
    /// open the way to it.
    pub fn admits(&self, host: &str) -> bool {
        domains_name_host(&self.0, host)
    }
}

/// Where a sandboxed command reaches the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyEndpoint {
    /// A loopback port: what a macOS profile can let through as
    /// `localhost:PORT`.
    Tcp(SocketAddr),
    /// A unix socket: what is bound into a Linux network namespace, where no
    /// host port is reachable.
    Unix(PathBuf),
}

/// A proxy admitting one set of hosts, for as long as the handle is held.
pub struct EgressHandle {
    pub endpoint: ProxyEndpoint,
    /// What a command needs to find the proxy: `HTTP_PROXY`, `HTTPS_PROXY`,
    /// `NO_PROXY` and their lowercase forms. A program that ignores them has
    /// no network at all.
    pub env: Vec<(String, String)>,
    keep: Arc<dyn Any + Send + Sync>,
}

impl EgressHandle {
    /// `keep` is whatever keeps the proxy serving these hosts; dropping the
    /// last handle releases it.
    pub fn new(
        endpoint: ProxyEndpoint,
        env: Vec<(String, String)>,
        keep: Arc<dyn Any + Send + Sync>,
    ) -> Self {
        Self {
            endpoint,
            env,
            keep,
        }
    }
}

impl Clone for EgressHandle {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            env: self.env.clone(),
            keep: Arc::clone(&self.keep),
        }
    }
}

impl std::fmt::Debug for EgressHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressHandle")
            .field("endpoint", &self.endpoint)
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

/// A proxy outside the sandbox that lets a command connect only to the hosts
/// it was admitted for.
pub trait EgressProxy: Send + Sync {
    /// A proxy serving `hosts`. `Unavailable` when no proxy can be started;
    /// the caller then keeps the network cut rather than opening it.
    fn admit(&self, hosts: &AllowedHosts) -> Result<EgressHandle, ExecError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listed_domain_admits_itself_and_its_subdomains_only() {
        let hosts = AllowedHosts::new(["example.com"]);
        assert!(hosts.admits("example.com"));
        assert!(hosts.admits("API.Example.com."));
        assert!(!hosts.admits("example.com.evil.net"));
        assert!(!hosts.admits("notexample.com"));
        assert!(!AllowedHosts::default().admits("example.com"));
    }

    #[test]
    fn a_handle_keeps_the_proxy_until_the_last_clone_is_dropped() {
        let keep: Arc<dyn Any + Send + Sync> = Arc::new(());
        let handle = EgressHandle::new(
            ProxyEndpoint::Unix(PathBuf::from("/run/proxy.sock")),
            vec![("HTTPS_PROXY".into(), "http://localhost:1".into())],
            keep.clone(),
        );
        let copy = handle.clone();
        assert_eq!(Arc::strong_count(&keep), 3);
        drop(handle);
        drop(copy);
        assert_eq!(Arc::strong_count(&keep), 1);
    }
}
