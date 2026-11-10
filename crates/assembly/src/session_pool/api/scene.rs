//! 场景目录的领域结果类型。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SceneSummary {
    pub scene: String,
    pub name: String,
    pub description: String,
    pub active: bool,
    /// Sessions in memory recorded under this scene.
    pub sessions: usize,
    pub requires_project: bool,
    pub supports_team: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SceneCapabilities {
    pub requires_project: bool,
    pub supports_team: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SceneTools {
    /// `null` is no whitelist: every registered tool.
    pub allowed: Option<Vec<String>>,
    pub disallowed: Vec<String>,
    pub deferred: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SceneDescription {
    pub scene: String,
    pub name: String,
    pub description: String,
    pub project_root: Option<String>,
    pub active: bool,
    pub capabilities: SceneCapabilities,
    pub tools: SceneTools,
    /// The merged settings a session here would resolve to.
    pub settings: serde_json::Value,
}

/// What rebuilding the scene table changed.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SceneReload {
    pub scenes: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Scenes that did not load, each with the reason.
    pub errors: Vec<String>,
}
