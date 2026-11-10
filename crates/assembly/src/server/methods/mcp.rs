//! `mcp.*`：属于 `mcp` 模块，没编进这个构建时答 `METHOD_NOT_FOUND` 加
//! `data.module`。

use super::*;
#[cfg(feature = "mcp")]
use crate::rpc::methods::mcp::McpRedirectKind;
use crate::rpc::methods::mcp::{
    McpAuthCancelParams, McpAuthCompleteParams, McpAuthLogoutParams, McpAuthStartParams,
    McpPutServerParams, McpSetIdentityParams, McpStatusList,
};
use crate::session_pool::api::{
    McpAuthDone, McpAuthStarted, McpReconnected, McpServerRef, McpServerWritten,
};
#[cfg(feature = "mcp")]
use crate::session_pool::api::{McpIdentity, McpRedirect};

rpc_methods! {
    #[cfg(feature = "mcp")]
    "mcp.status" => status(NoParams) -> McpStatusList ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.addServer" => add_server(McpPutServerParams) -> McpServerWritten ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.updateServer" => update_server(McpPutServerParams) -> McpServerWritten ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.start" => auth_start(McpAuthStartParams) -> McpAuthStarted ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.complete" => auth_complete(McpAuthCompleteParams) -> McpAuthDone ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.cancel" => auth_cancel(McpAuthCancelParams) -> McpServerRef ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.logout" => auth_logout(McpAuthLogoutParams) -> McpServerRef ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.setIdentity" => auth_set_identity(McpSetIdentityParams) -> McpReconnected ["mcp", cfg!(feature = "mcp")];
}

#[cfg(feature = "mcp")]
fn invalid(message: impl Into<String>) -> PoolError {
    PoolError::InvalidRequest(message.into())
}

#[cfg(feature = "mcp")]
async fn status(s: &DaemonServer, _: &Call, _: NoParams) -> Result<McpStatusList, PoolError> {
    Ok(McpStatusList {
        servers: s.pool.mcp_status().await,
    })
}

#[cfg(feature = "mcp")]
async fn add_server(
    s: &DaemonServer,
    _: &Call,
    params: McpPutServerParams,
) -> Result<McpServerWritten, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .add_mcp_server(project_root.as_deref(), &scene, &params.name, params.config)
        .await
}

#[cfg(feature = "mcp")]
async fn update_server(
    s: &DaemonServer,
    _: &Call,
    params: McpPutServerParams,
) -> Result<McpServerWritten, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .update_mcp_server(project_root.as_deref(), &scene, &params.name, params.config)
        .await
}

#[cfg(feature = "mcp")]
async fn auth_start(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthStartParams,
) -> Result<McpAuthStarted, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    let redirect = match (params.redirect, params.redirect_uri) {
        (McpRedirectKind::Loopback, None) => McpRedirect::Loopback,
        (McpRedirectKind::Loopback, Some(_)) => {
            return Err(invalid(
                "redirect_uri is only for \"redirect\": \"external\"",
            ))
        }
        (McpRedirectKind::External, Some(uri)) => McpRedirect::External(uri),
        (McpRedirectKind::External, None) => {
            return Err(invalid("\"redirect\": \"external\" needs redirect_uri"))
        }
    };
    s.pool
        .mcp_auth_start(
            project_root.as_deref(),
            &scene,
            &params.server,
            redirect,
            params.scopes,
        )
        .await
}

#[cfg(feature = "mcp")]
async fn auth_complete(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthCompleteParams,
) -> Result<McpAuthDone, PoolError> {
    s.pool
        .mcp_auth_complete(&params.flow_id, &params.callback_url)
        .await
}

#[cfg(feature = "mcp")]
async fn auth_cancel(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthCancelParams,
) -> Result<McpServerRef, PoolError> {
    s.pool.mcp_auth_cancel(&params.flow_id).await
}

#[cfg(feature = "mcp")]
async fn auth_logout(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthLogoutParams,
) -> Result<McpServerRef, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .mcp_auth_logout(project_root.as_deref(), &scene, &params.server)
        .await
}

#[cfg(feature = "mcp")]
async fn auth_set_identity(
    s: &DaemonServer,
    _: &Call,
    params: McpSetIdentityParams,
) -> Result<McpReconnected, PoolError> {
    let text = |value: Option<String>| value.filter(|token| !token.is_empty());
    let identity = match (text(params.id_token), text(params.refresh_token)) {
        (Some(token), None) => McpIdentity::IdToken(token),
        (None, Some(token)) => McpIdentity::RefreshToken(token),
        _ => return Err(invalid("give exactly one of id_token / refresh_token")),
    };
    s.pool.mcp_auth_set_identity(identity).await
}
