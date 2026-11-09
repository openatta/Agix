//! OAuth 2.0 client: builds authorize URL, exchanges code for token, refreshes.

use crate::pkce::PkceVerifier;
use crate::provider::ProviderConfig;
use base::interface::exec::local::LocalNetwork;
use base::interface::exec::{ExecError, HttpRequest, Network, Origin};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("network: {0}")]
    Network(#[from] ExecError),
    #[error("token endpoint returned status {0}: {1}")]
    Status(u16, String),
    #[error("provider returned malformed token JSON: {0}")]
    BadJson(#[from] serde_json::Error),
    #[error("invalid URL: {0}")]
    BadUrl(#[from] url::ParseError),
}

/// What the token endpoint returns. Field names match RFC 6749 §5.1.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

pub struct OAuth2Client<'a> {
    pub provider: &'a ProviderConfig,
    pub net: Arc<dyn Network>,
}

impl<'a> OAuth2Client<'a> {
    /// Construct a new instance.
    pub fn new(provider: &'a ProviderConfig) -> Self {
        Self {
            provider,
            net: Arc::new(LocalNetwork::default()),
        }
    }

    /// Build the URL the user's browser visits to start auth. Returns the URL
    /// and the `state` value the caller should later verify in the callback.
    pub fn build_authorize_url(
        &self,
        redirect_uri: &str,
        pkce: &PkceVerifier,
    ) -> Result<(String, String), OAuthError> {
        let state = generate_state();
        let mut url = Url::parse(&self.provider.authorize_url)?;
        {
            let mut qs = url.query_pairs_mut();
            qs.append_pair("response_type", "code");
            qs.append_pair("client_id", &self.provider.client_id);
            qs.append_pair("redirect_uri", redirect_uri);
            qs.append_pair("state", &state);
            qs.append_pair("code_challenge", &pkce.challenge);
            qs.append_pair("code_challenge_method", pkce.method_str());
            let scopes = self.provider.scope_string();
            if !scopes.is_empty() {
                qs.append_pair("scope", &scopes);
            }
            for (k, v) in &self.provider.authorize_extra_params {
                qs.append_pair(k, v);
            }
        }
        Ok((url.to_string(), state))
    }

    /// Exchange `code` for tokens.
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        pkce_verifier: &str,
    ) -> Result<TokenResponse, OAuthError> {
        let mut form: HashMap<&str, &str> = HashMap::new();
        form.insert("grant_type", "authorization_code");
        form.insert("code", code);
        form.insert("redirect_uri", redirect_uri);
        form.insert("client_id", &self.provider.client_id);
        form.insert("code_verifier", pkce_verifier);
        if let Some(secret) = &self.provider.client_secret {
            form.insert("client_secret", secret);
        }
        self.post_token(&form).await
    }

    /// Refresh using stored refresh_token.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, OAuthError> {
        let mut form: HashMap<&str, &str> = HashMap::new();
        form.insert("grant_type", "refresh_token");
        form.insert("refresh_token", refresh_token);
        form.insert("client_id", &self.provider.client_id);
        if let Some(secret) = &self.provider.client_secret {
            form.insert("client_secret", secret);
        }
        self.post_token(&form).await
    }

    async fn post_token(&self, form: &HashMap<&str, &str>) -> Result<TokenResponse, OAuthError> {
        let body = form
            .iter()
            .fold(
                url::form_urlencoded::Serializer::new(String::new()),
                |mut acc, (k, v)| {
                    acc.append_pair(k, v);
                    acc
                },
            )
            .finish();
        let mut req = HttpRequest::post(&self.provider.token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body);
        for (k, v) in &self.provider.token_request_headers {
            req = req.header(k, v);
        }
        // `Origin::Operator`: the token endpoint belongs to a provider the
        // operator configured, not to a destination the model picked.
        let resp = self.net.send(req, Origin::Operator).await?;
        let body = resp.body_text();
        if !(200..300).contains(&resp.status) {
            return Err(OAuthError::Status(resp.status, body));
        }
        let parsed: TokenResponse = serde_json::from_str(&body)?;
        Ok(parsed)
    }
}

