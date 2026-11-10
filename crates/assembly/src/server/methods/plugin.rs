//! `plugin.*`。插件子系统没编进来或被策略关掉时，每个方法都答 `PLUGINS_DISABLED`。

use super::*;

rpc_methods! {
    "plugin.list" => list(Untyped) -> Untyped;
    "plugin.install" => install(Untyped) -> Untyped;
    "plugin.uninstall" => uninstall(Untyped) -> Untyped;
    "plugin.enable" => enable(Untyped) -> Untyped;
    "plugin.disable" => disable(Untyped) -> Untyped;
    "plugin.reload" => reload(Untyped) -> Untyped;
}

fn refused(s: &DaemonServer, cx: &Call) -> Option<RpcResponse> {
    (!s.plugins_enabled()).then(|| s.plugins_disabled(cx.id()))
}

async fn list(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    if let Some(refused) = refused(s, cx) {
        return refused;
    }
    RpcResponse::ok(cx.id(), json!({"plugins": s.pool.list_plugins().await}))
}

async fn install(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    if let Some(refused) = refused(s, cx) {
        return refused;
    }
    s.method_plugin_install(cx.id(), params).await
}

async fn uninstall(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    if let Some(refused) = refused(s, cx) {
        return refused;
    }
    s.method_plugin_uninstall(cx.id(), params).await
}

async fn enable(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    if let Some(refused) = refused(s, cx) {
        return refused;
    }
    s.method_plugin_set_enabled(cx.id(), params, true).await
}

async fn disable(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    if let Some(refused) = refused(s, cx) {
        return refused;
    }
    s.method_plugin_set_enabled(cx.id(), params, false).await
}

async fn reload(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    if let Some(refused) = refused(s, cx) {
        return refused;
    }
    RpcResponse::ok(cx.id(), s.pool.reload_plugins().await)
}
