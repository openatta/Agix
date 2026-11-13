//! Permission engine — rule matching, mode dispatch, path safety.

pub mod dangerous;
pub mod error;
pub mod invocation;
pub mod path_rule;
pub mod path_safety;
pub mod policy;
pub mod rule;
pub mod ruleset;
pub mod settings_patch;
pub mod shadow;
pub mod shell;
