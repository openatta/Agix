//! 会话目标：`session.goal` / `setGoal` / `updateGoal`。

use super::*;

rpc_methods! {
    "session.goal" => goal(Untyped) -> Untyped;
    "session.setGoal" => change_goal(Untyped) -> Untyped;
    "session.updateGoal" => change_goal(Untyped) -> Untyped;
}

async fn goal(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_goal(cx.id(), params).await
}

async fn change_goal(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_change_goal(cx.id(), cx.method, params)
        .await
}
