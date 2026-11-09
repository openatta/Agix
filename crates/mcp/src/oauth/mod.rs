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
//!
//! Records live in an [`McpCredentialStore`] keyed by the canonical server
//! URL. Design and usage: `docs/mcp_auth.md`.

mod callback;
pub mod http;
pub mod store;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, AuthorizationMetadataSource,
    AuthorizationRequest, AuthorizationSession, OAuthClientConfig,
};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::config::{expand_env_vars, McpOAuthConfig, McpServerConfig};
use crate::error::McpError;

pub use callback::LoopbackCallback;
pub use store::{
    FileCredentialStore, McpClientRegistration, McpCredentialStore, McpCredentials, McpTokens,
};

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
    #[error("{0}")]
    Invalid(String),
    /// The authorization server, or the exchange with it, failed.
    #[error("{0}")]
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
    /// What a cancel returns the server to.
    before: AuthState,
}

pub(crate) struct Shared {
    store: Arc<dyn McpCredentialStore>,
    credential_dir: Option<PathBuf>,
    servers: Mutex<HashMap<String, Entry>>,
    flows: tokio::sync::Mutex<HashMap<String, Flow>>,
    write_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    listener: RwLock<Option<AuthListener>>,
    flow_ttl: Mutex<Duration>,
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

    pub(crate) fn refresh_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock_for(&self.refresh_locks, key)
    }

    fn remember(&self, key: &str, record: &McpCredentials) {
        self.with_entry(key, |e| {
            e.issuer = record.issuer.clone();
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
        flow.stop.cancel();
        self.fail(
            &flow.server,
            &flow.key,
            "the sign-in was not completed in time".into(),
        );
    }
}

/// See the module docs.
pub struct McpAuth {
    shared: Arc<Shared>,
    client: McpOAuthClient,
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
/// connected to directly.
fn check_endpoints(server_url: &str, metadata: &AuthorizationMetadata) -> Result<(), String> {
    let server = Url::parse(server_url).map_err(|e| e.to_string())?;
    let policy = http::AddressPolicy::for_server(&server);
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
            shared: Arc::new(Shared {
                store,
                credential_dir,
                servers: Mutex::new(HashMap::new()),
                flows: tokio::sync::Mutex::new(HashMap::new()),
                write_locks: Mutex::new(HashMap::new()),
                refresh_locks: Mutex::new(HashMap::new()),
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
            error: entry.error,
        })
    }

    async fn new_manager(&self, url: &str, key: &str) -> Result<AuthorizationManager, String> {
        let parsed = Url::parse(url).map_err(|e| format!("`{url}`: {e}"))?;
        let http = http::OAuthHttp::for_server(&parsed)?;
        let private = http.policy().admits_private();
        let mut manager = if private {
            AuthorizationManager::new_allowing_private_addresses(url).await
        } else {
            AuthorizationManager::new(url).await
        }
        .map_err(|e| describe(&e))?;
        manager
            .with_client(http.client())
            .map_err(|e| describe(&e))?;
        manager.set_credential_store(store::ServerStore {
            key: key.to_string(),
            shared: self.shared.clone(),
        });
        Ok(manager)
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

    /// Begin a browser sign-in for `server`. A sign-in already in progress
    /// for the same server is cancelled.
    pub async fn start(
        &self,
        server: &str,
        cfg: &McpServerConfig,
        redirect: Redirect,
        scopes: Option<Vec<String>>,
    ) -> Result<FlowStart, AuthFlowError> {
        let (url, key, oauth) = self.oauth_target(server, cfg)?;
        let failed = |e: String| AuthFlowError::Failed(e);

        let (listener, redirect_uri) = match redirect {
            Redirect::Loopback => {
                let (listener, uri) = callback::bind_loopback(oauth.callback_port)
                    .await
                    .map_err(failed)?;
                (Some(listener), uri)
            }
            Redirect::External(uri) => {
                callback::check_redirect_uri(&uri).map_err(AuthFlowError::Invalid)?;
                (None, uri)
            }
        };
        let redirect_url = Url::parse(&redirect_uri).map_err(|e| failed(e.to_string()))?;

        let record = self
            .shared
            .load(&key)
            .await
            .map_err(|e| failed(format!("cannot read the stored sign-in: {e}")))?
            .unwrap_or_default();
        let entry = self.shared.entry(&key);

        let mut manager = self.new_manager(&url, &key).await.map_err(failed)?;
        let resolution = manager
            .resolve_metadata_from_challenge(entry.challenge.as_deref())
            .await
            .map_err(|e| {
                failed(format!(
                    "OAuth discovery for `{server}` failed: {}",
                    describe(&e)
                ))
            })?;
        if resolution.source == AuthorizationMetadataSource::LegacyEndpointFallback {
            return Err(failed(format!(
                "`{server}` publishes no OAuth metadata (RFC 9728 / RFC 8414), so there is \
                 nothing to sign in to"
            )));
        }
        let metadata = resolution.metadata;
        check_endpoints(&url, &metadata).map_err(failed)?;
        require_pkce(&metadata)?;
        manager.set_metadata(metadata.clone());

        let scopes = match scopes.filter(|s| !s.is_empty()) {
            Some(explicit) => explicit,
            None if !entry.needed_scopes.is_empty() => {
                let granted = record.tokens.as_ref().map(|t| t.scopes.clone());
                union(&granted.unwrap_or_default(), &entry.needed_scopes)
            }
            None => oauth.scopes.clone(),
        };

        let mut request = AuthorizationRequest::new(redirect_uri.clone())
            .with_client_name(self.client.client_name.clone());
        request = if scopes.is_empty() {
            request.with_initial_scope_selection()
        } else {
            request.with_scopes(scopes.clone())
        };

        let issuer = metadata.issuer.clone();
        let stored = record
            .client
            .clone()
            .filter(|r| r.issuer == issuer && redirect_registered(&r.redirect_uris, &redirect_url));
        let cimd = self.client.client_metadata_url.clone().filter(|_| {
            metadata
                .additional_fields
                .get("client_id_metadata_document_supported")
                .and_then(|v| v.as_bool())
                == Some(true)
        });
        let identity = match (&oauth.client_id, stored) {
            (Some(id), _) => Some((
                id.clone(),
                oauth.client_secret().map_err(AuthFlowError::Invalid)?,
            )),
            (None, Some(reg)) => Some((reg.client_id, reg.client_secret)),
            (None, None) => None,
        };
        request = match (identity, cimd) {
            (Some((id, secret)), _) => {
                let r = request.with_preregistered_client(id);
                match secret {
                    Some(secret) => r.with_client_secret(secret),
                    None => r,
                }
            }
            (None, Some(cimd)) => request.with_client_metadata_url(cimd),
            (None, None) if metadata.registration_endpoint.is_some() => {
                let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
                let registered = manager
                    .register_client(&self.client.client_name, &redirect_uri, &scope_refs)
                    .await
                    .map_err(|e| {
                        failed(format!(
                            "dynamic client registration with {} failed: {}",
                            issuer.as_deref().unwrap_or("the authorization server"),
                            describe(&e)
                        ))
                    })?;
                let registration = McpClientRegistration {
                    client_id: registered.client_id.clone(),
                    client_secret: registered.client_secret.clone(),
                    redirect_uris: vec![redirect_uri.clone()],
                    issuer: issuer.clone(),
                };
                self.shared
                    .update(&key, |r| r.client = Some(registration))
                    .await
                    .map_err(|e| failed(format!("cannot store the client registration: {e}")))?;
                let r = request.with_preregistered_client(registered.client_id);
                match registered.client_secret {
                    Some(secret) => r.with_client_secret(secret),
                    None => r,
                }
            }
            (None, None) => {
                return Err(failed(format!(
                    "the authorization server {} offers no dynamic client registration; set \
                     `oauth.client_id` on `{server}` to a client registered there",
                    issuer.as_deref().unwrap_or("for this server")
                )))
            }
        };

        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, e)| failed(describe(&e)))?;
        let authorize_url = session.get_authorization_url().to_string();
        let state = Url::parse(&authorize_url)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
            })
            .ok_or_else(|| failed("the authorization URL carries no state".into()))?;
        let metadata_json = serde_json::to_value(&metadata).ok();
        self.shared
            .update(&key, |r| {
                r.issuer = issuer.clone();
                r.authorization_server = metadata_json;
            })
            .await
            .map_err(|e| failed(format!("cannot store the sign-in: {e}")))?;

        let flow_id = uuid::Uuid::new_v4().simple().to_string();
        let stop = CancellationToken::new();
        let callbacks = listener.map(|l| {
            callback::serve_loopback(l, redirect_uri.clone(), server.to_string(), stop.clone())
        });
        let ttl = *self
            .shared
            .flow_ttl
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = match entry.state {
            AuthState::Authorizing | AuthState::Failed => AuthState::Required,
            other => other,
        };
        {
            let mut flows = self.shared.flows.lock().await;
            flows.retain(|_, f| {
                let same = f.key == key;
                if same {
                    f.stop.cancel();
                }
                !same
            });
            flows.insert(
                flow_id.clone(),
                Flow {
                    server: server.to_string(),
                    key: key.clone(),
                    session,
                    state,
                    redirect_uri: redirect_url,
                    stop,
                    before,
                },
            );
        }
        self.shared.with_entry(&key, |e| {
            e.names.insert(server.to_string());
            e.state = AuthState::Authorizing;
            e.error = None;
        });
        let weak: Weak<Shared> = Arc::downgrade(&self.shared);
        let expiring = flow_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            if let Some(shared) = weak.upgrade() {
                shared.expire(&expiring).await;
            }
        });

        Ok(FlowStart {
            flow_id,
            authorize_url,
            redirect_uri,
            expires_in: ttl.as_secs(),
            callbacks,
        })
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
            if !params.contains_key("error") && params.get("state") != Some(&flow.state) {
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
        flow.stop.cancel();
        self.shared.with_entry(&flow.key, |e| {
            e.state = flow.before;
            e.error = None;
        });
        Ok(flow.server)
    }

    /// Sign out of `server`: revoke its tokens where the authorization
    /// server offers revocation (RFC 7009), then forget them. The client
    /// registration is kept for the next sign-in.
    pub async fn logout(&self, server: &str, cfg: &McpServerConfig) -> Result<(), AuthFlowError> {
        let (url, key, oauth) = self.oauth_target(server, cfg)?;
        let failed = |e: String| AuthFlowError::Failed(e);
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
        {
            let mut flows = self.shared.flows.lock().await;
            flows.retain(|_, f| {
                let same = f.key == key;
                if same {
                    f.stop.cancel();
                }
                !same
            });
        }
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
        let secret = client_secret_for(&tokens.client_id, Some(oauth), record);
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
        manager.set_metadata(metadata);
        if !manager
            .initialize_from_store()
            .await
            .map_err(|e| connect_failed(describe(&e)))?
        {
            return Ok((manager, false));
        }
        if let Some(secret) = client_secret_for(&tokens.client_id, oauth, &record) {
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

fn client_secret_for(
    client_id: &str,
    oauth: Option<&McpOAuthConfig>,
    record: &McpCredentials,
) -> Option<String> {
    if let Some(oauth) = oauth.filter(|o| o.client_id.as_deref() == Some(client_id)) {
        return oauth.client_secret().ok().flatten();
    }
    record
        .client
        .as_ref()
        .filter(|c| c.client_id == client_id)
        .and_then(|c| c.client_secret.clone())
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
        metadata.registration_endpoint = Some("http://10.0.0.1/register".into());
        assert!(check_endpoints("https://mcp.example.com/mcp", &metadata).is_err());
        assert!(check_endpoints("http://10.0.0.2/mcp", &metadata).is_ok());
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
}
