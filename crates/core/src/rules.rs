//! `.agix/rules/` discovery — a lightweight, lazy index surfaced in the
//! system prompt so the model knows rule documents exist without eagerly
//! loading their content.
//!
//! Rules stay intentionally lazy (Read-tool-on-demand, not auto-injected in
//! full). This module only makes sure a `.md` file sitting in `.agix/rules/`
//! is visible to the model even when nothing in `AGENTS.md` references it.
//! Discovery only reads each file's first line, never full content.
//!
//! Where the documents come from is the
//! [`RuleProvider`](crate::interface::instruction_provider::RuleProvider)
//! contract; this module owns the merge and the prompt block.

use crate::interface::instruction_provider::{RuleDirectory, RuleProvider};
use crate::paths::PromptFiles;
use crate::settings::PathSettings;
use std::path::PathBuf;
use std::sync::Arc;

/// One discovered rule document.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleEntry {
    /// Filename stem (e.g. "security" for `security.md`).
    pub name: String,
    /// First non-empty line of the file, `#`/whitespace-stripped — same
    /// "fallback to first body line" convention as skills. Capped at
    /// `MAX_RULE_DESCRIPTION_CHARS` regardless of how verbose the line is.
    pub description: String,
    /// Absolute path — always resolvable by the Read tool regardless of
    /// which tier (project/scene/global) the file came from.
    pub path: PathBuf,
}

/// Rule sources merged into one index: a later source's entry replaces an
/// earlier one of the same name, and the result is sorted by name.
#[derive(Clone)]
pub struct RuleTiers {
    sources: Vec<Arc<dyn RuleProvider>>,
}

impl RuleTiers {
    pub fn new(sources: Vec<Arc<dyn RuleProvider>>) -> Self {
        Self { sources }
    }

    /// The engine's own tiers, outermost first, so project overrides scene
    /// overrides global — same precedence as skills/agents
    /// (`crates/core/src/frozen/skill.rs`,
    /// `crates/agents/src/agent_tool/catalog.rs::merge_agent_types`).
    pub fn for_paths(paths: &PathSettings) -> Self {
        let files = PromptFiles::for_project(&crate::paths::ConfigPaths::from_settings(paths));
        let project = paths
            .project_root()
            .map(|project| project.join(".agix").join("rules"));
        let sources = [
            paths.global_data_dir.join("rules"),
            paths.user_data_dir.join("rules"),
        ]
        .into_iter()
        .chain(project)
        .map(|dir| {
            Arc::new(RuleDirectory::new(dir).reading_with(files.clone())) as Arc<dyn RuleProvider>
        })
        .collect();
        Self::new(sources)
    }
}

impl RuleProvider for RuleTiers {
    fn id(&self) -> &str {
        "tiers"
    }

