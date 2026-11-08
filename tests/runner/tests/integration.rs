//! The runner's integration tests, compiled into one test binary; each module
//! is one file of this directory, and `support` is shared by them. One file
//! alone: `cargo test -p test-runner --test integration turn_behavior_net::`.

mod support;

mod deterministic_log;
mod fixture_smoke;
mod mcp_toy_server_smoke;
mod plugin_lifecycle_smoke;
mod rust_and_quickjs_layering;
mod script_boundaries;
mod script_carrier;
mod script_task_profiles;
mod session_continuity;
mod turn_accounting;
mod turn_behavior_net;
