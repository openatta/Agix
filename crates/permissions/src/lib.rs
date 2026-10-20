//! Permission engine — rule matching, mode dispatch, path safety.

pub mod dangerous;
pub mod error;
pub mod gate;
pub mod llm_classifier;
pub mod path_rule;
pub mod path_safety;
pub mod rule;
pub mod rule_set_permission;
pub mod ruleset;
pub mod settings_patch;
pub mod shadow;
pub mod shell;
pub mod yolo;
