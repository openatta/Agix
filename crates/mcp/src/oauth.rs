//! OAuth bearer tokens for MCP StreamableHttp / SSE connections.
//!
//! Everything OAuth needs — the resolver, the token store, the server and
//! provider configurations, a flow waiting for its browser callback — lives on
//! one [`McpOAuth`], and a session pool owns one. Several pools in one process
//! therefore keep their tokens apart; nothing here is process-wide.
//!
//! This crate does not resolve tokens itself: the host hands in an
//! [`McpOAuthResolver`] (typically backed by the `auth` crate). Without one,
//! only tokens already in the store are used.
//!
//! The store persists tokens under the directory the owner names — by
//! convention `<scene root>/mcp/oauth/<provider>.json` — and is checked before
//! the resolver. On a 401, [`McpOAuth::clear_token`] forces the next connection
//! to resolve again.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use auth::{CallbackListener, OAuth2Client, PkceMethod, PkceVerifier, ProviderConfig};

// ── OAuth Resolver ──

/// Resolves OAuth bearer tokens for MCP servers.
///
/// The host implements this, typically with the `auth` crate (`TokenStore`,
/// `OAuth2Client`). It is asked when an MCP server config declares an
/// `oauth_provider` and the store has no token for it.
///
/// NOTE: returns `Result<String, String>` — the `String` error type is a legacy
/// pattern. The `auth` crate backing this trait uses its own error types, so
/// the string sugar is a bridge. Changing the trait signature is a breaking
/// API change across MCP consumers.
#[async_trait::async_trait]
pub trait McpOAuthResolver: Send + Sync {
    /// Resolve a fresh bearer token for the named provider.
    /// Returns `Ok(token)` or `Err(message)` on failure.
    async fn resolve_bearer(&self, provider_name: &str) -> Result<String, String>;
}

// ── OAuth Store (disk-persisted token cache) ──

/// Serialized form of a stored token on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
}

