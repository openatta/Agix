//! `config.*`。

use super::*;

rpc_methods! {
    "config.get" => get(Untyped) -> Untyped;
    "config.getProvider" => get_provider(Untyped) -> Untyped;
    "config.setProvider" => set_provider(Untyped) -> Untyped;
    "config.reload" => reload(Untyped) -> Untyped;
}

async fn get(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_config_get(cx.id(), params).await
}

async fn get_provider(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_config_get_provider(cx.id(), params).await
}

async fn set_provider(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_config_set_provider(cx.id(), params).await
}

/// Re-reads all three settings.json tiers from disk as they are, for a
/// person (or another process) who edited a file by hand; the same
/// `apply_reloaded_settings` as `config.setProvider`.
async fn reload(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_config_reload(cx.id(), params).await
}
