//! `InstructionProvider` and `RuleProvider` — where the project's standing
//! instructions come from.
//!
//! Two things reach the model without anyone asking for them: the instruction
//! file (`AGENTS.md` / `CLAUDE.md`), injected verbatim, and the `.agix/rules/`
//! index, advertised by name and first line so the model knows the documents
//! exist. Both were a fixed answer to a question a deployment may answer
//! differently — one path out of `settings.instruction_file`, and three
//! directory tiers written into the discovery function. A host whose
//! conventions live in a service, a monorepo tool that composes its own tier
//! list, or an embedder with no filesystem at all had nothing to replace.
//!
//! The two stay separate contracts because they are read differently:
//! instruction documents are content that goes into the prompt, rules are an
//! index whose content is deliberately *not* loaded until the model asks for
//! it.

use crate::rules::RuleEntry;
use std::path::{Path, PathBuf};

/// One standing-instruction document.
pub struct InstructionDoc {
    /// Where it came from, for the header the engine writes when more than
    /// one document applies. Need not exist on disk.
    pub path: PathBuf,
    pub content: String,
}

/// A place standing instructions come from.
pub trait InstructionProvider: Send + Sync {
    fn id(&self) -> &str;

    /// The documents that apply, outermost first — the engine treats the last
    /// one as the most specific and lets it win a conflict.
    fn documents(&self) -> Vec<InstructionDoc>;
}

/// The configured instruction file, read from disk.
pub struct InstructionFile {
    path: Option<PathBuf>,
}

impl InstructionFile {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }
}

impl InstructionProvider for InstructionFile {
    fn id(&self) -> &str {
        "instruction_file"
    }

    fn documents(&self) -> Vec<InstructionDoc> {
        let Some(ref path) = self.path else {
            return Vec::new();
        };
        match std::fs::read_to_string(path) {
            Ok(content) if !content.trim().is_empty() => vec![InstructionDoc {
                path: path.clone(),
                content,
            }],
            Ok(_) => Vec::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read instruction file for CLAUDE.md context injection");
                Vec::new()
            }
        }
    }
}

/// Instructions the host already has in memory.
///
/// A daemon that fetched the project's conventions over its own API, or a
/// test harness that wants a known instruction text without a temp directory,
/// hands them over directly rather than staging a file for the engine to read
/// back.
pub struct InlineInstructions {
    label: PathBuf,
    text: String,
}

impl InlineInstructions {
    pub fn new(label: impl Into<PathBuf>, text: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            text: text.into(),
        }
    }
}

impl InstructionProvider for InlineInstructions {
    fn id(&self) -> &str {
        "inline"
    }

    fn documents(&self) -> Vec<InstructionDoc> {
        if self.text.trim().is_empty() {
            return Vec::new();
        }
        vec![InstructionDoc {
            path: self.label.clone(),
            content: self.text.clone(),
        }]
    }
}

/// A place rule documents come from.
///
/// Ordered by the caller: a later source's entry replaces an earlier one of
/// the same name, which is how project beats scene beats global.
pub trait RuleProvider: Send + Sync {
    fn id(&self) -> &str;

    fn rules(&self) -> Vec<RuleEntry>;
}

/// How much of a rule's first line is kept as its description.
pub const MAX_RULE_DESCRIPTION_CHARS: usize = 150;

/// One directory of `.md` rule documents.
pub struct RuleDirectory {
    dir: PathBuf,
    id: String,
}

impl RuleDirectory {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let id = dir.display().to_string();
        Self { dir, id }
    }
}

impl RuleProvider for RuleDirectory {
    fn id(&self) -> &str {
        &self.id
    }

    fn rules(&self) -> Vec<RuleEntry> {
        scan_rules_dir(&self.dir)
    }
}

/// Rules the host already knows about.
///
/// The index is a name, a one-line summary and something the Read tool can
/// open — none of which has to come from a directory scan. A deployment
/// serving its rule documents out of a repository service builds the entries
/// itself and keeps the paths its own tooling understands.
pub struct StaticRules {
    id: String,
    entries: Vec<RuleEntry>,
}

