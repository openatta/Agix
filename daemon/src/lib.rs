//! `agixd`'s library half: the JSON-RPC server (Unix socket, TCP,
//! WebSocket) and instance discovery, over the session pool `assembly`
//! builds. The assembly's modules are re-exported under their old paths.

#[cfg(feature = "otel")]
pub use assembly::telemetry_otel;
pub use assembly::{
    assemble, config, doctor, model_router, plugins, rpc, session_manager, session_pool, watch_hub,
};

pub mod discovery;
pub mod server;
#[cfg(feature = "ws")]
pub mod ws;

pub use assembly::{
    AllowAllPermission, Assembly, SessionCtx, SessionCustomizer, SessionPool, Transcripts,
};
pub use discovery::write_lock_file;
pub use server::DaemonServer;
