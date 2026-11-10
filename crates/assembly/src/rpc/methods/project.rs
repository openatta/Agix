//! `project.*` 的 params。

use super::ConfigTarget;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ProjectTrustParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub trusted: bool,
}
