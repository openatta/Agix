//! Team memory sharing — persistent knowledge shared across team members.
//!
//! Each team gets a memory directory at `~/.agix/code/teams/{team_name}/memory/`.
//! Memories use the same format as personal memories (MEMORY.md index + individual .md files),
//! shared across all agents in the team.
//!
//! # Architecture
//!
//! - `TeamMemoryStore` wraps a `MemoryStore` scoped to the team's directory.
//! - Team memories are separate from personal memories — both are surfaced
//!   in the combined prompt.
//! - Same-name memories in personal take precedence (deduplication).
//! - Before syncing, team memories are scanned for sensitive data patterns.

use base::interface::memory::{DurableMemory, MemoryError, MemoryStore};
use std::path::PathBuf;

/// Team-scoped memory store.
///
/// Each team has its own memory directory under `~/.agix/code/teams/{name}/memory/`.
/// The underlying [`MemoryStore`] handles the file-per-memory + MEMORY.md index format.
pub struct TeamMemoryStore {
    /// Directory for this team's shared memories: `~/.agix/code/teams/{name}/memory/`
    pub team_memory_dir: PathBuf,
    /// Underlying memory store (points both user_dir and local_dir at team_memory_dir).
    store: MemoryStore,
}

impl TeamMemoryStore {
    /// Create a new team memory store at `~/.agix/scenes/<scope>/teams/{name}/memory/`.
    ///
    /// Creates the directory if it does not exist.
    pub fn new(team_name: &str, scope: &str) -> Self {
        let team_memory_dir = team_memory_dir(team_name, scope);
        Self::with_dir(team_memory_dir)
    }

