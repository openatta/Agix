//! Sub-agent summary — `AgentSummary` + `build_agent_summary`.
//!
//! The summary is a pure data structure derived from the sub-agent's final
//! message history; nothing about it depends on the AgentTool runtime, so it
//! lives in its own file.
//!
//! Used by:
//! - `AgentTool` to render the markdown back to the parent agent
//! - `TeamCreate` (multi-stage) to aggregate sub-agent outputs
//! - test consumers that just want to inspect a transcript

use crate::message::{ContentBlock, Message};

/// Structured summary of a sub-agent's full turn history.
///
/// Returned by `AgentTool::call` so the parent agent doesn't have to dig
/// through raw tool_use / tool_result blocks. Also embedded as
/// `structured_content` so downstream tools / hooks can read it.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentSummary {
    /// The user-given goal (from AgentInput.prompt).
    pub goal: String,
    /// Distinct tool names invoked by the sub-agent (in first-seen order).
    pub tools_used: Vec<String>,
    /// Total tool calls (counting duplicates).
    pub tool_call_count: u32,
    /// File paths the sub-agent wrote/edited (FileWrite + FileEdit + NotebookEdit).
    pub artifacts: Vec<String>,
    /// The sub-agent's final assistant text — its actual answer / report.
    pub final_answer: String,
    /// Followups the sub-agent itself flagged as unresolved (TodoWrite items
    /// left in `pending` / `in_progress` state at end of turn). Empty if no
    /// TodoWrite was used.
    pub unresolved_followups: Vec<String>,
}

impl AgentSummary {
    pub fn render_markdown(&self) -> String {
        let mut s = String::with_capacity(self.final_answer.len() + 512);
        s.push_str("## Agent summary\n\n");
        s.push_str(&format!(
            "**Goal**: {}\n\n",
            truncate_for_summary(&self.goal, 240)
        ));
        if !self.tools_used.is_empty() {
            s.push_str(&format!(
                "**Tools used** ({} call{}): {}\n\n",
                self.tool_call_count,
                if self.tool_call_count == 1 { "" } else { "s" },
                self.tools_used.join(", ")
            ));
        }
        if !self.artifacts.is_empty() {
            s.push_str("**Artifacts**:\n");
            for a in &self.artifacts {
                s.push_str(&format!("- {a}\n"));
            }
            s.push('\n');
        }
        if !self.unresolved_followups.is_empty() {
            s.push_str("**Unresolved followups**:\n");
            for u in &self.unresolved_followups {
                s.push_str(&format!("- {u}\n"));
            }
            s.push('\n');
        }
        s.push_str("---\n\n");
        s.push_str(&self.final_answer);
        if !self.final_answer.ends_with('\n') {
            s.push('\n');
        }
        s
    }
}

fn truncate_for_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Walk a sub-agent's final_messages and build the structured summary.
/// Public for testing.
pub fn build_agent_summary(goal: &str, messages: &[Message]) -> AgentSummary {
    let mut tools_seen: Vec<String> = Vec::new();
    let mut tool_count: u32 = 0;
    let mut artifacts: Vec<String> = Vec::new();
    let mut latest_todos: Vec<(String, String)> = Vec::new(); // (status, content)

    for m in messages {
        if let Message::Assistant { content, .. } = m {
            for block in content {
                if let ContentBlock::ToolUse { name, input, .. } = block {
                    tool_count += 1;
                    if !tools_seen.iter().any(|n| n == name) {
                        tools_seen.push(name.clone());
                    }
                    match name.as_str() {
                        "Write" | "Edit" | "NotebookEdit" => {
                            if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
                                if !artifacts.iter().any(|a| a == p) {
                                    artifacts.push(p.to_string());
                                }
                            }
                        }
                        "TodoWrite" => {
                            if let Some(arr) = input.get("todos").and_then(|v| v.as_array()) {
                                latest_todos = arr
                                    .iter()
                                    .filter_map(|t| {
                                        let status = t.get("status")?.as_str()?.to_string();
                                        let content = t.get("content")?.as_str()?.to_string();
                                        Some((status, content))
                                    })
                                    .collect();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Final assistant text — last assistant message's concatenated text blocks.
    let final_answer = messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant { content, .. } => {
                let mut s = String::new();
                for b in content {
                    if let ContentBlock::Text { text, .. } = b {
                        s.push_str(text);
                    }
                }
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            }
            _ => None,
        })
        .unwrap_or_else(|| "(subagent produced no text response)".to_string());

    let unresolved_followups = latest_todos
        .into_iter()
        .filter(|(status, _)| status != "completed")
        .map(|(_, content)| content)
        .collect();

    AgentSummary {
        goal: goal.to_string(),
        tools_used: tools_seen,
        tool_call_count: tool_count,
        artifacts,
        final_answer,
        unresolved_followups,
    }
}
