//! Agix foundation — types, traits, and context shared by all crates.

pub mod agent_type;
pub mod error;
pub mod event;
pub mod id;
pub mod memory;
pub mod message;
pub mod permission;
pub mod rules;
pub mod session;
pub mod settings;
pub use interface::tool;
pub mod context;
pub mod crash;
pub mod frozen;
pub mod interface;
pub use interface::log;
pub mod path;
pub mod paths;
pub mod process_identity;
pub mod prompt;
pub mod provider;
pub mod rpc;
pub mod summary;
pub mod text;
