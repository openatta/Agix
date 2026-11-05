//! `SkillCatalog` — the skills a session can list, invoke and be reminded of.
//!
//! The turn loop asks this, not the skills module, which skills exist, what
//! one expands to, and how often each has been used. A session assembled
//! without skills answers from [`NoSkills`].

use std::path::PathBuf;

use crate::frozen::SkillEntry;

pub trait SkillCatalog: Send + Sync {
    /// Sorted by name, so the listing a prompt carries is stable.
    fn list(&self) -> Vec<SkillEntry>;
    fn get(&self, name: &str) -> Option<SkillEntry>;
    /// The body a skill expands to when invoked.
    fn body(&self, name: &str) -> Option<String>;
    /// Reloads skills whose files changed since the last call; how many did.
    fn reload_changed(&self) -> usize;
    /// Skills found under `paths` that the catalog did not hold yet.
    fn discover_for_paths(&self, paths: &[PathBuf]) -> Vec<SkillEntry>;
    /// Conditional skills whose `paths` patterns match one of `files`, newly
    /// made available.
    fn activate_for_paths(&self, files: &[PathBuf]) -> Vec<SkillEntry>;
    fn invocation_count(&self, name: &str) -> u32;
    /// A sequence number that orders invocations; 0 for one never invoked.
    fn last_invoked_seq(&self, name: &str) -> u64;
}

pub struct NoSkills;

impl SkillCatalog for NoSkills {
    fn list(&self) -> Vec<SkillEntry> {
        Vec::new()
    }
    fn get(&self, _: &str) -> Option<SkillEntry> {
        None
    }
    fn body(&self, _: &str) -> Option<String> {
        None
    }
    fn reload_changed(&self) -> usize {
        0
    }
    fn discover_for_paths(&self, _: &[PathBuf]) -> Vec<SkillEntry> {
        Vec::new()
    }
    fn activate_for_paths(&self, _: &[PathBuf]) -> Vec<SkillEntry> {
        Vec::new()
    }
    fn invocation_count(&self, _: &str) -> u32 {
        0
    }
    fn last_invoked_seq(&self, _: &str) -> u64 {
        0
    }
}
