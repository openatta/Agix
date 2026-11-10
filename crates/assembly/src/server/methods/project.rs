//! `project.*`。

use super::*;

rpc_methods! {
    "project.status" => status(Untyped) -> Untyped;
    "project.trust" => trust(Untyped) -> Untyped;
}

async fn status(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_project_status(cx.id(), params).await
}

async fn trust(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_project_trust(cx.id(), params).await
}
