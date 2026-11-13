//! 插件目录与安装结果的领域类型。不依赖 plugin crate，没编进插件包方案的构建里
//! 也能用。

use serde::{Deserialize, Serialize};

/// Which tier's plugin directory a request acts on.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    /// Under the global root, shared by every scene.
    #[default]
    Global,
    /// Under the scene root, for this daemon's scene only.
    Scene,
}

impl PluginScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Scene => "scene",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkillOrigin {
    User,
    Project,
    Plugin,
}

/// A package skill whose name another source holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SkillConflict {
    pub skill: String,
    pub shadowed_by: SkillOrigin,
    /// The `SKILL.md` that holds the name.
    pub path: String,
    /// The other package, when `shadowed_by` is `plugin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    /// The unpacked directory.
    pub root: String,
    /// `[[script]]` bindings that could not be honoured, each with its reason.
    pub script_faults: Vec<String>,
    pub skills: Vec<String>,
    pub skill_conflicts: Vec<SkillConflict>,
}

/// A piece of text that will reach the model, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VisibleText {
    pub origin: String,
    pub text: String,
}

/// What a plugin will put into the model's context and start, shown before
/// anyone depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginDisclosure {
    pub plugin: String,
    pub version: String,
    pub capabilities: Vec<String>,
    pub scene: Option<String>,
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub model_visible: Vec<VisibleText>,
    pub inert: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginInstalled {
    pub success: bool,
    pub message: String,
    /// Whether it is enabled once installed (`plugins.enable_on_install`).
    pub enabled: bool,
    pub disclosure: PluginDisclosure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginRemoved {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginToggled {
    pub name: String,
    pub enabled: bool,
    pub scope: PluginScope,
}
