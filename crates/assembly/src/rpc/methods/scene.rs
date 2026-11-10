//! `scene.*` 的 params 与线格式。

use crate::session_pool::api::SceneSummary;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SceneList {
    pub scenes: Vec<SceneSummary>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SceneRef {
    pub scene: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SceneDescribeParams {
    pub scene: String,
    /// Absent and `null` both mean the scene with no project.
    #[serde(default)]
    pub project_root: Option<PathBuf>,
    #[serde(default)]
    pub include_secrets: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SceneActivated {
    pub scene: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SceneDeactivated {
    pub scene: String,
}

/// A `scene.json`, as an object or as its JSON text.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum SceneSource {
    Text(String),
    Object(serde_json::Map<String, serde_json::Value>),
}

impl SceneSource {
    pub fn into_json_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Object(object) => serde_json::Value::Object(object).to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SceneRegisterParams {
    pub scene: SceneSource,
    /// The scene's `prompt.md`.
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SceneRegistered {
    pub scene: String,
    pub registered: bool,
}
