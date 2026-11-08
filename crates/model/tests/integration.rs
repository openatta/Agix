//! The crate's integration tests, compiled into one test binary; each module
//! is one file of this directory. One file alone:
//! `cargo test -p model --test integration backoff_parity::`.

mod backoff_parity;
mod model_endpoint_egress;
