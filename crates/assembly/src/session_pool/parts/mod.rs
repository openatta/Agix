//! 会话池的协作者。每个协作者持有池状态的一部分，字段只在自己的模块里可见，
//! 门面（`facade/`）经它们的方法编排。

pub(super) mod hub;
pub(super) mod phase;
pub(super) mod scenes;
pub(super) mod scheduler;
pub(super) mod specs;
