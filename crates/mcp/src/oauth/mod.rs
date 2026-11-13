//! MCP authorization (the 2025-11-25 authorization spec, OAuth 2.1) for
//! streamable HTTP / SSE servers.
//!
//! One [`McpAuth`] per session pool. It keeps each server's authorization
//! state (`none` → `required` → `authorizing` → `authorized`, or `failed`),
//! runs the browser sign-in a host drives over RPC ([`McpAuth::start`] /
//! [`McpAuth::complete`]), hands the connect layer an rmcp
//! `AuthorizationManager` that attaches and refreshes the bearer token, and
//! reports transitions to the pool through an [`AuthListener`]. Discovery,
//! PKCE, the `resource` indicator, issuer checks and refresh are rmcp's;
//! what is decided here is which client identity to use, which scopes to ask
//! for, where the browser comes back to, and when tokens are thrown away.
//! Servers no person signs in to — client credentials, enterprise-managed —
//! are in [`unattended`].
//!
//! Records live in an [`McpCredentialStore`] keyed by the canonical server
//! URL. Design and usage: `docs/mcp_auth.md`.

mod callback;
pub mod http;
mod start;
pub mod store;
mod unattended;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, AuthorizationSession, OAuthClientConfig,
};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::config::{expand_env_vars, EnterpriseIdp, McpOAuthConfig, McpServerConfig};
use crate::error::{without_url_secrets, McpError};
use crate::policy::McpPolicyGate;

pub use callback::LoopbackCallback;
pub use store::{
    FileCredentialStore, McpClientRegistration, McpCredentialStore, McpCredentials, McpTokens,
};
pub use unattended::EnterpriseIdentity;

pub const FLOW_TTL: Duration = Duration::from_secs(10 * 60);

/// The client this daemon presents to authorization servers it has no
/// registration with.
#[derive(Debug, Clone)]
pub struct McpOAuthClient {
    /// The `client_name` sent with dynamic client registration, shown by the
    /// authorization server on its consent page.
    pub client_name: String,
    /// An HTTPS URL serving this client's metadata document (a Client ID
    /// Metadata Document), used as the client id where the authorization
    /// server supports that, before falling back to dynamic registration.
    pub client_metadata_url: Option<String>,
}

impl Default for McpOAuthClient {
    fn default() -> Self {
        Self {
            client_name: "Agix".into(),
            client_metadata_url: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    #[default]
    None,
    Required,
    Authorizing,
    Authorized,
    Failed,
}

/// A server's authorization as `mcp.status` reports it. Never carries a
/// token.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AuthStatus {
    pub state: AuthState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// RFC 3339.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthEvent {
    /// The server refuses its token, or has none: it is disconnected until a
    /// person signs in.
    Required {
        server: String,
        url: String,
        scopes: Vec<String>,
    },
    /// The server wants more scope for some request; it stays connected.
    StepUp {
        server: String,
        url: String,
        scopes: Vec<String>,
    },
    Completed {
        server: String,
        url: String,
    },
    Failed {
        server: String,
        url: String,
        error: String,
    },
}

pub type AuthListener = Arc<dyn Fn(AuthEvent) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum AuthFlowError {
    /// The request itself is wrong: no such server, flow or redirect.
    #[error("{}", without_url_secrets(.0))]
    Invalid(String),
    /// The MCP policy does not allow the authorization server.
    #[error("{}", without_url_secrets(.0))]
    Denied(String),
    /// The authorization server, or the exchange with it, failed.
    #[error("{}", without_url_secrets(.0))]
    Failed(String),
}

/// Where the browser returns after signing in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    /// `http://127.0.0.1:<port>/callback`, served by the daemon.
    Loopback,
    /// A URI the host owns; it passes the callback URL to [`McpAuth::complete`].
    External(String),
}

pub struct FlowStart {
    pub flow_id: String,
    pub authorize_url: String,
    pub redirect_uri: String,
    pub expires_in: u64,
    /// Loopback flows: each browser arrival at the listener.
    pub callbacks: Option<mpsc::Receiver<LoopbackCallback>>,
}

#[derive(Debug, Clone)]
pub struct FlowDone {
    pub server: String,
    pub url: String,
    pub scopes: Vec<String>,
}

/// `url` with no fragment or credentials and no trailing `/` on a bare
/// host: the resource a server's tokens are bound to and its record's key.
pub fn canonical_server_url(raw: &str) -> Result<String, String> {
    let mut url = Url::parse(raw).map_err(|e| format!("`{raw}` is not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("`{raw}` is not an http(s) URL"));
    }
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let mut s = url.to_string();
    if url.path() == "/" && url.query().is_none() {
        s.pop();
    }
    Ok(s)
}

/// The URL a server config connects to, environment variables expanded.
pub fn server_url(cfg: &McpServerConfig) -> Option<String> {
    cfg.url().map(expand_env_vars)
}

#[derive(Default, Clone)]
struct Entry {
    state: AuthState,
    names: BTreeSet<String>,
    challenge: Option<String>,
    needed_scopes: Vec<String>,
    issuer: Option<String>,
    has_tokens: bool,
    scopes: Vec<String>,
    expires_at: Option<u64>,
    error: Option<String>,
}

struct Flow {
    server: String,
    key: String,
    session: AuthorizationSession,
    state: String,
    redirect_uri: Url,
    stop: CancellationToken,
    /// The loopback listener's task, which owns the bound port.
    listening: Option<tokio::task::JoinHandle<()>>,
    /// What a cancel returns the server to.
    before: AuthState,
}

impl Flow {
    /// End a flow nobody will complete, returning once its callback port is
    /// free for the next sign-in.
    async fn release(mut self) -> Self {
        self.stop.cancel();
        if let Some(task) = self.listening.take() {
            task.abort();
            let _ = task.await;
        }
        self
    }
}

pub(crate) struct Shared {
    store: Arc<dyn McpCredentialStore>,
    credential_dir: Option<PathBuf>,
    servers: Mutex<HashMap<String, Entry>>,
    flows: tokio::sync::Mutex<HashMap<String, Flow>>,
    write_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    acquire_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    listener: RwLock<Option<AuthListener>>,
    flow_ttl: Mutex<Duration>,
}

pub(crate) struct RecordHold {
    _process: tokio::sync::OwnedMutexGuard<()>,
    _store: Option<Box<dyn Send>>,
}

fn lock_for(
    map: &Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    key: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    map.lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.to_string())
        .or_default()
        .clone()
}

