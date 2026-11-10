//! Agix foundation — types, traits, and context shared by all crates.

pub mod agent_type;
pub mod error;
pub mod event;
pub mod id;
pub mod managed_settings;
pub mod memory;
pub mod message;
pub mod permission;
pub mod rules;
pub mod session;
pub mod settings;
pub mod settings_file;
pub use interface::tool;
pub mod context;
pub mod dir_lock;
pub mod frozen;
pub mod interface;
pub mod path;
pub mod paths;
pub mod project_trust;
pub mod prompt;
pub mod provider;
pub mod public_suffix;
pub mod summary;
pub mod text;
pub mod url_rule;
