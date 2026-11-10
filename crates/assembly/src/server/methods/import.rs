//! `import.*`。

use super::*;
use crate::rpc::methods::import::{ImportRunParams, ImportSourceList};
use crate::rpc::methods::ConfigTarget;
use crate::session_pool::api::ImportRun;

rpc_methods! {
    "import.list" => list(ConfigTarget) -> ImportSourceList;
    "import.run" => run(ImportRunParams) -> ImportRun;
}

async fn list(
    s: &DaemonServer,
    _: &Call,
    target: ConfigTarget,
) -> Result<ImportSourceList, PoolError> {
    let (project_root, _scene) = s.target_of(target);
    Ok(ImportSourceList {
        sources: s.pool.list_import_sources(project_root.as_deref()).await,
    })
}

async fn run(s: &DaemonServer, _: &Call, params: ImportRunParams) -> Result<ImportRun, PoolError> {
    let (project_root, _scene) = s.target_of(params.target);
    s.pool
        .run_import(project_root.as_deref(), &params.source)
        .await
}