impl Shared {
    pub(crate) async fn load(&self, key: &str) -> std::io::Result<Option<McpCredentials>> {
        let record = self.store.load(key).await?;
        if let Some(record) = &record {
            self.remember(key, record);
        }
        Ok(record)
    }

    /// Read-modify-write one record, serialized per server.
    pub(crate) async fn update(
        &self,
        key: &str,
        change: impl FnOnce(&mut McpCredentials),
    ) -> std::io::Result<McpCredentials> {
        let lock = lock_for(&self.write_locks, key);
        let _held = lock.lock().await;
        let mut record = self.store.load(key).await?.unwrap_or_default();
        record.server_url = key.to_string();
        change(&mut record);
        if record.client.is_none()
            && record.tokens.is_none()
            && record.issuer.is_none()
            && record.authorization_server.is_none()
        {
            self.store.delete(key).await?;
        } else {
            self.store.save(key, &record).await?;
        }
        self.remember(key, &record);
        Ok(record)
    }

    /// Hold `key`'s record against every other refresh and sign-out of it:
    /// this process's, then, through the store, other processes'.
    pub(crate) async fn hold(&self, key: &str) -> std::io::Result<RecordHold> {
        let process = lock_for(&self.refresh_locks, key).lock_owned().await;
        let store = self.store.lock(key).await?;
        Ok(RecordHold {
            _process: process,
            _store: store,
        })
    }

    fn acquire_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock_for(&self.acquire_locks, key)
    }

    fn remember(&self, key: &str, record: &McpCredentials) {
        self.with_entry(key, |e| {
            e.issuer = record.issuer.clone();
            e.has_tokens = record.tokens.is_some();
            match &record.tokens {
                Some(t) => {
                    e.scopes = t.scopes.clone();
                    e.expires_at = t.expires_at;
                }
                None => {
                    e.scopes.clear();
                    e.expires_at = None;
                }
            }
        });
    }

    fn with_entry<T>(&self, key: &str, f: impl FnOnce(&mut Entry) -> T) -> T {
        let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        f(servers.entry(key.to_string()).or_default())
    }

    fn entry(&self, key: &str) -> Entry {
        self.with_entry(key, |e| e.clone())
    }

    fn emit(&self, event: AuthEvent) {
        let listener = self
            .listener
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(listener) = listener {
            listener(event);
        }
    }

    fn fail(&self, server: &str, key: &str, error: String) {
        let error = without_url_secrets(&error);
        self.with_entry(key, |e| {
            e.state = AuthState::Failed;
            e.error = Some(error.clone());
        });
        self.emit(AuthEvent::Failed {
            server: server.to_string(),
            url: key.to_string(),
            error,
        });
    }

    async fn expire(&self, flow_id: &str) {
        let Some(flow) = self.flows.lock().await.remove(flow_id) else {
            return;
        };
        let flow = flow.release().await;
        self.fail(
            &flow.server,
            &flow.key,
            "the sign-in was not completed in time".into(),
        );
    }

    /// End every sign-in in progress for `key`; the first one's `before`
    /// is what the server was in when they began.
    async fn release_flows_for(&self, key: &str) -> Option<AuthState> {
        let ended: Vec<Flow> = {
            let mut flows = self.flows.lock().await;
            let ids: Vec<String> = flows
                .iter()
                .filter(|(_, f)| f.key == key)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| flows.remove(id)).collect()
        };
        let mut before = None;
        for flow in ended {
            before.get_or_insert(flow.release().await.before);
        }
        before
    }
}

/// See the module docs.
pub struct McpAuth {
    shared: Arc<Shared>,
    client: McpOAuthClient,
    policy: RwLock<Arc<McpPolicyGate>>,
    enterprise_idp: RwLock<Option<EnterpriseIdp>>,
    identity: RwLock<Option<EnterpriseIdentity>>,
}

impl std::fmt::Debug for McpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpAuth")
            .field("credential_dir", &self.shared.credential_dir)
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

