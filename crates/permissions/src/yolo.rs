//! YOLO classifier — aggressive auto-approval classifier for power users.
//!
//! The `YoloClassifier` implements [`AutoClassifier`] with very permissive
//! heuristics: it allows all read operations, safe bash commands, web fetches,
//! and in-project writes, only deferring to user judgment for destructive
//! or out-of-scope operations.

use crate::gate::{AutoClassifier, ClassifyDecision};
use async_trait::async_trait;
use serde_json::Value;

/// Aggressive auto-approval classifier.
///
/// Designed for the "yolo" permission mode (feature-gated). Automatically
/// allows tool calls that are known to be safe, deferring only destructive
/// or boundary-crossing operations to user judgment.
pub struct YoloClassifier;

#[async_trait]
impl AutoClassifier for YoloClassifier {
    /// Classify a tool call for auto-approval.
    ///
    /// Rules:
    /// - Read/Grep/Glob/LSP/ListFiles: always allow (read-only)
    /// - WebSearch/WebFetch: always allow
    /// - Bash: allow safe patterns (git, cargo, npm, ls, cat, echo, etc.);
    ///   defer destructive commands (rm, dd, mkfs, chmod, chown)
    /// - Edit/Write/NotebookEdit: allow within project directory (relative paths);
    ///   defer absolute paths or parent-directory traversal
    /// - Everything else: defer
    async fn classify(
        &self,
        tool_name: &str,
        _tool_description: &str,
        input: &Value,
    ) -> ClassifyDecision {
        match tool_name {
            "Read" | "Grep" | "Glob" | "ListFiles" | "Search" => ClassifyDecision::Allow {
                reason: "yolo: read-only operation".into(),
            },

            "WebSearch" | "WebFetch" | "Fetch" | "Http" => ClassifyDecision::Allow {
                reason: "yolo: network fetch always allowed".into(),
            },

            "Bash" | "PowerShell" | "Shell" => {
                let cmd = input
                    .get("command")
                    .or_else(|| input.get("cmd"))
                    .or_else(|| input.get("script"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let trimmed = cmd.trim();

                // Always allow trivial/empty commands
                if trimmed.is_empty() || trimmed.len() < 3 {
                    return ClassifyDecision::Allow {
                        reason: "yolo: trivial command".into(),
                    };
                }

                // Safe command patterns
                let safe_patterns = [
                    "git ",
                    "cargo ",
                    "npm ",
                    "yarn ",
                    "pnpm ",
                    "bun ",
                    "ls",
                    "cat ",
                    "echo ",
                    "head ",
                    "tail ",
                    "wc ",
                    "pwd",
                    "whoami",
                    "uname",
                    "date",
                    "which ",
                    "type ",
                    "mkdir -p",
                    "cp ",
                    "mv ",
                    "touch ",
                    "chmod +x",
                    "rustc",
                    "python",
                    "python3",
                    "node ",
                    "deno ",
                    "make ",
                    "cmake",
                    "go ",
                    "cargo test",
                    "npm test",
                    "docker ",
                    "docker-compose",
                ];
                if safe_patterns.iter().any(|p| trimmed.starts_with(p)) {
                    return ClassifyDecision::Allow {
                        reason: "yolo: safe command pattern".into(),
                    };
                }

                // Destructive patterns — defer to user
                let destructive_patterns = [
                    "rm ",
                    "rmdir ",
                    "dd ",
                    "mkfs",
                    "fdisk",
                    "parted",
                    "chmod 0",
                    "chown ",
                    "kill ",
                    "pkill ",
                    "sudo ",
                    "passwd",
                    "shutdown",
                    "reboot",
                    "init ",
                    "> /dev/",
                    "> /etc/",
                    ":(){ :|:& };:",
                    "wget ",
                    "curl -",
                    "curl --",
                    "chattr",
                ];
                if destructive_patterns.iter().any(|p| trimmed.contains(p)) {
                    return ClassifyDecision::Defer;
                }

                // Unknown bash commands — allow aggressively (yolo mode)
                ClassifyDecision::Allow {
                    reason: "yolo: bash command".into(),
                }
            }

            "Edit" | "Write" | "NotebookEdit" | "FileWrite" | "FileEdit" => {
                let path = input
                    .get("file_path")
                    .or_else(|| input.get("path"))
                    .or_else(|| input.get("target_file"))
                    .or_else(|| input.get("filename"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                // Allow relative paths (within project directory)
                if !path.starts_with('/') && !path.contains("..") {
                    return ClassifyDecision::Allow {
                        reason: "yolo: in-project file write".into(),
                    };
                }
                // Absolute paths under /tmp, /var/tmp are safe
                if path.starts_with("/tmp") || path.starts_with("/var/tmp") {
                    return ClassifyDecision::Allow {
                        reason: "yolo: temp directory write".into(),
                    };
                }
                // Everything else (absolute path, parent traversal) defer
                ClassifyDecision::Defer
            }

            _ => ClassifyDecision::Defer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn classify(tool: &str, input: Value) -> ClassifyDecision {
        let classifier = YoloClassifier;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(classifier.classify(tool, "", &input))
    }

    #[test]
    fn allows_read() {
        let d = classify("Read", json!({"file_path": "/etc/passwd"}));
        assert!(matches!(d, ClassifyDecision::Allow { .. }));
    }

    #[test]
    fn allows_grep() {
        let d = classify("Grep", json!({"pattern": "fn main"}));
        assert!(matches!(d, ClassifyDecision::Allow { .. }));
    }

    #[test]
    fn allows_safe_bash() {
        let d = classify("Bash", json!({"command": "git status"}));
        assert!(matches!(d, ClassifyDecision::Allow { .. }));
    }

    #[test]
    fn defers_destructive_bash() {
        let d = classify("Bash", json!({"command": "rm -rf /"}));
        assert!(matches!(d, ClassifyDecision::Defer));
    }

    #[test]
    fn allows_in_project_write() {
        let d = classify("Write", json!({"file_path": "src/main.rs"}));
        assert!(matches!(d, ClassifyDecision::Allow { .. }));
    }

    #[test]
    fn defers_out_of_project_write() {
        let d = classify("Write", json!({"file_path": "/etc/passwd"}));
        assert!(matches!(d, ClassifyDecision::Defer));
    }

    #[test]
    fn allows_web_fetch() {
        let d = classify("WebFetch", json!({"url": "https://example.com"}));
        assert!(matches!(d, ClassifyDecision::Allow { .. }));
    }

    #[test]
    fn defers_unknown_tool() {
        let d = classify("DBQuery", json!({"query": "SELECT *"}));
        assert!(matches!(d, ClassifyDecision::Defer));
    }
}
