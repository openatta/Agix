//! AttaCore — agent runtime: turn loop, streaming, tool dispatch, agent lifecycle.

pub mod agent;
pub mod agent_spawner_impl;
pub mod agent_tool;
mod agent_type_watcher;
pub mod commands;
pub mod context;
pub mod elicitation;
pub mod event_bus;
pub mod hook_executors;
pub mod plugin_host;
pub mod steering;
pub mod streaming;
pub mod turn;
