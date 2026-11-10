//! `daemon.*` 与 `commands.list` 的线格式。

use crate::discovery::DaemonIdentity;
use crate::session_pool::api::CommandInfo;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Pong {
    #[serde(flatten)]
    pub identity: DaemonIdentity,
    pub pong: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DaemonStatus {
    #[serde(flatten)]
    pub identity: DaemonIdentity,
    pub uptime_secs: u64,
    /// Sessions in memory, sidechains included.
    pub sessions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventsSubscribed {
    pub subscribed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ShuttingDown {
    pub shutting_down: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CommandList {
    pub commands: Vec<CommandInfo>,
}
