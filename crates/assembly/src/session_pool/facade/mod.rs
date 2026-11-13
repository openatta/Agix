//! `impl SessionPool` 的各个部分：池对外的方法，按主题分文件。门面编排协作者
//! （`parts/`），协作者之间的配合都在这里发生。

mod background;
mod create;
mod goal;
pub(super) mod helpers;
pub(super) mod history_rpc;
mod listing;
mod plugins;
mod resume;
mod session_ops;
pub(super) mod settings_reload;
mod subscribers;
mod tasks;
mod teardown;
mod turn;
