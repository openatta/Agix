//! `schedule.*` 的 params 与线格式。

use serde::{Deserialize, Serialize};
use tools::cron::CronJob;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ScheduleCreateParams {
    pub session_id: String,
    /// Five fields, M H DoM Mon DoW, in UTC.
    pub cron: String,
    pub prompt: String,
    /// `false`: run once at the next match, then delete.
    #[serde(default = "yes")]
    pub recurring: bool,
    /// Written to disk, so it outlives a daemon restart.
    #[serde(default = "yes")]
    pub durable: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScheduleCreated {
    pub job: CronJob,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct ScheduleListParams {
    /// Only this session's jobs; all of them when omitted.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScheduleList {
    pub jobs: Vec<CronJob>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ScheduleDeleteParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScheduleDeleted {
    pub deleted: bool,
}