impl StaticRules {
    pub fn new(id: impl Into<String>, entries: Vec<RuleEntry>) -> Self {
        Self {
            id: id.into(),
            entries,
        }
    }
}

impl RuleProvider for StaticRules {
    fn id(&self) -> &str {
        &self.id
    }

    fn rules(&self) -> Vec<RuleEntry> {
        self.entries.clone()
    }
}

fn scan_rules_dir(dir: &Path) -> Vec<RuleEntry> {
    let mut out = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return out; // directory doesn't exist yet
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let description = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| first_line_description(&content))
            .unwrap_or_else(|| "(no description)".to_string());
        out.push(RuleEntry {
            name: name.to_string(),
            description,
            path,
        });
    }
    out
}

/// First non-empty, non-bare-`#` line, stripped and capped.
pub fn first_line_description(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let stripped = trimmed.trim_start_matches('#').trim();
        if stripped.is_empty() {
            continue;
        }
        let truncated: String = stripped.chars().take(MAX_RULE_DESCRIPTION_CHARS).collect();
        return Some(if stripped.chars().count() > MAX_RULE_DESCRIPTION_CHARS {
            format!("{truncated}...")
        } else {
            truncated
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn an_absent_instruction_file_contributes_nothing() {
        assert!(InstructionFile::new(None).documents().is_empty());
        assert!(
            InstructionFile::new(Some(PathBuf::from("/nonexistent/AGENTS.md")))
                .documents()
                .is_empty()
        );
    }

    #[test]
    fn a_whitespace_only_instruction_file_contributes_nothing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "   \n\n").unwrap();
        assert!(InstructionFile::new(Some(path)).documents().is_empty());
    }

    #[test]
    fn the_file_source_hands_back_the_text_untouched() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# Conventions\n\nBe brief.\n").unwrap();
        let docs = InstructionFile::new(Some(path.clone())).documents();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, path);
        assert_eq!(docs[0].content, "# Conventions\n\nBe brief.\n");
    }

    #[test]
    fn a_host_can_supply_instructions_without_a_file() {
        let docs = InlineInstructions::new("service://conventions", "Be brief.").documents();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content, "Be brief.");
        assert!(InlineInstructions::new("x", "  ").documents().is_empty());
    }

    #[test]
    fn a_rule_directory_reads_the_first_line_as_the_description() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("security.md"),
            "# Security guidelines\n\nBody.",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let rules = RuleDirectory::new(dir.path()).rules();
        assert_eq!(rules.len(), 1, "only .md files are rule documents");
        assert_eq!(rules[0].name, "security");
        assert_eq!(rules[0].description, "Security guidelines");
    }

    #[test]
    fn a_missing_rule_directory_is_not_an_error() {
        assert!(RuleDirectory::new("/nonexistent/rules").rules().is_empty());
    }

    #[test]
    fn a_host_can_supply_a_rule_index_it_built_itself() {
        let entries = vec![RuleEntry {
            name: "testing".into(),
            description: "How we test".into(),
            path: PathBuf::from("/srv/rules/testing.md"),
        }];
        let source = StaticRules::new("service", entries.clone());
        assert_eq!(source.id(), "service");
        assert_eq!(source.rules(), entries);
    }

    #[test]
    fn first_line_description_truncates_long_lines() {
        let long = "a".repeat(300);
        let desc = first_line_description(&long).unwrap();
        assert!(desc.ends_with("..."));
        assert!(desc.chars().count() <= MAX_RULE_DESCRIPTION_CHARS + 3);
    }

    #[test]
    fn first_line_description_skips_blank_lines_and_bare_hashes() {
        let content = "\n\n#  \n# Real Title\nbody";
        assert_eq!(
            first_line_description(content).as_deref(),
            Some("Real Title")
        );
    }
}
