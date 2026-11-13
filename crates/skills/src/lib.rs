//! Agix — skill manager and bundled skill definitions.

pub mod bundled;
mod bundled_files;
pub mod catalog;
pub mod manager;
pub mod mcp_builder;
pub mod module;
pub mod sources;
pub mod tool;
pub mod watcher;

pub use module::SkillsModule;
pub use tool::{expand_for_invocation, ExpandError};
