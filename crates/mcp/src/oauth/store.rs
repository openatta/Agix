//! Where an MCP server's authorization is kept between runs: one
//! [`McpCredentials`] record per canonical server URL, behind the
//! [`McpCredentialStore`] trait an embedder can implement (a keychain, a
//! vault). [`FileCredentialStore`] is the default, one 0600 file per server.
//!
//! [`ServerStore`] is the bridge rmcp's `AuthorizationManager` reads and
//! writes tokens through, so a refresh it performs lands in the same record.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, CredentialRefreshGuard, CredentialStore, OAuthTokenResponse, StoredCredentials,
};
use serde::{Deserialize, Serialize};

/// Everything one MCP server's authorization keeps.
///
/// Serializable so a store can hold it as one opaque secret
/// (`serde_json::to_string`). Its `Debug` never shows a secret.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpCredentials {
    pub server_url: String,
    /// The client this daemon is registered as at the authorization server —
    /// what dynamic registration returned. Survives a logout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<McpClientRegistration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<McpTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// The authorization server metadata the tokens were issued under, used
    /// when it cannot be discovered again and to find its revocation endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_server: Option<serde_json::Value>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct McpClientRegistration {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// The client the tokens were issued to.
    pub client_id: String,
}

impl std::fmt::Debug for McpCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCredentials")
            .field("server_url", &self.server_url)
            .field("client", &self.client)
            .field("tokens", &self.tokens)
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for McpClientRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClientRegistration")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[redacted]"),
            )
            .field("redirect_uris", &self.redirect_uris)
            .field("issuer", &self.issuer)
            .finish()
    }
}

impl std::fmt::Debug for McpTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTokens")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .field("client_id", &self.client_id)
            .finish()
    }
}

/// Persistence for [`McpCredentials`], keyed by the canonical server URL
/// (`mcp::oauth::canonical_server_url`), so a server renamed in settings keeps
/// its sign-in.
///
/// `load` of a URL never saved is `Ok(None)`; `delete` of one is `Ok(())`.
/// Report a backend that cannot be reached as an error rather than `None`:
/// `None` means "signed out" and asks the user to sign in again.
#[async_trait]
pub trait McpCredentialStore: Send + Sync {
    async fn load(&self, server_url: &str) -> std::io::Result<Option<McpCredentials>>;
    async fn save(&self, server_url: &str, credentials: &McpCredentials) -> std::io::Result<()>;
    async fn delete(&self, server_url: &str) -> std::io::Result<()>;
}

/// One JSON file per server under `dir`, named by a hash of the URL, readable
/// by the owner only and replaced atomically.
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    dir: PathBuf,
}

impl FileCredentialStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, server_url: &str) -> PathBuf {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(server_url.as_bytes());
        self.dir.join(format!("{}.json", hex::encode(digest)))
    }
}

