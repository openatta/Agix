//! `mcp.*` 的 params 与线格式。不依赖 mcp crate，没编进 mcp 模块的构建里 schema
//! 也一样。

use super::ConfigTarget;
use crate::session_pool::api::McpServerStatus;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpStatusList {
    pub servers: Vec<McpServerStatus>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpPutServerParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub name: String,
    /// An `McpServerConfig`, tagged by `type`; the pool validates it.
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpRedirectKind {
    #[default]
    Loopback,
    External,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpAuthStartParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub server: String,
    #[serde(default)]
    pub redirect: McpRedirectKind,
    /// Only for `"redirect": "external"`, and required there.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpAuthCompleteParams {
    pub flow_id: String,
    pub callback_url: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpAuthCancelParams {
    pub flow_id: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpAuthLogoutParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub server: String,
}

/// Exactly one of the two.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpSetIdentityParams {
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}
