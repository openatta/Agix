//! 会话的后台任务：`session.tasks` / `taskOutput` / `stopTask` / `messageTask` /
//! `startAgent`（属于 `agents` 模块）。

use super::*;
use crate::rpc::StartAgentParams;

rpc_methods! {
    "session.tasks" => tasks(Untyped) -> Untyped;
    "session.taskOutput" => task_output(Untyped) -> Untyped;
    "session.stopTask" => stop_task(Untyped) -> Untyped;
    "session.messageTask" => message_task(Untyped) -> Untyped;
    #[cfg(feature = "agents")]
    "session.startAgent" => start_agent(StartAgentParams) -> Untyped ["agents", cfg!(feature = "agents")];
}

async fn tasks(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
        return RpcResponse::err(cx.id(), codes::INVALID_PARAMS, "missing session_id");
    };
    if !s.pool.session_exists(session_id).await {
        return RpcResponse::err(
            cx.id(),
            codes::SESSION_NOT_FOUND,
            format!("no session `{session_id}`"),
        );
    }
    RpcResponse::ok(cx.id(), s.pool.session_tasks(session_id).await)
}

fn output_read(params: &Value) -> Result<TaskOutputRead, &'static str> {
    let given = |key: &str| params.get(key).filter(|v| !v.is_null());
    match (given("tail_bytes"), given("from_byte")) {
        (Some(_), Some(_)) => Err("session.taskOutput takes tail_bytes or from_byte, not both"),
        (None, None) => Ok(TaskOutputRead::Tail(DEFAULT_TASK_OUTPUT_TAIL)),
        (Some(v), None) => match v.as_u64() {
            Some(n) if n > 0 => Ok(TaskOutputRead::Tail(
                (n as usize).min(RunningTask::OUTPUT_KEPT_BYTES),
            )),
            _ => Err("tail_bytes must be a positive integer"),
        },
        (None, Some(v)) => v
            .as_u64()
            .map(TaskOutputRead::From)
            .ok_or("from_byte must be a non-negative integer"),
    }
}

async fn task_output(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let param = |key: &str| params.get(key).and_then(|v| v.as_str());
    let (Some(session_id), Some(task_id)) = (param("session_id"), param("task_id")) else {
        return RpcResponse::err(
            cx.id(),
            codes::INVALID_PARAMS,
            "session.taskOutput needs session_id and task_id",
        );
    };
    let read = match output_read(&params) {
        Ok(read) => read,
        Err(message) => return RpcResponse::err(cx.id(), codes::INVALID_PARAMS, message),
    };
    respond(cx.id(), s.pool.task_output(session_id, task_id, read).await)
}

async fn stop_task(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let param = |key: &str| params.get(key).and_then(|v| v.as_str());
    let (Some(session_id), Some(task_id)) = (param("session_id"), param("task_id")) else {
        return RpcResponse::err(
            cx.id(),
            codes::INVALID_PARAMS,
            "session.stopTask needs session_id and task_id",
        );
    };
    RpcResponse::ok(
        cx.id(),
        json!({"stopped": s.pool.stop_task(session_id, task_id).await}),
    )
}

async fn message_task(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    let param = |key: &str| params.get(key).and_then(|v| v.as_str());
    let (Some(session_id), Some(task_id), Some(message)) =
        (param("session_id"), param("task_id"), param("message"))
    else {
        return RpcResponse::err(
            cx.id(),
            codes::INVALID_PARAMS,
            "session.messageTask needs session_id, task_id and message",
        );
    };
    match s
        .pool
        .message_task(session_id, task_id, message.to_string())
        .await
    {
        None => RpcResponse::err(
            cx.id(),
            codes::SESSION_NOT_FOUND,
            format!("session `{session_id}` is not running"),
        ),
        Some(Ok(())) => RpcResponse::ok(cx.id(), json!({"delivered": true})),
        Some(Err(reason)) => {
            RpcResponse::ok(cx.id(), json!({"delivered": false, "reason": reason}))
        }
    }
}

#[cfg(feature = "agents")]
async fn start_agent(s: &DaemonServer, cx: &Call, params: StartAgentParams) -> RpcResponse {
    s.method_start_agent(cx.id(), params).await
}
