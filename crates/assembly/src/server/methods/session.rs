//! 会话生命周期：`session.create` / `get` / `list` / `close` / `delete` / `fork` /
//! `resume` / `rename` / `setModel` / `setPrompt` / `subscribe` / `unsubscribe` /
//! `interrupt`。

use super::*;

rpc_methods! {
    "session.create" => create(Untyped) -> Untyped;
    "session.get" => get(Untyped) -> Untyped;
    "session.list" => list(Untyped) -> Untyped;
    "session.close" => close(Untyped) -> Untyped;
    "session.delete" => delete(Untyped) -> Untyped;
    "session.fork" => fork(Untyped) -> Untyped;
    "session.resume" => resume(Untyped) -> Untyped;
    "session.rename" => rename(Untyped) -> Untyped;
    "session.setModel" => set_model(Untyped) -> Untyped;
    "session.setPrompt" => set_prompt(Untyped) -> Untyped;
    "session.subscribe" => subscribe(Untyped) -> Untyped;
    "session.unsubscribe" => unsubscribe(Untyped) -> Untyped;
    "session.interrupt" => interrupt(Untyped) -> Untyped;
}

async fn create(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_create(cx.id(), params).await
}

async fn get(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_get(cx.id(), params).await
}

async fn list(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let include_children = params
        .get("include_children")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let parent_session_id = params.get("parent_session_id").and_then(|v| v.as_str());
    let sessions = s.pool.list_all(include_children, parent_session_id).await;
    RpcResponse::ok(cx.id(), json!({"sessions": sessions}))
}

fn session_id_param(params: &Value) -> Option<String> {
    params
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

async fn close(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let Some(session_id) = session_id_param(&params) else {
        return RpcResponse::err(cx.id(), codes::INVALID_PARAMS, "missing session_id");
    };
    match s.pool.shutdown_session(&session_id).await {
        Ok(sidechains_deleted) => RpcResponse::ok(
            cx.id(),
            json!({
                "closed": session_id,
                "sidechains_deleted": sidechains_deleted,
            }),
        ),
        Err(e) => failure(cx.id(), e),
    }
}

async fn delete(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let Some(session_id) = session_id_param(&params) else {
        return RpcResponse::err(cx.id(), codes::INVALID_PARAMS, "missing session_id");
    };
    let dry_run = params
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    respond(cx.id(), s.pool.delete_session(&session_id, dry_run).await)
}

async fn fork(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_fork(cx.id(), params).await
}

async fn resume(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_resume(cx.id(), params).await
}

async fn rename(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_rename(cx.id(), params).await
}

async fn set_model(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_set_model(cx.id(), params).await
}

async fn set_prompt(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_set_prompt(cx.id(), params).await
}

async fn subscribe(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_subscribe(cx.id(), params, cx.client.clone())
        .await
}

async fn unsubscribe(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_unsubscribe(cx.id(), params, cx.client.clone())
        .await
}

async fn interrupt(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_interrupt(cx.id(), params).await
}
