//! `import.*`。

use super::*;

rpc_methods! {
    "import.list" => list(Untyped) -> Untyped;
    "import.run" => run(Untyped) -> Untyped;
}

async fn list(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_import_list(cx.id(), params).await
}

async fn run(s: &DaemonServer, cx: &Call, Untyped(params): Untyped) -> RpcResponse {
    s.method_import_run(cx.id(), params).await
}
