//! The engine's own skill sources, as
//! [`SkillProvider`](base::interface::skill_provider::SkillProvider)
//! implementations.
//!
//! One per place skills come from: the directory tiers, the built-ins
//! compiled into the binary, an installed plugin package, and the tools of a
//! connected MCP server.

use base::frozen::frontmatter::parse_skill_file;
use base::frozen::skill::{SkillEntry, SkillSource as FrozenSkillSource};
use base::interface::model::ToolDef;
use base::interface::skill_provider::{SkillPrecedence, SkillProvider};
use base::paths::PromptFiles;
use std::path::{Path, PathBuf};

use crate::manager::SkillSource;

fn frozen_source(source: SkillSource) -> FrozenSkillSource {
    match source {
        SkillSource::User => FrozenSkillSource::User,
        SkillSource::Project => FrozenSkillSource::Project,
        SkillSource::Plugin => FrozenSkillSource::Plugin,
    }
}

/// One skill directory.
///
/// `nested` is the `<name>/SKILL.md` convention plus the flat `<name>.md`
/// legacy form; without it only the flat form is read.
pub struct SkillDirectory {
    dir: PathBuf,
    tier: SkillSource,
    nested: bool,
    id: String,
    files: PromptFiles,
}

impl SkillDirectory {
    /// A tier as `build_default_skill_manager` scans it: both layouts, and
    /// the disk wins over anything registered before it.
    pub fn tier(dir: impl Into<PathBuf>, tier: SkillSource) -> Self {
        let dir = dir.into();
        let id = format!("dir:{}", dir.display());
        Self {
            dir,
            tier,
            nested: true,
            id,
            files: PromptFiles::unscoped(),
        }
    }

    pub fn reading_with(mut self, files: PromptFiles) -> Self {
        self.files = files;
        self
    }

    /// The flat `<name>.md` layout only.
    pub fn flat(dir: impl Into<PathBuf>, tier: SkillSource) -> Self {
        let mut s = Self::tier(dir, tier);
        s.nested = false;
        s
    }
}

impl SkillProvider for SkillDirectory {
    fn id(&self) -> &str {
        &self.id
    }

    fn precedence(&self) -> SkillPrecedence {
        SkillPrecedence::Override
    }

    fn skills(&self) -> Vec<SkillEntry> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if self.nested && path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.is_file() {
                    let dir_name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown");
                    if let Some(e) = self.parse_at_path(&skill_md, dir_name) {
                        out.push(e);
                    }
                }
            } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Some(e) = self.parse_at_path(&path, name) {
                        out.push(e);
                    }
                }
            }
        }
        out
    }
}

/// The skills compiled into the binary.
pub struct BundledSkills;

impl SkillProvider for BundledSkills {
    fn id(&self) -> &str {
        "bundled"
    }

    fn skills(&self) -> Vec<SkillEntry> {
        crate::bundled::bundled_skills()
    }

    fn body(&self, name: &str) -> Option<String> {
        crate::bundled::bundled_body(name).map(|s| s.to_string())
    }
}

/// One plugin package's skills, as read and checked when the package loaded.
///
/// Bodies are held rather than re-read from the package directory, so what a
/// skill expands to is the text its package disclosed. A name another source
/// loaded first stays with that source: the disk tiers win over a package,
/// and of two packages the one loaded first.
pub struct PluginSkills {
    id: String,
    entries: Vec<SkillEntry>,
    bodies: std::collections::HashMap<String, String>,
}

impl PluginSkills {
    pub fn new(plugin: &str, skills: Vec<(SkillEntry, String)>) -> Self {
        let mut entries = Vec::with_capacity(skills.len());
        let mut bodies = std::collections::HashMap::with_capacity(skills.len());
        for (mut entry, body) in skills {
            entry.source = FrozenSkillSource::Plugin;
            bodies.insert(entry.name.clone(), body);
            entries.push(entry);
        }
        Self {
            id: format!("plugin:{plugin}"),
            entries,
            bodies,
        }
    }
}

impl SkillProvider for PluginSkills {
    fn id(&self) -> &str {
        &self.id
    }

    fn precedence(&self) -> SkillPrecedence {
        SkillPrecedence::Fallback
    }

