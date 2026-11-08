//! Multi-agent team coordination.
//!
//! - [`coordinator::Coordinator`] — orchestrates sub-agent workflows
//! - [`tool::TeamTool`] — exposes team orchestration as a tool
//! - [`mailbox`] — inter-agent message passing
//! - [`registry`] — shared team state (`TeamList`/`TeamDelete` see what `TeamCreate` made)
//! - [`prompt`] — coordinator system prompt
//! - [`persist`] — on-disk team.json/state.json/events.jsonl
//! - [`lock`] — `<project>/.agix/teams/.lock`

pub mod coordinator;
pub mod lock;
pub mod mailbox;
pub mod persist;
pub mod prompt;
pub mod registry;
pub mod team_memory;
pub mod tool;
