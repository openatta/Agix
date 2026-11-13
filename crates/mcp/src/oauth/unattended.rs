//! Tokens no person signs in for: `"grant": "client_credentials"` (the
//! daemon authenticates as itself, with a secret or a signed JWT assertion)
//! and `"grant": "enterprise"` (Enterprise-Managed Authorization: the user's
//! enterprise identity, supplied by the host, is exchanged at the enterprise
//! IdP for an identity assertion grant that the server's authorization
//! server redeems). Either way the token is fetched again shortly before it
//! expires, and once more when the server refuses it.

use oauth2::{ClientSecret, RefreshToken};
use rmcp::transport::auth::enterprise::{
    EmaAuthorizationServer, EmaClientAuthentication, EmaError, EmaExchangeRequest, EmaExchangeStage,
};
use rmcp::transport::auth::{AuthorizationMetadata, ClientCredentialsConfig};
use rmcp::transport::JwtSigningAlgorithm;

use super::store::now_secs;
use super::{describe, McpAuth, McpTokens};
use crate::config::{ClientAuthMethod, McpOAuthConfig, OAuthGrant};
use crate::error::McpError;

/// Fetch again this long before the token expires.
pub(crate) const RENEW_BEFORE_SECS: u64 = 60;

/// The identity an enterprise-managed server's tokens are exchanged from.
#[derive(Clone)]
pub enum EnterpriseIdentity {
    /// An OIDC ID token the enterprise IdP issued to this client. The host
    /// keeps it fresh: an expired one is refused and has to be replaced.
    IdToken(String),
    /// A refresh token from the enterprise IdP.
    RefreshToken(String),
}

impl std::fmt::Debug for EnterpriseIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::IdToken(_) => "IdToken([redacted])",
            Self::RefreshToken(_) => "RefreshToken([redacted])",
        })
    }
}

pub(crate) enum Unattended {
    /// No person can fix this by signing in; the host has to act (supply an
    /// enterprise identity) or the configuration is wrong.
    NeedsIdentity(String),
    Failed(String),
}

/// Run the future `make` builds on a thread and runtime of its own.
///
/// rmcp's client-credentials exchange keeps a non-`Send` form serializer
/// alive across an `.await`, so its future cannot be awaited inside one
/// that has to be `Send`, as every MCP call's is.
async fn isolated<T, F>(make: impl FnOnce() -> F + Send + 'static) -> Result<T, String>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("mcp-oauth".into())
        .spawn(move || {
            let outcome = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|runtime| runtime.block_on(make()));
            let _ = tx.send(outcome);
        })
        .map_err(|e| format!("cannot start the token request: {e}"))?;
    rx.await
        .map_err(|_| "the token request was abandoned".to_string())?
        .map_err(|e| format!("cannot run the token request: {e}"))
}

fn signing_alg(alg: &str) -> Result<JwtSigningAlgorithm, String> {
    Ok(match alg {
        "RS256" => JwtSigningAlgorithm::RS256,
        "RS384" => JwtSigningAlgorithm::RS384,
        "RS512" => JwtSigningAlgorithm::RS512,
        "ES256" => JwtSigningAlgorithm::ES256,
        "ES384" => JwtSigningAlgorithm::ES384,
        other => return Err(format!("unsupported signing algorithm `{other}`")),
    })
}

fn supports_only_post(metadata: &AuthorizationMetadata) -> bool {
    metadata
        .additional_fields
        .get("token_endpoint_auth_methods_supported")
        .and_then(|v| v.as_array())
        .is_some_and(|methods| {
            let has = |m: &str| methods.iter().any(|v| v.as_str() == Some(m));
            has("client_secret_post") && !has("client_secret_basic")
        })
}

fn ema_error(e: EmaError) -> Unattended {
    match e {
        EmaError::InvalidGrant(EmaExchangeStage::IdentityProvider)
        | EmaError::InsufficientUserAuthentication(EmaExchangeStage::IdentityProvider) => {
            Unattended::NeedsIdentity(format!(
                "the enterprise IdP refused the identity ({e}); the host has to supply a fresh \
                 one with mcp.auth.setIdentity"
            ))
        }
        other => Unattended::Failed(format!("enterprise token exchange failed: {other}")),
    }
}

