//! The live tier — the `live_*` cases, against a real model behind a
//! recording proxy — compiled into one test binary behind the `real-model`
//! feature. One file alone:
//! `cargo test -p live --features real-model --test real_model live_basics::`.

mod live_agents;
mod live_autonomy;
mod live_basics;
mod live_commands;
mod live_context;
mod live_extensions;
mod live_hooks;
mod live_host;
mod live_mcp;
mod live_mcp_oauth;
mod live_memory;
mod live_multi_agent;
mod live_permissions;
mod live_plugins;
mod live_programs;
mod live_providers;
mod live_scenes;
mod live_sessions;
mod live_team;
mod live_transport;
mod live_unattended;
mod live_web;
mod live_workspace;
