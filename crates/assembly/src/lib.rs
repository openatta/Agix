//! The assembly: how a host turns settings, modules and a working directory
//! into sessions — the session pool, provider routing, plugin packages,
//! configuration loading — and the JSON-RPC server over it (Unix socket, TCP,
//! WebSocket) with instance discovery. `agixd` is a thin binary over this; an
//! embedder that wants sessions without a socket uses the pool alone, and the
//! server code is not linked in.
//!
//! The server lives here rather than in the binary's crate because it and the
//! pool share most of their generic instantiations (serde_json, tokio, rmcp);
//! split across two crates, each compiled its own copies, about 2.8 MB of a
//! release `agixd`.

pub mod assemble;
pub mod config;
pub mod discovery;
pub mod doctor;
pub mod doctor_posture;
pub mod export;
pub mod model_router;
pub mod modules;
pub mod plugins;
pub mod rpc;
pub mod server;
pub mod session_pool;
#[cfg(feature = "otel")]
pub mod telemetry_otel;
pub mod watch_hub;
#[cfg(feature = "ws")]
pub mod ws;

pub use assemble::{AllowAllPermission, Assembly, Transcripts};
pub use discovery::write_lock_file;
pub use server::{DaemonServer, UnixBinding};
pub use session_pool::{SessionCtx, SessionCustomizer, SessionPool};
