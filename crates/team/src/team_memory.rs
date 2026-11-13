//! Team shared memory: what a `TeamCreate` team's members learn and want the
//! rest of the team to know.
//!
//! Lives at `<project>/.agix/teams/<team_id>/memory/`, next to the team's
//! scratchpad and state, so `TeamDelete` removing the team directory takes it
//! along. The format is the personal-memory one (`MEMORY.md` index plus one
//! `.md` file per fact). Members reach it through [`TeamMemorySaveTool`] and
//! [`TeamMemoryReadTool`], and every member's task opens with
//! [`TeamMemoryStore::build_team_memory_prompt`], so a later stage sees what an
//! earlier one saved.

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
pub use base::memory::{scan_for_secrets, SecretPattern};
use base::memory::{DurableMemory, MemoryError, MemoryStore, MemoryType};
use base::tool::{
    PathStyle, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct TeamMemoryStore {
    pub team_memory_dir: PathBuf,
    store: MemoryStore,
    write_lock: std::sync::Mutex<()>,
}

impl TeamMemoryStore {
    pub fn for_team_dir(team_dir: &Path) -> Self {
        Self::with_dir(team_dir.join("memory"))
    }

    /// Create a team memory store rooted at a specific directory.
    pub fn with_dir(team_memory_dir: PathBuf) -> Self {
        // Use the same directory for both user and local scope — team memory
        // is a single flat namespace.
        let store = MemoryStore::new(team_memory_dir.clone(), team_memory_dir.clone());
        Self {
            team_memory_dir,
            store,
            write_lock: std::sync::Mutex::new(()),
        }
    }

    /// Returns a reference to the underlying `MemoryStore`.
    pub fn inner(&self) -> &MemoryStore {
        &self.store
    }

    /// Ensure the team memory directory exists.
    pub fn ensure_dir(&self) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(&self.team_memory_dir)
    }

    /// Load all team memories (shared across all team members).
    pub fn load_team_memories(&self) -> Vec<DurableMemory> {
        self.store.load_all()
    }

    /// Write a single memory to the team store.
    ///
    /// Runs secret scanning before persisting — warns on potential secrets but
    /// does not block.
    pub fn write_team_memory(&self, memory: DurableMemory) -> Result<usize, MemoryError> {
        let scanned = scan_for_secrets(&memory);
        if !scanned.is_empty() {
            tracing::warn!(
                target: "team_memory",
                secrets = ?scanned,
                memory_name = %memory.name,
                "Team memory contains potential secrets — proceeding anyway"
            );
        }
        self.store.persist_batch(vec![memory])
    }

    /// Write a batch of memories to the team store.
    pub fn write_team_memories(&self, memories: Vec<DurableMemory>) -> Result<usize, MemoryError> {
        for mem in &memories {
            let scanned = scan_for_secrets(mem);
            if !scanned.is_empty() {
                tracing::warn!(
                    target: "team_memory",
                    secrets = ?scanned,
                    memory_name = %mem.name,
                    "Team memory contains potential secrets — proceeding anyway"
                );
            }
        }
        self.store.persist_batch(memories)
    }

    /// Remove a memory by name from the team store.
    pub fn remove_team_memory(&self, name: &str) -> Result<bool, MemoryError> {
        self.store.remove(name)
    }

    /// Load the MEMORY.md index content for this team.
    pub fn load_index(&self) -> String {
        self.store.load_index()
    }

    /// Search team memories by query.
    pub fn search(&self, query: &str) -> Vec<DurableMemory> {
        self.store.search(query)
    }

    /// Delete the entire team memory directory (called on team deletion).
    pub fn delete_all(&self) -> Result<(), std::io::Error> {
        if self.team_memory_dir.exists() {
            std::fs::remove_dir_all(&self.team_memory_dir)?;
        }
        Ok(())
    }

    /// Build the combined memory prompt section showing both personal and team memories.
    ///
    /// Same-name memories in personal take precedence — team memories with a name
    /// that matches a personal memory are hidden from the team section to avoid
    /// conflicting information.
    ///
    /// Format:
    /// ```md
    /// # Memory
    /// ## Your personal memory
    /// {personal_memory_index}
    ///
    /// ## Team shared memory (visible to all team members)
    /// {team_memory_index}
    /// ```
    pub fn build_combined_memory_prompt(&self, personal_store: &MemoryStore) -> String {
        let personal_index = personal_store.load_index();

        // Load team index, excluding entries whose name matches a personal memory.
        // This implements the dedup rule: same-name personal takes precedence.
        let personal_memories = personal_store.load_all();
        let personal_names: std::collections::HashSet<String> =
            personal_memories.iter().map(|m| m.name.clone()).collect();

        let all_team = self.load_team_memories();
        let team_index = build_filtered_memory_index(
            &all_team,
            &personal_names,
            self.team_memory_dir.join("MEMORY.md"),
        );

        let mut parts = vec!["# Memory".to_string()];

        // Personal memory section
        if personal_index.trim().is_empty() {
            parts.push("## Your personal memory\n\n(No personal memories yet.)".into());
        } else {
            parts.push(format!("## Your personal memory\n\n{}", personal_index));
        }

        // Team shared memory section
        if team_index.trim().is_empty() {
            parts.push(
                "## Team shared memory (visible to all team members)\n\n\
                 (No team memories yet. Team members can write shared memories here.)"
                    .into(),
            );
        } else {
            parts.push(format!(
                "## Team shared memory (visible to all team members)\n\n{}",
                team_index
            ));
        }

        parts.join("\n\n")
    }

    pub fn build_team_memory_prompt(&self) -> String {
        let mut prompt = String::from(
            "# Team shared memory\n\n\
             This team keeps a shared memory that every member reads and writes. \
             Save something with `TeamMemorySave` when the rest of the team should know it: \
             a decision with lasting impact, an external constraint, design rationale that \
             crosses subsystems, a pointer to a shared resource. Read a full entry with \
             `TeamMemoryRead`. Do not save in-progress task state (that belongs in your \
             answer or the scratchpad) or anything the code already records.\n\n",
        );
        let index = self.load_index();
        if index.trim().is_empty() {
            prompt.push_str("_(No team memories yet.)_\n");
        } else {
            prompt.push_str("## Current team memory index\n\n");
            prompt.push_str(index.trim_end());
            prompt.push('\n');
        }
        prompt
    }
}

