//! 斜杠命令目录与定时任务的领域类型。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    pub kind: CommandKind,
    pub source: CommandSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CommandKind {
    /// Runs immediately, no model turn.
    #[serde(rename = "local")]
    Local,
    /// Expands a skill body and goes on to the model.
    #[serde(rename = "prompt")]
    Prompt,
    #[serde(rename = "mcp-prompt")]
    McpPrompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandSource {
    Builtin,
    User,
    Project,
    Plugin,
    Mcp,
}

impl From<runtime::commands::CommandInfo> for CommandInfo {
    fn from(info: runtime::commands::CommandInfo) -> Self {
        let kind = match info.kind {
            "local" => CommandKind::Local,
            "prompt" => CommandKind::Prompt,
            _ => CommandKind::McpPrompt,
        };
        let source = match info.source {
            "builtin" => CommandSource::Builtin,
            "user" => CommandSource::User,
            "project" => CommandSource::Project,
            "plugin" => CommandSource::Plugin,
            _ => CommandSource::Mcp,
        };
        Self {
            name: info.name,
            description: info.description,
            kind,
            source,
        }
    }
}

/// A job to schedule: `prompt` runs as a turn of `session_id` whenever
/// `cron` matches.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub session_id: String,
    pub cron: String,
    pub prompt: String,
    pub recurring: bool,
    pub durable: bool,
}
