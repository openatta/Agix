//! `McpAuth::start`: begins a browser sign-in — binds the redirect, discovers
//! the authorization server, settles the client identity, and registers the
//! flow that `complete` later finishes.

use std::sync::{Arc, Weak};

use rmcp::transport::auth::{
    AuthorizationManager, AuthorizationMetadata, AuthorizationMetadataSource, AuthorizationRequest,
    AuthorizationSession,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::store::McpClientRegistration;
use super::{
    callback, check_authorize_url, check_endpoints, describe, redirect_registered, require_pkce,
    union, AuthFlowError, AuthState, Entry, Flow, FlowStart, McpAuth, McpCredentials,
    McpOAuthConfig, Redirect, Shared,
};
use crate::config::McpServerConfig;

type Bound = (Option<tokio::net::TcpListener>, String);

async fn bind_redirect(redirect: Redirect, oauth: &McpOAuthConfig) -> Result<Bound, AuthFlowError> {
    match redirect {
        Redirect::Loopback => {
            let (listener, uri) = callback::bind_loopback(oauth.callback_port)
                .await
                .map_err(AuthFlowError::Failed)?;
            Ok((Some(listener), uri))
        }
        Redirect::External(uri) => {
            callback::check_redirect_uri(&uri).map_err(AuthFlowError::Invalid)?;
            Ok((None, uri))
        }
    }
}

fn starting_scopes(
    explicit: Option<Vec<String>>,
    entry: &Entry,
    record: &McpCredentials,
    oauth: &McpOAuthConfig,
) -> Vec<String> {
    match explicit.filter(|s| !s.is_empty()) {
        Some(explicit) => explicit,
        None if !entry.needed_scopes.is_empty() => {
            let granted = record.tokens.as_ref().map(|t| t.scopes.clone());
            union(&granted.unwrap_or_default(), &entry.needed_scopes)
        }
        None => oauth.scopes.clone(),
    }
}

fn state_of(authorize_url: &str) -> Option<String> {
    Url::parse(authorize_url).ok().and_then(|u| {
        u.query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
    })
}

fn with_secret(request: AuthorizationRequest, secret: Option<String>) -> AuthorizationRequest {
    match secret {
        Some(secret) => request.with_client_secret(secret),
        None => request,
    }
}

impl McpAuth {
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
        if oauth.grant.is_unattended() {
            return Err(AuthFlowError::Invalid(format!(
                "MCP server `{server}` is authorized without a sign-in (\"grant\": {}); \
                 there is nothing to start",
                serde_json::to_string(&oauth.grant).unwrap_or_default()
            )));
        }

        // Before binding: the sign-in being replaced may hold a fixed
        // `callback_port`.
        if let Some(before) = self.shared.release_flows_for(&key).await {
            self.shared.with_entry(&key, |e| e.state = before);
        }

        let (listener, redirect_uri) = bind_redirect(redirect, &oauth).await?;
        let redirect_url = Url::parse(&redirect_uri).map_err(|e| failed(e.to_string()))?;

        let record = self
            .shared
            .load(&key)
            .await
            .map_err(|e| failed(format!("cannot read the stored sign-in: {e}")))?
            .unwrap_or_default();
        let entry = self.shared.entry(&key);

        let mut manager = self.new_manager(&url, &key).await.map_err(failed)?;
        let metadata = self
            .discover_metadata(&mut manager, server, &url, &entry)
            .await?;
        let scopes = starting_scopes(scopes, &entry, &record, &oauth);

        let request = self
            .client_request(
                &mut manager,
                server,
                &key,
                &oauth,
                &record,
                &metadata,
                (&redirect_uri, &redirect_url),
                &scopes,
            )
            .await?;

        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, e)| failed(describe(&e)))?;
        let authorize_url = session.get_authorization_url().to_string();
        check_authorize_url(&authorize_url).map_err(failed)?;
        let state = state_of(&authorize_url)
            .ok_or_else(|| failed("the authorization URL carries no state".into()))?;
        let issuer = metadata.issuer.clone();
        let metadata_json = serde_json::to_value(&metadata).ok();
        self.shared
            .update(&key, |r| {
                r.issuer = issuer.clone();
                r.authorization_server = metadata_json;
            })
            .await
            .map_err(|e| failed(format!("cannot store the sign-in: {e}")))?;

        let (flow_id, callbacks, ttl) = self
            .register_flow(
                server,
                &key,
                &entry,
                session,
                state,
                redirect_url,
                listener,
                &redirect_uri,
            )
            .await;

        Ok(FlowStart {
            flow_id,
            authorize_url,
            redirect_uri,
            expires_in: ttl.as_secs(),
            callbacks,
        })
    }

    async fn discover_metadata(
        &self,
        manager: &mut AuthorizationManager,
        server: &str,
        url: &str,
        entry: &Entry,
    ) -> Result<AuthorizationMetadata, AuthFlowError> {
        let failed = |e: String| AuthFlowError::Failed(e);
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
        check_endpoints(url, &metadata).map_err(failed)?;
        self.admit_issuer(metadata.issuer.as_deref())
            .map_err(AuthFlowError::Denied)?;
        require_pkce(&metadata)?;
        manager.set_metadata(metadata.clone());
        Ok(metadata)
    }

    #[allow(clippy::too_many_arguments)]
    async fn client_request(
        &self,
        manager: &mut AuthorizationManager,
        server: &str,
        key: &str,
        oauth: &McpOAuthConfig,
        record: &McpCredentials,
        metadata: &AuthorizationMetadata,
        (redirect_uri, redirect_url): (&str, &Url),
        scopes: &[String],
    ) -> Result<AuthorizationRequest, AuthFlowError> {
        let failed = |e: String| AuthFlowError::Failed(e);
        let mut request = AuthorizationRequest::new(redirect_uri.to_string())
            .with_client_name(self.client.client_name.clone());
        request = if scopes.is_empty() {
            request.with_initial_scope_selection()
        } else {
            request.with_scopes(scopes.to_vec())
        };

        let issuer = metadata.issuer.clone();
        let stored = record
            .client
            .clone()
            .filter(|r| r.issuer == issuer && redirect_registered(&r.redirect_uris, redirect_url));
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
                self.configured_secret(oauth, issuer.as_deref())?,
            )),
            (None, Some(reg)) => Some((reg.client_id, reg.client_secret)),
            (None, None) => None,
        };
        Ok(match (identity, cimd) {
            (Some((id, secret)), _) => with_secret(request.with_preregistered_client(id), secret),
            (None, Some(cimd)) => request.with_client_metadata_url(cimd),
            (None, None) if metadata.registration_endpoint.is_some() => {
                let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
                let registered = manager
                    .register_client(&self.client.client_name, redirect_uri, &scope_refs)
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
                    redirect_uris: vec![redirect_uri.to_string()],
                    issuer: issuer.clone(),
                };
                self.shared
                    .update(key, |r| r.client = Some(registration))
                    .await
                    .map_err(|e| failed(format!("cannot store the client registration: {e}")))?;
                with_secret(
                    request.with_preregistered_client(registered.client_id),
                    registered.client_secret,
                )
            }
            (None, None) => {
                return Err(failed(format!(
                    "the authorization server {} offers no dynamic client registration; set \
                     `oauth.client_id` on `{server}` to a client registered there",
                    issuer.as_deref().unwrap_or("for this server")
                )))
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn register_flow(
        &self,
        server: &str,
        key: &str,
        entry: &Entry,
        session: AuthorizationSession,
        state: String,
        redirect_url: Url,
        listener: Option<tokio::net::TcpListener>,
        redirect_uri: &str,
    ) -> (
        String,
        Option<tokio::sync::mpsc::Receiver<super::LoopbackCallback>>,
        std::time::Duration,
    ) {
        let flow_id = uuid::Uuid::new_v4().simple().to_string();
        let stop = CancellationToken::new();
        let (callbacks, listening) = listener
            .map(|l| {
                callback::serve_loopback(
                    l,
                    redirect_uri.to_string(),
                    server.to_string(),
                    stop.clone(),
                )
            })
            .unzip();
        let ttl = *self
            .shared
            .flow_ttl
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = match entry.state {
            AuthState::Authorizing | AuthState::Failed => AuthState::Required,
            other => other,
        };
        self.shared.release_flows_for(key).await;
        self.shared.flows.lock().await.insert(
            flow_id.clone(),
            Flow {
                server: server.to_string(),
                key: key.to_string(),
                session,
                state,
                redirect_uri: redirect_url,
                stop,
                listening,
                before,
            },
        );
        self.shared.with_entry(key, |e| {
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
        (flow_id, callbacks, ttl)
    }
}
