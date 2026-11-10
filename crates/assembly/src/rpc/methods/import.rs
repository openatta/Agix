//! `import.*` 的 params 与线格式。

use super::ConfigTarget;
use crate::session_pool::api::ImportSource;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImportSourceList {
    pub sources: Vec<ImportSource>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ImportRunParams {
    /// One of the `source` values `import.list` returned for the same project.
    pub source: String,
    #[serde(flatten)]
    pub target: ConfigTarget,
}