fn scopes_of(challenge: &str) -> Vec<String> {
    let base = Url::parse("http://challenge.invalid/").expect("a constant URL");
    rmcp::transport::auth::WWWAuthenticateParams::parse(challenge, &base)
        .scope
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

fn union(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    for s in b {
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

fn describe(e: &AuthError) -> String {
    match e {
        AuthError::PkceUnsupported => {
            "the authorization server does not support PKCE with S256, which MCP requires".into()
        }
        other => other.to_string(),
    }
}

/// A registration made for `registered` redirects also serves `wanted` when
/// they are the same URI, or both loopback IP redirects differing only in
/// port, which an authorization server must accept (RFC 8252 §7.3).
fn redirect_registered(registered: &[String], wanted: &Url) -> bool {
    registered.iter().any(|r| {
        let Ok(r) = Url::parse(r) else {
            return false;
        };
        r == *wanted
            || (r.scheme() == "http"
                && wanted.scheme() == "http"
                && r.host_str() == Some("127.0.0.1")
                && wanted.host_str() == Some("127.0.0.1")
                && r.path() == wanted.path())
    })
}

/// The endpoints the daemon itself calls must pass the server's address
/// policy too: names are filtered when resolved, but a literal IP is
/// connected to directly. The authorization endpoint is opened by the host,
/// not called, so it is held to the scheme alone.
fn check_endpoints(server_url: &str, metadata: &AuthorizationMetadata) -> Result<(), String> {
    let server = Url::parse(server_url).map_err(|e| e.to_string())?;
    let policy = http::AddressPolicy::for_server(&server);
    if let Some(issuer) = &metadata.issuer {
        let url = Url::parse(issuer)
            .map_err(|e| format!("the authorization server's issuer `{issuer}`: {e}"))?;
        http::require_https_off_loopback(&url)?;
    }
    if !metadata.authorization_endpoint.is_empty() {
        check_authorize_url(&metadata.authorization_endpoint)?;
    }
    let revocation = metadata
        .additional_fields
        .get("revocation_endpoint")
        .and_then(|v| v.as_str());
    for endpoint in [
        Some(metadata.token_endpoint.as_str()),
        metadata.registration_endpoint.as_deref(),
        revocation,
    ]
    .into_iter()
    .flatten()
    {
        let url = Url::parse(endpoint)
            .map_err(|e| format!("the authorization server names `{endpoint}`: {e}"))?;
        policy.check_url(&url)?;
    }
    Ok(())
}

/// What the host is asked to open in a browser: a web page, never a file or
/// another application's URL scheme.
fn check_authorize_url(raw: &str) -> Result<(), String> {
    let url = Url::parse(raw)
        .map_err(|e| format!("the authorization endpoint `{raw}` is not a URL: {e}"))?;
    http::require_https_off_loopback(&url)
        .map_err(|e| format!("the authorization endpoint is not handed to the host: {e}"))
}

fn require_pkce(metadata: &AuthorizationMetadata) -> Result<(), AuthFlowError> {
    match &metadata.code_challenge_methods_supported {
        Some(methods) if methods.iter().any(|m| m == "S256") => Ok(()),
        _ => Err(AuthFlowError::Failed(format!(
            "the authorization server {} does not advertise PKCE with S256 \
             (code_challenge_methods_supported), so it cannot be used",
            metadata.issuer.as_deref().unwrap_or("(no issuer)")
        ))),
    }
}

impl McpAuth {
    pub fn new(store: Arc<dyn McpCredentialStore>) -> Self {
        Self::build(store, None)
    }

    /// Records as files under `dir` ([`FileCredentialStore`]).
    pub fn with_file_store(dir: PathBuf) -> Self {
        Self::build(Arc::new(FileCredentialStore::new(dir.clone())), Some(dir))
    }

    fn build(store: Arc<dyn McpCredentialStore>, credential_dir: Option<PathBuf>) -> Self {
        Self {
            client: McpOAuthClient::default(),
            policy: RwLock::new(Arc::new(McpPolicyGate::open())),
            enterprise_idp: RwLock::new(None),
            identity: RwLock::new(None),
            shared: Arc::new(Shared {
                store,
                credential_dir,
                servers: Mutex::new(HashMap::new()),
                flows: tokio::sync::Mutex::new(HashMap::new()),
                write_locks: Mutex::new(HashMap::new()),
                refresh_locks: Mutex::new(HashMap::new()),
                acquire_locks: Mutex::new(HashMap::new()),
                listener: RwLock::new(None),
                flow_ttl: Mutex::new(FLOW_TTL),
            }),
        }
    }

    pub fn with_client(mut self, client: McpOAuthClient) -> Self {
        self.client = client;
        self
    }

    /// The directory records are kept in, when they are files.
    pub fn credential_dir(&self) -> Option<&Path> {
        self.shared.credential_dir.as_deref()
    }

    pub fn store(&self) -> Arc<dyn McpCredentialStore> {
        self.shared.store.clone()
    }

    pub fn set_listener(&self, listener: AuthListener) {
        *self
            .shared
            .listener
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(listener);
    }

    /// The policy whose `allowed_authorization_servers` every authorization
    /// server is checked against before use.
    pub fn set_policy(&self, policy: Arc<McpPolicyGate>) {
        *self.policy.write().unwrap_or_else(|e| e.into_inner()) = policy;
    }

    pub fn set_enterprise_idp(&self, idp: Option<EnterpriseIdp>) {
        *self
            .enterprise_idp
            .write()
            .unwrap_or_else(|e| e.into_inner()) = idp;
    }

    pub fn enterprise_idp(&self) -> Option<EnterpriseIdp> {
        self.enterprise_idp
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The user's enterprise identity, held in memory only, that
    /// `"grant": "enterprise"` servers are authorized from.
    pub fn set_identity(&self, identity: Option<EnterpriseIdentity>) {
        *self.identity.write().unwrap_or_else(|e| e.into_inner()) = identity;
    }

    fn identity(&self) -> Option<EnterpriseIdentity> {
        self.identity
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn admit_issuer(&self, issuer: Option<&str>) -> Result<(), String> {
        self.policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .admit_authorization_server(issuer)
    }

    async fn remember_metadata(&self, key: &str, metadata: &AuthorizationMetadata) {
        let issuer = metadata.issuer.clone();
        let json = serde_json::to_value(metadata).ok();
        if let Err(e) = self
            .shared
            .update(key, |r| {
                r.issuer = issuer;
                r.authorization_server = json;
            })
            .await
        {
            tracing::warn!(error = %e, "could not store MCP authorization server metadata");
        }
    }

    /// How long a started sign-in may wait for its callback.
    pub fn set_flow_ttl(&self, ttl: Duration) {
        *self
            .shared
            .flow_ttl
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = ttl;
    }

    /// `None` for a server whose token cannot come from OAuth.
    pub fn status(&self, cfg: &McpServerConfig) -> Option<AuthStatus> {
        if !cfg.oauth_capable() {
            return None;
        }
        let key = canonical_server_url(&server_url(cfg)?).ok()?;
        let entry = self.shared.entry(&key);
        let expires_at = entry.expires_at.and_then(|at| {
            time::OffsetDateTime::from_unix_timestamp(at as i64)
                .ok()?
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        });
        let scopes =
            if matches!(entry.state, AuthState::Required) && !entry.needed_scopes.is_empty() {
                entry.needed_scopes.clone()
            } else {
                entry.scopes.clone()
            };
        Some(AuthStatus {
            state: entry.state,
            issuer: entry.issuer,
            scopes,
            expires_at,
            error: entry.error.as_deref().map(without_url_secrets),
        })
    }

    async fn new_manager(&self, url: &str, key: &str) -> Result<AuthorizationManager, String> {
        new_manager(&self.shared, url, key).await
    }

    fn oauth_target(
        &self,
        server: &str,
        cfg: &McpServerConfig,
    ) -> Result<(String, String, McpOAuthConfig), AuthFlowError> {
        if !cfg.oauth_capable() {
            return Err(AuthFlowError::Invalid(format!(
                "MCP server `{server}` does not use OAuth: only streamable_http and sse servers \
                 without a fixed Authorization header do"
            )));
        }
        let url = server_url(cfg).unwrap_or_default();
        let key = canonical_server_url(&url).map_err(AuthFlowError::Invalid)?;
        Ok((url, key, cfg.oauth().cloned().unwrap_or_default()))
    }

    /// The server a flow in progress signs in to.
    pub async fn flow_server(&self, flow_id: &str) -> Option<String> {
        self.shared
            .flows
            .lock()
            .await
            .get(flow_id)
            .map(|f| f.server.clone())
    }

    /// Finish a sign-in with the URL the browser was redirected to. A
    /// callback whose `state` is not this flow's is refused and leaves the
    /// flow waiting for the real one.
    pub async fn complete(
        &self,
        flow_id: &str,
        callback_url: &str,
    ) -> Result<FlowDone, AuthFlowError> {
        let callback = Url::parse(callback_url)
            .map_err(|e| AuthFlowError::Invalid(format!("callback_url is not a URL: {e}")))?;
        let params: HashMap<String, String> = callback.query_pairs().into_owned().collect();
        let flow = {
            let mut flows = self.shared.flows.lock().await;
            let flow = flows.get(flow_id).ok_or_else(|| {
                AuthFlowError::Invalid(format!(
                    "no sign-in `{flow_id}` is in progress; it may have completed, been \
                     cancelled or expired"
                ))
            })?;
            if !callback::is_callback_for(&callback, &flow.redirect_uri) {
                return Err(AuthFlowError::Invalid(format!(
                    "callback_url is not a redirect to this sign-in's redirect_uri {}",
                    flow.redirect_uri
                )));
            }
            if params.get("state") != Some(&flow.state) {
                return Err(AuthFlowError::Failed(
                    "the callback's state does not match this sign-in; it was not the response \
                     to it"
                        .into(),
                ));
            }
            flows.remove(flow_id).expect("found above")
        };
        flow.stop.cancel();

        if let Some(error) = params.get("error") {
            let detail = params
                .get("error_description")
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            let message = format!("the authorization server refused: {error}{detail}");
            self.shared.fail(&flow.server, &flow.key, message.clone());
            return Err(AuthFlowError::Failed(message));
        }

        if let Err(e) = flow.session.handle_callback_url(callback_url).await {
            let message = format!("signing in to `{}` failed: {}", flow.server, describe(&e));
            self.shared.fail(&flow.server, &flow.key, message.clone());
            return Err(AuthFlowError::Failed(message));
        }
        let scopes = flow.session.auth_manager.get_current_scopes().await;
        self.shared.with_entry(&flow.key, |e| {
            e.state = AuthState::Authorized;
            e.error = None;
            e.challenge = None;
            e.needed_scopes.clear();
        });
        self.shared.emit(AuthEvent::Completed {
            server: flow.server.clone(),
            url: flow.key.clone(),
        });
        Ok(FlowDone {
            server: flow.server,
            url: flow.key,
            scopes,
        })
    }

    /// Abandon a sign-in; the server goes back to the state it was in.
    pub async fn cancel(&self, flow_id: &str) -> Result<String, AuthFlowError> {
        let flow = self
            .shared
            .flows
            .lock()
            .await
            .remove(flow_id)
            .ok_or_else(|| {
                AuthFlowError::Invalid(format!("no sign-in `{flow_id}` is in progress"))
            })?;
        let flow = flow.release().await;
        self.shared.with_entry(&flow.key, |e| {
            e.state = flow.before;
            e.error = None;
        });
        Ok(flow.server)
    }

    /// Sign out of `server`: revoke its tokens where the authorization
    /// server offers revocation (RFC 7009), then forget them. The client
    /// registration is kept for the next sign-in. A refresh under way
    /// finishes first, so what is revoked is what it got.
    pub async fn logout(&self, server: &str, cfg: &McpServerConfig) -> Result<(), AuthFlowError> {
        let (url, key, oauth) = self.oauth_target(server, cfg)?;
        let failed = |e: String| AuthFlowError::Failed(e);
        let _held = self
            .shared
            .hold(&key)
            .await
            .map_err(|e| failed(format!("cannot hold the stored sign-in: {e}")))?;
        let record = self
            .shared
            .load(&key)
            .await
            .map_err(|e| failed(format!("cannot read the stored sign-in: {e}")))?
            .unwrap_or_default();
        if let Some(tokens) = &record.tokens {
            self.revoke(&url, &record, tokens, &oauth).await;
        }
        self.shared
            .update(&key, |r| r.tokens = None)
            .await
            .map_err(|e| failed(format!("cannot forget the tokens: {e}")))?;
        self.shared.release_flows_for(&key).await;
        self.shared.with_entry(&key, |e| {
            e.names.insert(server.to_string());
            e.state = AuthState::Required;
            e.error = None;
            e.needed_scopes.clear();
        });
        Ok(())
    }

    async fn revoke(
        &self,
        url: &str,
        record: &McpCredentials,
        tokens: &McpTokens,
        oauth: &McpOAuthConfig,
    ) {
        let Some(endpoint) = record
            .authorization_server
            .as_ref()
            .and_then(|m| m.get("revocation_endpoint"))
            .and_then(|v| v.as_str())
            .and_then(|e| Url::parse(e).ok())
        else {
            return;
        };
        let Ok(http) = Url::parse(url)
            .map_err(|e| e.to_string())
            .and_then(|u| http::OAuthHttp::for_server(&u))
        else {
            return;
        };
        let secret = self.client_secret_for(
            &tokens.client_id,
            Some(oauth),
            record,
            record.issuer.as_deref(),
        );
        let mut pending = vec![(tokens.access_token.as_str(), "access_token")];
        if let Some(refresh) = &tokens.refresh_token {
            pending.insert(0, (refresh.as_str(), "refresh_token"));
        }
        for (token, hint) in pending {
            let mut form = vec![("token", token), ("token_type_hint", hint)];
            let mut request = http.request(reqwest::Method::POST, &endpoint);
            match &secret {
                Some(secret) => request = request.basic_auth(&tokens.client_id, Some(secret)),
                None => form.push(("client_id", tokens.client_id.as_str())),
            }
            let Ok(request) = request.form(&form).build() else {
                continue;
            };
            match http.send(request).await {
                Ok((status, _)) if status.is_success() => {}
                Ok((status, _)) => {
                    tracing::warn!(%status, %endpoint, hint, "token revocation was refused")
                }
                Err(error) => tracing::warn!(%error, hint, "token revocation failed"),
            }
        }
    }

    /// The rmcp manager a connection to `url` authorizes its requests with,
    /// and whether it carries stored tokens.
    pub(crate) async fn transport_manager(
        &self,
        server: &str,
        url: &str,
        oauth: Option<&McpOAuthConfig>,
    ) -> Result<(AuthorizationManager, bool), McpError> {
        let connect_failed = |message: String| McpError::ConnectFailed {
            name: server.to_string(),
            source: anyhow::anyhow!(message),
        };
        let key = canonical_server_url(url).map_err(connect_failed)?;
        self.shared.with_entry(&key, |e| {
            e.names.insert(server.to_string());
        });
        let mut manager = self.new_manager(url, &key).await.map_err(connect_failed)?;
        if let Some(oauth) = oauth.filter(|o| o.grant.is_unattended()) {
            self.renew_unattended(server, url, oauth, false).await?;
            manager.set_credential_store(store::ServerStore::new(&key, self.shared.clone(), true));
            // With a client configured, rmcp's attempt to refresh a token it
            // was refused ends in "authorization required" — no refresh
            // token — which is what makes the caller fetch a new one.
            if let Ok(Some(record)) = self.shared.load(&key).await {
                let metadata = record
                    .authorization_server
                    .and_then(|m| serde_json::from_value::<AuthorizationMetadata>(m).ok());
                if let (Some(metadata), Some(tokens)) = (metadata, record.tokens) {
                    manager.set_metadata(metadata);
                    let _ = manager.configure_client_id(&tokens.client_id);
                }
            }
            return Ok((manager, true));
        }
        let record = self.shared.load(&key).await.map_err(|e| {
            connect_failed(format!(
                "cannot read the stored sign-in for `{server}`: {e}"
            ))
        })?;
        let Some((record, tokens)) = record.and_then(|r| r.tokens.clone().map(|t| (r, t))) else {
            return Ok((manager, false));
        };

        let cached = || {
            record
                .authorization_server
                .clone()
                .and_then(|m| serde_json::from_value::<AuthorizationMetadata>(m).ok())
        };
        let metadata = match manager.resolve_metadata().await {
            Ok(found) if found.source.is_discovered() => found.metadata,
            Ok(_) => cached().ok_or_else(|| {
                connect_failed(format!("`{server}` no longer publishes OAuth metadata"))
            })?,
            Err(e) => cached().ok_or_else(|| {
                connect_failed(format!(
                    "the authorization server for `{server}` cannot be reached: {}",
                    describe(&e)
                ))
            })?,
        };
        check_endpoints(url, &metadata).map_err(connect_failed)?;
        self.admit_issuer(metadata.issuer.as_deref())
            .map_err(connect_failed)?;
        let issuer = metadata.issuer.clone();
        manager.set_metadata(metadata);
        if !manager
            .initialize_from_store()
            .await
            .map_err(|e| connect_failed(describe(&e)))?
        {
            return Ok((manager, false));
        }
        if let Some(secret) =
            self.client_secret_for(&tokens.client_id, oauth, &record, issuer.as_deref())
        {
            let redirect = record
                .client
                .as_ref()
                .and_then(|c| c.redirect_uris.first().cloned())
                .unwrap_or_else(|| format!("http://127.0.0.1{}", callback::CALLBACK_PATH));
            manager
                .configure_client(
                    OAuthClientConfig::new(tokens.client_id.clone(), redirect)
                        .with_client_secret(secret),
                )
                .map_err(|e| connect_failed(describe(&e)))?;
        }
        Ok((manager, true))
    }

    pub(crate) fn connected(&self, server: &str, url: &str, with_tokens: bool) {
        let Ok(key) = canonical_server_url(url) else {
            return;
        };
        self.shared.with_entry(&key, |e| {
            e.names.insert(server.to_string());
            if with_tokens {
                e.state = AuthState::Authorized;
                e.error = None;
            } else if e.state != AuthState::Authorizing {
                e.state = AuthState::None;
            }
        });
    }

    /// The server answered 401. Tokens it was sent are no good — rmcp has
    /// already tried a refresh before this surfaces — so they are dropped,
    /// keeping the client registration.
    pub(crate) async fn note_unauthorized(
        &self,
        server: &str,
        url: &str,
        challenge: Option<&str>,
        sent_tokens: bool,
    ) {
        let Ok(key) = canonical_server_url(url) else {
            return;
        };
        if sent_tokens {
            if let Err(e) = self.shared.update(&key, |r| r.tokens = None).await {
                tracing::warn!(server, error = %e, "could not forget rejected MCP OAuth tokens");
            }
        }
        let needed = challenge.map(scopes_of).unwrap_or_default();
        let announce = self.shared.with_entry(&key, |e| {
            e.names.insert(server.to_string());
            e.challenge = challenge.map(str::to_string);
            e.needed_scopes = needed.clone();
            let announce = matches!(e.state, AuthState::None | AuthState::Authorized);
            if announce {
                e.state = AuthState::Required;
            }
            announce
        });
        if announce {
            self.shared.emit(AuthEvent::Required {
                server: server.to_string(),
                url: key,
                scopes: needed,
            });
        }
    }

    /// The server answered 403 `insufficient_scope`: the token works, but not
    /// for everything; a new sign-in asks for the union.
    pub(crate) fn note_insufficient_scope(&self, server: &str, url: &str, challenge: &str) {
        let Ok(key) = canonical_server_url(url) else {
            return;
        };
        let wanted = scopes_of(challenge);
        let needed = self.shared.with_entry(&key, |e| {
            e.names.insert(server.to_string());
            e.challenge = Some(challenge.to_string());
            e.needed_scopes = union(&e.scopes, &wanted);
            if e.state != AuthState::Authorizing {
                e.state = AuthState::Required;
            }
            e.needed_scopes.clone()
        });
        self.shared.emit(AuthEvent::StepUp {
            server: server.to_string(),
            url: key,
            scopes: needed,
        });
    }
}

/// An rmcp manager for the server at `url`, its HTTP under the server's
/// address policy and its tokens in the record for `key`.
async fn new_manager(
    shared: &Arc<Shared>,
    url: &str,
    key: &str,
) -> Result<AuthorizationManager, String> {
    let parsed = Url::parse(url).map_err(|e| format!("`{url}`: {e}"))?;
    let http = http::OAuthHttp::for_server(&parsed)?;
    let mut manager = if http.policy().admits_private() {
        AuthorizationManager::new_allowing_private_addresses(url).await
    } else {
        AuthorizationManager::new(url).await
    }
    .map_err(|e| describe(&e))?;
    manager
        .with_client(http.client())
        .map_err(|e| describe(&e))?;
    manager.set_credential_store(store::ServerStore::new(key, shared.clone(), false));
    Ok(manager)
}

impl McpAuth {
    /// The configured client secret, for the authorization server `issuer`
    /// only when it is the one the secret belongs to: the one `oauth.issuer`
    /// names, or, without that, one the policy's
    /// `allowed_authorization_servers` lists. A server's metadata picks its
    /// authorization server, so nothing less keeps it from collecting the
    /// secret.
    fn configured_secret(
        &self,
        oauth: &McpOAuthConfig,
        issuer: Option<&str>,
    ) -> Result<Option<String>, AuthFlowError> {
        let Some(secret) = oauth.client_secret().map_err(AuthFlowError::Invalid)? else {
            return Ok(None);
        };
        let shown = issuer.unwrap_or("an authorization server that names no issuer");
        match (&oauth.issuer, issuer) {
            (Some(own), Some(issuer)) if crate::policy::same_issuer(own, issuer) => {
                Ok(Some(secret))
            }
            (Some(own), _) => Err(AuthFlowError::Denied(format!(
                "oauth.client_secret_env belongs to {own} (oauth.issuer); it is not sent to {shown}"
            ))),
            (None, Some(issuer))
                if self
                    .policy
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .restricts_authorization_servers()
                    && self.admit_issuer(Some(issuer)).is_ok() =>
            {
                Ok(Some(secret))
            }
            (None, _) => Err(AuthFlowError::Invalid(format!(
                "oauth.client_secret_env is set, but nothing says which authorization server it \
                 belongs to: name it with oauth.issuer (this server's is {shown}), or list it in \
                 the MCP policy's allowed_authorization_servers"
            ))),
        }
    }

    /// The secret `client_id` authenticates with at `issuer`: the configured
    /// one ([`Self::configured_secret`]) or the one dynamic registration at
    /// that same issuer returned.
    fn client_secret_for(
        &self,
        client_id: &str,
        oauth: Option<&McpOAuthConfig>,
        record: &McpCredentials,
        issuer: Option<&str>,
    ) -> Option<String> {
        if let Some(oauth) = oauth.filter(|o| o.client_id.as_deref() == Some(client_id)) {
            return self
                .configured_secret(oauth, issuer)
                .inspect_err(|e| tracing::warn!(error = %e, "MCP client secret withheld"))
                .ok()
                .flatten();
        }
        record
            .client
            .as_ref()
            .filter(|c| c.client_id == client_id && c.issuer.as_deref() == issuer)
            .and_then(|c| c.client_secret.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_urls_are_canonicalized() {
        assert_eq!(
            canonical_server_url("HTTPS://MCP.Example.com:443/").unwrap(),
            "https://mcp.example.com"
        );
        assert_eq!(
            canonical_server_url("https://u:p@mcp.example.com/mcp#x").unwrap(),
            "https://mcp.example.com/mcp"
        );
        assert_eq!(
            canonical_server_url("http://127.0.0.1:8080/mcp/").unwrap(),
            "http://127.0.0.1:8080/mcp/"
        );
        assert!(canonical_server_url("ws://h/mcp").is_err());
        assert!(canonical_server_url("nonsense").is_err());
    }

    #[test]
    fn a_challenge_names_the_scopes_it_wants() {
        assert_eq!(
            scopes_of(r#"Bearer error="insufficient_scope", scope="files:read files:write""#),
            vec!["files:read".to_string(), "files:write".to_string()]
        );
        assert!(scopes_of("Bearer realm=\"x\"").is_empty());
    }

    #[test]
    fn a_loopback_registration_serves_any_port() {
        let wanted = Url::parse("http://127.0.0.1:5555/callback").unwrap();
        assert!(redirect_registered(
            &["http://127.0.0.1:4444/callback".into()],
            &wanted
        ));
        assert!(!redirect_registered(
            &["http://127.0.0.1:4444/other".into()],
            &wanted
        ));
        let external = Url::parse("https://app.example/cb").unwrap();
        assert!(redirect_registered(
            &["https://app.example/cb".into()],
            &external
        ));
        assert!(!redirect_registered(
            &["http://127.0.0.1:4444/callback".into()],
            &external
        ));
    }

    #[test]
    fn an_endpoint_at_a_refused_address_is_refused() {
        let mut metadata = AuthorizationMetadata::default();
        metadata.token_endpoint = "https://as.example.com/token".into();
        assert!(check_endpoints("https://mcp.example.com/mcp", &metadata).is_ok());
        metadata.token_endpoint = "http://169.254.169.254/token".into();
        assert!(check_endpoints("https://mcp.example.com/mcp", &metadata).is_err());
        metadata.token_endpoint = "https://as.example.com/token".into();
        metadata.registration_endpoint = Some("https://10.0.0.1/register".into());
        assert!(check_endpoints("https://mcp.example.com/mcp", &metadata).is_err());
        assert!(check_endpoints("https://10.0.0.2/mcp", &metadata).is_ok());
    }

    #[test]
    fn an_authorization_server_off_loopback_is_https_only() {
        let mut metadata = AuthorizationMetadata::default();
        metadata.issuer = Some("https://as.example.com".into());
        metadata.authorization_endpoint = "https://as.example.com/authorize".into();
        metadata.token_endpoint = "https://as.example.com/token".into();
        assert!(check_endpoints("https://mcp.example.com/mcp", &metadata).is_ok());

        let mut plain = metadata.clone();
        plain.token_endpoint = "http://as.example.com/token".into();
        assert!(check_endpoints("https://mcp.example.com/mcp", &plain).is_err());
        let mut plain = metadata.clone();
        plain.issuer = Some("http://as.example.com".into());
        assert!(check_endpoints("https://mcp.example.com/mcp", &plain).is_err());
        for authorize in [
            "http://as.example.com/authorize",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "com.example.app:/authorize",
        ] {
            let mut odd = metadata.clone();
            odd.authorization_endpoint = authorize.into();
            assert!(
                check_endpoints("https://mcp.example.com/mcp", &odd).is_err(),
                "{authorize}"
            );
        }

        let mut local = AuthorizationMetadata::default();
        local.issuer = Some("http://127.0.0.1:9001".into());
        local.authorization_endpoint = "http://127.0.0.1:9001/authorize".into();
        local.token_endpoint = "http://localhost:9001/token".into();
        assert!(check_endpoints("http://127.0.0.1:9000/mcp", &local).is_ok());
    }

    #[test]
    fn pkce_without_s256_is_refused() {
        let mut metadata = AuthorizationMetadata::default();
        assert!(require_pkce(&metadata).is_err(), "absent means unsupported");
        metadata.code_challenge_methods_supported = Some(vec!["plain".into()]);
        assert!(require_pkce(&metadata).is_err());
        metadata.code_challenge_methods_supported = Some(vec!["plain".into(), "S256".into()]);
        assert!(require_pkce(&metadata).is_ok());
    }

    #[tokio::test]
    async fn a_401_announces_once_and_forgets_only_the_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let auth = McpAuth::with_file_store(dir.path().to_path_buf());
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        auth.set_listener(Arc::new(move |e| seen.lock().unwrap().push(e)));
        let url = "https://mcp.example.com/mcp";
        auth.shared
            .update(url, |r| {
                r.client = Some(McpClientRegistration {
                    client_id: "c".into(),
                    client_secret: None,
                    redirect_uris: vec![],
                    issuer: None,
                });
                r.tokens = Some(McpTokens {
                    access_token: "a".into(),
                    refresh_token: None,
                    expires_at: None,
                    scopes: vec![],
                    client_id: "c".into(),
                });
            })
            .await
            .unwrap();

        let challenge = r#"Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", scope="read""#;
        auth.note_unauthorized("docs", url, Some(challenge), true)
            .await;
        auth.note_unauthorized("docs", url, Some(challenge), false)
            .await;

        let record = auth.store().load(url).await.unwrap().unwrap();
        assert!(record.tokens.is_none());
        assert_eq!(record.client.unwrap().client_id, "c");
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &[AuthEvent::Required {
                server: "docs".into(),
                url: url.into(),
                scopes: vec!["read".into()],
            }]
        );
        let cfg: McpServerConfig =
            serde_json::from_value(serde_json::json!({"type": "streamable_http", "url": url}))
                .unwrap();
        let status = auth.status(&cfg).unwrap();
        assert_eq!(status.state, AuthState::Required);
        assert_eq!(status.scopes, vec!["read".to_string()]);
    }

    fn remote(url: &str, oauth: serde_json::Value) -> McpServerConfig {
        serde_json::from_value(serde_json::json!({
            "type": "streamable_http", "url": url, "oauth": oauth,
        }))
        .unwrap()
    }

    fn external() -> Redirect {
        Redirect::External("https://app.example/cb".into())
    }

    /// A sign-in to a mock server with its tokens stored under `dir`.
    async fn signed_in(
        dir: &Path,
    ) -> (
        mcp_oauth_mock::McpServer,
        mcp_oauth_mock::AuthServer,
        McpServerConfig,
        String,
    ) {
        let (mcp, server_auth) = mcp_oauth_mock::McpServer::start().await;
        let cfg = remote(&mcp.url, serde_json::json!({}));
        let auth = McpAuth::with_file_store(dir.to_path_buf());
        let start = auth.start("docs", &cfg, external(), None).await.unwrap();
        let location = mcp_oauth_mock::consent(&start.authorize_url).await;
        auth.complete(&start.flow_id, &location).await.unwrap();
        let key = canonical_server_url(&mcp.url).unwrap();
        (mcp, server_auth, cfg, key)
    }

    #[tokio::test]
    async fn two_processes_refreshing_one_rotating_token_both_end_up_with_a_valid_one() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, server_auth, _, key) = signed_in(dir.path()).await;
        let first = McpAuth::with_file_store(dir.path().to_path_buf());
        let second = McpAuth::with_file_store(dir.path().to_path_buf());
        first
            .shared
            .update(&key, |r| {
                if let Some(t) = &mut r.tokens {
                    t.expires_at = Some(store::now_secs());
                }
            })
            .await
            .unwrap();
        let (a, with_a) = first
            .transport_manager("docs", &mcp.url, None)
            .await
            .unwrap();
        let (b, with_b) = second
            .transport_manager("docs", &mcp.url, None)
            .await
            .unwrap();
        assert!(with_a && with_b);

        let (got_a, got_b) = tokio::join!(a.get_access_token(), b.get_access_token());
        assert!(got_a.is_ok() && got_b.is_ok(), "{got_a:?} / {got_b:?}");
        let stored = first.store().load(&key).await.unwrap().unwrap();
        let tokens = stored.tokens.expect("still signed in");
        assert!(
            server_auth
                .state
                .lock()
                .unwrap()
                .tokens
                .contains_key(&tokens.access_token),
            "the stored token is one the authorization server still honours"
        );
    }

    #[tokio::test]
    async fn a_logout_while_a_refresh_is_under_way_is_not_undone_by_it() {
        let dir = tempfile::tempdir().unwrap();
        let (_mcp, _server_auth, cfg, key) = signed_in(dir.path()).await;
        let auth = Arc::new(McpAuth::with_file_store(dir.path().to_path_buf()));
        let refreshing = store::ServerStore::new(&key, auth.shared.clone(), false);
        use rmcp::transport::auth::CredentialStore;
        let guard = refreshing.acquire_refresh_guard().await.unwrap();
        let read = refreshing.load().await.unwrap().expect("tokens to refresh");

        let signing_out = auth.clone();
        let logout = tokio::spawn(async move { signing_out.logout("docs", &cfg).await });
        // Room for a logout that does not wait for the refresh to finish first.
        tokio::time::sleep(Duration::from_millis(200)).await;
        refreshing.save(read).await.unwrap();
        drop(guard);
        logout.await.unwrap().unwrap();

        let record = auth.store().load(&key).await.unwrap().unwrap();
        assert!(record.tokens.is_none(), "the refresh wrote the tokens back");
    }

    #[test]
    fn a_stored_or_configured_secret_is_chosen_by_client_and_issuer() {
        std::env::set_var("AGIX_UNIT_CHOSEN_SECRET", "configured");
        let auth = McpAuth::new(Arc::new(FileCredentialStore::new(
            std::env::temp_dir().join("agix-unused"),
        )));
        let record = McpCredentials {
            client: Some(McpClientRegistration {
                client_id: "dyn".into(),
                client_secret: Some("registered".into()),
                redirect_uris: vec![],
                issuer: Some("https://as.example".into()),
            }),
            ..Default::default()
        };
        let a = Some("https://as.example");
        let b = Some("https://as.other.example");
        assert_eq!(
            auth.client_secret_for("dyn", None, &record, a).as_deref(),
            Some("registered")
        );
        assert_eq!(auth.client_secret_for("dyn", None, &record, b), None);

        let oauth: McpOAuthConfig = serde_json::from_value(serde_json::json!({
            "client_id": "pre", "client_secret_env": "AGIX_UNIT_CHOSEN_SECRET",
            "issuer": "https://as.example/",
        }))
        .unwrap();
        assert_eq!(
            auth.client_secret_for("pre", Some(&oauth), &record, a)
                .as_deref(),
            Some("configured")
        );
        assert_eq!(
            auth.client_secret_for("pre", Some(&oauth), &record, b),
            None
        );
        let unpinned = McpOAuthConfig {
            issuer: None,
            ..oauth.clone()
        };
        assert_eq!(
            auth.client_secret_for("pre", Some(&unpinned), &record, a),
            None
        );
        auth.set_policy(Arc::new(McpPolicyGate::from_host(
            serde_json::from_value(serde_json::json!({
                "allowed_authorization_servers": ["https://as.example"],
            }))
            .unwrap(),
        )));
        assert_eq!(
            auth.client_secret_for("pre", Some(&unpinned), &record, a)
                .as_deref(),
            Some("configured")
        );
        std::env::remove_var("AGIX_UNIT_CHOSEN_SECRET");
    }

    #[tokio::test]
    async fn a_configured_client_secret_goes_only_to_the_authorization_server_it_is_for() {
        std::env::set_var("AGIX_UNIT_PREREGISTERED_SECRET", "pre-secret");
        let (mcp, server_auth) = mcp_oauth_mock::McpServer::start().await;
        server_auth.preregister("pre-1", Some("pre-secret"));
        let oauth = |issuer: Option<&str>| {
            let mut o = serde_json::json!({
                "client_id": "pre-1", "client_secret_env": "AGIX_UNIT_PREREGISTERED_SECRET",
            });
            if let Some(issuer) = issuer {
                o["issuer"] = issuer.into();
            }
            remote(&mcp.url, o)
        };
        let dir = tempfile::tempdir().unwrap();
        let auth = McpAuth::with_file_store(dir.path().to_path_buf());

        let unpinned = auth.start("docs", &oauth(None), external(), None).await;
        let Err(AuthFlowError::Invalid(why)) = unpinned else {
            panic!("a secret with no issuer to go to was used");
        };
        assert!(why.contains("oauth.issuer"), "{why}");

        let elsewhere = auth
            .start(
                "docs",
                &oauth(Some("https://as.elsewhere.example")),
                external(),
                None,
            )
            .await;
        assert!(
            matches!(elsewhere, Err(AuthFlowError::Denied(_))),
            "a secret was handed to another authorization server"
        );

        auth.start("docs", &oauth(Some(&server_auth.url)), external(), None)
            .await
            .unwrap();

        let listed = McpAuth::with_file_store(dir.path().join("listed"));
        listed.set_policy(Arc::new(McpPolicyGate::from_host(
            serde_json::from_value(serde_json::json!({
                "allowed_authorization_servers": [server_auth.url],
            }))
            .unwrap(),
        )));
        listed
            .start("docs", &oauth(None), external(), None)
            .await
            .unwrap();
        std::env::remove_var("AGIX_UNIT_PREREGISTERED_SECRET");
    }
}