impl McpAuth {
    /// Discover the server's authorization server and check it may be used.
    async fn unattended_metadata(
        &self,
        url: &str,
        key: &str,
    ) -> Result<(rmcp::transport::AuthorizationManager, AuthorizationMetadata), Unattended> {
        let failed = Unattended::Failed;
        let mut manager = self.new_manager(url, key).await.map_err(failed)?;
        let found = manager
            .resolve_metadata()
            .await
            .map_err(|e| failed(format!("OAuth discovery failed: {}", describe(&e))))?;
        if !found.source.is_discovered() {
            return Err(failed(
                "the server publishes no OAuth metadata (RFC 9728 / RFC 8414)".into(),
            ));
        }
        let metadata = found.metadata;
        super::check_endpoints(url, &metadata).map_err(failed)?;
        self.admit_issuer(metadata.issuer.as_deref())
            .map_err(failed)?;
        manager.set_metadata(metadata.clone());
        Ok((manager, metadata))
    }

    async fn fetch_client_credentials(
        &self,
        url: &str,
        key: &str,
        oauth: &McpOAuthConfig,
    ) -> Result<(), Unattended> {
        let failed = Unattended::Failed;
        let (_, metadata) = self.unattended_metadata(url, key).await?;
        let client_id = oauth.client_id.clone().unwrap_or_default();
        let config = match &oauth.private_key_jwt {
            Some(jwt) => ClientCredentialsConfig::PrivateKeyJwt {
                client_id,
                signing_key: jwt.pem().map_err(failed)?,
                signing_algorithm: signing_alg(&jwt.alg).map_err(failed)?,
                token_endpoint_audience: None,
                scopes: oauth.scopes.clone(),
                resource: Some(key.to_string()),
            },
            None => ClientCredentialsConfig::ClientSecret {
                client_id,
                client_secret: self
                    .configured_secret(oauth, metadata.issuer.as_deref())
                    .map_err(|e| failed(e.to_string()))?
                    .ok_or_else(|| failed("oauth.client_secret_env is not set".into()))?,
                scopes: oauth.scopes.clone(),
                resource: Some(key.to_string()),
            },
        };
        let (shared, url_owned, key_owned, discovered) = (
            self.shared.clone(),
            url.to_string(),
            key.to_string(),
            metadata.clone(),
        );
        let exchanged = isolated(move || async move {
            let rejected = |e| format!("client credentials were refused: {}", describe(&e));
            let mut manager = super::new_manager(&shared, &url_owned, &key_owned).await?;
            manager.set_metadata(discovered);
            manager
                .validate_client_credentials_metadata(&config)
                .map_err(rejected)?;
            manager
                .configure_client_credentials(&config)
                .map_err(rejected)?;
            manager
                .exchange_client_credentials(&config)
                .await
                .map_err(rejected)?;
            Ok::<(), String>(())
        })
        .await;
        exchanged.and_then(|r| r).map_err(failed)?;
        self.remember_metadata(key, &metadata).await;
        Ok(())
    }

