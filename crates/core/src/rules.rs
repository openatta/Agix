//! `.agix/rules/` discovery — a lightweight, lazy index surfaced in the
//! system prompt so the model knows rule documents exist without eagerly
//! loading their content.
//!
//! Rules stay intentionally lazy (Read-tool-on-demand, not auto-injected in
//! full). This module only
//! closes the "orphaned rule file" gap: a `.md` file sitting in `.agix/rules/`
//! that nobody referenced from `AGENTS.md` used to be invisible to the model.
//! Discovery still only reads each file's first line, never full content.
//!
//! Where the documents come from is the
//! [`RuleProvider`](crate::interface::instruction_provider::RuleProvider)
//! contract; this module owns the merge and the prompt block.

use crate::interface::instruction_provider::{RuleDirectory, RuleProvider};
use crate::settings::PathSettings;
use std::path::PathBuf;

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

/// The engine's own rule sources: the three tiers, outermost first, so
/// project overrides scene overrides global — same precedence as
/// skills/agents (`crates/core/src/frozen/skill.rs`,
/// `crates/runtime/src/agent_tool.rs::merge_agent_types`).
pub fn default_rule_sources(paths: &PathSettings) -> Vec<Box<dyn RuleProvider>> {
    vec![
        Box::new(RuleDirectory::new(paths.global_data_dir.join("rules"))),
        Box::new(RuleDirectory::new(paths.user_data_dir.join("rules"))),
        Box::new(RuleDirectory::new(
            paths.project_root().join(".agix").join("rules"),
        )),
    ]
}

/// Scan the three tiers and return one entry per unique filename stem.
pub fn discover_rules(paths: &PathSettings) -> Vec<RuleEntry> {
    discover_rules_from(&default_rule_sources(paths))
}

/// Merge `sources` into one index: later sources win a name collision, and
/// the result is sorted by name for stable output.
pub fn discover_rules_from(sources: &[Box<dyn RuleProvider>]) -> Vec<RuleEntry> {
    let mut by_name: std::collections::HashMap<String, RuleEntry> =
        std::collections::HashMap::new();
    for source in sources {
        for entry in source.rules() {
            by_name.insert(entry.name.clone(), entry);
        }
    }
    let mut entries: Vec<RuleEntry> = by_name.into_values().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Render the "Available Rules" system prompt block. Returns `None` when no
/// rule documents exist anywhere — keeps token cost at zero for sessions
/// that don't use this feature at all, matching the "宽进严出" philosophy
/// used elsewhere in the config system (unused config = zero runtime cost).
pub fn build_rules_prompt(paths: &PathSettings) -> Option<String> {
    let entries = discover_rules(paths);
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
    for e in &entries {
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
        assert!(discover_rules(&paths).is_empty());
        assert!(build_rules_prompt(&paths).is_none());
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
        let entries = discover_rules(&paths);
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
        let entries = discover_rules(&paths);
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
        let entries = discover_rules(&paths);
        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"architecture"));
        assert!(names.contains(&"security"));
    }

    #[test]
    fn build_rules_prompt_lists_filenames_and_descriptions() {
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
        let text = build_rules_prompt(&paths).expect("expected a rules block");
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
        let sources: Vec<Box<dyn RuleProvider>> = vec![
            Box::new(StaticRules::new(
                "service",
                vec![RuleEntry {
                    name: "testing".into(),
                    description: "from the service".into(),
                    path: PathBuf::from("/srv/testing.md"),
                }],
            )),
            Box::new(StaticRules::new(
                "override",
                vec![RuleEntry {
                    name: "testing".into(),
                    description: "from the later source".into(),
                    path: PathBuf::from("/late/testing.md"),
                }],
            )),
        ];
        let entries = discover_rules_from(&sources);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].description, "from the later source");
    }
}
