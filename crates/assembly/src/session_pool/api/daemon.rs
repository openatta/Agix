//! 池级事件：不属于某个会话的通知（MCP 连接与登录、可导入配置、会话进出内存、
//! 一轮的起止、项目信任变化）。server 把它们原样包成 `daemon.event` 推给订阅者。

use super::Initiator;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DaemonEvent {
    McpConnected {
        server: String,
        transport: String,
        tool_count: usize,
    },
    McpConnectFailed {
        server: String,
        error: String,
    },
    McpAuthRequired {
        server: String,
        scopes: Vec<String>,
    },
    McpAuthCompleted {
        server: String,
    },
    McpAuthFailed {
        server: String,
        error: String,
    },
    ImportDetected {
        sources: Vec<ImportSource>,
    },
    SessionCreated {
        session_id: String,
        scene: String,
        project_root: Option<PathBuf>,
    },
    SessionResumed {
        session_id: String,
        scene: String,
        project_root: Option<PathBuf>,
    },
    SessionEvicted {
        session_id: String,
        reason: EvictReason,
    },
    SessionClosed {
        session_id: String,
        reason: CloseReason,
    },
    SessionDeleted {
        session_id: String,
        /// Set on a sidechain deleted along with its parent.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_session_id: Option<String>,
    },
    TurnStarted {
        session_id: String,
        turn_id: String,
        initiator: Initiator,
    },
    TurnCompleted {
        session_id: String,
        turn_id: String,
        initiator: Initiator,
    },
    ProjectTrustChanged {
        project_root: PathBuf,
        /// `none`, `trusted` or `untrusted`, as `project.status` reports it.
        trust: String,
    },
}

impl DaemonEvent {
    /// `session_resumed` or `session_created`.
    pub(crate) fn session_loaded(
        resumed: bool,
        session_id: String,
        scene: String,
        project_root: Option<PathBuf>,
    ) -> Self {
        if resumed {
            Self::SessionResumed {
                session_id,
                scene,
                project_root,
            }
        } else {
            Self::SessionCreated {
                session_id,
                scene,
                project_root,
            }
        }
    }

    /// The pool-level telemetry this event also is, if any.
    pub(crate) fn telemetry(&self) -> Option<telemetry::TelemetryEvent> {
        Some(match self {
            Self::McpConnected {
                server,
                transport,
                tool_count,
            } => telemetry::TelemetryEvent::mcp_server_connected(
                "",
                0,
                None,
                telemetry::McpServerConnectedPayload {
                    server_name: server.clone(),
                    transport: transport.clone(),
                    tool_count: *tool_count,
                },
            ),
            Self::McpConnectFailed { server, error } => {
                telemetry::TelemetryEvent::mcp_connection_error(
                    "",
                    0,
                    None,
                    telemetry::McpConnectionErrorPayload {
                        server_name: server.clone(),
                        error_message: error.clone(),
                    },
                )
            }
            _ => return None,
        })
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ImportSource {
    pub source: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EvictReason {
    Idle,
    Capacity,
    /// An in-place rebuild could not go through once it had stopped the old
    /// engine, or the old engine did not stop in time.
    Rebuild,
    /// A turn ended without giving the session back — what ran it failed —
    /// and the engine it ran on could not be used again.
    EngineFault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CloseReason {
    Closed,
    EngineStopped,
}
