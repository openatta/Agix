//! The assembly: how a host turns settings, modules and a working directory
//! into sessions — the session pool, provider routing, plugin carriers,
//! configuration loading, and the JSON-RPC types the pool speaks to its
//! subscribers. `agixd` is one host of it; an embedder that wants sessions
//! without a socket is another.

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
     `--no-default-features --features plugins` (or `plugin-compile`)."
);

pub mod assemble;
pub mod config;
pub mod doctor;
pub mod model_router;
pub mod plugins;
pub mod rpc;
pub mod session_manager;
pub mod session_pool;
#[cfg(feature = "otel")]
pub mod telemetry_otel;
pub mod watch_hub;

pub use assemble::{AllowAllPermission, Assembly, Transcripts};
pub use session_pool::{SessionCtx, SessionCustomizer, SessionPool};
