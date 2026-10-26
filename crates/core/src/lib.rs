//! Agix foundation — types, traits, and context shared by all crates.

pub mod error;
pub mod event;
pub mod id;
pub mod memory;
pub mod message;
pub mod permission;
pub mod result;
pub mod rules;
pub mod session;
pub mod settings;
pub use interface::tool;
// `memory` above is the durable memory store. A second, JSON-backed
// `DurableMemoryStore` used to sit alongside it with no production wiring — it
// was deleted rather than kept as a parallel format.
pub mod context;
pub mod features;
pub mod frozen;
pub mod intent;
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