/// Disk-persisted OAuth token cache.
///
/// Tokens are stored in `~/.agix/code/mcp/oauth/<provider>.json` as JSON
/// with an `access_token` field. The store is checked before initiating an
/// OAuth flow, and tokens are written back after successful resolution.
///
/// On 401 responses, callers should call [`McpOAuth::clear_token`] to force
/// re-resolution on the next connection attempt.
#[derive(Debug)]
pub struct McpOAuthStore {
    dir: PathBuf,
    cache: tokio::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl McpOAuthStore {
    /// Create an OAuth token store rooted at `dir` — conventionally
    /// `<scene root>/mcp/oauth/`, but the caller says where.
    ///
    /// These are credentials. A constructor that derived the directory from
    /// `$HOME` would decide, on its own, whose machine account owns the
    /// tokens — so there isn't one.
    pub fn with_dir(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Path to the token file for a given provider.
    fn token_path(&self, provider: &str) -> PathBuf {
        self.dir.join(format!("{provider}.json"))
    }

    /// Retrieve a cached token, checking memory then disk.
    pub async fn get_token(&self, provider: &str) -> Option<String> {
        // Check in-memory cache first.
        {
            let cache = self.cache.lock().await;
            if let Some(token) = cache.get(provider) {
                return Some(token.clone());
            }
        }

        // Fall back to disk.
        let path = self.token_path(provider);
        let content = std::fs::read_to_string(path).ok()?;
        let stored: StoredToken = serde_json::from_str(&content).ok()?;
        let token = stored.access_token;

        // Populate in-memory cache for future lookups.
        self.cache
            .lock()
            .await
            .insert(provider.to_string(), token.clone());
        Some(token)
    }

    /// Store a token for the given provider, persisting to disk.
    pub async fn set_token(&self, provider: &str, token: &str) {
        // Update in-memory cache.
        self.cache
            .lock()
            .await
            .insert(provider.to_string(), token.to_string());

        // Persist to disk.
        let path = self.token_path(provider);
        let stored = StoredToken {
            access_token: token.to_string(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&stored) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// Remove a cached token for the given provider, both from memory and disk.
    pub async fn clear_token(&self, provider: &str) {
        self.cache.lock().await.remove(provider);
        let path = self.token_path(provider);
        let _ = std::fs::remove_file(&path);
    }
}

/// Pending OAuth flow awaiting browser callback + code exchange.
struct PendingFlow {
    verifier: String,
    redirect_uri: String,
    provider: String,
    listener: CallbackListener,
}

/// One session pool's OAuth state for its MCP connections.
pub struct McpOAuth {
    resolver: Option<Arc<dyn McpOAuthResolver>>,
    store: McpOAuthStore,
    servers: std::sync::RwLock<HashMap<String, crate::config::McpServerConfig>>,
    providers: HashMap<String, ProviderConfig>,
    pending: tokio::sync::Mutex<Option<PendingFlow>>,
}

impl std::fmt::Debug for McpOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuth")
            .field("resolver", &self.resolver.is_some())
            .field("store", &self.store)
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl McpOAuth {
    /// OAuth state whose tokens persist under `store_dir`.
    pub fn new(store_dir: PathBuf) -> Self {
        Self {
            resolver: None,
            store: McpOAuthStore::with_dir(store_dir),
            servers: std::sync::RwLock::new(HashMap::new()),
            providers: HashMap::new(),
            pending: tokio::sync::Mutex::new(None),
        }
    }

    /// Where tokens persist.
    pub fn store_dir(&self) -> &std::path::Path {
        self.store.dir()
    }

    pub fn with_resolver(mut self, resolver: Arc<dyn McpOAuthResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// OAuth provider configurations (settings `oauth_providers`), needed to
    /// start a flow.
    pub fn with_providers(mut self, providers: HashMap<String, ProviderConfig>) -> Self {
        self.providers = providers;
        self
    }

    /// Note server configurations this pool connects to, so a flow can be
    /// started by server name. Merges into what is already known.
    pub fn remember_servers(&self, servers: &HashMap<String, crate::config::McpServerConfig>) {
        let mut known = self.servers.write().unwrap_or_else(|e| e.into_inner());
        for (name, cfg) in servers {
            known.insert(name.clone(), cfg.clone());
        }
    }

    /// The OAuth provider a known server is configured with, if any.
    pub fn server_oauth_provider(&self, server_name: &str) -> Option<String> {
        self.servers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_name)?
            .oauth_provider()
            .map(str::to_string)
    }

    /// Remove a cached token for `provider`, in memory and on disk. Called on a
    /// 401 so the next connection attempt resolves again.
    pub async fn clear_token(&self, provider: &str) {
        self.store.clear_token(provider).await;
    }

    /// A bearer token for `provider`: the stored one if there is one, else one
    /// from the resolver, which is then stored.
    ///
    /// NOTE: Returns `Result<String, String>` mirroring the trait. The connect
    /// layer wraps this into `anyhow::Error`.
    pub async fn resolve_bearer(&self, provider: &str) -> Result<String, String> {
        if let Some(token) = self.store.get_token(provider).await {
            return Ok(token);
        }
        let token = match &self.resolver {
            Some(r) => r.resolve_bearer(provider).await?,
            None => return Err("no OAuth resolver installed".into()),
        };
        self.store.set_token(provider, &token).await;
        Ok(token)
    }

    /// The first known server that has OAuth enabled and no cached token.
    pub async fn find_first_unauthenticated_server(&self) -> Option<String> {
        let candidates: Vec<(String, String)> = self
            .servers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(name, cfg)| Some((name.clone(), cfg.oauth_provider()?.to_string())))
            .collect();
        for (name, provider) in candidates {
            if self.store.get_token(&provider).await.is_none() {
                return Some(name);
            }
        }
        None
    }

    /// Start an OAuth flow for the named server.
    ///
    /// 1. Looks up the server's `oauth_provider` in the known configs.
    /// 2. Creates a PKCE challenge (S256) and starts a local HTTP callback listener
    ///    on `127.0.0.1:<ephemeral>`.
    /// 3. Builds the authorization URL.
    /// 4. Keeps the pending flow for [`Self::complete_flow`].
    ///
    /// Returns the authorization URL the user should open in their browser.
    pub async fn start_flow(&self, server_name: &str) -> Result<String, String> {
        let provider_name = {
            let servers = self.servers.read().unwrap_or_else(|e| e.into_inner());
            if servers.is_empty() {
                return Err("No MCP server configurations loaded. \
                            Configure MCP servers in settings.json."
                    .to_string());
            }
            let server_cfg = servers.get(server_name).ok_or_else(|| {
                format!("Unknown MCP server: '{server_name}'. Check your MCP server configuration.")
            })?;
            server_cfg
                .oauth_provider()
                .ok_or_else(|| {
                    format!("MCP server '{server_name}' does not support OAuth authentication")
                })?
                .to_string()
        };
        let provider_cfg = self.providers.get(&provider_name).ok_or_else(|| {
            format!(
                "OAuth provider '{provider_name}' not found. \
                 Add it to settings.json under oauth_providers."
            )
        })?;

        let pkce = PkceVerifier::new(PkceMethod::S256);
        let verifier = pkce.verifier.clone();

        let listener = CallbackListener::start()
            .await
            .map_err(|e| format!("Failed to start OAuth callback listener: {e}"))?;
        let redirect_uri = listener.redirect_uri().to_string();

        let client = OAuth2Client::new(provider_cfg);
        let (url, _state) = client
            .build_authorize_url(&redirect_uri, &pkce)
            .map_err(|e| format!("Failed to build authorization URL: {e}"))?;

        *self.pending.lock().await = Some(PendingFlow {
            verifier,
            redirect_uri,
            provider: provider_name,
            listener,
        });

        Ok(url)
    }

    /// Complete the flow [`Self::start_flow`] began: wait for the browser
    /// callback (or take `code` directly) and exchange it for a token.
    ///
    /// `timeout_secs` bounds the wait for the callback (ignored with a `code`);
    /// it is at least 30 seconds.
    pub async fn complete_flow(
        &self,
        code: Option<&str>,
        timeout_secs: u64,
    ) -> Result<String, String> {
        let pending = self
            .pending
            .lock()
            .await
            .take()
            .ok_or_else(|| "No pending OAuth flow. Call start_flow first.".to_string())?;

        let auth_code = match code {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => {
                let timeout = std::time::Duration::from_secs(timeout_secs.max(30));
                pending
                    .listener
                    .await_callback(timeout)
                    .await
                    .map_err(|e| format!("OAuth callback failed: {e}"))?
                    .code
            }
        };

        let provider_cfg = self
            .providers
            .get(&pending.provider)
            .ok_or_else(|| format!("OAuth provider '{}' not found", pending.provider))?;

        let client = OAuth2Client::new(provider_cfg);
        let token_response = client
            .exchange_code(&auth_code, &pending.redirect_uri, &pending.verifier)
            .await
            .map_err(|e| format!("Token exchange failed: {e}"))?;

        self.store
            .set_token(&pending.provider, &token_response.access_token)
            .await;

        Ok(format!(
            "OAuth authentication successful for '{}'. Token stored.",
            pending.provider
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_persists_and_retrieves_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = McpOAuthStore::with_dir(dir.path().to_path_buf());
        store.set_token("github", "gh_token_123").await;
        let token = store.get_token("github").await;
        assert_eq!(token.as_deref(), Some("gh_token_123"));
    }

    #[tokio::test]
    async fn store_returns_none_for_unknown_provider() {
        let dir = tempfile::tempdir().unwrap();
        let store = McpOAuthStore::with_dir(dir.path().to_path_buf());
        let token = store.get_token("unknown").await;
        assert!(token.is_none());
    }

    #[tokio::test]
    async fn store_persists_to_disk_for_cross_session_survival() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let store = McpOAuthStore::with_dir(path.clone());
            store.set_token("github", "disk_token").await;
        }
        // Second instance should read from disk.
        let store = McpOAuthStore::with_dir(path);
        let token = store.get_token("github").await;
        assert_eq!(token.as_deref(), Some("disk_token"));
    }

    #[tokio::test]
    async fn store_clear_removes_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = McpOAuthStore::with_dir(dir.path().to_path_buf());
        store.set_token("github", "tok").await;
        store.clear_token("github").await;
        let token = store.get_token("github").await;
        assert!(token.is_none());
    }

    /// The store owns creating its directory — callers hand it a path that
    /// may not exist yet.
    #[test]
    fn the_store_creates_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("scenes/code/mcp/oauth");
        let _ = McpOAuthStore::with_dir(dir.clone());
        assert!(dir.is_dir());
    }

    struct FixedResolver(&'static str);

    #[async_trait::async_trait]
    impl McpOAuthResolver for FixedResolver {
        async fn resolve_bearer(&self, _provider: &str) -> Result<String, String> {
            Ok(self.0.to_string())
        }
    }

    #[tokio::test]
    async fn without_a_resolver_only_stored_tokens_are_used() {
        let dir = tempfile::tempdir().unwrap();
        let oauth = McpOAuth::new(dir.path().to_path_buf());
        assert!(oauth.resolve_bearer("github").await.is_err());
        McpOAuthStore::with_dir(dir.path().to_path_buf())
            .set_token("github", "stored")
            .await;
        assert_eq!(
            oauth.resolve_bearer("github").await.as_deref(),
            Ok("stored")
        );
    }

    #[tokio::test]
    async fn two_owners_keep_their_tokens_apart() {
        let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a =
            McpOAuth::new(a_dir.path().to_path_buf()).with_resolver(Arc::new(FixedResolver("a")));
        let b =
            McpOAuth::new(b_dir.path().to_path_buf()).with_resolver(Arc::new(FixedResolver("b")));
        assert_eq!(a.resolve_bearer("github").await.as_deref(), Ok("a"));
        assert_eq!(b.resolve_bearer("github").await.as_deref(), Ok("b"));
        a.clear_token("github").await;
        assert_eq!(b.resolve_bearer("github").await.as_deref(), Ok("b"));
    }
}