    fn rules(&self) -> Vec<RuleEntry> {
        let mut by_name: std::collections::HashMap<String, RuleEntry> =
            std::collections::HashMap::new();
        for source in &self.sources {
            for entry in source.rules() {
                by_name.insert(entry.name.clone(), entry);
            }
        }
        let mut entries: Vec<RuleEntry> = by_name.into_values().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

/// Render the "Available Rules" system prompt block. Returns `None` when no
/// rule documents exist anywhere — keeps token cost at zero for sessions
/// that don't use this feature at all, matching the "宽进严出" philosophy
/// used elsewhere in the config system (unused config = zero runtime cost).
pub fn render_rules_prompt(entries: &[RuleEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut lines = vec![
        "## Available Rules".to_string(),
        String::new(),
        "The following rule documents exist under `.agix/rules/`. They are NOT \
         auto-loaded — use the Read tool to load one when it's relevant to the \
         current task."
            .to_string(),
        String::new(),
    ];
    for e in entries {
        lines.push(format!("- `{}`: {}", e.path.display(), e.description));
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::instruction_provider::StaticRules;
    use crate::settings::PathSettings;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_rule(dir: &Path, filename: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(filename), content).unwrap();
    }

    fn paths_for(project: &Path, scene: &Path, global: &Path) -> PathSettings {
        PathSettings {
            user_data_dir: scene.to_path_buf(),
            global_data_dir: global.to_path_buf(),
            workspace: crate::settings::Workspace::project_of(&project.join(".agix")),
            local_data_dir: project.join(".agix"),
            scope: "code".into(),
        }
    }

    #[test]
    fn discover_rules_empty_when_no_dirs_exist() {
        let root = TempDir::new().unwrap();
        let paths = paths_for(
            &root.path().join("project"),
            &root.path().join("scene"),
            &root.path().join("global"),
        );
        assert!(RuleTiers::for_paths(&paths).rules().is_empty());
        assert!(render_rules_prompt(&RuleTiers::for_paths(&paths).rules()).is_none());
    }

    #[test]
    fn discover_rules_reads_first_line_as_description() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        write_rule(
            &project.join(".agix").join("rules"),
            "security.md",
            "# Security guidelines\n\nBody text here.",
        );
        let paths = paths_for(
            &project,
            &root.path().join("scene"),
            &root.path().join("global"),
        );
        let entries = RuleTiers::for_paths(&paths).rules();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "security");
        assert_eq!(entries[0].description, "Security guidelines");
    }

    #[test]
    fn discover_rules_project_overrides_scene_overrides_global() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        let scene = root.path().join("scene");
        let global = root.path().join("global");
        write_rule(&global.join("rules"), "testing.md", "from global");
        write_rule(&scene.join("rules"), "testing.md", "from scene");
        write_rule(
            &project.join(".agix").join("rules"),
            "testing.md",
            "from project",
        );

        let paths = paths_for(&project, &scene, &global);
        let entries = RuleTiers::for_paths(&paths).rules();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].description, "from project");
    }

    #[test]
    fn discover_rules_keeps_distinct_names_from_different_tiers() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        let global = root.path().join("global");
        write_rule(&global.join("rules"), "architecture.md", "global rule");
        write_rule(
            &project.join(".agix").join("rules"),
            "security.md",
            "project rule",
        );

        let paths = paths_for(&project, &root.path().join("scene"), &global);
        let entries = RuleTiers::for_paths(&paths).rules();
        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"architecture"));
        assert!(names.contains(&"security"));
    }

    #[test]
    fn the_rules_block_lists_filenames_and_descriptions() {
        let root = TempDir::new().unwrap();
        let project = root.path().join("project");
        write_rule(
            &project.join(".agix").join("rules"),
            "testing.md",
            "Testing conventions",
        );
        let paths = paths_for(
            &project,
            &root.path().join("scene"),
            &root.path().join("global"),
        );
        let text = render_rules_prompt(&RuleTiers::for_paths(&paths).rules())
            .expect("expected a rules block");
        assert!(text.contains("Available Rules"));
        assert!(text.contains("testing.md"));
        assert!(text.contains("Testing conventions"));
        assert!(text.contains("Read tool"));
    }

    /// The tier list is a composition of sources, not three paths written
    /// into the discovery function — so a fourth source merges by the same
    /// last-wins rule the tiers use.
    #[test]
    fn a_source_the_engine_does_not_own_takes_part_in_the_merge() {
        let sources: Vec<Arc<dyn RuleProvider>> = vec![
            Arc::new(StaticRules::new(
                "service",
                vec![RuleEntry {
                    name: "testing".into(),
                    description: "from the service".into(),
                    path: PathBuf::from("/srv/testing.md"),
                }],
            )),
            Arc::new(StaticRules::new(
                "override",
                vec![RuleEntry {
                    name: "testing".into(),
                    description: "from the later source".into(),
                    path: PathBuf::from("/late/testing.md"),
                }],
            )),
        ];
        let entries = RuleTiers::new(sources).rules();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].description, "from the later source");
    }
}
