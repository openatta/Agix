//! The crate's integration tests, compiled into one test binary; each module
//! is one file of this directory. One file alone:
//! `cargo test -p tools --test integration path_style::`.

mod edits_after_writes;
mod execution_providers;
mod outside_the_project;
mod path_style;
mod permission_rules;
mod search_at_scale;
mod structured_output;
mod write_path_policy;
