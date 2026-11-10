//! The daemon's integration tests, compiled into one test binary; each module
//! is one file of this directory. One file alone:
//! `cargo test -p daemon --test integration session_lifecycle::`.

mod assemble_defaults;
mod bash_read_only_lane;
mod carrier_invariants;
mod daemon_e2e;
mod daemon_socket_from_a_command;
mod dependency_direction;
mod extension_entries;
mod extension_points_doc;
mod home_is_never_discovered;
mod hook_event_wiring;
mod hook_events_doc;
mod host_scenes;
mod mcp_oauth;
mod mcp_policy;
mod one_assembly;
mod project_trust;
mod protocol_doc_examples;
mod protocol_doc_matches_dispatch;
mod readme_matches_the_code;
mod resume_recorder;
mod rpc_smoke;
mod schedule;
mod session_context;
mod session_customizer;
mod session_event_compact;
mod session_export;
mod session_goal;
mod session_lifecycle;
mod session_provider;
mod session_set_model;
mod session_tasks;
mod two_pools_one_process;
mod wiring_coverage;
mod ws_transport;
