//! `PoolError` 到 JSON-RPC 错误的唯一映射：错误码、`message`，以及
//! `docs/daemon_rpc_protocol.md` §9.1 规定的 `data` 形状。

use crate::rpc::{codes, RpcError, RpcResponse};
use crate::session_pool::api::{Busy, PoolError};
use serde_json::{json, Value};

impl From<PoolError> for RpcError {
    fn from(e: PoolError) -> Self {
        let message = e.to_string();
        let (code, data) = match &e {
            PoolError::InvalidRequest(_) | PoolError::OutputOutOfRange { .. } => {
                (codes::INVALID_PARAMS, None)
            }
            PoolError::SessionNotFound { .. } => (codes::SESSION_NOT_FOUND, None),
            PoolError::SessionBusy(busy) => (codes::SESSION_BUSY, Some(busy_data(busy))),
            PoolError::SceneNotFound { .. }
            | PoolError::SceneNotActive { .. }
            | PoolError::SceneNotRegistered { .. } => (codes::SCENE_NOT_FOUND, None),
            PoolError::SceneMismatch {
                session_id,
                recorded,
                requested,
            } => (
                codes::SCENE_MISMATCH,
                Some(json!({
                    "session_id": session_id,
                    "recorded_scene": recorded,
                    "requested_scene": requested,
                })),
            ),
            PoolError::SceneHasActiveSessions { .. } => (codes::SCENE_HAS_ACTIVE_SESSIONS, None),
            PoolError::ProjectNotFound { .. } => (codes::PROJECT_NOT_FOUND, None),
            PoolError::ProjectRequired { .. } => (codes::PROJECT_REQUIRED, None),
            PoolError::ProjectUntrusted(_) => (codes::PROJECT_UNTRUSTED, None),
            PoolError::SidechainTerminal {
                session_id,
                parent_session_id,
                final_state,
            } => (
                codes::SIDECHAIN_TERMINAL,
                Some(json!({
                    "session_id": session_id,
                    "parent_session_id": parent_session_id,
                    "final_state": final_state,
                })),
            ),
            PoolError::TaskNotFound { .. } => (codes::TASK_NOT_FOUND, None),
            PoolError::TaskLimit(_) => (codes::TASK_LIMIT, None),
            PoolError::SpawnUnsupported(_) => (codes::INVALID_REQUEST, None),
            PoolError::Engine(_) | PoolError::EngineStopped | PoolError::EngineUnreachable(_) => {
                (codes::ENGINE_ERROR, None)
            }
            PoolError::McpServerDenied(_) => (codes::MCP_SERVER_DENIED, None),
            PoolError::McpAuthFailed(_) => (codes::MCP_AUTH_FAILED, None),
            PoolError::PluginsDisabled { .. } => (codes::PLUGINS_DISABLED, None),
            PoolError::ModuleNotCompiled { module, .. } => {
                (codes::METHOD_NOT_FOUND, Some(json!({ "module": module })))
            }
            PoolError::Internal(_) => (codes::INTERNAL_ERROR, None),
        };
        RpcError {
            code,
            message,
            data,
        }
    }
}

fn busy_data(busy: &Busy) -> Value {
    let mut data = json!({
        "session_id": busy.session_id,
        "reason": busy.reason,
        "current_turn_id": busy.current_turn_id,
    });
    if !busy.running_task_ids.is_empty() {
        data["running_task_ids"] = json!(busy.running_task_ids);
    }
    data
}

/// The response to request `id` that failed with `e`.
pub(crate) fn failure(id: Value, e: PoolError) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(e.into()),
    }
}

/// `result` as the response to `id`, or its failure.
pub(crate) fn respond(id: Value, result: Result<Value, PoolError>) -> RpcResponse {
    match result {
        Ok(result) => RpcResponse::ok(id, result),
        Err(e) => failure(id, e),
    }
}
