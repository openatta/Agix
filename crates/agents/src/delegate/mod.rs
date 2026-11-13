//! Delegation: what a session's delegates are built from, captured once when
//! the session is assembled, and the limits, tools and members they share.

mod admission;
mod env;
mod members;
mod tools;

pub(crate) use admission::{Admission, Limits};
pub use admission::{SubagentSlots, DEFAULT_MAX_CONCURRENT_SUBAGENTS};
pub(crate) use env::*;
pub(crate) use members::*;
pub(crate) use tools::*;
