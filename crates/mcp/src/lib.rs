//! MCP (Model Context Protocol) client — server connection, tool adaptation,
//! and the OAuth authorization remote servers ask for.

pub mod adapter;
pub mod client;
pub mod config;
pub mod connect;
pub mod error;
pub mod manager;
pub mod module;
pub mod naming;
pub mod notify;
pub mod oauth;
pub mod output_cache;
pub mod policy;
pub mod prompts;
pub mod resources;
pub mod tools;

pub use module::McpModule;