/// Convert a TokenResponse into the on-disk StoredToken shape, stamping
/// expires_at relative to now.
pub fn token_to_stored(t: &TokenResponse) -> crate::store::StoredToken {
    let now = OffsetDateTime::now_utc();
    let expires_at = t.expires_in.map(|secs| now.unix_timestamp() + secs);
    crate::store::StoredToken {
        access_token: t.access_token.clone(),
        expires_at,
        refresh_token: t.refresh_token.clone(),
        scopes: t
            .scope
            .as_deref()
            .map(|s| s.split_whitespace().map(|x| x.to_string()).collect())
            .unwrap_or_default(),
        token_type: t.token_type.clone().unwrap_or_else(|| "Bearer".into()),
        stored_at: now.unix_timestamp(),
    }
}

/// 32 hex chars; collision-resistant for state-CSRF prevention purposes.
fn generate_state() -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_be_bytes());
    hasher.update(pid.to_be_bytes());
    hex::encode(&hasher.finalize()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkce::{PkceMethod, PkceVerifier};

    fn provider() -> ProviderConfig {
        ProviderConfig {
            authorize_url: "https://x.example/oauth/authorize".into(),
            token_url: "https://x.example/oauth/token".into(),
            client_id: "abc123".into(),
            client_secret: None,
            scopes: vec!["read".into(), "write".into()],
            token_request_headers: Default::default(),
            authorize_extra_params: Default::default(),
        }
    }

    #[test]
    fn build_authorize_url_includes_required_pkce_params() {
        let p = provider();
        let c = OAuth2Client::new(&p);
        let pkce = PkceVerifier::new(PkceMethod::S256);
        let (url, state) = c
            .build_authorize_url("http://127.0.0.1:1234/callback", &pkce)
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let qs: HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(qs.get("response_type").map(|s| s.as_str()), Some("code"));
        assert_eq!(qs.get("client_id").map(|s| s.as_str()), Some("abc123"));
        assert_eq!(
            qs.get("redirect_uri").map(|s| s.as_str()),
            Some("http://127.0.0.1:1234/callback")
        );
        assert!(qs.contains_key("state"));
        assert_eq!(qs.get("state").unwrap(), &state);
        assert_eq!(
            qs.get("code_challenge_method").map(|s| s.as_str()),
            Some("S256")
        );
        assert_eq!(qs.get("scope").map(|s| s.as_str()), Some("read write"));
    }

    #[test]
    fn build_authorize_url_appends_extra_params() {
        let mut p = provider();
        p.authorize_extra_params
            .insert("audience".into(), "console".into());
        let c = OAuth2Client::new(&p);
        let pkce = PkceVerifier::new(PkceMethod::S256);
        let (url, _) = c.build_authorize_url("http://127.0.0.1/cb", &pkce).unwrap();
        assert!(url.contains("audience=console"));
    }

    #[test]
    fn token_to_stored_stamps_expires_at() {
        let t = TokenResponse {
            access_token: "x".into(),
            refresh_token: Some("rt".into()),
            expires_in: Some(3600),
            token_type: Some("Bearer".into()),
            scope: Some("a b".into()),
        };
        let s = token_to_stored(&t);
        assert_eq!(s.access_token, "x");
        assert_eq!(s.refresh_token.as_deref(), Some("rt"));
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let exp = s.expires_at.unwrap();
        assert!(exp >= now + 3590 && exp <= now + 3610);
        assert_eq!(s.scopes, vec!["a", "b"]);
    }

    #[test]
    fn generate_state_is_32_hex_chars() {
        let s = generate_state();
        assert_eq!(s.len(), 32);
        for c in s.chars() {
            assert!(c.is_ascii_hexdigit());
        }
        // Two consecutive should differ
        assert_ne!(generate_state(), s);
    }
}