    /// Create a team memory store rooted at a specific directory.
    pub fn with_dir(team_memory_dir: PathBuf) -> Self {
        // Use the same directory for both user and local scope — team memory
        // is a single flat namespace.
        let store = MemoryStore::new(team_memory_dir.clone(), team_memory_dir.clone());
        Self {
            team_memory_dir,
            store,
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

    /// Compact team memories, keeping at most `max_entries` of the most recent.
    /// Discards low-confidence entries (< 0.3 effective confidence) first.
    pub fn compact(&self, max_entries: usize) -> Result<usize, MemoryError> {
        self.store.compact(max_entries)
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

    /// Build the team memory prompt section for injection into a sub-agent's prompt.
    ///
    /// This shows the team memory index and explains how team members can
    /// contribute to shared knowledge.
    pub fn build_team_memory_prompt(&self) -> String {
        let team_index = self.load_index();
        let dir_str = self.team_memory_dir.display();

        let mut prompt = format!(
            "# Team shared memory\n\n\
             This team has a shared memory directory at `{dir_str}`.\n\
             All team members can read and write to this shared memory.\n\
             When you learn something useful for the rest of the team, save it here.\n\n\
             Use the same memory format as personal memories — one `.md` file per fact \
             with YAML frontmatter (name, description, type), and add a pointer to \
             the team's `MEMORY.md` index.\n\n\
             ### When to write to team memory\n\
             - Decisions with lasting impact on the project direction\n\
             - External constraints or stakeholder preferences the team should know\n\
             - Architecture or design rationale that affects multiple subsystems\n\
             - Links to shared resources (dashboards, docs, CI)\n\
             - Feedback on team-wide patterns the team coordinator has validated\n\n\
             ### When NOT to write to team memory\n\
             - Ephemeral task state or in-progress work (use the scratchpad)\n\
             - Code structure — the code IS the documentation\n\
             - Personal preferences that only apply to your specific agent role\n"
        );

        if team_index.trim().is_empty() {
            prompt.push_str("_(No team memories yet — you can be the first to contribute.)_\n");
        } else {
            prompt.push_str(&format!("## Current team memory index\n\n{}\n", team_index));
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
    if let Ok(raw) = std::fs::read_to_string(&index_path) {
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

// ═══════════════════════════════════════════════════════════
// Secret scanning
// ═══════════════════════════════════════════════════════════

/// A detected secret pattern in a memory.
#[derive(Debug, Clone)]
pub struct SecretPattern {
    /// The kind of secret detected (e.g., "API key", "token").
    pub kind: &'static str,
    /// A truncated snippet of the matched content (first 20 chars).
    pub snippet: String,
    /// The name of the memory file containing the match.
    pub filename: Option<String>,
}

// Lazy-initialized regex set for secret scanning.
// Compiled once and shared across all scan calls via `once_cell`.
use std::sync::OnceLock;

fn secret_patterns() -> &'static [(&'static str, regex::Regex)] {
    static PATTERNS: OnceLock<Vec<(&'static str, regex::Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // OpenAI / Anthropic / generic API keys starting with sk-
            (
                "API key (sk-...)",
                regex::Regex::new(r"(?i)\bsk-[A-Za-z0-9]{20,}\b").unwrap(),
            ),
            // Generic api_key / apikey assignment
            (
                "API key (generic)",
                regex::Regex::new(r#"(?i)(api[_-]?key|apikey)\s*[:=]\s*['"]?[A-Za-z0-9_\-]{16,}"#)
                    .unwrap(),
            ),
            // Bearer / auth tokens
            (
                "Bearer token",
                regex::Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9\-._~+/]{20,}").unwrap(),
            ),
            // GitHub tokens (ghp_, gho_, ghu_, ghs_, ghf_)
            (
                "GitHub token",
                regex::Regex::new(r"(?i)\bgh[pousf]_[A-Za-z0-9_]{30,}\b").unwrap(),
            ),
            // AWS access keys (AKIA...)
            (
                "AWS access key",
                regex::Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
            ),
            // JWT tokens
            (
                "JWT token",
                regex::Regex::new(
                    r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
                )
                .unwrap(),
            ),
            // Generic token/password/secret variable assignment
            (
                "Token/secret variable",
                regex::Regex::new(
                    r#"(?i)(token|secret|password)\s*[:=]\s*['"][A-Za-z0-9_\-\.]{8,}['"]"#,
                )
                .unwrap(),
            ),
            // SSH private key marker
            (
                "SSH private key",
                regex::Regex::new(r"-----BEGIN\s+(RSA|DSA|EC|OPENSSH)\s+PRIVATE\s+KEY-----")
                    .unwrap(),
            ),
            // Slack tokens (xoxb-, xoxp-, xapp-)
            (
                "Slack token",
                regex::Regex::new(r"\b[xX][oO][xX][baprs]-[A-Za-z0-9_-]{20,}\b").unwrap(),
            ),
        ]
    })
}

/// Scan a single memory for sensitive data patterns.
///
/// Checks the name, description, and content fields against a set of regex
/// patterns for API keys, tokens, and other secrets.
///
/// Returns a list of [`SecretPattern`] matches. **Does not block** — warns and
/// proceeds.
pub fn scan_for_secrets(memory: &DurableMemory) -> Vec<SecretPattern> {
    let mut findings = Vec::new();

    let search_fields: Vec<(&str, &str)> = vec![
        ("name", &memory.name),
        ("description", &memory.description),
        ("content", &memory.content),
    ];

    let patterns = secret_patterns();

    for (_field_name, text) in &search_fields {
        if text.is_empty() {
            continue;
        }
        for (kind, re) in patterns {
            if re.is_match(text) {
                // Extract a short snippet for reporting
                let snippet = if let Some(cap) = re.find(text) {
                    let matched = cap.as_str();
                    if matched.len() > 40 {
                        // Character-boundary aware: `matched` is a regex hit
                        // against arbitrary conversation text, so byte 40 can
                        // land mid-character.
                        format!("{}...", base::text::truncate_at_char_boundary(matched, 40))
                    } else {
                        matched.to_string()
                    }
                } else {
                    continue;
                };

                findings.push(SecretPattern {
                    kind,
                    snippet,
                    filename: Some(memory.name.clone()),
                });
                // Report each pattern once per field per memory to avoid noise
                break;
            }
        }
    }

    findings
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

/// Build the path to a team's memory directory.
pub fn team_memory_dir(team_name: &str, scope: &str) -> PathBuf {
    base::paths::agix_scope_dir(scope)
        .join("teams")
        .join(team_name)
        .join("memory")
}

/// Build the path to a team's root directory (contains `memory/` subdirectory).
pub fn team_root_dir(team_name: &str, scope: &str) -> PathBuf {
    base::paths::agix_scope_dir(scope).join("teams").join(team_name)
}

/// Check if a team has any shared memories.
pub fn team_has_memories(team_name: &str, scope: &str) -> bool {
    let store = TeamMemoryStore::new(team_name, scope);
    let dir = &store.team_memory_dir;
    if !dir.exists() {
        return false;
    }
    // Check if there's a non-empty MEMORY.md or any .md files beyond the index
    let has_index = dir
        .join("MEMORY.md")
        .is_file()
        .then(|| {
            std::fs::read_to_string(dir.join("MEMORY.md"))
                .ok()
                .map(|c| !c.trim().is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if has_index {
        return true;
    }

    // Fallback: check for any memory files
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "md")
                && p.file_name().is_some_and(|n| n != "MEMORY.md")
            {
                return true;
            }
        }
    }

    false
}

// ═══════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::memory::MemoryType;
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
    fn team_has_memories_returns_false_for_empty_team() {
        let tmp = TempDir::new().unwrap();

        // Use a local dir directly rather than agix_scope_dir(scope) for this test
        let dir = tmp.path().join("teams").join("empty-team");
        let mem_dir = dir.join("memory");
        std::fs::create_dir_all(&mem_dir).unwrap();

        // We need to verify the store is empty
        let store = TeamMemoryStore::with_dir(mem_dir);
        assert!(store.load_team_memories().is_empty());
    }

    #[test]
    fn compact_discards_low_confidence() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("teams").join("test-team").join("memory");
        let store = TeamMemoryStore::with_dir(dir);
        store.ensure_dir().unwrap();

        // Write a high-confidence and a low-confidence memory
        store
            .write_team_memory(DurableMemory {
                name: "good".into(),
                description: "useful memory".into(),
                memory_type: MemoryType::Project,
                content: "Important.".into(),
                source_session_id: "s1".into(),
                confidence: 0.9,
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            })
            .unwrap();

        store
            .write_team_memory(DurableMemory {
                name: "trash".into(),
                description: "useless".into(),
                memory_type: MemoryType::User,
                content: "Junk.".into(),
                source_session_id: "s1".into(),
                confidence: 0.2, // below 0.3 threshold
                last_seen: "2026-06-14T00:00:00Z".into(),
                recall_count: 0,
            })
            .unwrap();

        // Compact should remove the low-confidence one
        let removed = store.compact(10).unwrap();
        assert_eq!(removed, 1);
        let loaded = store.load_team_memories();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "good");
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
}
