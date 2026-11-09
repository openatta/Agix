//! Multi-agent team coordination.
//!
//! - [`coordinator::Coordinator`] — orchestrates sub-agent workflows
//! - [`tool::TeamTool`] — exposes team orchestration as a tool
//! - [`mailbox`] — inter-agent message passing
//! - [`registry`] — shared team state (`TeamList`/`TeamDelete` see what `TeamCreate` made)
//! - [`prompt`] — the team-context block a member's task opens with
//! - [`persist`] — on-disk team.json/state.json/events.jsonl

pub mod coordinator;
pub mod mailbox;
pub mod persist;
pub mod prompt;
pub mod registry;
pub mod team_memory;
pub mod tool;
