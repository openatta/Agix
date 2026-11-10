//! `schedule.*`。

use super::*;

rpc_methods! {
    "schedule.create" => create(Untyped) -> Untyped;
    "schedule.list" => list(Untyped) -> Untyped;
    "schedule.delete" => delete(Untyped) -> Untyped;
}

async fn create(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_schedule_create(cx.id(), params).await
}

async fn list(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let session_id = params.get("session_id").and_then(|v| v.as_str());
    RpcResponse::ok(cx.id(), json!({"jobs": s.pool.schedule_list(session_id)}))
}

async fn delete(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    match params.get("id").and_then(|v| v.as_str()) {
        Some(job) => RpcResponse::ok(cx.id(), json!({"deleted": s.pool.schedule_remove(job)})),
        None => RpcResponse::err(cx.id(), codes::INVALID_PARAMS, "missing id"),
    }
}
