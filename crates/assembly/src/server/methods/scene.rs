//! `scene.*`。

use super::*;

rpc_methods! {
    "scene.list" => list(Untyped) -> Untyped;
    "scene.describe" => describe(Untyped) -> Untyped;
    "scene.activate" => activate(Untyped) -> Untyped;
    "scene.deactivate" => deactivate(Untyped) -> Untyped;
    "scene.register" => register(Untyped) -> Untyped;
    "scene.unregister" => unregister(Untyped) -> Untyped;
    "scene.reload" => reload(Untyped) -> Untyped;
}

async fn list(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    RpcResponse::ok(cx.id(), json!({"scenes": s.pool.list_scenes().await}))
}

async fn describe(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_scene_describe(cx.id(), params).await
}

fn scene_param(params: &Value) -> Option<String> {
    params
        .get("scene")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

async fn activate(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let Some(scene_id) = scene_param(&params) else {
        return RpcResponse::err(cx.id(), codes::INVALID_PARAMS, "missing scene");
    };
    match s.pool.activate_scene(&scene_id).await {
        Ok(()) => RpcResponse::ok(cx.id(), json!({"scene": scene_id, "active": true})),
        Err(e) => failure(cx.id(), e),
    }
}

async fn deactivate(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let Some(scene_id) = scene_param(&params) else {
        return RpcResponse::err(cx.id(), codes::INVALID_PARAMS, "missing scene");
    };
    match s.pool.deactivate_scene(&scene_id).await {
        Ok(()) => RpcResponse::ok(cx.id(), json!({"scene": scene_id})),
        Err(e) => failure(cx.id(), e),
    }
}

async fn register(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_scene_register(cx.id(), params).await
}

async fn unregister(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let Some(scene) = scene_param(&params) else {
        return RpcResponse::err(cx.id(), codes::INVALID_PARAMS, "missing scene");
    };
    match s.pool.unregister_scene(&scene).await {
        Ok(()) => RpcResponse::ok(cx.id(), json!({"scene": scene, "registered": false})),
        Err(e) => failure(cx.id(), e),
    }
}

async fn reload(s: &DaemonServer, cx: &Call, _: Untyped) -> RpcResponse {
    RpcResponse::ok(cx.id(), s.pool.reload_scenes().await)
}
