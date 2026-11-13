//! 方法表：每个命名空间在 `methods/<ns>.rs` 里用一次 `rpc_methods!`，展开成元数据
//! 表 `METHODS` 和分派函数；这里把它们串成一张表。分派、`rpc.schema.json`、协议
//! 文档对账都从 [`methods`] 派生。`daemon.auth` 是连接握手（协议 §5.1），不在表里。

use super::DaemonServer;
use crate::rpc::{codes, Client, MethodInfo, RpcError, RpcRequest, RpcResponse, Untyped};
use crate::session_pool::api::PoolError;
use serde_json::Value;
use std::sync::Arc;

/// One namespace's table. Each entry is
/// `"ns.method" => handler(Params) -> Result;`, optionally followed by
/// `[module, compiled]` for a method of an optional module; attributes before
/// an entry (a `#[cfg]`) apply to its dispatch arm only, so the method stays
/// listed in a build without the module.
///
/// A handler is `async fn(&DaemonServer, &Call, Params) -> R` where `R` is a
/// [`Reply`] of `Result`.
macro_rules! rpc_methods {
    (@module) => { None };
    (@module $module:literal) => { Some($module) };
    (@compiled) => { true };
    (@compiled $compiled:expr) => { $compiled };
    ($(
        $(#[$attr:meta])*
        $name:literal => $handler:ident($params:ty) -> $result:ty $([$module:literal, $compiled:expr])?;
    )*) => {
        pub(in crate::server) const METHODS: &[crate::rpc::MethodInfo] = &[$(
            crate::rpc::MethodInfo {
                name: $name,
                module: rpc_methods!(@module $($module)?),
                compiled: rpc_methods!(@compiled $($compiled)?),
                params: crate::rpc::schema_of::<$params>,
                result: crate::rpc::schema_of::<$result>,
            },
        )*];

        #[allow(unused_variables)]
        pub(in crate::server) async fn dispatch(
            server: &crate::server::DaemonServer,
            cx: &crate::server::table::Call,
            params: serde_json::Value,
        ) -> Option<Result<serde_json::Value, crate::rpc::RpcError>> {
            match cx.method {
                $(
                    $(#[$attr])*
                    $name => Some(async {
                        let params: $params = crate::server::table::parse_params(cx.method, params)?;
                        let result: $result = crate::server::table::Reply::<$result>::into_reply(
                            $handler(server, cx, params).await,
                        )?;
                        crate::server::table::to_result(result)
                    }.await),
                )*
                _ => None,
            }
        }
    };
}

/// One request as a handler sees it.
pub(super) struct Call {
    pub id: Value,
    pub client: Arc<Client>,
    pub method: &'static str,
}

/// What a handler returns, as the result its table entry declares.
pub(super) trait Reply<R> {
    fn into_reply(self) -> Result<R, RpcError>;
}

impl Reply<Untyped> for RpcResponse {
    fn into_reply(self) -> Result<Untyped, RpcError> {
        match (self.error, self.result) {
            (Some(error), _) => Err(error),
            (None, result) => Ok(Untyped(result.unwrap_or(Value::Null))),
        }
    }
}

impl<R> Reply<R> for Result<R, PoolError> {
    fn into_reply(self) -> Result<R, RpcError> {
        self.map_err(RpcError::from)
    }
}

impl<R> Reply<R> for Result<R, RpcError> {
    fn into_reply(self) -> Result<R, RpcError> {
        self
    }
}

pub(super) fn parse_params<P: serde::de::DeserializeOwned>(
    method: &str,
    params: Value,
) -> Result<P, RpcError> {
    let params = match params {
        Value::Null => Value::Object(Default::default()),
        other => other,
    };
    serde_json::from_value(params).map_err(|e| RpcError {
        code: codes::INVALID_PARAMS,
        message: format!("invalid params for {method}: {e}"),
        data: None,
    })
}

pub(super) fn to_result<R: serde::Serialize>(result: R) -> Result<Value, RpcError> {
    serde_json::to_value(result).map_err(|e| RpcError {
        code: codes::INTERNAL_ERROR,
        message: format!("could not serialize the result: {e}"),
        data: None,
    })
}

macro_rules! namespaces {
    ($($ns:ident),* $(,)?) => {
        /// Every method the daemon answers, namespace by namespace.
        pub fn methods() -> impl Iterator<Item = &'static MethodInfo> {
            [$(super::methods::$ns::METHODS),*].into_iter().flatten()
        }

        async fn route(
            server: &DaemonServer,
            cx: &Call,
            params: Value,
        ) -> Option<Result<Value, RpcError>> {
            $(
                if super::methods::$ns::METHODS.iter().any(|m| m.name == cx.method) {
                    return super::methods::$ns::dispatch(server, cx, params).await;
                }
            )*
            None
        }
    };
}

namespaces!(
    daemon, config, project, import, scene, mcp, plugin, schedule, session, history, goal, turn,
    tasks,
);

impl DaemonServer {
    pub(super) async fn dispatch(&self, req: RpcRequest, client: Arc<Client>) -> RpcResponse {
        let id = req.id.unwrap_or(Value::Null);
        let Some(info) = methods().find(|m| m.name == req.method) else {
            return RpcResponse::err(
                id,
                codes::METHOD_NOT_FOUND,
                format!("unknown: {}", req.method),
            );
        };
        if !info.compiled {
            return super::errors::failure(
                id,
                PoolError::ModuleNotCompiled {
                    method: req.method,
                    module: info.module.unwrap_or_default(),
                },
            );
        }
        let cx = Call {
            id,
            client,
            method: info.name,
        };
        let reply = route(self, &cx, req.params).await.unwrap_or_else(|| {
            Err(RpcError {
                code: codes::METHOD_NOT_FOUND,
                message: format!("unknown: {}", cx.method),
                data: None,
            })
        });
        match reply {
            Ok(result) => RpcResponse::ok(cx.id, result),
            Err(error) => RpcResponse {
                jsonrpc: "2.0",
                id: cx.id,
                result: None,
                error: Some(error),
            },
        }
    }
}
