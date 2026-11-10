//! The architecture guards, compiled into one test binary that reads the
//! workspace once; each module is one family of guards. One family alone:
//! `cargo test -p guards --test integration process_state::`.

mod api_surface;
mod base;
mod delegation;
mod deps;
mod layering;
mod process_state;
mod size;
mod sources;
mod support_selftest;
mod unused_deps;
mod wiring;
