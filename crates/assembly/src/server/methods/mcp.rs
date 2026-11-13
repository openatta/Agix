//! `mcp.*`：属于 `mcp` 模块，没编进这个构建时答 `METHOD_NOT_FOUND` 加
//! `data.module`。

use super::*;
use crate::rpc::methods::mcp::{
    McpAuthCancelParams, McpAuthCompleteParams, McpAuthLogoutParams, McpAuthStartParams,
    McpPutServerParams, McpRedirectKind, McpSetIdentityParams, McpStatusList,
};
use crate::session_pool::api::{
    McpAuthDone, McpAuthStarted, McpIdentity, McpReconnected, McpRedirect, McpServerRef,
    McpServerWritten,
};
use crate::session_pool::MCP_COMPILED;

rpc_methods! {
    "mcp.status" => status(NoParams) -> McpStatusList ["mcp", MCP_COMPILED];
    "mcp.addServer" => add_server(McpPutServerParams) -> McpServerWritten ["mcp", MCP_COMPILED];
    "mcp.updateServer" => update_server(McpPutServerParams) -> McpServerWritten ["mcp", MCP_COMPILED];
    "mcp.auth.start" => auth_start(McpAuthStartParams) -> McpAuthStarted ["mcp", MCP_COMPILED];
    "mcp.auth.complete" => auth_complete(McpAuthCompleteParams) -> McpAuthDone ["mcp", MCP_COMPILED];
    "mcp.auth.cancel" => auth_cancel(McpAuthCancelParams) -> McpServerRef ["mcp", MCP_COMPILED];
    "mcp.auth.logout" => auth_logout(McpAuthLogoutParams) -> McpServerRef ["mcp", MCP_COMPILED];
    "mcp.auth.setIdentity" => auth_set_identity(McpSetIdentityParams) -> McpReconnected ["mcp", MCP_COMPILED];
}

fn invalid(message: impl Into<String>) -> PoolError {
    PoolError::InvalidRequest(message.into())
}

async fn status(s: &DaemonServer, _: &Call, _: NoParams) -> Result<McpStatusList, PoolError> {
    Ok(McpStatusList {
        servers: s.pool.mcp_status().await,
    })
}

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

async fn auth_complete(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthCompleteParams,
) -> Result<McpAuthDone, PoolError> {
    s.pool
        .mcp_auth_complete(&params.flow_id, &params.callback_url)
        .await
}

async fn auth_cancel(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthCancelParams,
) -> Result<McpServerRef, PoolError> {
    s.pool.mcp_auth_cancel(&params.flow_id).await
}

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