    async fn exchange_enterprise_identity(
        &self,
        url: &str,
        key: &str,
        oauth: &McpOAuthConfig,
    ) -> Result<(), Unattended> {
        let failed = Unattended::Failed;
        let idp = self.enterprise_idp().ok_or_else(|| {
            failed(
                "no enterprise IdP is configured (Assembly::mcp.enterprise_idp or the MCP \
                 policy's enterprise_idp)"
                    .into(),
            )
        })?;
        let identity = self.identity().ok_or_else(|| {
            Unattended::NeedsIdentity(
                "no enterprise identity yet; the host supplies one with mcp.auth.setIdentity"
                    .into(),
            )
        })?;
        let (_, metadata) = self.unattended_metadata(url, key).await?;
        let issuer = metadata
            .issuer
            .clone()
            .ok_or_else(|| failed("the server's authorization server names no issuer".into()))?;

        let idp_auth = match idp.auth_method() {
            ClientAuthMethod::None => EmaClientAuthentication::None,
            method => {
                let secret = ClientSecret::new(
                    idp.client_secret()
                        .map_err(failed)?
                        .ok_or_else(|| failed("enterprise_idp needs client_secret_env".into()))?,
                );
                match method {
                    ClientAuthMethod::ClientSecretPost => {
                        EmaClientAuthentication::ClientSecretPost(secret)
                    }
                    _ => EmaClientAuthentication::ClientSecretBasic(secret),
                }
            }
        };
        let server_auth = match self
            .configured_secret(oauth, Some(&issuer))
            .map_err(|e| failed(e.to_string()))?
        {
            None => EmaClientAuthentication::None,
            Some(secret) if supports_only_post(&metadata) => {
                EmaClientAuthentication::ClientSecretPost(ClientSecret::new(secret))
            }
            Some(secret) => EmaClientAuthentication::ClientSecretBasic(ClientSecret::new(secret)),
        };
        let idp_server = EmaAuthorizationServer::new(
            idp.issuer.clone(),
            idp.token_endpoint.clone(),
            idp.client_id.clone(),
        )
        .with_client_authentication(idp_auth);
        let resource_server = EmaAuthorizationServer::new(
            issuer,
            metadata.token_endpoint.clone(),
            oauth.client_id.clone().unwrap_or_default(),
        )
        .with_client_authentication(server_auth);

        let idp_http = url::Url::parse(&idp.token_endpoint)
            .map_err(|e| format!("enterprise_idp.token_endpoint: {e}"))
            .and_then(|u| super::http::OAuthHttp::for_configured(&u))
            .map_err(failed)?;
        let server_http = url::Url::parse(url)
            .map_err(|e| e.to_string())
            .and_then(|u| super::http::OAuthHttp::for_server(&u))
            .map_err(failed)?;
        let refresh;
        let request = match &identity {
            EnterpriseIdentity::IdToken(token) => {
                EmaExchangeRequest::new_with_id_token(idp_server, resource_server, key, token)
            }
            EnterpriseIdentity::RefreshToken(token) => {
                refresh = RefreshToken::new(token.clone());
                EmaExchangeRequest::new(idp_server, resource_server, key, &refresh)
            }
        }
        .with_scopes(oauth.scopes.clone());
        let token = request
            .exchange(&idp_http, &server_http)
            .await
            .map_err(ema_error)?;

        let mut scopes: Vec<String> = token.scopes.into_iter().collect();
        scopes.sort();
        let tokens = McpTokens {
            access_token: token.access_token.secret().to_string(),
            refresh_token: None,
            expires_at: token.expires_in.map(|d| now_secs() + d.as_secs()),
            scopes,
            client_id: oauth.client_id.clone().unwrap_or_default(),
        };
        let issuer = metadata.issuer.clone();
        let metadata = serde_json::to_value(&metadata).ok();
        self.shared
            .update(key, |r| {
                r.tokens = Some(tokens);
                r.issuer = issuer;
                r.authorization_server = metadata;
            })
            .await
            .map_err(|e| failed(format!("cannot store the token: {e}")))?;
        Ok(())
    }

    /// [`Self::ensure_unattended`], as the connection sees it.
    pub(crate) async fn renew_unattended(
        &self,
        server: &str,
        url: &str,
        oauth: &McpOAuthConfig,
        renew: bool,
    ) -> Result<(), McpError> {
        self.ensure_unattended(server, url, oauth, renew)
            .await
            .map_err(|e| match e {
                Unattended::NeedsIdentity(_) => McpError::AuthRequired {
                    name: server.to_string(),
                    challenge: None,
                },
                Unattended::Failed(message) => McpError::ConnectFailed {
                    name: server.to_string(),
                    source: anyhow::anyhow!(message),
                },
            })
    }

