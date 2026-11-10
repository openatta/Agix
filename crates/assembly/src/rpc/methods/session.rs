//! 会话生命周期方法（`session.create` … `session.interrupt`）的参数，以及池结果
//! 之外的线上回执。

use super::Patch;
pub use super::SessionRef;
use crate::rpc::RawOptions;
use crate::session_pool::{ProjectSelector, SessionInfo};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `project_root`: the key omitted, `null`, or a path (protocol §3.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProjectRootParam {
    #[default]
    Omitted,
    Null,
    Path(PathBuf),
}

impl ProjectRootParam {
    /// For `#[serde(default, deserialize_with = "ProjectRootParam::present")]`.
    pub fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<PathBuf>::deserialize(deserializer)? {
            None => Self::Null,
            Some(path) => Self::Path(path),
        })
    }
}

impl From<ProjectRootParam> for ProjectSelector {
    fn from(param: ProjectRootParam) -> Self {
        match param {
            ProjectRootParam::Omitted => Self::Default,
            ProjectRootParam::Null => Self::NoProject,
            ProjectRootParam::Path(path) => Self::Path(path),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateParams {
    /// Omitted: the daemon's default scene.
    #[serde(default)]
    pub scene: Option<String>,
    /// Omitted: the daemon's default project; `null`: no project.
    #[serde(default, deserialize_with = "ProjectRootParam::present")]
    #[schemars(with = "Option<PathBuf>")]
    pub project_root: ProjectRootParam,
    #[serde(default)]
    pub options: Option<RawOptions>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ListParams {
    #[serde(default)]
    pub include_children: bool,
    /// Given: only the sessions recording this one as their parent.
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionList {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Closed {
    /// The session id, not a flag.
    pub closed: String,
    pub sidechains_deleted: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeleteParams {
    pub session_id: String,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ForkParams {
    pub session_id: String,
    /// In `session.history`'s positions; omitted copies the whole transcript.
    #[serde(default)]
    pub at_message: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResumeParams {
    pub session_id: String,
    #[serde(default)]
    pub create_if_missing: bool,
    #[serde(default)]
    pub options: Option<RawOptions>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RenameParams {
    pub session_id: String,
    /// `null` or omitted clears the name.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SetModelParams {
    pub session_id: String,
    /// At least one of `model` and `provider`.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SetPromptParams {
    pub session_id: String,
    /// A string sets it, `null` clears it back to the settings files', an
    /// absent key leaves it.
    #[serde(default, deserialize_with = "Patch::present")]
    #[schemars(with = "Option<String>")]
    pub append: Patch<String>,
    #[serde(default, rename = "override", deserialize_with = "Patch::present")]
    #[schemars(with = "Option<String>")]
    pub override_text: Patch<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SubscribeParams {
    pub session_id: String,
    /// Resume the session first when it is not in memory.
    #[serde(default)]
    pub resume: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SubscribeResult {
    pub session_id: String,
    /// The transcript watermark to catch up to (protocol §5.6).
    pub last_seq: usize,
    /// Each unanswered `kind:"prompt"` `session.event` frame, as sent.
    pub pending_prompts: Vec<serde_json::Value>,
    pub resumed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scene_inferred: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Unsubscribed {
    pub session_id: String,
    /// Always `false`.
    pub subscribed: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Interrupted {
    pub session_id: String,
    /// `false`: no turn was running.
    pub interrupted: bool,
}
