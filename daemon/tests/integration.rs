//! The daemon's integration tests, compiled into one test binary; each module
//! is one file of this directory. One file alone:
//! `cargo test -p daemon --test integration session_lifecycle::`.

mod accept_edits_reads;
mod assemble_defaults;
mod audit2_doc_findings;
mod audit3_trust_boundaries;
mod audit_security_findings;
mod bash_read_only_lane;
mod daemon_e2e;
mod daemon_socket_from_a_command;
mod extension_entries;
mod extension_points_doc;
mod hook_events_doc;
mod host_scenes;
mod managed_settings;
mod mcp_oauth;
mod mcp_policy;
mod model_rate_limit;
mod permission_pipeline;
mod plugin_skills;
mod plugin_tools_in_scene;
mod project_trust;
mod protocol_doc_examples;
mod protocol_doc_matches_dispatch;
mod protocol_doc_wire;
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
mod tool_progress;
mod two_pools_one_process;
mod wiring_coverage;
mod ws_transport;
