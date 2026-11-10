//! This machine: `base`'s four execution contracts answered with the local
//! process table, filesystem, network and OS sandbox, plus the daemon's crash
//! hook and the identity a discovery file records for a running process.
//!
//! `base` defines the contracts and never names this crate; whatever runs
//! tools on a real machine depends on both.

pub mod crash;
pub mod filesystem;
pub mod lineage;
#[cfg(feature = "http")]
pub mod network;
pub mod orphans;
pub mod process;
pub mod process_identity;
mod providers;
pub mod sandbox;
#[cfg(all(unix, feature = "testing"))]
pub mod testing;

pub use base::interface::exec::env;

pub use env::{SubprocessEnv, SubprocessEnvMode};
pub use filesystem::LocalFileSystem;
#[cfg(feature = "http")]
pub use network::{install_tls_provider, LocalNetwork};
pub use process::{GroupLedger, LocalProcess};
pub use providers::{
    local_providers, local_providers_with, local_providers_with_ledger, tool_context_for_test,
    tool_context_from_engine_ctx,
};
pub use sandbox::PlatformSandbox;
