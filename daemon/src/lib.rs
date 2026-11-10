//! `agixd`'s library half: the assembly re-exported under the paths the
//! daemon's tests and embedders have always used (`daemon::config`,
//! `daemon::SessionPool`, `daemon::DaemonServer`, …). The binary is `main.rs`.

pub mod logging;

#[cfg(feature = "otel")]
pub use assembly::telemetry_otel;
#[cfg(feature = "ws")]
pub use assembly::ws;
pub use assembly::{
    assemble, config, discovery, doctor, model_router, plugins, rpc, server, session_pool,
    watch_hub,
};

pub use assembly::{
    write_lock_file, AllowAllPermission, Assembly, DaemonServer, SessionCtx, SessionCustomizer,
    SessionPool, Transcripts,
};
