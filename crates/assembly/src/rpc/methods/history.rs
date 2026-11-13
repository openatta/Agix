//! 会话日志方法（`session.history` / `appendEntry` / `entries` / `export`）的
//! params。

use crate::session_pool::api::ArchiveFormat;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    pub session_id: String,
    #[serde(default)]
    pub offset: u64,
    /// Clamped to 500 rather than refused; 100 when omitted.
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AppendEntryParams {
    pub session_id: String,
    pub ns: String,
    pub event: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct EntriesParams {
    pub session_id: String,
    /// Only these namespaces; all of the host's when omitted.
    #[serde(default)]
    pub namespaces: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ExportParams {
    pub session_id: String,
    /// Absolute, on the daemon's machine; must not exist yet.
    pub path: PathBuf,
    #[serde(default)]
    pub format: ArchiveFormat,
    /// `sidechains`, `attachments`, `recording`; the first two when omitted.
    #[serde(default)]
    pub include: Option<Vec<String>>,
}
