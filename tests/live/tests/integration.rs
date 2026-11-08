//! The offline cases of `tests/live` — the assembled product over JSON-RPC
//! against a model stub — compiled into one test binary; each module is one
//! file of this directory. One file alone:
//! `cargo test -p live --test integration daemon_surface::`.

mod background_agents;
mod crash_recovery;
mod daemon_surface;
mod four_project_pipeline;
mod host_agents;
mod minimal_tier;
mod session_prompt;
mod turn_throughput;
