//! OAuth bearer token resolution for MCP StreamableHttp connections.
//!
//! Agent itself does NOT depend on the `auth` crate (which depends on agent).
//! Instead, CLI injects an `McpOAuthResolver` implementation backed by the
//! `auth` crate. Without injection, OAuth is skipped (no bearer token).
//!
//! # OAuth Store
//!
//! Tokens are persisted to `~/.agix/code/mcp/oauth/<provider>.json` so they
//! survive restarts. The store is checked before calling the resolver; the
//! resolver is only invoked when no cached token is available or the token
//! has been explicitly cleared (e.g. after a 401 response).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use auth::{CallbackListener, OAuth2Client, PkceMethod, PkceVerifier, ProviderConfig};

// ── OAuth Resolver ──

/// Resolves OAuth bearer tokens for MCP servers.
///
/// CLI implements this via the `auth` crate (`TokenStore`, `OAuth2Client`).
/// Agent is agnostic — it just calls `resolve()` when an MCP server config
/// declares an `oauth_provider`.
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

/// Shared resolver for MCP connections — set by CLI at startup.
static RESOLVER: std::sync::OnceLock<Arc<dyn McpOAuthResolver>> = std::sync::OnceLock::new();

/// Install the process-wide OAuth resolver. Called once by CLI at startup.
pub fn set_oauth_resolver(r: Arc<dyn McpOAuthResolver>) {
    let _ = RESOLVER.set(r);
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
/// On 401 responses, callers should call [`clear_oauth_token`] to force
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

/// Process-wide OAuth token store.
static OAUTH_STORE: std::sync::OnceLock<McpOAuthStore> = std::sync::OnceLock::new();

/// Initialise the process-wide OAuth token store.
/// Safe to call multiple times; only the first call takes effect.
pub fn init_oauth_store_with(dir: PathBuf) {
    let _ = OAUTH_STORE.set(McpOAuthStore::with_dir(dir));
}

/// Clear the cached OAuth token for a given provider, both in the in-memory
/// cache and on disk. Should be called when a 401 is received so that the
/// next connection attempt re-runs the OAuth flow.
pub async fn clear_oauth_token(provider_name: &str) {
    if let Some(store) = OAUTH_STORE.get() {
        store.clear_token(provider_name).await;
    }
}

/// Retrieve the process-wide OAuth store, if initialised.
fn get_oauth_store() -> Option<&'static McpOAuthStore> {
    OAUTH_STORE.get()
}

// ── Resolver helpers ──

/// Resolve a bearer token using the installed resolver.
/// Returns `None` if no resolver is installed (OAuth disabled).
///
/// Checks the OAuth token store first. If a cached token exists, returns it
/// without calling the resolver. Otherwise calls the resolver and stores the
/// result for future use.
///
/// NOTE: Returns `Result<String, String>` mirroring the trait. The connect
/// layer wraps this into `anyhow::Error`. When the trait is updated, this
/// function should return a dedicated error type.
pub(crate) async fn resolve_oauth_bearer(provider_name: &str) -> Result<String, String> {
    // Check the OAuth store first for a cached token.
    if let Some(store) = get_oauth_store() {
        if let Some(token) = store.get_token(provider_name).await {
            return Ok(token);
        }
    }

    // No cached token; call the resolver.
    let token = match RESOLVER.get() {
        Some(r) => r.resolve_bearer(provider_name).await?,
        None => return Err("no OAuth resolver installed".into()),
    };

    // Store the resolved token for future use.
    if let Some(store) = get_oauth_store() {
        store.set_token(provider_name, &token).await;
    }

    Ok(token)
}

// ── OAuth flow initiator (used by McpAuthTool) ──

/// Registered MCP server configurations for tool queries.
static MCP_SERVER_CONFIGS: std::sync::OnceLock<HashMap<String, crate::config::McpServerConfig>> =
    std::sync::OnceLock::new();

/// Registered OAuth provider configurations from settings `oauth_providers`.
static OAUTH_PROVIDER_CONFIGS: std::sync::OnceLock<HashMap<String, ProviderConfig>> =
    std::sync::OnceLock::new();

/// Pending OAuth flow awaiting browser callback + code exchange.
struct PendingFlow {
    verifier: String,
    redirect_uri: String,
    provider: String,
    listener: CallbackListener,
}

static PENDING_FLOW: std::sync::OnceLock<tokio::sync::Mutex<Option<PendingFlow>>> =
    std::sync::OnceLock::new();

fn pending_flow_lock() -> &'static tokio::sync::Mutex<Option<PendingFlow>> {
    PENDING_FLOW.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// Register the full set of MCP server configurations so tools can query
/// them (e.g., to discover which servers support OAuth).
/// Called once at startup by the CLI/daemon after loading settings.
pub fn register_mcp_server_configs(configs: HashMap<String, crate::config::McpServerConfig>) {
    let _ = MCP_SERVER_CONFIGS.set(configs);
}

/// Register OAuth provider configurations (from settings `oauth_providers`).
/// Called once at startup by the CLI/daemon after loading settings.
pub fn register_oauth_providers(providers: HashMap<String, ProviderConfig>) {
    let _ = OAUTH_PROVIDER_CONFIGS.set(providers);
}

