//! `session.goal` / `setGoal` / `updateGoal` 的 params 与线格式。

use super::SessionRef;
use base::context::goal::GoalView;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct GoalResult {
    pub goal: Option<GoalView>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SetGoalParams {
    #[serde(flatten)]
    pub session: SessionRef,
    pub objective: String,
    #[serde(default)]
    pub max_rounds: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalActionName {
    Pause,
    Resume,
    Edit,
    Clear,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct UpdateGoalParams {
    #[serde(flatten)]
    pub session: SessionRef,
    pub action: GoalActionName,
    /// For `edit`.
    #[serde(default)]
    pub objective: Option<String>,
    /// For `edit`.
    #[serde(default)]
    pub max_rounds: Option<u32>,
}
