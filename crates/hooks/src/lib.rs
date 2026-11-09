//! Lifecycle hook runner — event callbacks (Command / Prompt / HTTP / Agent).
//!
//! 30 event types × 4 hook backends. No hooks registered = zero overhead (noop runner).

pub mod catalog;
pub mod config;
pub mod payload;
pub mod runner;
pub mod watcher;

pub use config::{
    parse_hooks_settings, HookConfig, HookEvent, HooksParseReport, HooksSettings, UNWIRED_EVENTS,
};
pub use payload::{HookDecision, HookInput, HookResponse};
pub use runner::HookRunner;
