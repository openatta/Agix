//! Agix — plugin manifest loader, marketplace integration, dependency
//! resolution, versioned cache, and the plugin lifecycle CLI commands.

pub mod cache;
pub mod cli;
pub mod disclosure;
pub mod discovery;
pub mod fetch;
pub mod homograph;
pub mod locate;
pub mod manifest;
pub mod marketplace;
pub mod mcp;
pub mod resolver;
pub mod state;

pub use disclosure::Disclosure;
pub use discovery::discover_plugins;
pub use manifest::{Plugin, PluginError, PluginManifest, ScriptPayload};
