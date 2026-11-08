//! Agix — agent runtime: turn loop, streaming, tool dispatch, agent lifecycle.

pub mod agent;
pub mod commands;
pub mod context;
pub mod elicitation;
pub mod event_bus;
pub mod hook_executors;
pub mod module;
pub mod plugin;
pub mod plugin_host;
mod repeat_guard;
pub mod steering;
pub mod streaming;
pub mod turn;

/// Whether this build carries `RunCode` (the `ptc` feature).
pub const PTC_ENABLED: bool = cfg!(feature = "ptc");