    fn skills(&self) -> Vec<SkillEntry> {
        self.entries.clone()
    }

    fn body(&self, name: &str) -> Option<String> {
        self.bodies.get(name).cloned()
    }
}

/// One MCP server's tools, each as an invocable skill.
pub struct McpSkills {
    server: String,
    tools: Vec<ToolDef>,
    id: String,
}

impl McpSkills {
    pub fn new(server: impl Into<String>, tools: Vec<ToolDef>) -> Self {
        let server = server.into();
        let id = format!("mcp:{server}");
        Self { server, tools, id }
    }
}

impl SkillProvider for McpSkills {
    fn id(&self) -> &str {
        &self.id
    }

    fn skills(&self) -> Vec<SkillEntry> {
        crate::mcp_builder::build_skills_from_mcp(&self.server, &self.tools)
    }

    fn body(&self, name: &str) -> Option<String> {
        crate::mcp_builder::mcp_skill_body(&self.server, &self.tools, name)
    }
}

impl SkillDirectory {
    fn parse_at_path(&self, path: &Path, dir_name: &str) -> Option<SkillEntry> {
        let content = self.files.read_to_string(path).ok()?;
        parse_skill_file(
            &content,
            dir_name.to_string(),
            path,
            frozen_source(self.tier),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn a_tier_reads_both_the_nested_and_the_flat_layout() {
        let dir = tempfile::TempDir::new().unwrap();
        write(
            &dir.path().join("review").join("SKILL.md"),
            "---\nname: review\ndescription: Review code\n---\nbody",
        );
        write(
            &dir.path().join("deploy.md"),
            "---\nname: deploy\ndescription: Ship it\n---\nbody",
        );
        let mut names: Vec<String> = SkillDirectory::tier(dir.path(), SkillSource::Project)
            .skills()
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["deploy", "review"]);
    }

    #[test]
    fn a_project_skill_linked_out_of_the_project_is_not_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (project, outside) = (tmp.path().join("project"), tmp.path().join("outside"));
        let skills = project.join(".agents").join("skills");
        write(
            &outside.join("shared").join("SKILL.md"),
            "---\nname: shared\ndescription: From outside\n---\nbody",
        );
        write(
            &project.join("docs").join("local.md"),
            "---\nname: local\ndescription: From inside\n---\nbody",
        );
        std::fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(outside.join("shared"), skills.join("shared")).unwrap();
        std::os::unix::fs::symlink("../../docs/local.md", skills.join("local.md")).unwrap();
        let names: Vec<String> = SkillDirectory::tier(&skills, SkillSource::Project)
            .reading_with(base::paths::SensitivePaths::default().prompt_files_within(&project))
            .skills()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["local"]);
    }

    #[test]
    fn the_flat_layout_alone_skips_subdirectories() {
        let dir = tempfile::TempDir::new().unwrap();
        write(
            &dir.path().join("review").join("SKILL.md"),
            "---\nname: review\ndescription: Review code\n---\nbody",
        );
        write(
            &dir.path().join("deploy.md"),
            "---\nname: deploy\ndescription: Ship it\n---\nbody",
        );
        let names: Vec<String> = SkillDirectory::flat(dir.path(), SkillSource::Project)
            .skills()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["deploy"]);
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        assert!(
            SkillDirectory::tier("/nonexistent/skills", SkillSource::User)
                .skills()
                .is_empty()
        );
    }

    /// The two in-memory sources are the reason `body` is on the contract at
    /// all: their entries point at a sentinel path no reader can open.
    #[test]
    fn the_built_in_source_answers_for_its_own_bodies() {
        let bundled = BundledSkills;
        let first = bundled
            .skills()
            .into_iter()
            .next()
            .expect("the binary ships at least one skill");
        assert!(
            bundled.body(&first.name).is_some(),
            "'{}' lists but cannot be expanded",
            first.name
        );
    }

    #[test]
    fn an_mcp_server_answers_for_its_own_bodies() {
        let source = McpSkills::new(
            "files",
            vec![ToolDef::new(
                "read",
                "Read a file",
                serde_json::json!({"type": "object"}),
            )],
        );
        let entries = source.skills();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "mcp__files__read");
        assert!(source.body("mcp__files__read").is_some());
    }
}