/// Build a filtered memory index string, excluding entries whose name appears in
/// `exclude_names` (personal memories that take precedence).
///
/// Attempts to read the MEMORY.md file for a canonical index but also rebuilds
/// from the in-memory list when needed.
fn build_filtered_memory_index(
    all_memories: &[DurableMemory],
    exclude_names: &std::collections::HashSet<String>,
    index_path: PathBuf,
) -> String {
    // Try to use the on-disk MEMORY.md as the canonical index, but filter out
    // lines referencing excluded memory names.
    let files = match index_path.parent() {
        Some(dir) => base::paths::SensitivePaths::default().prompt_files_within(dir),
        None => base::paths::PromptFiles::unscoped(),
    };
    if let Ok(raw) = files.read_to_string(&index_path) {
        let filtered: String = raw
            .lines()
            .filter(|line| {
                // Check if the line references an excluded memory name.
                // MEMORY.md format: `- [name](name.md) — description`
                // Extract the name from the markdown link.
                !exclude_names.iter().any(|excluded| {
                    line.contains(&format!("]({}.md)", excluded))
                        || line.contains(&format!("[{}]", excluded))
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !filtered.trim().is_empty() {
            return filtered;
        }
    }

    // Fallback: rebuild index from the DurableMemory list, filtering exclusions.
    let filtered: Vec<&DurableMemory> = all_memories
        .iter()
        .filter(|m| !exclude_names.contains(&m.name))
        .collect();

    if filtered.is_empty() {
        return String::new();
    }

    filtered
        .iter()
        .map(|m| format!("- [{}]({}.md) — {}", m.name, m.name, m.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Scan all memories in a `TeamMemoryStore` for secrets.
pub fn scan_store_for_secrets(store: &TeamMemoryStore) -> Vec<SecretPattern> {
    let memories = store.load_team_memories();
    let mut all_findings = Vec::new();
    for mem in &memories {
        all_findings.extend(scan_for_secrets(mem));
    }
    all_findings
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TeamMemorySaveInput {
    /// Short kebab-case slug; saving under an existing name replaces that entry.
    pub name: String,
    /// One line saying what this is, so a teammate can tell from the index whether to read it.
    pub description: String,
    /// `project` (default), `feedback`, `reference` or `user`.
    #[serde(default, rename = "type")]
    pub memory_type: Option<String>,
    pub content: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TeamMemoryReadInput {
    /// Name of the entry, as listed in the team memory index.
    pub name: String,
}

fn valid_memory_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn parse_memory_type(t: Option<&str>) -> Option<MemoryType> {
    match t.unwrap_or("project") {
        "user" => Some(MemoryType::User),
        "feedback" => Some(MemoryType::Feedback),
        "project" => Some(MemoryType::Project),
        "reference" => Some(MemoryType::Reference),
        _ => None,
    }
}

pub struct TeamMemorySaveTool {
    store: Arc<TeamMemoryStore>,
    author: String,
}

impl TeamMemorySaveTool {
    pub fn new(store: Arc<TeamMemoryStore>, author: impl Into<String>) -> Self {
        Self {
            store,
            author: author.into(),
        }
    }
}

#[async_trait]
impl Tool for TeamMemorySaveTool {
    fn name(&self) -> &str {
        "TeamMemorySave"
    }
    fn description(&self) -> &str {
        ""
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(TeamMemorySaveInput))
            .expect("schemars output is valid JSON")
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(
            "Save a fact to this team's shared memory, where every member of the team will see it \
         in the memory index. Use a kebab-case `name`; saving under an existing name replaces \
         that entry. Never save credentials or tokens: content that looks like a secret is \
         refused."
                .to_string(),
        )
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<TeamMemorySaveInput>(input.clone()) {
            Ok(i) if !valid_memory_name(&i.name) => ValidationResult::err(
                "name must be 1-64 characters of lowercase letters, digits and '-'",
                1,
            ),
            Ok(i) if parse_memory_type(i.memory_type.as_deref()).is_none() => {
                ValidationResult::err("type must be one of project, feedback, reference, user", 2)
            }
            Ok(i) if i.content.trim().is_empty() => ValidationResult::err("content is empty", 3),
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 4),
        }
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }

    // Writes only under the team's own directory, the way the mailbox
    // persists; plan-mode members must still be able to contribute.

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().stance(base::interface::permission::ModeStance::Coordination)
    }

    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TeamMemorySaveInput = serde_json::from_value(input)?;
        let memory = DurableMemory {
            memory_type: parse_memory_type(input.memory_type.as_deref())
                .unwrap_or(MemoryType::Project),
            name: input.name,
            description: input.description,
            content: input.content,
            source_session_id: self.author.clone(),
            confidence: 0.8,
            last_seen: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Iso8601::DEFAULT)
                .unwrap_or_default(),
            recall_count: 0,
        };
        let secrets = scan_for_secrets(&memory);
        if let Some(first) = secrets.first() {
            return Ok(ToolResult::error_text(format!(
                "not saved: the entry looks like it contains a secret ({}). \
                 Team memory is plain files in the project; describe where the secret lives instead.",
                first.kind
            )));
        }
        let name = memory.name.clone();
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _serial = store.write_lock.lock().unwrap_or_else(|e| e.into_inner());
            store
                .ensure_dir()
                .map_err(|e| MemoryError::Io(e.to_string()))?;
            store.write_team_memory(memory)
        })
        .await
        .map_err(|e| ToolError::Execution(anyhow::anyhow!("team memory write: {e}")))?
        .map_err(|e| ToolError::Execution(anyhow::anyhow!("team memory write: {e}")))?;
        Ok(ToolResult::text(format!("saved team memory `{name}`")))
    }
}

pub struct TeamMemoryReadTool {
    store: Arc<TeamMemoryStore>,
}

impl TeamMemoryReadTool {
    pub fn new(store: Arc<TeamMemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for TeamMemoryReadTool {
    fn name(&self) -> &str {
        "TeamMemoryRead"
    }
    fn description(&self) -> &str {
        ""
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(TeamMemoryReadInput))
            .expect("schemars output is valid JSON")
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(
            "Read one entry of this team's shared memory by the name the memory index lists."
                .to_string(),
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }

    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TeamMemoryReadInput = serde_json::from_value(input)?;
        let store = self.store.clone();
        let found = tokio::task::spawn_blocking(move || {
            store
                .load_team_memories()
                .into_iter()
                .find(|m| m.name == input.name)
        })
        .await
        .map_err(|e| ToolError::Execution(anyhow::anyhow!("team memory read: {e}")))?;
        Ok(match found {
            Some(m) => ToolResult::text(format!(
                "# {}\n\n{} ({})\n\n{}",
                m.name,
                m.description,
                m.memory_type.as_str(),
                m.content
            )),
            None => ToolResult::error_text("no team memory by that name"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::ToolResultContent;
    use tempfile::TempDir;

    #[test]
    fn team_memory_store_creates_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("teams").join("test-team").join("memory");
        let store = TeamMemoryStore::with_dir(dir.clone());
        store.ensure_dir().unwrap();
        assert!(dir.exists());
    }

    #[test]
    fn write_and_load_team_memory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("teams").join("test-team").join("memory");
        let store = TeamMemoryStore::with_dir(dir);
        store.ensure_dir().unwrap();

        let mem = DurableMemory {
            name: "team-knowledge".into(),
            description: "Shared knowledge for the team".into(),
            memory_type: MemoryType::Reference,
            content: "The team decided to use Rust for all new services.".into(),
            source_session_id: "test-session".into(),
            confidence: 0.9,
            last_seen: "2026-06-14T00:00:00Z".into(),
            recall_count: 0,
        };
        store.write_team_memory(mem).unwrap();

        let loaded = store.load_team_memories();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "team-knowledge");
        assert_eq!(loaded[0].memory_type, MemoryType::Reference);
    }

    #[test]
    fn write_multiple_and_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("teams").join("multi").join("memory");
        let store = TeamMemoryStore::with_dir(dir);
        store.ensure_dir().unwrap();

        let mems = vec![
            DurableMemory {
                name: "decision-1".into(),
                description: "First decision".into(),
                memory_type: MemoryType::Project,
                content: "Decision one.".into(),
                source_session_id: "s1".into(),
                confidence: 0.9,
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            },
            DurableMemory {
                name: "decision-2".into(),
                description: "Second decision".into(),
                memory_type: MemoryType::Project,
                content: "Decision two.".into(),
                source_session_id: "s1".into(),
                confidence: 0.8,
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            },
        ];
        store.write_team_memories(mems).unwrap();
        assert_eq!(store.load_team_memories().len(), 2);
    }

    #[test]
    fn remove_team_memory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("teams").join("test-team").join("memory");
        let store = TeamMemoryStore::with_dir(dir);
        store.ensure_dir().unwrap();

        store
            .write_team_memory(DurableMemory {
                name: "delete-me".into(),
                description: "to be deleted".into(),
                memory_type: MemoryType::Project,
                content: "test".into(),
                source_session_id: "s1".into(),
                confidence: 0.9,
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            })
            .unwrap();

        assert_eq!(store.load_team_memories().len(), 1);
        store.remove_team_memory("delete-me").unwrap();
        assert!(store.load_team_memories().is_empty());
    }

    #[test]
    fn delete_all_removes_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("teams").join("test-team").join("memory");
        let store = TeamMemoryStore::with_dir(dir.clone());
        store.ensure_dir().unwrap();
        assert!(dir.exists());
        store.delete_all().unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn secret_scan_detects_api_key() {
        let mem = DurableMemory {
            name: "config".into(),
            description: "API configuration".into(),
            memory_type: MemoryType::Reference,
            content: "Use api_key = 'sk-abc123def456ghi789jkl' for the service".into(),
            source_session_id: "s1".into(),
            confidence: 0.9,
            last_seen: "2026-06-14T00:00:00Z".into(),
            recall_count: 0,
        };
        let findings = scan_for_secrets(&mem);
        assert!(!findings.is_empty(), "should detect API key");
        assert!(
            findings.iter().any(|f| f.kind.contains("API")),
            "should flag API key pattern"
        );
    }

    #[test]
    fn secret_scan_detects_ssh_key() {
        let mem = DurableMemory {
            name: "ssh-config".into(),
            description: "SSH setup".into(),
            memory_type: MemoryType::Reference,
            content: "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...".into(),
            source_session_id: "s1".into(),
            confidence: 0.9,
            last_seen: "2026-06-14T00:00:00Z".into(),
            recall_count: 0,
        };
        let findings = scan_for_secrets(&mem);
        assert!(!findings.is_empty(), "should detect SSH key");
        assert!(
            findings.iter().any(|f| f.kind.contains("SSH")),
            "should flag SSH private key"
        );
    }

    #[test]
    fn secret_scan_detects_github_token() {
        let mem = DurableMemory {
            name: "ci-config".into(),
            description: "CI configuration".into(),
            memory_type: MemoryType::Reference,
            content: "GITHUB_TOKEN=ghp_abc123def456ghi789jkl012mno345pqr678stuv".into(),
            source_session_id: "s1".into(),
            confidence: 0.9,
            last_seen: "2026-06-14T00:00:00Z".into(),
            recall_count: 0,
        };
        let findings = scan_for_secrets(&mem);
        assert!(!findings.is_empty(), "should detect GitHub token");
        assert!(
            findings.iter().any(|f| f.kind.contains("GitHub")),
            "should flag GitHub token"
        );
    }

    #[test]
    fn secret_scan_clean_memory_no_false_positive() {
        let mem = DurableMemory {
            name: "architecture".into(),
            description: "System architecture decision".into(),
            memory_type: MemoryType::Project,
            content: "We decided to use PostgreSQL for the main database.".into(),
            source_session_id: "s1".into(),
            confidence: 0.9,
            last_seen: "2026-06-14T00:00:00Z".into(),
            recall_count: 0,
        };
        let findings = scan_for_secrets(&mem);
        assert!(findings.is_empty(), "clean memory should have no findings");
    }

    #[test]
    fn scan_store_for_secrets_scans_all_memories() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("teams").join("test-team").join("memory");
        let store = TeamMemoryStore::with_dir(dir);
        store.ensure_dir().unwrap();

        store
            .write_team_memory(DurableMemory {
                name: "clean".into(),
                description: "harmless".into(),
                memory_type: MemoryType::Project,
                content: "Just a normal decision.".into(),
                source_session_id: "s1".into(),
                confidence: 0.9,
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            })
            .unwrap();

        store
            .write_team_memory(DurableMemory {
                name: "leaky".into(),
                description: "has a secret".into(),
                memory_type: MemoryType::Reference,
                content: "token = 'ghp_abc123def456ghi789jkl012mno345pqr'".into(),
                source_session_id: "s1".into(),
                confidence: 0.9,
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            })
            .unwrap();

        let findings = scan_store_for_secrets(&store);
        assert!(!findings.is_empty(), "should find secrets in store");
    }

    #[test]
    fn build_combined_memory_prompt_shows_both_sections() {
        let tmp = TempDir::new().unwrap();
        let personal_dir = tmp.path().join("personal").join("memory");
        let team_dir = tmp.path().join("teams").join("test-team").join("memory");

        let personal_store =
            MemoryStore::new(personal_dir, tmp.path().join("local").join("memory"));
        let team_store = TeamMemoryStore::with_dir(team_dir);
        team_store.ensure_dir().unwrap();

        // Write a team memory so it shows in the index
        team_store
            .write_team_memory(DurableMemory {
                name: "team-fact".into(),
                description: "Important team fact".into(),
                memory_type: MemoryType::Reference,
                content: "Shared knowledge".into(),
                source_session_id: "s1".into(),
                confidence: 0.9,
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            })
            .unwrap();

        let prompt = team_store.build_combined_memory_prompt(&personal_store);

        assert!(
            prompt.contains("Your personal memory"),
            "should have personal section"
        );
        assert!(
            prompt.contains("Team shared memory"),
            "should have team section"
        );
    }

    #[test]
    fn dedup_personal_takes_precedence_over_team() {
        let tmp = TempDir::new().unwrap();
        let personal_dir = tmp.path().join("personal").join("memory");
        let team_dir = tmp.path().join("teams").join("test-team").join("memory");

        let personal_store = MemoryStore::new(
            personal_dir.clone(),
            tmp.path().join("local").join("memory"),
        );
        let team_store = TeamMemoryStore::with_dir(team_dir);
        team_store.ensure_dir().unwrap();

        // Write a team memory named "config"
        team_store
            .write_team_memory(DurableMemory {
                name: "config".into(),
                description: "Team config".into(),
                memory_type: MemoryType::Reference,
                content: "Team config content".into(),
                source_session_id: "s1".into(),
                confidence: 0.9,
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            })
            .unwrap();

        // Write a personal memory with the same name
        personal_store
            .persist_batch(vec![DurableMemory {
                name: "config".into(),
                description: "Personal config".into(),
                memory_type: MemoryType::Project,
                content: "Personal config content".into(),
                source_session_id: "s1".into(),
                confidence: 0.9,
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            }])
            .unwrap();

        let prompt = team_store.build_combined_memory_prompt(&personal_store);

        // Personal memory should still reference "config"
        assert!(
            prompt.contains("Personal config"),
            "should have personal memory"
        );
        // Team memory "config" should be hidden (dedup)
        assert!(
            !prompt.contains("Team config"),
            "team memory with same name should be hidden"
        );
    }

    #[test]
    fn inner_store_delegates_correctly() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("teams").join("test-team").join("memory");
        let store = TeamMemoryStore::with_dir(dir);
        store.ensure_dir().unwrap();

        // Verify inner() returns the same store
        let inner = store.inner();
        let loaded = inner.load_all();
        assert!(loaded.is_empty());
    }
    async fn save(tool: &TeamMemorySaveTool, input: serde_json::Value) -> ToolResult {
        let ctx = platform::tool_context_for_test(std::env::temp_dir());
        if !tool.validate_input(&input, &ctx).await.is_ok() {
            return ToolResult::error_text("invalid");
        }
        tool.call(input, ctx, ProgressSender::noop("test"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_save_tool_writes_an_entry_the_read_tool_returns() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(TeamMemoryStore::for_team_dir(tmp.path()));
        let saver = TeamMemorySaveTool::new(store.clone(), "team-x/alice");
        let result = save(
            &saver,
            serde_json::json!({
                "name": "ci-dashboard",
                "description": "Where CI results live",
                "type": "reference",
                "content": "Build results are on the team's CI board."
            }),
        )
        .await;
        assert!(!result.is_error);
        assert!(store
            .load_index()
            .contains("[ci-dashboard](ci-dashboard.md)"));

        let reader = TeamMemoryReadTool::new(store);
        let ctx = platform::tool_context_for_test(tmp.path().to_path_buf());
        let read = reader
            .call(
                serde_json::json!({"name": "ci-dashboard"}),
                ctx,
                ProgressSender::noop("test"),
            )
            .await
            .unwrap();
        let ToolResultContent::Text(text) = read.content else {
            panic!("expected text")
        };
        assert!(text.contains("CI board") && text.contains("reference"));
    }

    #[tokio::test]
    async fn the_save_tool_refuses_secrets_and_bad_names() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(TeamMemoryStore::for_team_dir(tmp.path()));
        let saver = TeamMemorySaveTool::new(store.clone(), "team-x/alice");
        let secret = save(
            &saver,
            serde_json::json!({
                "name": "deploy-key",
                "description": "key",
                "content": "api_key = abcdefghijklmnop1234"
            }),
        )
        .await;
        assert!(secret.is_error);
        let bad_name = save(
            &saver,
            serde_json::json!({"name": "Has Spaces", "description": "d", "content": "c"}),
        )
        .await;
        assert!(bad_name.is_error);
        assert!(store.load_team_memories().is_empty());
    }

    async fn states_a_contained_call(tool: &dyn base::tool::Tool, input: serde_json::Value) {
        use base::interface::permission::Reach;
        let ctx = platform::tool_context_for_test(std::path::PathBuf::from("/tmp"));
        let stated = tool.permission_facts(&input, &ctx).await;
        assert!(
            matches!(
                stated.reach,
                Reach::SessionOnly | Reach::Delegates | Reach::Operator | Reach::Inside
            ),
            "{}: {stated:?}",
            tool.name()
        );
        assert_eq!(stated.runs, None, "{}", tool.name());
    }

    #[tokio::test]
    async fn the_team_memory_tools_touch_only_the_session() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(TeamMemoryStore::with_dir(tmp.path().join("memory")));
        states_a_contained_call(
            &TeamMemorySaveTool::new(store.clone(), "t/a"),
            serde_json::json!({}),
        )
        .await;
        states_a_contained_call(&TeamMemoryReadTool::new(store), serde_json::json!({})).await;
    }
}
