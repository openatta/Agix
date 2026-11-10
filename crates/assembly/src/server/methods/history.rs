//! 会话日志：`session.history` / `appendEntry` / `entries` / `export`。

use super::*;

rpc_methods! {
    "session.history" => history(Untyped) -> Untyped;
    "session.appendEntry" => append_entry(Untyped) -> Untyped;
    "session.entries" => entries(Untyped) -> Untyped;
    "session.export" => export(Untyped) -> Untyped;
}

async fn history(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_history(cx.id(), params).await
}

async fn append_entry(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_append_entry(cx.id(), params).await
}

async fn entries(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_entries(cx.id(), params).await
}

async fn export(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_export(cx.id(), params).await
}
