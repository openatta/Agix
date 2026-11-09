//! [`SkillManager`] as the kernel's [`SkillCatalog`]: the turn loop, slash
//! commands and delegates see skills only through this.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base::frozen::SkillEntry;
use base::interface::permission::Permission;
use base::interface::scene::Scene;
use base::interface::skill_catalog::{SkillCatalog, SkillExpansion};
use base::tool::ToolContext;
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

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
    fn expand<'a>(
        &'a self,
        name: &'a str,
        args: &'a str,
        ctx: &'a ToolContext,
        permission: &'a dyn Permission,
    ) -> BoxFuture<'a, Option<Result<SkillExpansion, String>>> {
        Box::pin(async move {
            let info = SkillManager::get(self, name)?;
            match crate::tool::expand_for_invocation(self, &info, args, ctx, Some(permission)).await
            {
                Ok(expansion) => {
                    self.record_invocation(name);
                    Some(Ok(expansion))
                }
                Err(crate::tool::ExpandError::NotFound) => None,
                Err(crate::tool::ExpandError::Rejected(msg)) => Some(Err(msg)),
            }
        })
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

/// Which built-in skills a session lists: the ones its scene names, less any
/// whose required tools the scene does not permit. Skills from disk and host
/// providers always pass, so a project's own `loop` still shows in a scene
/// that leaves the built-in one out.
#[derive(Clone, Default)]
pub struct BundledAdmission(Option<Arc<HashSet<String>>>);

impl BundledAdmission {
    pub fn for_scene(scene: &dyn Scene) -> Self {
        let names = scene
            .bundled_skills()
            .into_iter()
            .filter(|name| {
                crate::bundled::required_tools(name)
                    .iter()
                    .all(|tool| scene.permits_tool(tool))
            })
            .collect();
        Self(Some(Arc::new(names)))
    }

    pub fn admits(&self, name: &str, path: &Path) -> bool {
        match &self.0 {
            None => true,
            Some(names) => !path.to_string_lossy().starts_with("(bundled:") || names.contains(name),
        }
    }
}

/// A [`SkillManager`] as one session's scene sees it — see [`BundledAdmission`].
pub struct SceneSkills {
    manager: Arc<SkillManager>,
    admission: BundledAdmission,
}

impl SceneSkills {
    pub fn new(manager: Arc<SkillManager>, admission: BundledAdmission) -> Self {
        Self { manager, admission }
    }

    fn keep(&self, entries: Vec<SkillEntry>) -> Vec<SkillEntry> {
        entries
            .into_iter()
            .filter(|e| self.admission.admits(&e.name, &e.path))
            .collect()
    }
}

impl SkillCatalog for SceneSkills {
    fn list(&self) -> Vec<SkillEntry> {
        self.keep(SkillCatalog::list(&*self.manager))
    }
    fn get(&self, name: &str) -> Option<SkillEntry> {
        SkillCatalog::get(&*self.manager, name).filter(|e| self.admission.admits(&e.name, &e.path))
    }
    fn body(&self, name: &str) -> Option<String> {
        SkillCatalog::get(self, name)?;
        self.manager.get_skill_content(name)
    }
    fn expand<'a>(
        &'a self,
        name: &'a str,
        args: &'a str,
        ctx: &'a ToolContext,
        permission: &'a dyn Permission,
    ) -> BoxFuture<'a, Option<Result<SkillExpansion, String>>> {
        Box::pin(async move {
            SkillCatalog::get(self, name)?;
            self.manager.expand(name, args, ctx, permission).await
        })
    }
    fn reload_changed(&self) -> usize {
        self.manager.check_for_changes()
    }
    fn discover_for_paths(&self, paths: &[PathBuf]) -> Vec<SkillEntry> {
        self.keep(SkillCatalog::discover_for_paths(&*self.manager, paths))
    }
    fn activate_for_paths(&self, files: &[PathBuf]) -> Vec<SkillEntry> {
        self.keep(SkillCatalog::activate_for_paths(&*self.manager, files))
    }
    fn invocation_count(&self, name: &str) -> u32 {
        self.manager.invocation_count(name)
    }
    fn last_invoked_seq(&self, name: &str) -> u64 {
        self.manager.last_invoked_seq(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::prompt::PromptBlock;
    use base::interface::scene::{ScenePromptContext, TokenBudget};

    struct Picky(Vec<&'static str>, Vec<&'static str>);
    impl Scene for Picky {
        fn id(&self) -> &str {
            "picky"
        }
        fn name(&self) -> &str {
            "picky"
        }
        fn description(&self) -> &str {
            ""
        }
        fn build_system_prompt(&self, _: &ScenePromptContext) -> Vec<PromptBlock> {
            vec![]
        }
        fn tools(&self) -> Vec<String> {
            self.1.iter().map(|s| s.to_string()).collect()
        }
        fn token_budget(&self) -> TokenBudget {
            TokenBudget {
                compact_threshold: 1,
                compact_keep_recent: 1,
            }
        }
        fn bundled_skills(&self) -> Vec<String> {
            self.0.iter().map(|s| s.to_string()).collect()
        }
    }

    fn manager() -> Arc<SkillManager> {
        let mgr = SkillManager::new();
        for skill in crate::bundled::bundled_skills() {
            mgr.register_bundled(skill);
        }
        Arc::new(mgr)
    }

    fn names(catalog: &SceneSkills) -> Vec<String> {
        let mut names: Vec<_> = SkillCatalog::list(catalog)
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_scene_lists_the_built_ins_it_names_and_can_run() {
        let scene = Picky(vec!["skillify", "loop", "debug"], vec!["Read", "Write"]);
        let catalog = SceneSkills::new(manager(), BundledAdmission::for_scene(&scene));
        assert_eq!(
            names(&catalog),
            ["debug", "skillify"],
            "loop needs CronCreate"
        );
        assert!(SkillCatalog::get(&catalog, "loop").is_none());
        assert!(SkillCatalog::body(&catalog, "loop").is_none());
        assert!(SkillCatalog::body(&catalog, "skillify").is_some());
    }

    #[test]
    fn skills_from_elsewhere_pass_whatever_the_scene_names() {
        let mgr = manager();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("loop.md");
        std::fs::write(&file, "---\nname: loop\ndescription: mine\n---\nbody").unwrap();
        mgr.reload_skill(&file).unwrap();
        let catalog = SceneSkills::new(mgr, BundledAdmission::for_scene(&Picky(vec![], vec![])));
        assert_eq!(names(&catalog), ["loop"]);
    }
}
