//! `plugin.*` 的 params 与线格式。

use crate::session_pool::api::{PluginInfo, PluginScope};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginList {
    pub plugins: Vec<PluginInfo>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PluginInstallParams {
    /// A cache directory name: 1 to 128 of ASCII letters, digits, `.`, `_`,
    /// `-`, `+`, not starting with `.`.
    pub name: String,
    pub version: String,
    /// `file://` or `https://`; the daemon fetches it.
    pub download_url: String,
    /// SHA-256 of the archive, hex. Required for `http(s)://`.
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub scope: PluginScope,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PluginUninstallParams {
    pub name: String,
    /// All versions when omitted.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub scope: PluginScope,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PluginToggleParams {
    pub name: String,
    #[serde(default)]
    pub scope: PluginScope,
}
