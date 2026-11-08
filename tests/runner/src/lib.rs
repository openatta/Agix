//! The harness the integration tests share.
//!
//! A library and no binary: what `tests/*.rs` builds sessions with — a
//! scripted model, a session driver, the fixture plumbing.

pub mod api_runner;
pub mod fixture;
pub mod mutations;
pub mod plugin_fixture;
pub mod script;
pub mod script_session;
pub mod scripted_model;
