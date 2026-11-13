//! 会话的后台任务：`session.tasks` / `taskOutput` / `stopTask` / `messageTask` /
//! `startAgent`（属于 `agents` 模块）。

use super::*;
use crate::rpc::methods::session::SessionRef;
use crate::rpc::methods::tasks::*;
use crate::rpc::{RpcError, StartAgentParams};
use crate::session_pool::api::{
    AgentStarted, MessageDelivery, PoolError, SessionTasks, TaskOutput,
};

rpc_methods! {
    "session.tasks" => tasks(SessionRef) -> SessionTasks;
    "session.taskOutput" => task_output(TaskOutputParams) -> TaskOutput;
    "session.stopTask" => stop_task(TaskRef) -> Stopped;
    "session.messageTask" => message_task(MessageTaskParams) -> MessageDelivery;
    #[cfg(feature = "agents")]
    "session.startAgent" => start_agent(StartAgentParams) -> AgentStarted ["agents", cfg!(feature = "agents")];
}

fn invalid(message: impl Into<String>) -> RpcError {
    PoolError::InvalidRequest(message.into()).into()
}

async fn tasks(s: &DaemonServer, _: &Call, p: SessionRef) -> Result<SessionTasks, PoolError> {
    s.pool.session_tasks(&p.session_id).await
}

fn output_read(p: &TaskOutputParams) -> Result<TaskOutputRead, RpcError> {
    match (p.tail_bytes, p.from_byte) {
        (Some(_), Some(_)) => Err(invalid(
            "session.taskOutput takes tail_bytes or from_byte, not both",
        )),
        (None, None) => Ok(TaskOutputRead::Tail(DEFAULT_TASK_OUTPUT_TAIL)),
        (Some(0), None) => Err(invalid("tail_bytes must be a positive integer")),
        (Some(n), None) => Ok(TaskOutputRead::Tail(
            usize::try_from(n)
                .unwrap_or(usize::MAX)
                .min(RunningTask::OUTPUT_KEPT_BYTES),
        )),
        (None, Some(from)) => Ok(TaskOutputRead::From(from)),
    }
}

async fn task_output(
    s: &DaemonServer,
    _: &Call,
    p: TaskOutputParams,
) -> Result<TaskOutput, RpcError> {
    let read = output_read(&p)?;
    Ok(s.pool.task_output(&p.session_id, &p.task_id, read).await?)
}

async fn stop_task(s: &DaemonServer, _: &Call, p: TaskRef) -> Result<Stopped, PoolError> {
    Ok(Stopped {
        stopped: s.pool.stop_task(&p.session_id, &p.task_id).await,
    })
}

async fn message_task(
    s: &DaemonServer,
    _: &Call,
    p: MessageTaskParams,
) -> Result<MessageDelivery, PoolError> {
    s.pool
        .message_task(&p.session_id, &p.task_id, p.message)
        .await
}

#[cfg(feature = "agents")]
async fn start_agent(
    s: &DaemonServer,
    _: &Call,
    p: StartAgentParams,
) -> Result<AgentStarted, RpcError> {
    if p.prompt.trim().is_empty() {
        return Err(invalid("session.startAgent needs a prompt"));
    }
    if p.provider.is_some() {
        return Err(invalid(
            "session.startAgent: a sub-agent runs on its parent's provider; give `model` only",
        ));
    }
    let session_id = p.session_id.clone();
    Ok(s.pool.start_agent(&session_id, p.into_request()).await?)
}
