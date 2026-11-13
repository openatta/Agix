//! `mcp.status` 的条目与 mcp crate 状态到领域类型的转换。

use super::*;
use crate::session_pool::*;

/// `mcp.status` entries for the project's own servers that are not started
/// because the project is not trusted.
pub(in crate::session_pool) fn untrusted_mcp_entries(
    settings: &LoadedSettings,
) -> Vec<api::McpServerStatus> {
    if !settings.trust().is_untrusted() {
        return Vec::new();
    }
    settings
        .project_mcp_servers()
        .iter()
        .filter(|(name, _)| !settings.mcp_servers.contains_key(*name))
        .map(|(name, cfg)| {
            let transport = serde_json::from_value::<mcp::config::McpServerConfig>(cfg.clone())
                .map(|c| c.transport_kind().to_string())
                .unwrap_or_else(|_| "unknown".into());
            api::McpServerStatus {
                name: name.clone(),
                transport,
                tool_count: 0,
                state: api::McpServerState::Untrusted,
                error: Some(
                    "the project is not trusted; its own MCP servers start once it is (project.trust)"
                        .into(),
                ),
                auth: api::McpAuthStatus::default(),
            }
        })
        .collect()
}

impl From<mcp::oauth::AuthFlowError> for PoolError {
    fn from(e: mcp::oauth::AuthFlowError) -> Self {
        use mcp::oauth::AuthFlowError;
        match e {
            AuthFlowError::Invalid(message) => Self::InvalidRequest(message),
            AuthFlowError::Denied(message) => Self::McpServerDenied(message),
            AuthFlowError::Failed(message) => Self::McpAuthFailed(message),
        }
    }
}

impl From<mcp::oauth::AuthStatus> for api::McpAuthStatus {
    fn from(status: mcp::oauth::AuthStatus) -> Self {
        use mcp::oauth::AuthState;
        Self {
            state: match status.state {
                AuthState::None => api::McpAuthState::None,
                AuthState::Required => api::McpAuthState::Required,
                AuthState::Authorizing => api::McpAuthState::Authorizing,
                AuthState::Authorized => api::McpAuthState::Authorized,
                AuthState::Failed => api::McpAuthState::Failed,
            },
            issuer: status.issuer,
            scopes: status.scopes,
            expires_at: status.expires_at,
            error: status.error,
        }
    }
}

/// `mcp.status`'s entries: the connected servers, those waiting for a
/// sign-in, then the denied ones. Each carries its authorization state.
pub(in crate::session_pool) fn mcp_status_entries(
    manager: &McpManager,
) -> Vec<api::McpServerStatus> {
    use api::McpServerState as State;
    let auth = |name: &str| {
        manager
            .config_of(name)
            .and_then(|cfg| manager.auth()?.status(cfg))
            .map(api::McpAuthStatus::from)
            .unwrap_or_default()
    };
    let entry = |name: &str,
                 transport: String,
                 tool_count: usize,
                 state: State,
                 error: Option<String>,
                 auth: api::McpAuthStatus| api::McpServerStatus {
        name: name.to_string(),
        transport,
        tool_count,
        state,
        error,
        auth,
    };
    let connected = manager.server_statuses().into_iter().map(|s| {
        let state = if s.connected {
            State::Connected
        } else {
            State::Disconnected
        };
        entry(
            &s.name,
            s.transport.to_string(),
            s.tool_count,
            state,
            None,
            auth(&s.name),
        )
    });
    let connecting = manager.connecting().iter().map(|c| {
        entry(
            &c.name,
            c.transport.to_string(),
            0,
            State::Connecting,
            None,
            auth(&c.name),
        )
    });
    let failed = manager.failed().iter().map(|f| {
        entry(
            &f.name,
            f.transport.to_string(),
            0,
            State::Failed,
            Some(f.error.clone()),
            auth(&f.name),
        )
    });
    let pending = manager.pending().iter().map(|p| {
        entry(
            &p.name,
            p.transport.to_string(),
            0,
            State::AuthRequired,
            None,
            auth(&p.name),
        )
    });
    let denied = manager.denied().iter().map(|d| {
        entry(
            &d.name,
            d.transport.to_string(),
            0,
            State::Denied,
            Some(d.reason.clone()),
            api::McpAuthStatus::default(),
        )
    });
    connected
        .chain(connecting)
        .chain(pending)
        .chain(failed)
        .chain(denied)
        .collect()
}
