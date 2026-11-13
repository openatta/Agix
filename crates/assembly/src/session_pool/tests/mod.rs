//! `SessionPool` 的单元测试，按主题分文件。

use super::builder::SessionPoolBuilder;
use super::*;

/// A watcher that takes every frame and keeps none.
pub(super) struct NullWatcher(pub(super) u64);

impl SessionWatcher for NullWatcher {
    fn id(&self) -> WatcherId {
        WatcherId(self.0)
    }

    fn deliver(&self, _frame: &SessionFrame) -> bool {
        true
    }
}

/// An ask of `session_id` as the engine would raise it.
pub(super) fn raise_ask(
    hub: &SessionHub,
    session_id: &str,
    prompt_id: &str,
    task_id: Option<&str>,
) {
    hub.publish_ask(
        Arc::new(frame_between_turns(session_id, SessionEvent::EngineStopped)),
        prompt_id.to_string(),
        task_id.map(str::to_string),
        AskKind::Permission,
    );
}

mod audit3;
mod auto_name;
mod eviction;
mod goal_record;
mod mcp_for_project;
/// Multiple scenes coexisting in one daemon process.
mod multi_scene;
mod pool_telemetry;
mod rebuild;
mod recorded_spec;
mod redact;
mod resolve_session_permission;
mod script_binding;
/// `session.list`'s historical half is a `HistoryStore` query, not a walk the
/// pool does itself.
mod session_listing;
mod set_model;
mod with_session_lock;
