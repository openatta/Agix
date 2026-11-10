//! 跑一轮与上下文：`session.run_turn` / `compact` / `clear` / `respondToPrompt` /
//! `inspectContext` / `dropMessages` / `editMessage`。

use super::*;

rpc_methods! {
    "session.run_turn" => run_turn(Untyped) -> Untyped;
    "session.compact" => compact(Untyped) -> Untyped;
    "session.clear" => clear(Untyped) -> Untyped;
    "session.respondToPrompt" => respond_to_prompt(Untyped) -> Untyped;
    "session.inspectContext" => context(Untyped) -> Untyped;
    "session.dropMessages" => context(Untyped) -> Untyped;
    "session.editMessage" => context(Untyped) -> Untyped;
}

async fn run_turn(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_run_turn(cx.id(), params, cx.client.clone())
        .await
}

async fn compact(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_command(cx.id(), params, cx.client.clone(), "/compact")
        .await
}

async fn clear(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_command(cx.id(), params, cx.client.clone(), "/clear")
        .await
}

async fn respond_to_prompt(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_session_respond_to_prompt(cx.id(), params).await
}

async fn context(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_context(cx.id(), cx.method, &params).await
}
