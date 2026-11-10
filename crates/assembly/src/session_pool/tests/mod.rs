//! `SessionPool` 的单元测试，按主题分文件。

use super::*;

mod auto_name;
mod event_golden;
mod eviction;
mod goal_record;
mod mcp_for_project;
/// Multiple scenes coexisting in one daemon process.
mod multi_scene;
mod pool_telemetry;
mod rebuild;
mod redact;
mod resolve_session_permission;
mod script_binding;
/// `session.list`'s historical half is a `HistoryStore` query, not a walk the
/// pool does itself.
mod session_listing;
mod set_model;
mod tool_progress_frame;
mod with_session_lock;
