//! The assembly: how a host turns settings, modules and a working directory
//! into sessions — the session pool, provider routing, plugin carriers,
//! configuration loading — and the JSON-RPC server over it (Unix socket, TCP,
//! WebSocket) with instance discovery. `agixd` is a thin binary over this; an
//! embedder that wants sessions without a socket uses the pool alone, and the
//! server code is not linked in.
//!
//! The server lives here rather than in the binary's crate because it and the
//! pool share most of their generic instantiations (serde_json, tokio, rmcp);
//! split across two crates, each compiled its own copies, about 2.8 MB of a
//! release `agixd`.

// A build carries one extension carrier or none.
//
// Both at once is not a configuration anyone asked for: it is twice the
// attack surface and about twenty extra megabytes, and cargo's feature
// unification makes it something you arrive at by accident — `--features
// plugins` without `--no-default-features` is enough. Refusing here is what
// keeps the choice a choice.
#[cfg(all(feature = "scripts", feature = "plugins"))]
compile_error!(
    "the script carrier (`scripts`) and the plugin carrier (`plugins`) are \
     mutually exclusive. `scripts` is the default, so a plugin build needs \
     `--no-default-features --features full,plugins` (or `plugin-compile`)."
);

pub mod assemble;
pub mod config;
pub mod discovery;
pub mod doctor;
pub mod export;
pub mod model_router;
pub mod modules;
pub mod plugins;
pub mod rpc;
pub mod server;
pub mod session_manager;
pub mod session_pool;
#[cfg(feature = "otel")]
pub mod telemetry_otel;
pub mod watch_hub;
#[cfg(feature = "ws")]
pub mod ws;

pub use assemble::{AllowAllPermission, Assembly, Transcripts};
pub use discovery::write_lock_file;
pub use server::DaemonServer;
pub use session_pool::{SessionCtx, SessionCustomizer, SessionPool};
