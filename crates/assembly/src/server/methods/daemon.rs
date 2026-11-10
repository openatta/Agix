//! `daemon.*` 与 `commands.list`。

use super::*;

rpc_methods! {
    "daemon.status" => status(Untyped) -> Untyped;
    "daemon.ping" => ping(Untyped) -> Untyped;
    "daemon.doctor" => doctor(Untyped) -> Untyped;
    "daemon.subscribeEvents" => subscribe_events(Untyped) -> Untyped;
    "daemon.shutdown" => shutdown(Untyped) -> Untyped;
    "commands.list" => commands_list(Untyped) -> Untyped;
}

async fn status(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    let count = s.pool.active_count().await;
    let mut result = crate::discovery::identity();
    result["uptime_secs"] = json!(s.started_at.elapsed().as_secs());
    result["sessions"] = json!(count);
    RpcResponse::ok(cx.id(), result)
}

async fn ping(_: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    let mut result = crate::discovery::identity();
    result["pong"] = json!(true);
    RpcResponse::ok(cx.id(), result)
}

async fn doctor(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    RpcResponse::ok(cx.id(), s.pool.doctor_report().await)
}

async fn subscribe_events(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    s.method_daemon_subscribe_events(cx.id(), cx.client.clone())
        .await
}

async fn shutdown(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    s.shutdown().await;
    RpcResponse::ok(cx.id(), json!({"shutting_down": true}))
}

async fn commands_list(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    RpcResponse::ok(cx.id(), json!({"commands": s.pool.list_commands().await}))
}
