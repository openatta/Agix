//! The client's integration tests, compiled into one test binary; each module
//! is one file of this directory. One file alone:
//! `cargo test -p rpc-client --test integration multi_client::`.

mod multi_client;
mod transport_parity;