/// Return a reference to the registered MCP server configs, if any.
pub fn get_mcp_server_configs() -> Option<&'static HashMap<String, crate::config::McpServerConfig>>
{
    MCP_SERVER_CONFIGS.get()
}

/// Return a reference to the registered OAuth provider configs, if any.
pub fn get_oauth_providers() -> Option<&'static HashMap<String, ProviderConfig>> {
    OAUTH_PROVIDER_CONFIGS.get()
}

/// Get the OAuth provider name for a given server, if any.
pub fn get_server_oauth_provider(server_name: &str) -> Option<&'static str> {
    MCP_SERVER_CONFIGS.get()?.get(server_name)?.oauth_provider()
}

/// Find the first configured server that has OAuth enabled and no cached token.
/// Returns the server name, or `None` if all OAuth-eligible servers are already
/// authenticated.
pub async fn find_first_unauthenticated_server() -> Option<String> {
    let configs = MCP_SERVER_CONFIGS.get()?;
    let store = get_oauth_store()?;
    for (name, cfg) in configs {
        if let Some(provider) = cfg.oauth_provider() {
            if store.get_token(provider).await.is_none() {
                return Some(name.clone());
            }
        }
    }
    None
}

/// Start an OAuth flow for the specified MCP server.
///
/// 1. Looks up the server's `oauth_provider` in the registered configs.
/// 2. Creates a PKCE challenge (S256) and starts a local HTTP callback listener
///    on `127.0.0.1:<ephemeral>`.
/// 3. Builds the authorization URL.
/// 4. Stores the pending flow state for later completion via
///    [`complete_oauth_flow`].
///
/// Returns the authorization URL the user should open in their browser.
pub async fn start_oauth_flow(server_name: &str) -> Result<String, String> {
    let configs = MCP_SERVER_CONFIGS.get().ok_or_else(|| {
        "No MCP server configurations loaded. Configure MCP servers in settings.json.".to_string()
    })?;
    let server_cfg = configs.get(server_name).ok_or_else(|| {
        format!("Unknown MCP server: '{server_name}'. Check your MCP server configuration.")
    })?;
    let provider_name = server_cfg.oauth_provider().ok_or_else(|| {
        format!("MCP server '{server_name}' does not support OAuth authentication")
    })?;
    let providers = OAUTH_PROVIDER_CONFIGS
        .get()
        .ok_or_else(|| "No OAuth provider configurations loaded".to_string())?;
    let provider_cfg = providers.get(provider_name).ok_or_else(|| {
        format!(
            "OAuth provider '{provider_name}' not found. \
             Add it to settings.json under oauth_providers."
        )
    })?;

    // Create PKCE challenge (S256)
    let pkce = PkceVerifier::new(PkceMethod::S256);
    let verifier = pkce.verifier.clone();

    // Start a local callback listener on 127.0.0.1:<ephemeral>
    let listener = CallbackListener::start()
        .await
        .map_err(|e| format!("Failed to start OAuth callback listener: {e}"))?;
    let redirect_uri = listener.redirect_uri().to_string();

    // Build the authorization URL
    let client = OAuth2Client::new(provider_cfg);
    let (url, _state) = client
        .build_authorize_url(&redirect_uri, &pkce)
        .map_err(|e| format!("Failed to build authorization URL: {e}"))?;

    // Store the pending flow state for later completion
    *pending_flow_lock().lock().await = Some(PendingFlow {
        verifier,
        redirect_uri,
        provider: provider_name.to_string(),
        listener,
    });

    Ok(url)
}

/// Complete a previously started OAuth flow by waiting for the browser callback
/// and exchanging the authorization code for a token.
///
/// If `code` is provided, uses it directly instead of waiting for the callback.
/// The `timeout_secs` controls how long to wait for the browser callback
/// (ignored if `code` is provided). Minimum timeout is 30 seconds.
pub async fn complete_oauth_flow(code: Option<&str>, timeout_secs: u64) -> Result<String, String> {
    let mut guard = pending_flow_lock().lock().await;
    let pending = guard
        .take()
        .ok_or_else(|| "No pending OAuth flow. Call start_oauth_flow first.".to_string())?;

    // Get the authorization code
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

    // Look up the provider config
    let providers = OAUTH_PROVIDER_CONFIGS
        .get()
        .ok_or_else(|| "OAuth provider configurations no longer available".to_string())?;
    let provider_cfg = providers
        .get(&pending.provider)
        .ok_or_else(|| format!("OAuth provider '{}' not found", pending.provider))?;

    // Exchange the code for a token
    let client = OAuth2Client::new(provider_cfg);
    let token_response = client
        .exchange_code(&auth_code, &pending.redirect_uri, &pending.verifier)
        .await
        .map_err(|e| format!("Token exchange failed: {e}"))?;

    // Store the token for future use
    if let Some(store) = get_oauth_store() {
        store
            .set_token(&pending.provider, &token_response.access_token)
            .await;
    }

    Ok(format!(
        "OAuth authentication successful for '{}'. Token stored.",
        pending.provider
    ))
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
}
