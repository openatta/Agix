//! `project.*`。

use super::*;
use crate::rpc::methods::project::ProjectTrustParams;
use crate::rpc::methods::ConfigTarget;
use crate::session_pool::api::{ProjectStatus, ProjectTrustSet};

rpc_methods! {
    "project.status" => status(ConfigTarget) -> ProjectStatus;
    "project.trust" => trust(ProjectTrustParams) -> ProjectTrustSet;
}

async fn status(
    s: &DaemonServer,
    _: &Call,
    target: ConfigTarget,
) -> Result<ProjectStatus, PoolError> {
    let (project_root, scene) = s.target_of(target);
    Ok(s.pool.project_status(project_root.as_deref(), &scene).await)
}

async fn trust(
    s: &DaemonServer,
    _: &Call,
    params: ProjectTrustParams,
) -> Result<ProjectTrustSet, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .set_project_trust(project_root.as_deref(), &scene, params.trusted)
        .await
}
