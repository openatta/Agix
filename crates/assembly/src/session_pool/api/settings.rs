//! 设置、provider、项目信任与配置导入的领域结果类型。设置文件的内容本身
//! （`ConfigTier.settings`、`managed`）是不透明的 JSON，保持 `Value`。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Which settings layer `config.get` reads.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ConfigTierName {
    Global,
    Scene,
    Project,
    #[default]
    Effective,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConfigTier {
    pub scene: String,
    pub project_root: Option<String>,
    pub tier: ConfigTierName,
    /// The tier's settings file as it sits on disk (`null` when it has none),
    /// or the merged result for `effective`.
    pub settings: serde_json::Value,
    /// The deployment's managed layer; only `effective` has one.
    pub managed: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderView {
    pub api_type: Option<String>,
    pub base_url: Option<String>,
    /// Redacted to `***<last 4>` unless secrets were asked for.
    pub api_key: Option<String>,
    pub default_model: Option<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderConfigView {
    pub providers: BTreeMap<String, ProviderView>,
    pub default_provider: Option<String>,
    pub task_models: BTreeMap<String, base::provider::TaskModelOverride>,
}

/// What reading the settings back and resolving provider routing found.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoutingStatus {
    /// The configuration's shape parses.
    pub ok: bool,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    /// A usable router was actually built; `ok` can hold without it.
    pub router_rebuilt: bool,
    pub router_error: Option<String>,
    pub mcp_connected: Vec<String>,
    pub mcp_failed: Vec<String>,
    pub mcp_connecting: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettingsReloaded {
    pub providers: Vec<String>,
    pub default_provider: Option<String>,
    pub task_models: Vec<String>,
    pub routing: RoutingStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderSaved {
    pub written_to: String,
    /// Set when a credential was written, to the `settings.local.json`
    /// beside `written_to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_written_to: Option<String>,
    #[serde(flatten)]
    pub reloaded: SettingsReloaded,
}

/// What `set_provider` is asked to write.
#[derive(Debug, Clone)]
pub struct ProviderPatch {
    pub provider_id: String,
    pub delete: bool,
    pub config: Option<serde_json::Value>,
    pub default_provider: Option<String>,
    pub task_models: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TrustState {
    /// No project tier of its own.
    None,
    Trusted,
    Untrusted,
}

impl From<base::project_trust::ProjectTrust> for TrustState {
    fn from(trust: base::project_trust::ProjectTrust) -> Self {
        use base::project_trust::ProjectTrust;
        match trust {
            ProjectTrust::NoProject => Self::None,
            ProjectTrust::Trusted => Self::Trusted,
            ProjectTrust::Untrusted => Self::Untrusted,
        }
    }
}

impl TrustState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectStatus {
    pub project_root: PathBuf,
    pub trust: TrustState,
    /// The `project_trust.trusted` entry that matched; `null` when the
    /// default decided.
    pub matched: Option<String>,
    pub default: base::project_trust::TrustLevel,
    /// What the project sets that takes effect only while it is trusted.
    pub requires_trust: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectTrustSet {
    #[serde(flatten)]
    pub status: ProjectStatus,
    pub written_to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImportRun {
    pub source: String,
    pub actions: Vec<String>,
}