#[async_trait]
impl McpCredentialStore for FileCredentialStore {
    async fn load(&self, server_url: &str) -> std::io::Result<Option<McpCredentials>> {
        let path = self.path_for(server_url);
        match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn save(&self, server_url: &str, credentials: &McpCredentials) -> std::io::Result<()> {
        let dir = self.dir.clone();
        let path = self.path_for(server_url);
        let bytes = serde_json::to_vec_pretty(credentials)?;
        tokio::task::spawn_blocking(move || write_private(&dir, &path, &bytes))
            .await
            .map_err(std::io::Error::other)?
    }

    async fn delete(&self, server_url: &str) -> std::io::Result<()> {
        match tokio::fs::remove_file(self.path_for(server_url)).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn write_private(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;

    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// rmcp's view of one server's record.
pub(crate) struct ServerStore {
    pub(crate) key: String,
    pub(crate) shared: Arc<super::Shared>,
    /// The daemon fetches this server's tokens again before they expire
    /// (an unattended grant), so rmcp is not told when they do.
    pub(crate) renewed_by_us: bool,
}

fn store_error(e: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(e.to_string())
}

pub(crate) fn tokens_to_rmcp(tokens: &McpTokens, issuer: Option<String>) -> StoredCredentials {
    let now = now_secs();
    let mut response = serde_json::json!({
        "access_token": tokens.access_token,
        "token_type": "bearer",
    });
    if let Some(refresh) = &tokens.refresh_token {
        response["refresh_token"] = refresh.clone().into();
    }
    if let Some(at) = tokens.expires_at {
        response["expires_in"] = at.saturating_sub(now).into();
    }
    if !tokens.scopes.is_empty() {
        response["scope"] = tokens.scopes.join(" ").into();
    }
    let response: Option<OAuthTokenResponse> = serde_json::from_value(response).ok();
    StoredCredentials::new(
        tokens.client_id.clone(),
        response,
        tokens.scopes.clone(),
        tokens.expires_at.map(|_| now),
    )
    .with_issuer(issuer)
}

pub(crate) fn tokens_from_rmcp(credentials: &StoredCredentials) -> Option<McpTokens> {
    let response = serde_json::to_value(credentials.token_response.as_ref()?).ok()?;
    let text = |key: &str| {
        response
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let expires_at = response
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .map(|secs| credentials.token_received_at.unwrap_or_else(now_secs) + secs);
    let scopes = if credentials.granted_scopes.is_empty() {
        text("scope")
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    } else {
        credentials.granted_scopes.clone()
    };
    Some(McpTokens {
        access_token: text("access_token")?,
        refresh_token: text("refresh_token"),
        expires_at,
        scopes,
        client_id: credentials.client_id.clone(),
    })
}

#[async_trait]
impl CredentialStore for ServerStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let record = self.shared.load(&self.key).await.map_err(store_error)?;
        Ok(record.and_then(|r| {
            let issuer = r.issuer.clone();
            r.tokens.map(|mut t| {
                // rmcp gives up on a token with less than its margin (30 s)
                // left and, with no refresh token, asks for a sign-in; one
                // that lives shorter than that would never be used.
                if self.renewed_by_us {
                    t.expires_at = None;
                }
                tokens_to_rmcp(&t, issuer)
            })
        }))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let tokens = tokens_from_rmcp(&credentials);
        self.shared
            .update(&self.key, |record| {
                record.tokens = tokens;
                if credentials.issuer.is_some() {
                    record.issuer = credentials.issuer.clone();
                }
            })
            .await
            .map(drop)
            .map_err(store_error)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.shared
            .update(&self.key, |record| {
                record.tokens = None;
                record.client = None;
                record.issuer = None;
                record.authorization_server = None;
            })
            .await
            .map(drop)
            .map_err(store_error)
    }

    async fn on_refresh_token_rejected(
        &self,
        rejected: &StoredCredentials,
    ) -> Result<(), AuthError> {
        let rejected = tokens_from_rmcp(rejected).map(|t| t.access_token);
        self.shared
            .update(&self.key, |record| {
                if record.tokens.as_ref().map(|t| &t.access_token) == rejected.as_ref() {
                    record.tokens = None;
                }
            })
            .await
            .map(drop)
            .map_err(store_error)
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        let lock = self.shared.refresh_lock(&self.key);
        Ok(Some(CredentialRefreshGuard::new(lock.lock_owned().await)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> McpCredentials {
        McpCredentials {
            server_url: "https://mcp.example/mcp".into(),
            client: Some(McpClientRegistration {
                client_id: "dyn-1".into(),
                client_secret: Some("shh".into()),
                redirect_uris: vec!["http://127.0.0.1:1/callback".into()],
                issuer: Some("https://as.example".into()),
            }),
            tokens: Some(McpTokens {
                access_token: "at-secret".into(),
                refresh_token: Some("rt-secret".into()),
                expires_at: Some(now_secs() + 600),
                scopes: vec!["read".into()],
                client_id: "dyn-1".into(),
            }),
            issuer: Some("https://as.example".into()),
            authorization_server: None,
        }
    }

    #[tokio::test]
    async fn a_saved_record_loads_back_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(dir.path().join("mcp/oauth"));
        let record = sample();
        store.save(&record.server_url, &record).await.unwrap();
        assert_eq!(
            store.load(&record.server_url).await.unwrap(),
            Some(record.clone())
        );
        assert!(store.load("https://other/mcp").await.unwrap().is_none());

        let path = store.path_for(&record.server_url);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = std::fs::metadata(store.dir()).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
        let names: Vec<_> = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            names.len(),
            1,
            "no temporary file is left behind: {names:?}"
        );
        assert!(
            !names[0].contains("mcp.example"),
            "the URL is hashed: {names:?}"
        );

        store.delete(&record.server_url).await.unwrap();
        store.delete(&record.server_url).await.unwrap();
        assert!(store.load(&record.server_url).await.unwrap().is_none());
    }

    #[test]
    fn debug_output_never_shows_a_secret() {
        let shown = format!("{:?}", sample());
        for secret in ["at-secret", "rt-secret", "shh"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[test]
    fn tokens_survive_the_trip_through_rmcp() {
        let tokens = sample().tokens.unwrap();
        let back = tokens_from_rmcp(&tokens_to_rmcp(&tokens, None)).unwrap();
        assert_eq!(back.access_token, tokens.access_token);
        assert_eq!(back.refresh_token, tokens.refresh_token);
        assert_eq!(back.scopes, tokens.scopes);
        assert_eq!(back.client_id, tokens.client_id);
        let drift = back
            .expires_at
            .unwrap()
            .abs_diff(tokens.expires_at.unwrap());
        assert!(drift <= 1, "expiry moved by {drift}s");
    }
}
