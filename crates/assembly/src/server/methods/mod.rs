//! 各命名空间的方法表与处理函数，一个命名空间一个文件。处理函数的主体多数还在
//! `methods_*.rs` 的 `impl DaemonServer` 里，这里的函数把请求交过去。

use super::table::Call;
use super::*;
use crate::rpc::{NoParams, Untyped};
use crate::session_pool::api::PoolError;
use serde_json::{json, Value};

pub(super) mod config;
pub(super) mod daemon;
pub(super) mod goal;
pub(super) mod history;
pub(super) mod import;
pub(super) mod mcp;
pub(super) mod plugin;
pub(super) mod project;
pub(super) mod scene;
pub(super) mod schedule;
pub(super) mod session;
pub(super) mod tasks;
pub(super) mod turn;
