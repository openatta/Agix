//! `config.*` 的 params，以及配置类方法共用的定位参数 [`ConfigTarget`]。

use crate::session_pool::api::ConfigTierName;
use serde::{Deserialize, Deserializer};
use std::path::PathBuf;

/// The `(scene, project_root)` pair a configuration method acts on. Omitted
/// means this daemon's own; a named `project_root` reaches another project
/// the daemon serves. `null` is refused: these methods act on the *project*
/// tier, and a session with no project has none.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct ConfigTarget {
    #[serde(default)]
    pub scene: Option<String>,
    #[serde(default, deserialize_with = "project_path")]
    #[schemars(with = "Option<String>")]
    pub project_root: Option<PathBuf>,
}

fn project_path<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(path) => Ok(Some(PathBuf::from(path))),
        _ => Err(serde::de::Error::custom(
            "project_root must be a path: a session with no project has no project tier",
        )),
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ConfigGetParams {
    pub scene: String,
    /// Absent and `null` both mean the scene with no project.
    #[serde(default)]
    pub project_root: Option<PathBuf>,
    #[serde(default)]
    pub tier: ConfigTierName,
    #[serde(default)]
    pub include_secrets: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ConfigGetProviderParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    #[serde(default)]
    pub include_secrets: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ConfigSetProviderParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub provider_id: String,
    /// A partial patch merged key by key onto the project tier's entry.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub default_provider: Option<String>,
    /// A partial patch merged key by key.
    #[serde(default)]
    pub task_models: Option<serde_json::Value>,
    /// Remove the provider instead of merging.
    #[serde(default)]
    pub delete: bool,
}
