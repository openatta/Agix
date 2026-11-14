//! `SkillCatalog` — the skills a session can list, invoke and be reminded of.
//!
//! The turn loop asks this, not the skills module, which skills exist, what
//! one expands to, and how often each has been used. A session assembled
//! without skills answers from [`NoSkills`].

use std::path::PathBuf;

use futures::future::BoxFuture;

use crate::frozen::SkillEntry;
use crate::interface::permission::PermissionPolicy;
use crate::interface::tool::ToolContext;

/// One invocation's worth of a skill: the text the conversation receives,
/// and the `(tool, pattern)` grants its `allowed-tools` makes for the rest of
/// the turn — already narrowed to what its source may grant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillExpansion {
    pub text: String,
    pub grants: Vec<(String, Option<String>)>,
}

pub trait SkillCatalog: Send + Sync {
    /// Sorted by name, so the listing a prompt carries is stable.
    fn list(&self) -> Vec<SkillEntry>;
    fn get(&self, name: &str) -> Option<SkillEntry>;
    /// The body a skill expands to when invoked.
    fn body(&self, name: &str) -> Option<String>;
    /// Expand `name` for an invocation with `args`, running what its body
    /// asks to run under `ctx` and `permission`. `None` when the catalog does
    /// not hold it; `Err` is a message for the model, such as a bundled file
    /// that escapes the skill's folder.
    fn expand<'a>(
        &'a self,
        name: &'a str,
        args: &'a str,
        ctx: &'a ToolContext,
        permission: &'a dyn PermissionPolicy,
    ) -> BoxFuture<'a, Option<Result<SkillExpansion, String>>> {
        let _ = permission;
        Box::pin(async move {
            let entry = self.get(name)?;
            let body = self.body(name)?;
            let text = crate::frozen::skill::expand_skill_vars_named(
                &body,
                args,
                entry.arguments.as_deref().unwrap_or(&[]),
                Some(&crate::frozen::skill::InvocationVars {
                    skill_md: &entry.path,
                    session_id: Some(&ctx.session_id),
                }),
            );
            Some(Ok(SkillExpansion {
                text,
                grants: Vec::new(),
            }))
        })
    }
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
