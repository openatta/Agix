//! 各命名空间的方法表与类型化的处理函数，一个命名空间一个文件。

use super::table::Call;
use super::*;
use crate::rpc::NoParams;
use crate::session_pool::api::PoolError;

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
