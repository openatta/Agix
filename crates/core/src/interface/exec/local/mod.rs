//! This machine's own answers to the four contracts.
//!
//! They live beside the contracts rather than in a crate of their own because
//! `ToolContext` has to be able to build a default set, and `ToolContext` is
//! defined here — a separate crate would have to be a dependency of the crate
//! that defines the thing it fills in.

pub mod env;
pub mod filesystem;
pub mod network;
pub mod process;
pub mod sandbox;

pub use env::{SubprocessEnv, SubprocessEnvMode};
pub use filesystem::LocalFileSystem;
pub use network::LocalNetwork;
pub use process::{GroupLedger, LocalProcess};
pub use sandbox::PlatformSandbox;
