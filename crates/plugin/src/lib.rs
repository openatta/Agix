//! Agix — plugin packages: the manifest format, finding installed packages,
//! the versioned cache, installing and enabling, what a package declares
//! (agent types, scenes, MCP servers), and the disclosure an installer shows.

pub mod agents;
pub mod blocklist;
pub mod cache;
pub mod disclosure;
pub mod discovery;
pub mod install;
pub mod locate;
pub mod manifest;
pub mod mcp;
pub mod scenes;
pub mod state;

pub use disclosure::Disclosure;
pub use discovery::discover_plugins;
pub use manifest::{Plugin, PluginError, PluginManifest, ScriptPayload};