    /// Make sure an unattended server's stored token lasts a while yet —
    /// fetching a new one when it does not, or always when `renew`.
    pub(crate) async fn ensure_unattended(
        &self,
        server: &str,
        url: &str,
        oauth: &McpOAuthConfig,
        renew: bool,
    ) -> Result<(), Unattended> {
        let key = super::canonical_server_url(url).map_err(Unattended::Failed)?;
        let fresh = |entry: &super::Entry| {
            entry.has_tokens
                && entry
                    .expires_at
                    .is_none_or(|at| at > now_secs() + RENEW_BEFORE_SECS)
        };
        if !renew && fresh(&self.shared.entry(&key)) {
            return Ok(());
        }
        let lock = self.shared.acquire_lock(&key);
        let _held = lock.lock().await;
        if !renew {
            self.shared
                .load(&key)
                .await
                .map_err(|e| Unattended::Failed(format!("cannot read the stored token: {e}")))?;
            if fresh(&self.shared.entry(&key)) {
                return Ok(());
            }
        }
        let fetched = match oauth.grant {
            OAuthGrant::ClientCredentials => self.fetch_client_credentials(url, &key, oauth).await,
            OAuthGrant::Enterprise => self.exchange_enterprise_identity(url, &key, oauth).await,
            OAuthGrant::AuthorizationCode => Ok(()),
        };
        match &fetched {
            Ok(()) => self.shared.with_entry(&key, |e| {
                e.names.insert(server.to_string());
                e.state = super::AuthState::Authorized;
                e.error = None;
            }),
            Err(Unattended::NeedsIdentity(error)) => {
                let announce = self.shared.with_entry(&key, |e| {
                    e.names.insert(server.to_string());
                    let announce = e.state != super::AuthState::Required;
                    e.state = super::AuthState::Required;
                    e.error = Some(error.clone());
                    announce
                });
                if announce {
                    self.shared.emit(super::AuthEvent::Required {
                        server: server.to_string(),
                        url: key.clone(),
                        scopes: oauth.scopes.clone(),
                    });
                }
            }
            Err(Unattended::Failed(error)) => self.shared.with_entry(&key, |e| {
                e.names.insert(server.to_string());
                e.state = super::AuthState::Failed;
                e.error = Some(error.clone());
            }),
        }
        fetched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_never_shows_in_debug_output() {
        let shown = format!(
            "{:?} {:?}",
            EnterpriseIdentity::IdToken("eyJ-secret".into()),
            EnterpriseIdentity::RefreshToken("rt-secret".into())
        );
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[test]
    fn every_configurable_algorithm_maps_to_a_signer() {
        for alg in crate::config::SIGNING_ALGS {
            assert!(signing_alg(alg).is_ok(), "{alg}");
        }
        assert!(signing_alg("HS256").is_err());
    }

    #[tokio::test]
    async fn work_off_the_runtime_comes_back() {
        let value = isolated(|| async { 41 + 1 }).await.unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn waiting_for_an_identity_announces_once() {
        let dir = tempfile::tempdir().unwrap();
        let auth = McpAuth::with_file_store(dir.path().to_path_buf());
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = events.clone();
        auth.set_listener(std::sync::Arc::new(move |e| seen.lock().unwrap().push(e)));
        auth.set_enterprise_idp(Some(crate::config::EnterpriseIdp {
            issuer: "https://idp.example".into(),
            token_endpoint: "https://idp.example/token".into(),
            client_id: "agix".into(),
            client_secret_env: None,
            client_auth: None,
        }));
        let oauth = McpOAuthConfig {
            grant: OAuthGrant::Enterprise,
            client_id: Some("c".into()),
            ..Default::default()
        };
        for _ in 0..2 {
            let waiting = auth
                .ensure_unattended("docs", "https://mcp.example.com/mcp", &oauth, false)
                .await;
            assert!(matches!(waiting, Err(Unattended::NeedsIdentity(_))));
        }
        assert_eq!(events.lock().unwrap().len(), 1);
    }
}
