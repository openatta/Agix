//! `scene.*`。

use super::*;
use crate::rpc::methods::scene::{
    SceneActivated, SceneDeactivated, SceneDescribeParams, SceneList, SceneRef,
    SceneRegisterParams, SceneRegistered,
};
use crate::session_pool::api::{SceneDescription, SceneReload};

rpc_methods! {
    "scene.list" => list(NoParams) -> SceneList;
    "scene.describe" => describe(SceneDescribeParams) -> SceneDescription;
    "scene.activate" => activate(SceneRef) -> SceneActivated;
    "scene.deactivate" => deactivate(SceneRef) -> SceneDeactivated;
    "scene.register" => register(SceneRegisterParams) -> SceneRegistered;
    "scene.unregister" => unregister(SceneRef) -> SceneRegistered;
    "scene.reload" => reload(NoParams) -> SceneReload;
}

async fn list(s: &DaemonServer, _: &Call, _: NoParams) -> Result<SceneList, PoolError> {
    Ok(SceneList {
        scenes: s.pool.list_scenes().await,
    })
}

async fn describe(
    s: &DaemonServer,
    _: &Call,
    params: SceneDescribeParams,
) -> Result<SceneDescription, PoolError> {
    s.pool
        .describe_scene(
            &params.scene,
            params.project_root.as_deref(),
            params.include_secrets,
        )
        .await
}

async fn activate(
    s: &DaemonServer,
    _: &Call,
    params: SceneRef,
) -> Result<SceneActivated, PoolError> {
    s.pool.activate_scene(&params.scene).await?;
    Ok(SceneActivated {
        scene: params.scene,
        active: true,
    })
}

async fn deactivate(
    s: &DaemonServer,
    _: &Call,
    params: SceneRef,
) -> Result<SceneDeactivated, PoolError> {
    s.pool.deactivate_scene(&params.scene).await?;
    Ok(SceneDeactivated {
        scene: params.scene,
    })
}

async fn register(
    s: &DaemonServer,
    _: &Call,
    params: SceneRegisterParams,
) -> Result<SceneRegistered, PoolError> {
    let scene = s
        .pool
        .register_scene_from_sources(&params.scene.into_json_text(), &params.prompt)
        .await?;
    Ok(SceneRegistered {
        scene,
        registered: true,
    })
}

async fn unregister(
    s: &DaemonServer,
    _: &Call,
    params: SceneRef,
) -> Result<SceneRegistered, PoolError> {
    s.pool.unregister_scene(&params.scene).await?;
    Ok(SceneRegistered {
        scene: params.scene,
        registered: false,
    })
}

async fn reload(s: &DaemonServer, _: &Call, _: NoParams) -> Result<SceneReload, PoolError> {
    Ok(s.pool.reload_scenes().await)
}
