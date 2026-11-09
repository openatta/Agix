//! MCP (Model Context Protocol) client — server connection, tool adaptation.

pub mod adapter;
pub mod auth_tool;
pub mod client;
pub mod config;
pub mod connect;
pub mod error;
pub mod manager;
pub mod module;
pub mod notify;
pub mod oauth;
pub mod output_cache;
pub mod prompts;
pub mod resources;
pub mod tools;

pub use module::McpModule;
