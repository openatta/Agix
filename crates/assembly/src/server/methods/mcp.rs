//! `mcp.*`：属于 `mcp` 模块，没编进这个构建时答 `METHOD_NOT_FOUND` 加
//! `data.module`。

use super::*;

rpc_methods! {
    #[cfg(feature = "mcp")]
    "mcp.status" => status(Untyped) -> Untyped ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.addServer" => add_server(Untyped) -> Untyped ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.updateServer" => update_server(Untyped) -> Untyped ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.start" => auth_start(Untyped) -> Untyped ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.complete" => auth_complete(Untyped) -> Untyped ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.cancel" => auth_cancel(Untyped) -> Untyped ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.logout" => auth_logout(Untyped) -> Untyped ["mcp", cfg!(feature = "mcp")];
    #[cfg(feature = "mcp")]
    "mcp.auth.setIdentity" => auth_set_identity(Untyped) -> Untyped ["mcp", cfg!(feature = "mcp")];
}

#[cfg(feature = "mcp")]
async fn status(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    RpcResponse::ok(cx.id(), json!({"servers": s.pool.mcp_status().await}))
}

#[cfg(feature = "mcp")]
async fn add_server(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_mcp_put_server(cx.id(), params, false).await
}

#[cfg(feature = "mcp")]
async fn update_server(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_mcp_put_server(cx.id(), params, true).await
}

#[cfg(feature = "mcp")]
async fn auth_start(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_mcp_auth_start(cx.id(), params).await
}

#[cfg(feature = "mcp")]
async fn auth_complete(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_mcp_auth_complete(cx.id(), params).await
}

#[cfg(feature = "mcp")]
async fn auth_cancel(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_mcp_auth_cancel(cx.id(), params).await
}

#[cfg(feature = "mcp")]
async fn auth_logout(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_mcp_auth_logout(cx.id(), params).await
}

#[cfg(feature = "mcp")]
async fn auth_set_identity(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_mcp_auth_set_identity(cx.id(), params).await
}
