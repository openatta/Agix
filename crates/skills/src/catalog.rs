//! [`SkillManager`] as the kernel's [`SkillCatalog`]: the turn loop, slash
//! commands and delegates see skills only through this.

use std::path::PathBuf;

use base::frozen::SkillEntry;
use base::interface::skill_catalog::SkillCatalog;

use crate::manager::{SkillInfo, SkillManager, SkillSource};

pub fn to_entry(skill: &SkillInfo) -> SkillEntry {
    SkillEntry {
        name: skill.name.clone(),
        description: skill.description.clone(),
        when_to_use: skill.when_to_use.clone(),
        source: match skill.source {
            SkillSource::User => base::frozen::SkillSource::User,
            SkillSource::Project => base::frozen::SkillSource::Project,
            SkillSource::Plugin => base::frozen::SkillSource::Plugin,
        },
        path: skill.path.clone(),
        argument_hint: skill.argument_hint.clone(),
        allowed_tools: skill.allowed_tools.clone(),
        disallowed_tools: skill.disallowed_tools.clone(),
        arguments: skill.arguments.clone(),
        model: skill.model.clone(),
        effort: skill.effort.clone(),
        context: skill.context.clone(),
        agent: skill.agent.clone(),
        background: skill.background,
        disable_model_invocation: skill.disable_model_invocation,
        user_invocable: skill.user_invocable,
        paths: skill.paths.clone(),
        version: skill.version.clone(),
        // `SkillInfo` has no counterpart for these two — they are dropped on
        // load, so the catalog cannot give them back. Fields listed
        // exhaustively (no `..Default::default()`) so that adding one to
        // `SkillEntry` fails the build here instead of silently arriving as
        // `None`, which is how `arguments` once went missing and disabled
        // named-argument expansion for every slash command.
        files: None,
        hooks: None,
    }
}

impl SkillCatalog for SkillManager {
    fn list(&self) -> Vec<SkillEntry> {
        SkillManager::list(self).iter().map(to_entry).collect()
    }
    fn get(&self, name: &str) -> Option<SkillEntry> {
        SkillManager::get(self, name).as_ref().map(to_entry)
    }
    fn body(&self, name: &str) -> Option<String> {
        self.get_skill_content(name)
    }
    fn reload_changed(&self) -> usize {
        self.check_for_changes()
    }
    fn discover_for_paths(&self, paths: &[PathBuf]) -> Vec<SkillEntry> {
        SkillManager::discover_for_paths(self, paths)
            .iter()
            .map(to_entry)
            .collect()
    }
    fn activate_for_paths(&self, files: &[PathBuf]) -> Vec<SkillEntry> {
        self.activate_conditional_skills_for_paths(files)
            .iter()
            .map(to_entry)
            .collect()
    }
    fn invocation_count(&self, name: &str) -> u32 {
        SkillManager::invocation_count(self, name)
    }
    fn last_invoked_seq(&self, name: &str) -> u64 {
        SkillManager::last_invoked_seq(self, name)
    }
}
